//! Persistent, isolated loopback browser routes; authority is checked per request.
use crate::Client;
use iroh::EndpointId;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};
#[path = "gateway_http.rs"]
pub(crate) mod http;
#[path = "gateway_store.rs"]
mod store;
// Reserved only to strip obsolete Wispnet credentials from legacy browser cookies.
const COOKIE: &str = "_wispnet_session";
struct Worker {
    binding: store::Binding,
    cancel: tokio::sync::watch::Sender<()>,
    route: Arc<Route>,
}
pub struct Gateways {
    enabled: AtomicBool,
    workers: Mutex<Vec<Worker>>,
    registry: Mutex<Option<JoinHandle<()>>>,
    pub(crate) connections: Arc<Semaphore>,
    pub(crate) settings_lock: Mutex<()>,
}
impl Default for Gateways {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
            registry: Mutex::new(None),
            connections: Arc::new(Semaphore::new(32)),
            settings_lock: Mutex::new(()),
        }
    }
}
impl Gateways {
    pub async fn settings(&self, client: &Client) -> Result<serde_json::Value, String> {
        let settings = crate::access_settings::Settings::load(&client.state)?;
        let routes = store::load(&client.state, client.endpoint.id())?;
        Ok(
            serde_json::json!({"settings":settings,"routes":routes.iter().map(|b|serde_json::json!({"host":b.peer,"service":b.service,"port":b.port,"url":b.url()})).collect::<Vec<_>>(),"enabled":self.is_enabled()}),
        )
    }
    pub async fn configure(
        &self,
        client: Arc<Client>,
        settings: crate::access_settings::Settings,
    ) -> Result<serde_json::Value, String> {
        let _lock = self.settings_lock.lock().await;
        settings.validate()?;
        let previous = crate::access_settings::Settings::load(&client.state)?;
        if previous == settings {
            return self.settings(&client).await;
        }
        let routes = store::load(&client.state, client.endpoint.id())?;
        // Check requested ports before stopping working listeners or changing state.
        let probe = if !self.is_enabled() || previous.registry_port != settings.registry_port {
            Some(
                TcpListener::bind(("127.0.0.1", settings.registry_port))
                    .await
                    .map_err(|_| {
                        format!(
                            "Port {} unavailable; choose another Wispnet port",
                            settings.registry_port
                        )
                    })?,
            )
        } else {
            None
        };
        let enabled = self.is_enabled();
        settings.save(&client.state)?;
        if enabled {
            self.interrupt().await;
            drop(probe);
            if let Err(error) = self.restore(client.clone()).await {
                self.interrupt().await;
                previous.save(&client.state)?;
                store::save(&client.state, client.endpoint.id(), &routes)?;
                if self.restore(client.clone()).await.is_err() {
                    self.stop().await;
                    return Err(format!(
                        "{error}; could not restore previous listeners; access stopped"
                    ));
                }
                return Err(error);
            }
        }
        self.settings(&client).await
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }
    pub async fn stop(&self) {
        self.enabled.store(false, Ordering::Release);
        self.interrupt().await;
    }
    /// Interrupt sockets without replaying requests or discarding saved addresses.
    pub async fn interrupt(&self) -> usize {
        let mut guard = self.workers.lock().await;
        let interrupted = guard
            .iter()
            .map(|w| w.route.active_requests.load(Ordering::Acquire))
            .sum();
        if let Some(task) = self.registry.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        guard.clear();
        interrupted
    }
    /// Remove this host's listeners and bookmarks while preserving other hosts.
    pub async fn forget(&self, client: &Client, peer: EndpointId) -> Result<(), String> {
        let mut workers = self.workers.lock().await;
        let mut saved = store::load(&client.state, client.endpoint.id())?;
        let mut settings = crate::access_settings::Settings::load(&client.state)?;
        settings.service_ports.retain(|route| route.host != peer);
        settings.save(&client.state)?;
        saved.retain(|binding| binding.peer != peer);
        store::save(&client.state, client.endpoint.id(), &saved)?;
        workers.retain(|w| w.binding.peer != peer);
        Ok(())
    }
    /// Rebind exactly the saved ports. Never silently move a bookmark to a new port.
    pub async fn restore(&self, client: Arc<Client>) -> Result<(), String> {
        self.start_listener(client.clone()).await?;
        for binding in store::load(&client.state, client.endpoint.id())? {
            self.open(
                client.clone(),
                binding.peer,
                binding.service,
                binding.incarnation,
            )
            .await?;
        }
        Ok(())
    }
    async fn start_listener(&self, client: Arc<Client>) -> Result<(), String> {
        let mut listener = self.registry.lock().await;
        if listener.as_ref().is_none_or(|task| task.is_finished()) {
            *listener = Some(
                crate::service_registry::start(
                    client.clone(),
                    crate::access_settings::Settings::load(&client.state)?.registry_port,
                )
                .await?,
            );
        }
        Ok(())
    }
    pub(crate) async fn handle(&self, mut socket: TcpStream, request: Request) -> Result<(), ()> {
        let selected = {
            let workers = self.workers.lock().await;
            workers
                .iter()
                .find(|w| w.route.authorities.iter().any(|a| request.safe(a)))
                .map(|w| (w.route.clone(), w.cancel.subscribe()))
        };
        let Some((route, mut cancel)) = selected else {
            return response(
                &mut socket,
                "421 Misdirected Request",
                "Unknown local authority",
                "",
            )
            .await;
        };
        tokio::select! {
            _ = cancel.changed() => Ok(()),
            result = route.handle(socket, request) => result,
        }
    }
    pub async fn open(
        &self,
        client: Arc<Client>,
        peer: EndpointId,
        service: String,
        incarnation: String,
    ) -> Result<String, String> {
        self.open_named(client, peer, service, incarnation, None)
            .await
    }
    pub async fn open_named(
        &self,
        client: Arc<Client>,
        peer: EndpointId,
        service: String,
        incarnation: String,
        system_name: Option<&str>,
    ) -> Result<String, String> {
        if system_name.is_some_and(|s| !wispnet_core::enrollment::valid_name(s)) {
            return Err("Invalid system display name".into());
        }
        if !wispnet_core::protocol::valid_service_id(&service)
            || !wispnet_core::enrollment::valid_code(&incarnation)
        {
            return Err("Invalid browser route".into());
        }
        let mut workers = self.workers.lock().await;
        if !self.is_enabled() {
            return Err("Start browser access first".into());
        }
        let mut saved = store::load(&client.state, client.endpoint.id())?;
        let index = saved
            .iter()
            .position(|b| b.peer == peer && b.service == service);
        if index.is_none() && saved.len() >= 16 {
            return Err("Too many saved browser services".into());
        }
        let old = index.map(|i| &saved[i]);
        let id_component = format!("-{}-", &peer.to_string()[..12]);
        let hostname = match old {
            Some(binding) if system_name.is_none() && binding.hostname.contains(&id_component) => {
                binding.hostname.clone()
            }
            _ => {
                let others: Vec<_> = saved
                    .iter()
                    .filter(|b| b.peer != peer || b.service != service)
                    .cloned()
                    .collect();
                store::readable_hostname(system_name, peer, &service, &others)
            }
        };
        if saved.iter().any(|binding| {
            (binding.peer != peer || binding.service != service)
                && binding.names().any(|name| name == &hostname)
        }) {
            return Err(
                "Service hostname conflicts with another saved route; rename the service".into(),
            );
        }
        let aliases = Vec::new();
        let settings = crate::access_settings::Settings::load(&client.state)?;
        let port = settings.registry_port;
        if let Some(worker) = workers.iter().find(|w| {
            w.binding.peer == peer
                && w.binding.service == service
                && w.binding.incarnation == incarnation
                && w.binding.hostname == hostname
                && w.binding.port == port
        }) {
            return Ok(worker.binding.url());
        }

        // Dropping the cancellation sender terminates streams of a replaced route.
        workers.retain(|w| w.binding.peer != peer || w.binding.service != service);
        let binding = store::Binding {
            peer,
            service,
            incarnation,
            hostname,
            aliases,
            port,
        };
        if let Some(i) = index {
            saved[i] = binding.clone();
        } else {
            saved.push(binding.clone());
        }
        store::save(&client.state, client.endpoint.id(), &saved)?;
        let local_binding = if peer == client.endpoint.id() {
            if let Some(authority) = &client.local_authority {
                authority
                    .services()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|s| {
                        s.id == binding.service
                            && s.incarnation == binding.incarnation
                            && s.kind == wispnet_core::host::ServiceKind::Http
                    })
                    .map(|s| s.backend)
            } else {
                None
            }
        } else {
            None
        };
        let route = Arc::new(Route {
            client,
            peer,
            service: binding.service.clone(),
            incarnation: binding.incarnation.clone(),
            local_binding,
            authorities: binding
                .names()
                .map(|name| format!("{}:{}", name, binding.port))
                .collect(),
            active_requests: AtomicUsize::new(0),
        });
        let (cancel, _) = tokio::sync::watch::channel(());
        let url = binding.url();
        workers.push(Worker {
            binding,
            cancel,
            route,
        });
        Ok(url)
    }
}
trait ServiceIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ServiceIo for T {}

struct ActiveRequest<'a>(&'a AtomicUsize);
impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Route {
    active_requests: AtomicUsize,
    client: Arc<Client>,
    peer: EndpointId,
    service: String,
    incarnation: String,
    local_binding: Option<std::net::SocketAddr>,
    authorities: Vec<String>,
}
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    headers: Vec<(String, String)>,
}
impl Request {
    pub(crate) fn values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }
    pub(crate) fn one(&self, name: &str) -> Option<&str> {
        let values = self.values(name);
        if values.len() == 1 {
            Some(values[0])
        } else {
            None
        }
    }
    pub(crate) fn safe(&self, authority: &str) -> bool {
        self.values("host") == [authority]
            && self.path.starts_with('/')
            && !self.path.starts_with("//")
            && !self.path.contains(['\r', '\n', '\\'])
            && self.method.bytes().all(|b| b.is_ascii_uppercase())
    }
    pub(crate) fn native(&self) -> bool {
        [
            "origin",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-dest",
        ]
        .iter()
        .all(|h| self.values(h).is_empty())
    }
    pub(crate) fn allowed_website<'a>(
        &'a self,
        _settings: &crate::access_settings::Settings,
    ) -> Option<&'a str> {
        if [
            "origin",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-dest",
        ]
        .iter()
        .any(|h| self.values(h).len() > 1)
        {
            return None;
        }
        self.one("origin").filter(|o| {
            *o == "null"
                || url::Url::parse(o).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https") && u.origin().ascii_serialization() == *o
                })
        })
    }
    fn same_origin(&self, origin: &str) -> bool {
        self.values("origin").iter().all(|v| *v == origin)
            && self
                .one("sec-fetch-site")
                .is_none_or(|s| s == "none" || s == "same-origin")
            && self.values("origin").len() <= 1
            && self.values("sec-fetch-site").len() <= 1
            && self.values("sec-fetch-mode").len() <= 1
            && self.values("sec-fetch-dest").len() <= 1
    }
}
pub(crate) async fn headers(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, ()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        while bytes.len() < 32768 {
            bytes.push(stream.read_u8().await.map_err(|_| ())?);
            if bytes.ends_with(b"\r\n\r\n") {
                return Ok(bytes);
            }
        }
        Err(())
    })
    .await
    .map_err(|_| ())?
}
pub(crate) fn parse(bytes: &[u8]) -> Result<Request, ()> {
    let mut storage = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut storage);
    if !request.parse(bytes).map_err(|_| ())?.is_complete() || request.version != Some(1) {
        return Err(());
    }
    Ok(Request {
        method: request.method.ok_or(())?.into(),
        path: request.path.ok_or(())?.into(),
        headers: request
            .headers
            .iter()
            .map(|h| {
                Ok((
                    h.name.to_string(),
                    std::str::from_utf8(h.value).map_err(|_| ())?.to_string(),
                ))
            })
            .collect::<Result<_, ()>>()?,
    })
}
pub(crate) async fn response(
    socket: &mut TcpStream,
    status: &str,
    body: &str,
    extra: &str,
) -> Result<(), ()> {
    let value = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n{extra}\r\n{body}",
        body.len()
    );
    socket.write_all(value.as_bytes()).await.map_err(|_| ())
}
impl Route {
    async fn handle(&self, mut socket: TcpStream, request: Request) -> Result<(), ()> {
        if !self
            .authorities
            .iter()
            .any(|authority| request.safe(authority))
        {
            return response(
                &mut socket,
                "421 Misdirected Request",
                "Unknown local authority",
                "",
            )
            .await;
        }
        let origin = format!("http://{}", request.one("host").ok_or(())?);
        let (websocket, length) = match http::framing(&request) {
            Ok(result) => result,
            Err(()) => {
                return response(
                    &mut socket,
                    "400 Bad Request",
                    "Unsupported request framing",
                    "",
                )
                .await;
            }
        };
        if length > 16 * 1024 * 1024 {
            return response(&mut socket, "413 Content Too Large", "", "").await;
        }
        // Top-level links (including Android app intents) need no browser session.
        // Frames and cross-site requests without Origin remain restricted.
        let navigation = request.method == "GET"
            && length == 0
            && !websocket
            && request.one("sec-fetch-mode") == Some("navigate")
            && request.one("sec-fetch-dest") == Some("document")
            && request.values("origin").is_empty()
            && request.values("sec-fetch-site").len() <= 1;
        let settings =
            crate::access_settings::Settings::load(&self.client.state).map_err(|_| ())?;
        let allowed_origin = request.allowed_website(&settings);
        let api_access = request.native() || allowed_origin.is_some();
        let cors = allowed_origin
            .map(|o| format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"))
            .unwrap_or_default();
        if !cors.is_empty()
            && request.method == "OPTIONS"
            && length == 0
            && !request.path.starts_with("/_wispnet/")
        {
            return response(&mut socket,"204 No Content","",&format!("{cors}Access-Control-Allow-Methods: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Max-Age: 60\r\n")).await;
        }
        if !request.same_origin(&origin) && cors.is_empty() && !navigation {
            return response(
                &mut socket,
                "403 Forbidden",
                "Cross-origin request denied",
                "",
            )
            .await;
        }
        // Retired bootstrap paths must never forward local credentials upstream.
        if request.path.starts_with("/_wispnet/") || request.path.starts_with("/_wipsnet/") {
            return response(&mut socket, "404 Not Found", "", "").await;
        }
        if !request.values("service-worker").is_empty()
            || request.one("sec-fetch-dest") == Some("serviceworker")
        {
            return response(
                &mut socket,
                "403 Forbidden",
                "Service workers are unsupported",
                "",
            )
            .await;
        }
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
            .contains(&request.method.as_str())
        {
            return response(
                &mut socket,
                "405 Method Not Allowed",
                "Unsupported HTTP method",
                "",
            )
            .await;
        }
        if http::cookies(&request).is_err() {
            return response(&mut socket, "400 Bad Request", "Invalid cookies", "").await;
        }
        if (websocket || !["GET", "HEAD"].contains(&request.method.as_str()))
            && !api_access
            && request.one("origin") != Some(origin.as_str())
        {
            return response(
                &mut socket,
                "403 Forbidden",
                "Same-origin caller required",
                "",
            )
            .await;
        }
        self.active_requests.fetch_add(1, Ordering::AcqRel);
        let _active_request = ActiveRequest(&self.active_requests);
        let local = if self.peer == self.client.endpoint.id() {
            self.client.local_authority.as_ref()
        } else {
            None
        };
        // Resolve the binding from current durable owner state, never from UI/cache input.
        let binding = if let Some(authority) = local {
            match authority.services().await {
                Ok(services) => services
                    .into_iter()
                    .find(|s| {
                        s.id == self.service
                            && s.incarnation == self.incarnation
                            && s.kind == wispnet_core::host::ServiceKind::Http
                            && Some(s.backend) == self.local_binding
                    })
                    .map(|s| s.backend),
                Err(_) => None,
            }
        } else {
            None
        };
        let _connection;
        let mut service: Box<dyn ServiceIo> = if local.is_some() {
            let Some(binding) = binding else {
                return response(
                    &mut socket,
                    "403 Forbidden",
                    "Local service unavailable",
                    "",
                )
                .await;
            };
            match TcpStream::connect(binding).await {
                Ok(stream) => Box::new(stream),
                Err(_) => {
                    return response(&mut socket, "502 Bad Gateway", "Backend unavailable", "")
                        .await;
                }
            }
        } else {
            if !matches!(self.client.saved_host(self.peer).await, Ok(Some(_))) {
                return response(
                    &mut socket,
                    "403 Forbidden",
                    "Pairing no longer available",
                    "",
                )
                .await;
            }
            _connection = match self
                .client
                .peer_connection(self.peer, crate::connections::Protocol::Http)
                .await
            {
                Ok(c) => c,
                Err(_) => {
                    return response(&mut socket, "502 Bad Gateway", "Host unavailable", "").await;
                }
            };
            match wispnet_agent::tunnel::open_service_bound(
                &_connection,
                &self.service,
                &self.incarnation,
            )
            .await
            {
                Ok(stream) => {
                    self.client.record_telemetry(self.peer, &_connection);
                    Box::new(stream)
                }
                Err(_) => {
                    return response(
                        &mut socket,
                        "403 Forbidden",
                        "Host denied service access",
                        "",
                    )
                    .await;
                }
            }
        };
        let forwarding = async {
            let mut outgoing = format!("{} {} HTTP/1.1\r\n", request.method, request.path);
            for (name, value) in &request.headers {
                let lower = name.to_ascii_lowercase();
                if lower == "cookie" {
                    let clean = value
                        .split(';')
                        .map(str::trim)
                        .filter(|c| c.split('=').next() != Some(COOKIE))
                        .collect::<Vec<_>>()
                        .join("; ");
                    if !clean.is_empty() {
                        outgoing.push_str(&format!("Cookie: {clean}\r\n"));
                    }
                    continue;
                }
                if [
                    "connection",
                    "keep-alive",
                    "te",
                    "trailer",
                    "proxy-authenticate",
                    "proxy-authorization",
                    "x-wispnet-access",
                    "proxy-connection",
                    "forwarded",
                    "sec-fetch-site",
                ]
                .contains(&lower.as_str())
                    || lower.starts_with("x-forwarded-")
                {
                    continue;
                }
                outgoing.push_str(&format!("{name}: {value}\r\n"));
            }
            outgoing.push_str(if websocket {
                "Connection: Upgrade\r\n\r\n"
            } else {
                "Connection: close\r\n\r\n"
            });
            service
                .write_all(outgoing.as_bytes())
                .await
                .map_err(|_| ())?;
            let copied = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::io::copy(&mut (&mut socket).take(length), &mut service),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            if copied != length {
                return Err(());
            }
            let mut head = match headers(&mut service)
                .await
                .and_then(|bytes| http::response(&bytes, &request, &origin, binding, websocket))
            {
                Ok(head) => head,
                Err(()) => {
                    return response(
                        &mut socket,
                        "502 Bad Gateway",
                        "Unsupported or malformed backend response",
                        "",
                    )
                    .await;
                }
            };
            if !cors.is_empty() && api_access {
                let end = head.bytes.len() - 2;
                head.bytes.splice(end..end, cors.bytes());
            }
            socket.write_all(&head.bytes).await.map_err(|_| ())?;
            if matches!(head.body, http::Body::WebSocket) {
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut service).await;
            } else {
                http::body(&mut service, &mut socket, head.body).await?;
            }
            Ok(())
        };
        let policy = async {
            let Some(authority) = local else {
                std::future::pending::<()>().await;
                return;
            };
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let current =
                    tokio::time::timeout(Duration::from_secs(1), authority.services()).await;
                if !matches!(current, Ok(Ok(ref services)) if services.iter().any(|s| s.id == self.service && s.incarnation == self.incarnation && s.kind == wispnet_core::host::ServiceKind::Http && Some(s.backend) == binding))
                {
                    return;
                }
            }
        };
        tokio::select! { result = forwarding => result, _ = policy => Err(()) }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    async fn raw(url: &str, method: &str, path: &str, extra: &str) -> String {
        let u = url::Url::parse(url).unwrap();
        let mut socket = TcpStream::connect(("127.0.0.1", u.port().unwrap()))
            .await
            .unwrap();
        socket
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\n{extra}\r\n",
                    u.host_str().unwrap(),
                    u.port().unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut data = Vec::new();
        socket.read_to_end(&mut data).await.unwrap();
        String::from_utf8(data).unwrap()
    }
    const NAV: &str =
        "Sec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n";
    async fn client(
        state: &std::path::Path,
        key: wispnet_core::identity::SecretKey,
    ) -> Arc<Client> {
        Client::new(
            key,
            state.into(),
            serde_json::from_value(serde_json::json!({"bind_addr":"127.0.0.1:0"})).unwrap(),
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn readable_names_include_host_identity_and_survive_duplicate_system_names() {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let key = wispnet_core::identity::SecretKey::generate();
        let c = client(state.path(), key.clone()).await;
        let peer = wispnet_core::identity::SecretKey::generate().public();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut binding = store::Binding {
            peer,
            service: "llama".into(),
            incarnation: "a".repeat(64),
            hostname: format!("w{}.localhost", "b".repeat(32)),
            port: listener.local_addr().unwrap().port(),
            aliases: vec![],
        };
        drop(listener);
        store::save(
            state.path(),
            c.endpoint.id(),
            std::slice::from_ref(&binding),
        )
        .unwrap();
        binding.port = crate::access_settings::Settings::load(state.path())
            .unwrap()
            .registry_port;
        let path = state.path().join("browser-routes.json");
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["version"] = 1.into();
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let url = c
            .gateway
            .open_named(
                c.clone(),
                peer,
                "llama".into(),
                "a".repeat(64),
                Some("J Strix"),
            )
            .await
            .unwrap();
        assert_eq!(
            url,
            format!(
                "http://j-strix-{}-llama.localhost:{}/",
                &peer.to_string()[..12],
                binding.port
            )
        );
        assert!(
            raw(&binding.url(), "GET", "/", NAV)
                .await
                .starts_with("HTTP/1.1 421")
        );
        assert!(raw(&url, "GET", "/", NAV).await.starts_with("HTTP/1.1 403"));
        let other_peer = wispnet_core::identity::SecretKey::generate().public();
        let other = c
            .gateway
            .open_named(
                c.clone(),
                other_peer,
                "llama".into(),
                "a".repeat(64),
                Some("J Strix"),
            )
            .await
            .unwrap();
        assert_eq!(
            url::Url::parse(&other).unwrap().host_str(),
            Some(format!("j-strix-{}-llama.localhost", &other_peer.to_string()[..12]).as_str())
        );
        c.gateway.stop().await;
        c.endpoint.close().await;
        drop(c);
        let c = client(state.path(), key).await;
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        assert_eq!(
            url.replace("j-strix-", "renamed-"),
            c.gateway
                .open_named(
                    c.clone(),
                    peer,
                    "llama".into(),
                    "a".repeat(64),
                    Some("Renamed")
                )
                .await
                .unwrap()
        );
        assert_eq!(
            other.replace("j-strix-", "renamed-"),
            c.gateway
                .open_named(
                    c.clone(),
                    other_peer,
                    "llama".into(),
                    "a".repeat(64),
                    Some("Renamed")
                )
                .await
                .unwrap()
        );
        assert!(
            raw(&binding.url(), "GET", "/", NAV)
                .await
                .starts_with("HTTP/1.1 421")
        );
        c.gateway.stop().await;
        c.endpoint.close().await;
    }
    #[tokio::test]
    async fn unpair_removes_only_target_routes_and_pin() {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let c = client(state.path(), wispnet_core::identity::SecretKey::generate()).await;
        let peer = wispnet_core::identity::SecretKey::generate().public();
        let other = wispnet_core::identity::SecretKey::generate().public();
        for host in [peer, other] {
            wispnet_agent::peers::save(
                state.path(),
                &wispnet_core::pairing::Bootstrap {
                    version: 1,
                    host,
                    relay_url: "https://relay.example/".into(),
                    direct_hint: None,
                },
            )
            .unwrap();
        }
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        for host in [peer, other] {
            c.gateway
                .open(c.clone(), host, "website".into(), "a".repeat(64))
                .await
                .unwrap();
        }
        let port = store::load(state.path(), c.endpoint.id())
            .unwrap()
            .into_iter()
            .find(|binding| binding.peer == peer)
            .unwrap()
            .port;
        let mut settings = crate::access_settings::Settings::load(state.path()).unwrap();
        settings.service_ports = store::load(state.path(), c.endpoint.id())
            .unwrap()
            .iter()
            .map(|b| crate::access_settings::ServicePort {
                host: b.peer,
                service: b.service.clone(),
                port: b.port,
            })
            .collect();
        settings.save(state.path()).unwrap();
        c.command(&serde_json::json!({"op":"unpair", "host":peer.to_string()}))
            .await
            .unwrap();
        assert!(
            wispnet_agent::peers::load(state.path(), peer)
                .unwrap()
                .is_none()
        );
        assert!(
            wispnet_agent::peers::load(state.path(), other)
                .unwrap()
                .is_some()
        );
        let workers = c.gateway.workers.lock().await;
        assert!(!workers.iter().any(|w| w.binding.peer == peer));
        assert!(workers.iter().any(|w| w.binding.peer == other));
        drop(workers);
        // Other hosts still share this listener; the removed hostname is rejected.
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok());
        let removed_url = format!(
            "http://system-{}-website.localhost:{port}/",
            &peer.to_string()[..12]
        );
        assert!(
            raw(&removed_url, "GET", "/", NAV)
                .await
                .starts_with("HTTP/1.1 421")
        );
        let saved = store::load(state.path(), c.endpoint.id()).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].peer, other);
        let settings = crate::access_settings::Settings::load(state.path()).unwrap();
        assert_eq!(settings.service_ports.len(), 1);
        assert_eq!(settings.service_ports[0].host, other);
        c.command(&serde_json::json!({"op":"network"}))
            .await
            .unwrap();

        assert!(
            c.command(&serde_json::json!({"op":"unpair", "host":c.endpoint.id().to_string()}))
                .await
                .is_err()
        );
        c.gateway.stop().await;
        c.endpoint.close().await;
    }
    #[tokio::test]
    async fn stable_bookmarks_restore_without_open_and_keep_host_authorization() {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }
        let key = wispnet_core::identity::SecretKey::generate();
        let c = client(state.path(), key.clone()).await;
        let peer = wispnet_core::identity::SecretKey::generate().public();
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let url = c
            .gateway
            .open(c.clone(), peer, "website".into(), "a".repeat(64))
            .await
            .unwrap();
        assert!(url.ends_with('/') && !url.contains('#'));
        // Even direct/app/same-origin navigations still require the host's grant.
        for headers in [
            NAV,
            "Sec-Fetch-Site: same-origin\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
            "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
            "Cookie: _wispnet_session=obsolete\r\nSec-Fetch-Site: same-origin\r\n",
        ] {
            let reply = raw(&url, "GET", "/bookmarked?x=1", headers).await;
            assert!(
                reply.starts_with("HTTP/1.1 403") && reply.contains("Pairing no longer available")
            );
            assert!(!reply.contains("Set-Cookie:"));
        }
        for path in ["/_wispnet/start", "/_wispnet/session", "/_wipsnet/session"] {
            assert!(
                raw(&url, "GET", path, NAV)
                    .await
                    .starts_with("HTTP/1.1 404")
            );
        }
        c.gateway.stop().await;
        let port = url::Url::parse(&url).unwrap().port().unwrap();
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
        let blocker = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        assert!(
            c.command(&serde_json::json!({"op":"enable"}))
                .await
                .is_err()
        );
        assert!(!c.gateway.is_enabled());
        drop(blocker);
        c.endpoint.close().await;
        drop(c);
        let c = client(state.path(), key).await;
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        // Do not call Open after restarting: the old URL is already listening.
        assert_eq!(
            c.gateway.settings(&c).await.unwrap()["routes"][0]["url"],
            url
        );
        assert!(
            raw(&url, "GET", "/", NAV)
                .await
                .contains("Pairing no longer available")
        );
        let recovered = c
            .command(&serde_json::json!({"op":"network"}))
            .await
            .unwrap();
        assert_eq!(recovered["interrupted_requests"], 0);
        assert!(
            raw(&url, "GET", "/", NAV)
                .await
                .contains("Pairing no longer available")
        );
        c.gateway.stop().await;
        c.endpoint.close().await;
        let different = client(state.path(), wispnet_core::identity::SecretKey::generate()).await;
        assert!(
            different
                .command(&serde_json::json!({"op":"enable"}))
                .await
                .is_err()
        );
        different.endpoint.close().await;
    }
    #[tokio::test]
    async fn network_recovery_reports_only_inflight_browser_requests() {
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let key = wispnet_core::identity::SecretKey::generate();
        let peer = key.public();
        host::initialize(state.path(), peer).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = backend.accept().await else {
                    break;
                };
                let request = headers(&mut s).await.unwrap();
                if request.starts_with(b"GET /hold ") {
                    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                        .await
                        .unwrap();
                    std::future::pending::<()>().await;
                }
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        {
            let (mut store, owner) = host::owner_store(state.path(), peer).unwrap();
            store
                .host_mutate(
                    peer,
                    &owner,
                    Mutation::Service {
                        expected_revision: 0,
                        id: "site".into(),
                        backend: address,
                        allow: vec![],
                        display_name: None,
                        kind: None,
                    },
                )
                .unwrap();
        }
        let authority = Authority::open(state.path(), peer).unwrap();
        let service = authority
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|service| service.id == "site")
            .expect("HTTP fixture service");
        let config =
            serde_json::from_value(serde_json::json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .unwrap();
        let c = Client::from_endpoint(
            endpoint,
            key,
            state.path().into(),
            config,
            authority.clone(),
        );
        assert_eq!(
            c.command(&serde_json::json!({"op":"relay_status"}))
                .await
                .unwrap()["relay"],
            "disabled"
        );
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let url = c
            .gateway
            .open(c.clone(), peer, "site".into(), service.incarnation)
            .await
            .unwrap();
        assert!(
            raw(&url, "GET", "/", "Sec-Fetch-Site: same-origin\r\n")
                .await
                .ends_with("ok")
        );
        let parsed = url::Url::parse(&url).unwrap();
        let _idle = TcpStream::connect(("127.0.0.1", parsed.port().unwrap()))
            .await
            .unwrap();
        let result = c
            .command(&serde_json::json!({"op":"network"}))
            .await
            .unwrap();
        assert_eq!(result["interrupted_requests"], 0);
        let mut stream = TcpStream::connect(("127.0.0.1", parsed.port().unwrap()))
            .await
            .unwrap();
        stream
            .write_all(
                format!(
                    "GET /hold HTTP/1.1\r\nHost: {}:{}\r\nSec-Fetch-Site: same-origin\r\n\r\n",
                    parsed.host_str().unwrap(),
                    parsed.port().unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        headers(&mut stream).await.unwrap();
        let result = c
            .command(&serde_json::json!({"op":"network"}))
            .await
            .unwrap();
        assert_eq!(result["interrupted_requests"], 1);
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut remaining))
            .await
            .unwrap()
            .unwrap();
        let result = c
            .command(&serde_json::json!({"op":"network"}))
            .await
            .unwrap();
        assert_eq!(result["interrupted_requests"], 0);
        c.gateway.stop().await;
        c.endpoint.close().await;
        server.abort();
    }
    #[tokio::test]
    async fn persistent_route_checks_local_policy_on_every_request_and_survives_old_deadline() {
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let key = wispnet_core::identity::SecretKey::generate();
        let peer = key.public();
        host::initialize(state.path(), peer).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = backend.accept().await else {
                    break;
                };
                let _ = headers(&mut s).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        {
            let (mut store, owner) = host::owner_store(state.path(), peer).unwrap();
            store
                .host_mutate(
                    peer,
                    &owner,
                    Mutation::Service {
                        expected_revision: 0,
                        id: "site".into(),
                        backend: address,
                        allow: vec![],
                        display_name: None,
                        kind: None,
                    },
                )
                .unwrap();
        }
        let authority = Authority::open(state.path(), peer).unwrap();
        let service = authority
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|service| service.id == "site")
            .expect("HTTP fixture service");
        let config =
            serde_json::from_value(serde_json::json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .unwrap();
        let c = Client::from_endpoint(
            endpoint,
            key,
            state.path().into(),
            config,
            authority.clone(),
        );
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let url = c
            .gateway
            .open(c.clone(), peer, "site".into(), service.incarnation)
            .await
            .unwrap();
        assert!(
            raw(&url, "GET", "/", "Sec-Fetch-Site: same-origin\r\n")
                .await
                .ends_with("ok")
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(601)).await;
        tokio::time::resume();
        assert!(
            raw(&url, "GET", "/", "Sec-Fetch-Site: same-origin\r\n")
                .await
                .ends_with("ok")
        );
        {
            let (mut store, owner) = host::owner_store(state.path(), peer).unwrap();
            store
                .host_mutate(
                    peer,
                    &owner,
                    Mutation::Remove {
                        expected_revision: 1,
                        id: "site".into(),
                    },
                )
                .unwrap();
        }
        assert!(
            raw(&url, "GET", "/", "Sec-Fetch-Site: same-origin\r\n")
                .await
                .starts_with("HTTP/1.1 403")
        );
        c.gateway.stop().await;
        c.endpoint.close().await;
        server.abort();
    }
    #[tokio::test]
    async fn native_calls_need_no_token_and_stop_cancels_active_requests() {
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let key = wispnet_core::identity::SecretKey::generate();
        let peer = key.public();
        host::initialize(state.path(), peer).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = backend.accept().await else {
                    break;
                };
                let incoming = headers(&mut s).await.unwrap();
                let text = String::from_utf8_lossy(&incoming).to_ascii_lowercase();
                assert!(!text.contains("x-wispnet-access"));
                assert!(text.contains("authorization: bearer backend-key"));
                if text.starts_with("get /hold ") {
                    s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n")
                        .await
                        .unwrap();
                    std::future::pending::<()>().await;
                }
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        {
            let (mut store, owner) = host::owner_store(state.path(), peer).unwrap();
            store
                .host_mutate(
                    peer,
                    &owner,
                    Mutation::Service {
                        expected_revision: 0,
                        id: "site".into(),
                        backend: address,
                        allow: vec![],
                        display_name: None,
                        kind: None,
                    },
                )
                .unwrap();
        }
        let authority = Authority::open(state.path(), peer).unwrap();
        let service = authority
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|service| service.id == "site")
            .expect("HTTP fixture service");
        let config =
            serde_json::from_value(serde_json::json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .unwrap();
        let c = Client::from_endpoint(
            endpoint,
            key,
            state.path().into(),
            config,
            authority.clone(),
        );
        c.command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let url = c
            .gateway
            .open(c.clone(), peer, "site".into(), service.incarnation)
            .await
            .unwrap();

        let auth = "Authorization: Bearer backend-key\r\n";
        assert!(raw(&url, "POST", "/", auth).await.ends_with("ok"));
        assert!(
            raw(
                &url,
                "GET",
                "/",
                &format!("{auth}Origin: https://unlisted.example\r\n")
            )
            .await
            .starts_with("HTTP/1.1 200")
        );
        let parsed = url::Url::parse(&url).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", parsed.port().unwrap()))
            .await
            .unwrap();
        stream.write_all(format!("GET /hold HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Bearer backend-key\r\n\r\n", parsed.host_str().unwrap(), parsed.port().unwrap()).as_bytes()).await.unwrap();
        headers(&mut stream).await.unwrap();
        c.gateway.stop().await;
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
        c.gateway.stop().await;
        c.endpoint.close().await;
        server.abort();
    }
    #[tokio::test]
    async fn route_and_socket_limits_release_capacity_and_header_deadline_is_total() {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            crate::access_settings::Settings {
                registry_port: listener.local_addr().unwrap().port(),
                ..Default::default()
            }
            .save(state.path())
            .unwrap();
        }

        let config =
            serde_json::from_value(serde_json::json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let client = Client::new(
            wispnet_core::identity::SecretKey::generate(),
            state.path().into(),
            config,
        )
        .await
        .unwrap();
        client
            .command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        let peer = wispnet_core::identity::SecretKey::generate().public();
        let mut urls = Vec::new();
        for i in 0..16 {
            urls.push(
                client
                    .gateway
                    .open(client.clone(), peer, format!("website-{i}"), "a".repeat(64))
                    .await
                    .unwrap(),
            );
        }
        assert!(
            client
                .gateway
                .open(client.clone(), peer, "overflow".into(), "a".repeat(64))
                .await
                .is_err()
        );
        let port = url::Url::parse(&urls[0]).unwrap().port().unwrap();
        let mut sockets = Vec::new();
        for _ in 0..32 {
            sockets.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.gateway.connections.available_permits() != 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        let mut overflow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), overflow.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        // A continuously trickling header cannot reset the overall ten-second deadline.
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let trickle = tokio::spawn(async move {
            loop {
                if writer.write_all(b"x").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        let started = tokio::time::Instant::now();
        assert!(headers(&mut reader).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(12));
        trickle.abort();
        client.gateway.stop().await;
        assert_eq!(client.gateway.connections.available_permits(), 32);
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
        client
            .command(&serde_json::json!({"op":"enable"}))
            .await
            .unwrap();
        assert!(
            client
                .gateway
                .open(client.clone(), peer, "website-0".into(), "a".repeat(64))
                .await
                .is_ok()
        );
        client.gateway.stop().await;
        client.endpoint.close().await;
    }
    #[test]
    fn authority_origin_framing_and_cookie_isolation() {
        let request=parse(b"GET / HTTP/1.1\r\nHost: a.localhost:123\r\nOrigin: http://a.localhost:123\r\nSec-Fetch-Site: same-origin\r\n\r\n").unwrap();
        assert!(request.safe("a.localhost:123"));
        assert!(!request.safe("b.localhost:123"));
        assert!(request.same_origin("http://a.localhost:123"));
        assert!(!request.same_origin("http://b.localhost:123"));
        let duplicate =
            parse(b"GET / HTTP/1.1\r\nHost: a.localhost:123\r\nHost: b.localhost:123\r\n\r\n")
                .unwrap();
        assert!(!duplicate.safe("a.localhost:123"));
        assert_eq!(
            http::clean_cookie("app=yes; Domain=.localhost; Path=/"),
            Ok(Some("app=yes; Path=/".into()))
        );
        assert_eq!(http::clean_cookie("_wispnet_session=bad; Path=/"), Ok(None));
    }
}

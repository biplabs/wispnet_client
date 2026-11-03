//! Owner-controlled public routes. No paired client can publish a remote service.
use crate::Client;
use iroh::{EndpointId, endpoint::Connection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, Semaphore},
    task::{JoinHandle, JoinSet},
};
use wispnet_core::{host::ServiceKind, protocol};
#[cfg(test)]
#[path = "test_support/public_ingress.rs"]
mod ingress;
pub const ALPN: &[u8] = b"wispnet/public/1";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub endpoint_id: EndpointId,
    pub relay_url: String,
    pub domain: String,
    #[serde(default)]
    pub direct_addr: Option<std::net::SocketAddr>,
}
impl GatewayConfig {
    pub fn validate(&self) -> Result<(), String> {
        let url = url::Url::parse(&self.relay_url).map_err(|_| "Invalid public gateway relay")?;
        if url.scheme() != "https" || url.host_str().is_none() || !valid_domain(&self.domain) {
            return Err("Invalid public gateway configuration".into());
        }
        Ok(())
    }
}
pub fn valid_domain(value: &str) -> bool {
    value.len() <= 180
        && value.contains('.')
        && value.split('.').all(|s| {
            !s.is_empty()
                && s.len() <= 63
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}
pub fn hostname(name: &str, owner: EndpointId, service: &str, domain: &str) -> String {
    let slug = |value: &str, limit: usize| {
        let mut s = String::new();
        for b in value.bytes() {
            if s.len() == limit {
                break;
            }
            if b.is_ascii_alphanumeric() {
                s.push((b as char).to_ascii_lowercase());
            } else if !s.is_empty() && !s.ends_with('-') {
                s.push('-');
            }
        }
        s.trim_matches('-').to_owned()
    };
    let name = slug(name, 20);
    let mut service_slug = slug(service, 24);
    if service_slug != service {
        use sha1::Digest;
        let digest = sha1::Sha1::digest(service.as_bytes());
        let suffix = digest[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        service_slug = format!("{}-{suffix}", slug(service, 15));
    }
    format!(
        "{}-{}-{}.{}",
        if name.is_empty() { "node" } else { &name },
        &owner.to_string()[..12],
        service_slug,
        domain
    )
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    service: String,
    incarnation: String,
    backend: std::net::SocketAddr,
    name: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    owner: EndpointId,
    gateway: Option<GatewayConfig>,
    services: Vec<Publication>,
}
fn load(state: &Path, owner: EndpointId) -> Result<Saved, String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("public-services.json"))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Saved {
                version: 1,
                owner,
                gateway: serde_json::from_str(include_str!("../../../config/public/gateway.json"))
                    .map_err(|_| "Invalid default public gateway")?,
                services: vec![],
            });
        }
        Err(_) => return Err("Cannot read public service settings".into()),
    };
    let m = file
        .metadata()
        .map_err(|_| "Cannot inspect public service settings")?;
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.len() > 32768
    {
        return Err("Unsafe public service settings".into());
    }
    let mut bytes = vec![];
    file.take(32769)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read public service settings")?;
    let saved: Saved =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid public service settings")?;
    if saved.version != 1
        || saved.owner != owner
        || saved.services.len() > 16
        || saved.services.iter().enumerate().any(|(i, s)| {
            !protocol::valid_service_id(&s.service)
                || !wispnet_core::enrollment::valid_code(&s.incarnation)
                || !s.backend.ip().is_loopback()
                || s.backend.port() == 0
                || !wispnet_core::enrollment::valid_name(&s.name)
                || saved.services[..i].iter().any(|r| r.service == s.service)
        })
    {
        return Err("Invalid public service settings".into());
    }
    if let Some(g) = &saved.gateway {
        g.validate()?;
    }
    Ok(saved)
}
fn save(state: &Path, saved: &Saved) -> Result<(), String> {
    let path = state.join("public-services.json");
    let mut file = wispnet_registry::control::credential_temp(&path)
        .map_err(|_| "Cannot save public service settings")?;
    serde_json::to_writer(&mut file, saved).map_err(|_| "Cannot save public service settings")?;
    file.flush()
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| "Cannot save public service settings")?;
    file.persist(path)
        .map_err(|_| "Cannot save public service settings")?;
    std::fs::File::open(state)
        .and_then(|f| f.sync_all())
        .map_err(|_| "Cannot sync public service settings")?;
    Ok(())
}
pub struct Publications {
    workers: Mutex<BTreeMap<String, JoinHandle<()>>>,
    states: Mutex<BTreeMap<String, String>>,
    lock: Mutex<()>,
    links_lock: Mutex<()>,
    slots: Arc<Semaphore>,
}
impl Default for Publications {
    fn default() -> Self {
        Self::new()
    }
}
impl Publications {
    pub fn new() -> Self {
        Self {
            workers: Mutex::new(BTreeMap::new()),
            states: Mutex::new(BTreeMap::new()),
            lock: Mutex::new(()),
            links_lock: Mutex::new(()),
            slots: Arc::new(Semaphore::new(32)),
        }
    }
    pub async fn status(&self, client: &Client) -> Result<Value, String> {
        let saved = load(&client.state, client.endpoint.id())?;
        let states = self.states.lock().await;
        Ok(
            json!({"gateway":saved.gateway,"services":saved.services.iter().map(|s|json!({"service":s.service,"incarnation":s.incarnation,"enabled":true,"url":saved.gateway.as_ref().map(|g|format!("https://{}/",hostname(&s.name,saved.owner,&s.service,&g.domain))),"state":if client.services_enabled() { states.get(&s.service).map(String::as_str).unwrap_or("connecting") } else { "offline" }})).collect::<Vec<_>>()}),
        )
    }
    async fn sync_public_links(&self, client: &Client) -> Result<(), String> {
        let _lock = self.links_lock.lock().await;
        let Some(authority) = &client.local_authority else {
            return Ok(());
        };
        let saved = match load(&client.state, client.endpoint.id()) {
            Ok(saved) => saved,
            Err(error) => {
                let _ = authority.set_public_links(Vec::new());
                return Err(error);
            }
        };
        let states = self.states.lock().await;
        let mut links: Vec<_> = saved
            .services
            .iter()
            .filter_map(|s| {
                let gateway = saved.gateway.as_ref()?;
                let state = match (
                    client.services_enabled(),
                    states.get(&s.service).map(String::as_str),
                ) {
                    (false, _) => "offline",
                    (true, Some("online")) => "online",
                    (true, Some("connecting") | None) => "connecting",
                    _ => "offline",
                };
                Some(wispnet_core::host::PublicLink {
                    service: s.service.clone(),
                    incarnation: s.incarnation.clone(),
                    url: format!(
                        "https://{}/",
                        hostname(&s.name, saved.owner, &s.service, &gateway.domain)
                    ),
                    state: state.into(),
                })
            })
            .collect();
        links.sort_by(|a, b| a.service.cmp(&b.service));
        authority.set_public_links(links).map_err(|e| e.to_string())
    }
    async fn set_state(&self, client: &Client, service: &str, state: &str) {
        self.states
            .lock()
            .await
            .insert(service.into(), state.into());
        let _ = self.sync_public_links(client).await;
    }
    pub async fn stop(&self) {
        let _lock = self.lock.lock().await;
        self.stop_locked().await;
    }
    async fn stop_locked(&self) {
        for (_, task) in std::mem::take(&mut *self.workers.lock().await) {
            task.abort();
            let _ = task.await;
        }
        self.states.lock().await.clear();
    }
    pub async fn reconcile(&self, client: &Client) -> Result<(), String> {
        let _lock = self.lock.lock().await;
        self.reconcile_locked(client).await
    }
    async fn reconcile_locked(&self, client: &Client) -> Result<(), String> {
        if !client.services_enabled() {
            self.stop_locked().await;
            return self.sync_public_links(client).await;
        }
        let mut saved = load(&client.state, client.endpoint.id())?;
        if saved.services.is_empty() {
            return self.sync_public_links(client).await;
        }
        let authority = client
            .local_authority
            .as_ref()
            .ok_or("Only a service owner can publish")?;
        // Paused services keep their publication configuration for re-enabling.
        let services = authority
            .snapshot()
            .await
            .map_err(|e| e.to_string())?
            .services;
        let removed: Vec<_> = saved
            .services
            .iter()
            .filter(|p| {
                !services.iter().any(|s| {
                    s.id == p.service
                        && s.incarnation == p.incarnation
                        && s.backend == p.backend
                        && s.kind == ServiceKind::Http
                })
            })
            .map(|p| p.service.clone())
            .collect();
        if removed.is_empty() {
            return self.sync_public_links(client).await;
        }
        saved.services.retain(|p| !removed.contains(&p.service));
        save(&client.state, &saved)?;
        for id in removed {
            if let Some(task) = self.workers.lock().await.remove(&id) {
                task.abort();
                let _ = task.await;
            }
            self.states.lock().await.remove(&id);
        }
        self.sync_public_links(client).await
    }
    pub async fn restore(&self, client: Arc<Client>) -> Result<(), String> {
        let _lock = self.lock.lock().await;
        self.reconcile_locked(&client).await?;
        self.restart(client).await
    }
    async fn restart(&self, client: Arc<Client>) -> Result<(), String> {
        self.stop_locked().await;
        if !client.services_enabled() {
            return self.sync_public_links(&client).await;
        }
        let saved = load(&client.state, client.endpoint.id())?;
        if let Some(gateway) = saved.gateway {
            for publication in saved.services {
                let c = client.clone();
                let gateway = gateway.clone();
                let id = publication.service.clone();
                let task = tokio::spawn(async move {
                    publish(c, gateway, publication).await;
                });
                self.workers.lock().await.insert(id, task);
            }
        }
        self.sync_public_links(&client).await
    }
    pub async fn configure(&self, client: Arc<Client>, value: &Value) -> Result<Value, String> {
        let _lock = self.lock.lock().await;
        let authority = client
            .local_authority
            .as_ref()
            .ok_or("Only a service owner can publish")?;
        if value
            .get("host")
            .is_some_and(|h| h.as_str() != Some(client.endpoint.id().to_string().as_str()))
        {
            return Err("Only the owning node can publish a service".into());
        }
        let mut saved = load(&client.state, client.endpoint.id())?;
        if value["op"] == "configure_public_gateway" {
            if !saved.services.is_empty() {
                return Err("Disable public services before changing the gateway".into());
            }
            let gateway: GatewayConfig =
                serde_json::from_value(value["gateway"].clone()).map_err(|_| "Invalid gateway")?;
            gateway.validate()?;
            saved.gateway = Some(gateway);
        } else {
            let id = crate::argument(value, "service")?;
            let enabled = value["enabled"]
                .as_bool()
                .ok_or("Missing public access setting")?;
            let selected = if enabled {
                if saved.gateway.is_none() {
                    return Err("Public gateway is not configured".into());
                }
                let incarnation = crate::argument(value, "incarnation")?;
                Some(
                    authority
                        .services()
                        .await
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .find(|s| {
                            s.id == id
                                && s.incarnation == incarnation
                                && s.kind == ServiceKind::Http
                        })
                        .ok_or("Refresh the owned HTTP service before publishing")?,
                )
            } else {
                None
            };
            saved.services.retain(|s| s.service != id);
            if let Some(s) = selected {
                if saved.services.len() >= 16 {
                    return Err("Too many public services".into());
                }
                let name = crate::argument(value, "name")?;
                if !wispnet_core::enrollment::valid_name(name) {
                    return Err("Invalid machine name".into());
                }
                saved.services.push(Publication {
                    service: id.into(),
                    incarnation: s.incarnation,
                    backend: s.backend,
                    name: name.into(),
                });
            }
        }
        save(&client.state, &saved)?;
        if !client.services_enabled() {
            self.stop_locked().await;
        } else if value["op"] == "configure_public_gateway" {
            self.restart(client.clone()).await?;
        } else {
            let id = crate::argument(value, "service")?;
            if let Some(task) = self.workers.lock().await.remove(id) {
                task.abort();
                let _ = task.await;
            }
            self.states.lock().await.remove(id);
            if let (Some(gateway), Some(publication)) = (
                saved.gateway,
                saved.services.into_iter().find(|s| s.service == id),
            ) {
                let c = client.clone();
                let task = tokio::spawn(async move {
                    publish(c, gateway, publication).await;
                });
                self.workers.lock().await.insert(id.into(), task);
            }
        }
        self.sync_public_links(&client).await?;
        self.status(&client).await
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub version: u8,
    pub service: String,
    pub incarnation: String,
    pub name: String,
}
pub async fn write_json(
    stream: &mut iroh::endpoint::SendStream,
    value: &impl Serialize,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Invalid registration")?;
    if bytes.len() > 4096 {
        return Err("Registration too large".into());
    }
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .await
        .map_err(|_| "Registration failed")?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| "Registration failed")?;
    Ok(())
}
pub async fn read_json<T: serde::de::DeserializeOwned>(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<T, String> {
    let mut len = [0; 2];
    stream
        .read_exact(&mut len)
        .await
        .map_err(|_| "Registration failed")?;
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > 4096 {
        return Err("Invalid registration size".into());
    }
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "Registration failed")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid registration".into())
}
struct CloseConnection(Connection);
impl Drop for CloseConnection {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"publication stopped");
    }
}
async fn publish(client: Arc<Client>, gateway: GatewayConfig, publication: Publication) {
    let mut delay = 1;
    loop {
        if !permitted(&client, &publication).await {
            client
                .publications
                .set_state(&client, &publication.service, "offline")
                .await;
            delay = 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        client
            .publications
            .set_state(&client, &publication.service, "connecting")
            .await;
        let result = publish_once(client.clone(), &gateway, &publication).await;
        client
            .publications
            .set_state(
                &client,
                &publication.service,
                &result.err().unwrap_or("offline".into()),
            )
            .await;
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}
async fn permitted(client: &Client, publication: &Publication) -> bool {
    if !client.services_enabled() {
        return false;
    }
    let Some(authority) = &client.local_authority else {
        return false;
    };
    matches!(authority.services().await,Ok(ref s)if s.iter().any(|s|s.id==publication.service && s.incarnation==publication.incarnation && s.backend==publication.backend && s.kind==ServiceKind::Http))
}
async fn publish_once(
    client: Arc<Client>,
    gateway: &GatewayConfig,
    publication: &Publication,
) -> Result<(), String> {
    if !permitted(&client, publication).await {
        return Err("service changed; disable and re-enable public access".into());
    }
    let transport = wispnet_core::config::TransportConfig {
        backup_relay_urls: Vec::new(),
        discovery_relay_url: None,
        relay_url: Some(gateway.relay_url.clone()),
        ..client.config.clone()
    };
    let target = wispnet_agent::tunnel::peer_target(
        gateway.endpoint_id,
        Some(&transport),
        gateway.direct_addr,
    )
    .map_err(|e| e.to_string())?;
    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        client.endpoint.connect(target, ALPN),
    )
    .await
    .map_err(|_| "public gateway unavailable")?
    .map_err(|_| "public gateway unavailable")?;
    let _close = CloseConnection(connection.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|_| "registration failed")?;
        write_json(
            &mut send,
            &Registration {
                version: 1,
                service: publication.service.clone(),
                incarnation: publication.incarnation.clone(),
                name: publication.name.clone(),
            },
        )
        .await?;
        send.finish().map_err(|_| "registration failed")?;
        let response: Value = read_json(&mut recv).await?;
        if response["ok"] != true {
            return Err("public registration rejected".into());
        }
        Ok::<_, String>(())
    })
    .await
    .map_err(|_| "public registration timed out")??;
    client
        .publications
        .set_state(&client, &publication.service, "online")
        .await;
    let mut tasks = JoinSet::new();
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _=timer.tick()=>{if !permitted(&client,publication).await{return Err("service changed; public access stopped".into());}},
            incoming=connection.accept_bi()=>{let(send,recv)=incoming.map_err(|_|"public gateway disconnected")?;let Ok(permit)=client.publications.slots.clone().try_acquire_owned()else{drop(send);drop(recv);continue};let c=client.clone();let p=publication.clone();tasks.spawn(async move{let _permit=permit;serve_public(c,p,send,recv).await;});},
            _=tasks.join_next(),if !tasks.is_empty()=>{}
        }
    }
}
async fn serve_public(
    client: Arc<Client>,
    publication: Publication,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) {
    let open = tokio::time::timeout(Duration::from_secs(5), async {
        let mut size = [0; 2];
        recv.read_exact(&mut size).await.map_err(|_| ())?;
        let size = u16::from_be_bytes(size) as usize;
        if size == 0 || size > protocol::MAX_FRAME {
            return Err(());
        }
        let mut data = vec![0; size];
        recv.read_exact(&mut data).await.map_err(|_| ())?;
        let (id, incarnation) = protocol::decode_bound_request(&data).map_err(|_| ())?;
        if id != publication.service
            || incarnation.as_deref() != Some(&publication.incarnation)
            || !permitted(&client, &publication).await
        {
            return Err(());
        }
        TcpStream::connect(publication.backend)
            .await
            .map_err(|_| ())
    })
    .await;
    let Ok(Ok(mut backend)) = open else {
        let _ = send
            .write_all(&protocol::response(protocol::Status::Denied))
            .await;
        return;
    };
    if send
        .write_all(&protocol::response(protocol::Status::Ok))
        .await
        .is_err()
    {
        return;
    }
    let mut stream = wispnet_agent::tunnel::ServiceStream::new(send, recv);
    let _ = tokio::time::timeout(
        Duration::from_secs(3600),
        tokio::io::copy_bidirectional(&mut stream, &mut backend),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::TcpListener;
    async fn http(port: u16, host: &str, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut bytes = vec![];
        stream.read_to_end(&mut bytes).await.unwrap();
        String::from_utf8_lossy(&bytes).into()
    }
    #[tokio::test]
    async fn only_owner_publishes_and_disable_or_binding_change_closes_public_route() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use wispnet_core::identity::SecretKey;
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = SecretKey::generate();
        let own = owner.public();
        host::initialize(state.path(), own).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = backend.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                let Ok((mut stream, _)) = backend.accept().await else {
                    break;
                };
                tasks.spawn(async move {
                    let mut bytes = vec![];
                    while bytes.len() < 32768 {
                        let mut b = [0];
                        if stream.read_exact(&mut b).await.is_err() {
                            return;
                        }
                        bytes.push(b[0]);
                        if bytes.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    if bytes.starts_with(b"GET /ws ") {
                        let _=stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await;
                        let mut frame=[0;8];if stream.read_exact(&mut frame).await.is_ok(){assert_eq!(&frame[..2],&[0x81,0x82]);let _=stream.write_all(b"\x81\x02hi").await;std::future::pending::<()>().await;}
                        return;
                    }
                    if bytes.starts_with(b"GET /hold ") {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
                            )
                            .await;
                        std::future::pending::<()>().await;
                    } else {
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\npublic")
                            .await;
                    }
                });
            }
        });
        let (mut policy, secret) = host::owner_store(state.path(), own).unwrap();
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::Service {
                    expected_revision: 0,
                    id: "llama".into(),
                    backend: addr,
                    allow: vec![],
                    display_name: None,
                    kind: None,
                },
            )
            .unwrap();
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::Service {
                    expected_revision: 1,
                    id: "private".into(),
                    backend: addr,
                    allow: vec![],
                    display_name: None,
                    kind: None,
                },
            )
            .unwrap();
        let config: wispnet_core::config::TransportConfig =
            serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&owner, Some(&config))
            .await
            .unwrap();
        let client = Client::from_endpoint(
            endpoint,
            owner,
            state.path().into(),
            config.clone(),
            Authority::open(state.path(), own).unwrap(),
        );
        let gateway =
            wispnet_agent::tunnel::bind_configured_client(&SecretKey::generate(), Some(&config))
                .await
                .unwrap();
        let mut direct = gateway.bound_sockets()[0];
        direct.set_ip("127.0.0.1".parse().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ingress = ingress::Ingress::new("wispnet.test".into(), vec![own]).unwrap();
        let gateway_task = tokio::spawn(ingress.clone().run(gateway.clone(), listener));
        client.command(&json!({"op":"configure_public_gateway","gateway":{"endpoint_id":gateway.id(),"relay_url":"https://relay.invalid","domain":"wispnet.test","direct_addr":direct}})).await.unwrap();
        let incarnation = client
            .local_authority
            .as_ref()
            .unwrap()
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.id == "llama")
            .unwrap()
            .incarnation;
        let request = json!({"op":"configure_public","service":"llama","incarnation":incarnation,"enabled":true,"name":"J desktop"});
        let mut remote = request.clone();
        remote["host"] = json!(SecretKey::generate().public());
        assert!(client.command(&remote).await.is_err());
        client.command(&request).await.unwrap();
        let name = hostname("J desktop", own, "llama", "wispnet.test");
        for _ in 0..100 {
            if client.publications.status(&client).await.unwrap()["services"][0]["state"]
                == "online"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let gateway_connection = ingress.test_connection(&name).await;
        let private = client
            .local_authority
            .as_ref()
            .unwrap()
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.id == "private")
            .unwrap();
        assert!(
            wispnet_agent::tunnel::open_service_bound(
                &gateway_connection,
                "private",
                &private.incarnation
            )
            .await
            .is_err(),
            "Public gateway reached an unpublished private service"
        );
        let response = http(port, &name, "/").await;
        assert!(response.ends_with("public"), "{response}");
        assert!(
            http(port, "other.wispnet.test", "/")
                .await
                .starts_with("HTTP/1.1 404")
        );
        assert!(
            http(
                port,
                &format!("127.0.0.1:{port}"),
                &format!("/_wispnet/allow?domain={name}")
            )
            .await
            .starts_with("HTTP/1.1 200")
        );
        let attacker =
            wispnet_agent::tunnel::bind_configured_client(&SecretKey::generate(), Some(&config))
                .await
                .unwrap();
        if let Ok(connection) = attacker
            .connect(
                iroh::EndpointAddr::new(gateway.id()).with_ip_addr(direct),
                ALPN,
            )
            .await
        {
            tokio::time::timeout(Duration::from_secs(2), connection.closed())
                .await
                .unwrap();
        }
        attacker.close().await;
        assert!(
            client
                .local_authority
                .as_ref()
                .unwrap()
                .services()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.id == "llama")
                .unwrap()
                .allow
                .is_empty()
        );
        let viewer = SecretKey::generate().public();
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::Approve {
                    expected_revision: 2,
                    peer: viewer,
                    name: "paired viewer".into(),
                },
            )
            .unwrap();
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::Service {
                    expected_revision: 3,
                    id: "llama".into(),
                    backend: addr,
                    allow: vec![viewer],
                    display_name: None,
                    kind: None,
                },
            )
            .unwrap();
        let authority = client.local_authority.as_ref().unwrap();
        let links = authority.public_links(viewer).await.unwrap();
        assert_eq!(links.services.len(), 1);
        assert_eq!(links.services[0].url, format!("https://{name}/"));
        assert_eq!(links.services[0].state, "online");
        // Pausing the service must close public ingress without forgetting its URL.
        let incarnation = links.services[0].incarnation.clone();
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::SetServiceEnabled {
                    expected_revision: 4,
                    id: "llama".into(),
                    incarnation: incarnation.clone(),
                    enabled: false,
                },
            )
            .unwrap();
        client.publications.reconcile(&client).await.unwrap();
        assert_eq!(load(state.path(), own).unwrap().services.len(), 1);
        assert!(
            authority
                .public_links(viewer)
                .await
                .unwrap()
                .services
                .is_empty()
        );
        for _ in 0..100 {
            if !http(port, &name, "/").await.ends_with("public") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!http(port, &name, "/").await.ends_with("public"));
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::SetServiceEnabled {
                    expected_revision: 5,
                    id: "llama".into(),
                    incarnation,
                    enabled: true,
                },
            )
            .unwrap();
        for _ in 0..100 {
            if http(port, &name, "/").await.ends_with("public") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(http(port, &name, "/").await.ends_with("public"));
        let mut websocket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        websocket.write_all(format!("GET /ws HTTP/1.1\r\nHost: {name}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").as_bytes()).await.unwrap();
        let mut handshake = vec![];
        loop {
            let mut byte = [0];
            websocket.read_exact(&mut byte).await.unwrap();
            handshake.push(byte[0]);
            if handshake.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(handshake.starts_with(b"HTTP/1.1 101"));
        websocket
            .write_all(&[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2])
            .await
            .unwrap();
        let mut echo = [0; 4];
        websocket.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"\x81\x02hi");
        let mut held = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        held.write_all(format!("GET /hold HTTP/1.1\r\nHost: {name}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut first = [0; 64];
        let n = held.read(&mut first).await.unwrap();
        assert!(n > 0);
        client
            .command(&json!({"op":"configure_public","service":"llama","enabled":false}))
            .await
            .unwrap();
        assert!(
            authority
                .public_links(viewer)
                .await
                .unwrap()
                .services
                .is_empty()
        );
        let mut remaining = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(2), held.read_to_end(&mut remaining))
            .await
            .unwrap();
        for _ in 0..50 {
            if http(port, &name, "/").await.starts_with("HTTP/1.1 404") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!http(port, &name, "/").await.ends_with("public"));
        let mut ws_rest = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(2), websocket.read_to_end(&mut ws_rest))
            .await
            .unwrap();
        client.command(&request).await.unwrap();
        assert_eq!(
            authority.public_links(viewer).await.unwrap().services.len(),
            1
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        policy
            .host_mutate(
                own,
                &secret,
                Mutation::Service {
                    expected_revision: 6,
                    id: "llama".into(),
                    backend: "127.0.0.1:1".parse().unwrap(),
                    allow: vec![],
                    display_name: None,
                    kind: None,
                },
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!http(port, &name, "/").await.ends_with("public"));
        client.publications.reconcile(&client).await.unwrap();
        assert!(
            authority
                .public_links(viewer)
                .await
                .unwrap()
                .services
                .is_empty()
        );
        assert!(load(state.path(), own).unwrap().services.is_empty());
        client.publications.stop().await;
        client.endpoint.close().await;
        gateway_task.abort();
        gateway.close().await;
        server.abort();
    }
}

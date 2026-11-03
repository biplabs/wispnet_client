//! Public HTTP ingress, reached only through the local HTTPS front end.
use super::{ALPN, Registration, hostname, read_json, valid_domain, write_json};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response,
    body::Incoming,
    header::{HeaderName, HeaderValue},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;
struct Route {
    owner: EndpointId,
    service: String,
    incarnation: String,
    connection: Connection,
    slots: Arc<Semaphore>,
}
pub struct Ingress {
    domain: String,
    publishers: Vec<EndpointId>,
    routes: Mutex<BTreeMap<String, Arc<Route>>>,
    requests: Arc<Semaphore>,
}
impl Ingress {
    pub fn new(domain: String, publishers: Vec<EndpointId>) -> Result<Arc<Self>, String> {
        if !valid_domain(&domain) || publishers.is_empty() || publishers.len() > 128 {
            return Err("Invalid public gateway domain or publisher list".into());
        }
        Ok(Arc::new(Self {
            domain,
            publishers,
            routes: Mutex::new(BTreeMap::new()),
            requests: Arc::new(Semaphore::new(128)),
        }))
    }
    #[cfg(test)]
    pub(super) async fn test_connection(&self, name: &str) -> Connection {
        self.routes
            .lock()
            .await
            .get(name)
            .unwrap()
            .connection
            .clone()
    }
    pub async fn run(self: Arc<Self>, endpoint: Endpoint, listener: TcpListener) {
        endpoint.set_alpns(vec![ALPN.to_vec()]);
        let connections = Arc::new(Semaphore::new(256));
        let http = Arc::new(Semaphore::new(256));
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                incoming=endpoint.accept()=>{let Some(incoming)=incoming else{break};let Ok(permit)=connections.clone().try_acquire_owned()else{incoming.refuse();continue};let state=self.clone();tasks.spawn(async move{let _permit=permit;if let Ok(Ok(connection))=tokio::time::timeout(Duration::from_secs(10),incoming).await {state.register(connection).await;}});},
                accepted=listener.accept()=>{let Ok((stream,peer))=accepted else{break};if !peer.ip().is_loopback(){continue;}let Ok(permit)=http.clone().try_acquire_owned()else{continue};let state=self.clone();tasks.spawn(async move{let _permit=permit;let _=hyper::server::conn::http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).max_buf_size(32768).serve_connection(TokioIo::new(stream),service_fn(move|req|state.clone().request(req))).with_upgrades().await;});},
                _=tasks.join_next(),if !tasks.is_empty()=>{}
            }
        }
    }
    async fn register(self: Arc<Self>, connection: Connection) {
        let owner = connection.remote_id();
        if !self.publishers.contains(&owner) {
            connection.close(1u32.into(), b"publisher not authorized");
            return;
        }
        let registration = tokio::time::timeout(Duration::from_secs(5), async {
            let (mut send, mut recv) = connection.accept_bi().await.map_err(|_| ())?;
            let record: Registration = read_json(&mut recv).await.map_err(|_| ())?;
            if record.version != 1
                || !wispnet_core::protocol::valid_service_id(&record.service)
                || !wispnet_core::enrollment::valid_code(&record.incarnation)
                || !wispnet_core::enrollment::valid_name(&record.name)
            {
                return Err(());
            }
            let name = hostname(&record.name, owner, &record.service, &self.domain);
            let mut routes = self.routes.lock().await;
            if routes.len() >= 1024
                || routes.values().filter(|r| r.owner == owner).count() >= 16
                    && !routes.contains_key(&name)
                || routes
                    .get(&name)
                    .is_some_and(|r| r.owner != owner || r.service != record.service)
            {
                return Err(());
            }
            let route = Arc::new(Route {
                owner,
                service: record.service,
                incarnation: record.incarnation,
                connection: connection.clone(),
                slots: Arc::new(Semaphore::new(16)),
            });
            write_json(
                &mut send,
                &serde_json::json!({"ok":true,"url":format!("https://{name}/")}),
            )
            .await
            .map_err(|_| ())?;
            send.finish().map_err(|_| ())?;
            if let Some(old) = routes.insert(name.clone(), route) {
                old.connection.close(0u32.into(), b"publication replaced");
            }
            Ok::<_, ()>(name)
        })
        .await;
        let Ok(Ok(name)) = registration else {
            connection.close(1u32.into(), b"registration rejected");
            return;
        };
        connection.closed().await;
        let mut routes = self.routes.lock().await;
        if routes
            .get(&name)
            .is_some_and(|r| r.connection.stable_id() == connection.stable_id())
        {
            routes.remove(&name);
        }
    }
    async fn request(
        self: Arc<Self>,
        mut request: Request<Incoming>,
    ) -> Result<Response<Body>, Infallible> {
        // Caddy's on-demand certificate authorization endpoint is local-only.
        if request.uri().path() == "/_wispnet/allow" {
            if !request
                .headers()
                .get("host")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|h| h.starts_with("127.0.0.1:") || h.starts_with("localhost:"))
            {
                return Ok(error(404, "Not found"));
            }
            if request.method() != hyper::Method::GET {
                return Ok(error(405, "Method not allowed"));
            }
            let domain = request.uri().query().and_then(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .find(|(k, _)| k == "domain")
                    .map(|(_, v)| v.into_owned())
            });
            let allowed = if let Some(name) = domain {
                self.routes
                    .lock()
                    .await
                    .get(&name)
                    .is_some_and(|r| r.connection.close_reason().is_none())
            } else {
                false
            };
            return Ok(error(
                if allowed { 200 } else { 403 },
                if allowed { "Allowed" } else { "Not published" },
            ));
        }
        let host = if request.headers().get_all("host").iter().count() == 1 {
            request
                .headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned)
        } else {
            None
        };
        let Some(host) = host else {
            return Ok(error(421, "Unknown public hostname"));
        };
        if !host.ends_with(&format!(".{}", self.domain))
            || request.uri().scheme().is_some()
            || request.uri().authority().is_some()
        {
            return Ok(error(421, "Unknown public hostname"));
        }
        let route = self.routes.lock().await.get(&host).cloned();
        let Some(route) = route else {
            return Ok(error(404, "Service is not published"));
        };
        if route.connection.close_reason().is_some() {
            return Ok(error(503, "Publishing node is offline"));
        }
        let Ok(global) = self.requests.clone().try_acquire_owned() else {
            return Ok(error(503, "Public gateway busy"));
        };
        let Ok(local) = route.slots.clone().try_acquire_owned() else {
            return Ok(error(503, "Service busy"));
        };
        if request
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > 16 * 1024 * 1024)
        {
            return Ok(error(413, "Request too large"));
        }
        if request.uri().path().starts_with("/_wispnet/") {
            return Ok(error(404, "Not found"));
        }
        let upgrade = request
            .headers()
            .get(hyper::header::UPGRADE)
            .is_some_and(|h| h.as_bytes().eq_ignore_ascii_case(b"websocket"));
        let downstream = if upgrade {
            Some(hyper::upgrade::on(&mut request))
        } else {
            None
        };
        sanitize(request.headers_mut(), upgrade);
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let stream = match wispnet_agent::tunnel::open_service_bound(
            &route.connection,
            &route.service,
            &route.incarnation,
        )
        .await
        {
            Ok(s) => s,
            Err(_) => return Ok(error(503, "Published service unavailable")),
        };
        let (mut sender, connection) =
            match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
                Ok(c) => c,
                Err(_) => return Ok(error(502, "Published service connection failed")),
            };
        let permits = Arc::new((global, local));
        let connection_permits = permits.clone();
        let owner = route.connection.clone();
        tokio::spawn(async move {
            let _permits = connection_permits;
            tokio::select! {_=connection.with_upgrades()=>{},_=owner.closed()=>{}}
        });
        let (parts, body) = request.into_parts();
        let request = Request::from_parts(parts, Limited::new(body, 16 * 1024 * 1024));
        let mut response = match tokio::time::timeout(
            Duration::from_secs(120),
            sender.send_request(request),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => return Ok(error(502, "Published service did not respond")),
        };
        if upgrade && response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
            let upstream = hyper::upgrade::on(&mut response);
            let owner = route.connection.clone();
            tokio::spawn(async move {
                let _permits = permits;
                if let (Some(downstream), Ok(upstream)) = (downstream, upstream.await)
                    && let Ok(downstream) = downstream.await
                {
                    let mut a = TokioIo::new(downstream);
                    let mut b = TokioIo::new(upstream);
                    tokio::select! {_=tokio::io::copy_bidirectional(&mut a,&mut b)=>{},_=owner.closed()=>{}}
                }
            });
        }
        let (mut parts, body) = response.into_parts();
        sanitize(
            &mut parts.headers,
            upgrade && parts.status == hyper::StatusCode::SWITCHING_PROTOCOLS,
        );
        parts
            .headers
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        Ok(Response::from_parts(
            parts,
            body.map_err(|e| -> Error { Box::new(e) }).boxed_unsync(),
        ))
    }
}
fn sanitize(headers: &mut hyper::HeaderMap, upgrade: bool) {
    let nominated: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| HeaderName::from_bytes(v.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        if !(upgrade && name == hyper::header::UPGRADE) {
            headers.remove(name);
        }
    }
    let names: Vec<_> = headers
        .keys()
        .filter(|n| n.as_str().starts_with("x-forwarded-"))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "forwarded",
        "x-wispnet-access",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
    } else {
        headers.remove("upgrade");
    }
}
fn error(status: u16, message: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(
            Full::new(Bytes::from_static(message.as_bytes()))
                .map_err(|e| -> Error { match e {} })
                .boxed_unsync(),
        )
        .unwrap()
}

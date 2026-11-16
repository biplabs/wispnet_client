//! Loopback-only discovery for native clients and websites.
use crate::{
    Client,
    access_settings::Settings,
    gateway::{headers, parse, response},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

impl Client {
    pub(crate) async fn service_list(self: &Arc<Self>) -> Result<Value, String> {
        let mut hosts = std::collections::BTreeSet::new();
        if let Some(authority) = &self.local_authority {
            for pin in authority.paired_hosts().await.map_err(|e| e.to_string())? {
                hosts.insert(pin.host);
            }
        }
        for entry in std::fs::read_dir(self.state.join("peer-pins"))
            .into_iter()
            .flatten()
            .take(129)
            .flatten()
        {
            if let Some(host) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|s| s.parse().ok())
            {
                hosts.insert(host);
            }
        }
        let mut services = vec![];
        let mut errors = vec![];
        if let Some(authority) = &self.local_authority {
            for service in authority.services().await.map_err(|e| e.to_string())? {
                if service.kind == wispnet_core::host::ServiceKind::Http {
                    services.push(json!({"host_id":self.endpoint.id(),"service_id":service.id,"display_name":service.display_name,"incarnation":service.incarnation,"kind":"http"}));
                }
            }
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        let mut pending = tokio::task::JoinSet::new();
        for host in hosts
            .into_iter()
            .take(128)
            .filter(|h| *h != self.endpoint.id())
        {
            let client = self.clone();
            let slots = slots.clone();
            pending.spawn(async move {
                let catalog = tokio::time::timeout(Duration::from_secs(6), async {
                    let _permit = slots.acquire().await.map_err(|_| "Catalog queue closed")?;
                    if client.saved_host(host).await?.is_none() {
                        return Err("Host not paired".into());
                    }
                    client.catalog(host).await
                })
                .await;
                (host, catalog)
            });
        }
        while let Some(result) = pending.join_next().await {
            let (host, catalog) = result.map_err(|_| "Catalog task failed")?;
            match catalog {
                Ok(Ok(c)) => {
                    if let Some(items) = c["services"].as_array() {
                        for s in items {
                            if s["kind"] == "http" {
                                services.push(json!({"host_id":host,"service_id":s["id"],"display_name":s["id"],"incarnation":s["incarnation"],"kind":"http"}));
                            }
                        }
                    }
                }
                _ => errors.push(json!({"host_id":host,"error":"unavailable"})),
            }
        }
        let addresses = self.gateway.settings(self).await?;
        for service in &mut services {
            service["link"] = json!(format!(
                "wispnet://{}/{}",
                service["host_id"].as_str().unwrap_or(""),
                service["service_id"].as_str().unwrap_or("")
            ));
            service["url"] = addresses["routes"]
                .as_array()
                .and_then(|routes| {
                    routes.iter().find(|r| {
                        r["host"] == service["host_id"] && r["service"] == service["service_id"]
                    })
                })
                .map(|r| r["url"].clone())
                .unwrap_or(Value::Null);
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        let mut pending = tokio::task::JoinSet::new();
        for (index, service) in services
            .iter()
            .enumerate()
            .filter(|(_, s)| s["url"].is_null())
        {
            let client = self.clone();
            let slots = slots.clone();
            let request = json!({"host":service["host_id"], "service":service["service_id"], "incarnation":service["incarnation"]});
            pending.spawn(async move {
                let route = tokio::time::timeout(Duration::from_secs(6), async {
                    let _permit = slots
                        .acquire()
                        .await
                        .map_err(|_| "Route queue closed".to_owned())?;
                    client.open_route(&request).await
                })
                .await
                .unwrap_or_else(|_| Err("Browser route creation timed out".into()));
                (index, route)
            });
        }
        while let Some(result) = pending.join_next().await {
            let (index, route) = result.map_err(|_| "Browser route task failed")?;
            match route {
                Ok(url) => services[index]["url"] = json!(url),
                Err(error) => services[index]["error"] = json!(error),
            }
        }
        Ok(json!({"api_version":1,"services":services,"errors":errors}))
    }
    pub(crate) async fn service_incarnation(
        &self,
        host: iroh::EndpointId,
        id: &str,
    ) -> Result<String, String> {
        let incarnation = if host == self.endpoint.id() {
            let authority = self
                .local_authority
                .as_ref()
                .ok_or("Local service unavailable")?;
            authority
                .services()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|s| s.id == id && s.kind == wispnet_core::host::ServiceKind::Http)
                .ok_or("Service unavailable")?
                .incarnation
        } else {
            if self.saved_host(host).await?.is_none() {
                return Err("Host not paired".into());
            }
            let catalog = self.catalog(host).await?;
            catalog["services"]
                .as_array()
                .and_then(|s| s.iter().find(|s| s["id"] == id && s["kind"] == "http"))
                .and_then(|s| s["incarnation"].as_str())
                .ok_or("Service unavailable")?
                .to_owned()
        };
        Ok(incarnation)
    }
    pub(crate) async fn resolve_service(self: &Arc<Self>, value: &Value) -> Result<Value, String> {
        let host = crate::peer(value)?;
        let id = crate::argument(value, "service")?;
        if !wispnet_core::protocol::valid_service_id(id) {
            return Err("Invalid service ID".into());
        }
        let incarnation = self.service_incarnation(host, id).await?;
        let url = self
            .open_route(&json!({"host":host,"service":id,"incarnation":incarnation}))
            .await?;
        Ok(
            json!({"api_version":1,"host_id":host,"service_id":id,"link":format!("wispnet://{host}/{id}"),"url":url}),
        )
    }
}
pub(crate) async fn start(client: Arc<Client>, port: u16) -> Result<JoinHandle<()>, String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|_| {
        format!("Registry port {port} unavailable; choose another port in Access settings")
    })?;
    Ok(tokio::spawn(async move {
        let slots = client.gateway.connections.clone();
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted=listener.accept()=>{let Ok((socket,_))=accepted else {break};let Ok(permit)=slots.clone().try_acquire_owned() else {continue};let c=client.clone();tasks.spawn(async move {let _permit=permit;let _=handle(c,socket,port).await;});},
                _=tasks.join_next(),if !tasks.is_empty()=>{}
            }
        }
    }))
}
async fn handle(client: Arc<Client>, mut socket: TcpStream, port: u16) -> Result<(), ()> {
    let mut request = match headers(&mut socket).await.and_then(|b| parse(&b)) {
        Ok(r) => r,
        Err(_) => return response(&mut socket, "400 Bad Request", "Invalid request", "").await,
    };
    if request.safe(&format!("wispnet.localhost:{port}")) {
        if request.path == "/mcp" {
            return crate::mcp::serve(client, socket, request).await;
        }
        request.path = match request.path.as_str() {
            "/registry" | "/registry/" | "/registry/v1/services" => "/v1/services".into(),
            "/registry/v1/resolve" => "/v1/resolve".into(),
            _ => {
                return response(&mut socket, "404 Not Found", "Unknown Wispnet endpoint", "")
                    .await;
            }
        };
        return tokio::time::timeout(
            Duration::from_secs(25),
            handle_registry(client, socket, request),
        )
        .await
        .map_err(|_| ())?;
    }
    let authorities = [
        format!("registry.wispnet.localhost:{port}"),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
    ];
    if !authorities.iter().any(|a| request.safe(a)) {
        return client.gateway.handle(socket, request).await;
    }
    // Registry bodies stay bounded; service streams have their own deadlines.
    tokio::time::timeout(
        Duration::from_secs(25),
        handle_registry(client, socket, request),
    )
    .await
    .map_err(|_| ())?
}
async fn handle_registry(
    client: Arc<Client>,
    mut socket: TcpStream,
    request: crate::gateway::Request,
) -> Result<(), ()> {
    let settings = Settings::load(&client.state).map_err(|_| ())?;
    let cors = if let Some(origin) = request.allowed_website(&settings) {
        format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n")
    } else if request.native()
        || (request.method == "GET"
            && request.values("origin").is_empty()
            && matches!(
                request.one("sec-fetch-site"),
                Some("none" | "same-origin" | "same-site" | "cross-site")
            )
            && request.one("sec-fetch-mode") == Some("navigate")
            && request.one("sec-fetch-dest") == Some("document")
            && ["sec-fetch-site", "sec-fetch-mode", "sec-fetch-dest"]
                .iter()
                .all(|header| request.values(header).len() == 1))
    {
        String::new()
    } else {
        return response(
            &mut socket,
            "403 Forbidden",
            "Website origin not allowed",
            "",
        )
        .await;
    };
    let (_, length) = match super::gateway::http::framing(&request) {
        Ok((false, n)) if n <= 4096 => (false, n),
        _ => return response(&mut socket, "400 Bad Request", "Invalid framing", "").await,
    };
    if request.method == "OPTIONS"
        && length == 0
        && ["/v1/services", "/v1/resolve"].contains(&request.path.as_str())
    {
        return response(&mut socket,"204 No Content","",&format!("{cors}Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Max-Age: 60\r\n")).await;
    }
    let result = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/services") if length == 0 => client.service_list().await,
        ("POST", "/v1/resolve")
            if request
                .one("content-type")
                .is_some_and(|s| s.split(';').next() == Some("application/json")) =>
        {
            let mut bytes = vec![0; length as usize];
            socket.read_exact(&mut bytes).await.map_err(|_| ())?;
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) if v.get("link").is_some() => {
                    match v["link"]
                        .as_str()
                        .ok_or("Invalid Wispnet link")
                        .and_then(wispnet_core::service_link::ServiceLink::parse)
                    {
                        Ok(link) => {
                            client
                                .resolve_service(&json!({"host":link.host,"service":link.service}))
                                .await
                        }
                        Err(error) => Err(error.into()),
                    }
                }
                Ok(v) => {
                    client
                        .resolve_service(&json!({"host":v["host_id"],"service":v["service_id"]}))
                        .await
                }
                Err(_) => Err("Invalid JSON".into()),
            }
        }
        _ => {
            return response(
                &mut socket,
                "404 Not Found",
                "Unknown registry endpoint",
                &cors,
            )
            .await;
        }
    };
    let (status, body) = match result {
        Ok(v) => ("200 OK", v),
        Err(e) => ("400 Bad Request", json!({"error":e})),
    };
    let body = body.to_string();
    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{cors}\r\n{body}",body.len()).as_bytes()).await.map_err(|_|())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_settings::ServicePort;
    use std::os::unix::fs::PermissionsExt;
    fn unused_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
    async fn call(port: u16, method: &str, path: &str, extra: &str, body: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n{body}",body.len()).as_bytes()).await.unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).await.unwrap();
        reply
    }
    async fn unified(port: u16, method: &str, path: &str, extra: &str, body: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: wispnet.localhost:{port}\r\nContent-Length: {}\r\n{extra}\r\n{body}",body.len()).as_bytes()).await.unwrap();
        let head = tokio::time::timeout(Duration::from_secs(35), headers(&mut socket))
            .await
            .unwrap()
            .unwrap();
        let head = String::from_utf8(head).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        head + &String::from_utf8(body).unwrap()
    }
    #[tokio::test]
    async fn shared_listener_dispatches_services_and_links_without_cross_routing() {
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = wispnet_core::identity::SecretKey::generate();
        let own = key.public();
        host::initialize(state.path(), own).unwrap();
        let (mut store, owner) = host::owner_store(state.path(), own).unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut servers = vec![];
        for (revision, id) in ["gike", "llama"].into_iter().enumerate() {
            let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
            store
                .host_mutate(
                    own,
                    &owner,
                    Mutation::Service {
                        expected_revision: revision as u64,
                        id: id.into(),
                        backend: backend.local_addr().unwrap(),
                        allow: vec![],
                        display_name: None,
                        kind: None,
                    },
                )
                .unwrap();
            let hits = hits.clone();
            servers.push(tokio::spawn(async move {
                while let Ok((mut socket, _)) = backend.accept().await {
                    headers(&mut socket).await.unwrap();
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{id}",
                                id.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            }));
        }
        let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .unwrap();
        let authority = Authority::open(state.path(), own).unwrap();
        authority.set_device_id("j-strix").unwrap();
        let client = Client::from_endpoint(endpoint, key, state.path().into(), config, authority);
        let port = unused_port();
        Settings {
            registry_port: port,
            ..Default::default()
        }
        .save(state.path())
        .unwrap();
        client.command(&json!({"op":"enable"})).await.unwrap();
        let directory = unified(port, "GET", "/registry", "", "").await;
        assert!(directory.starts_with("HTTP/1.1 200"), "{directory}");
        assert!(directory.contains("gike"));
        assert!(!directory.contains("mcp-token"));
        assert!(!state.path().join("mcp-token").exists());
        let rpc_headers =
            "Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n";
        let init=json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}).to_string();
        // Previously configured clients can keep sending their unused credential.
        for extra in [
            rpc_headers.to_owned(),
            format!("{rpc_headers}Authorization: Bearer legacy-unused-token\r\n"),
        ] {
            assert!(
                unified(port, "POST", "/mcp", &extra, &init)
                    .await
                    .starts_with("HTTP/1.1 200")
            );
        }
        for extra in [
            format!("{rpc_headers}Origin: https://evil.example\r\n"),
            format!("{rpc_headers}Origin: null\r\n"),
            format!("{rpc_headers}Sec-Fetch-Site: cross-site\r\n"),
        ] {
            assert!(
                unified(port, "POST", "/mcp", &extra, &init)
                    .await
                    .starts_with("HTTP/1.1 403")
            );
        }
        let initialized = unified(port, "POST", "/mcp", rpc_headers, &init).await;
        assert!(initialized.starts_with("HTTP/1.1 200"), "{initialized}");
        assert!(initialized.contains("2025-11-25"));
        assert!(!initialized.contains("legacy-unused-token"));
        assert!(
            unified(port, "GET", "/mcp", rpc_headers, "")
                .await
                .starts_with("HTTP/1.1 405")
        );
        let notification =
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
        assert!(
            unified(port, "POST", "/mcp", rpc_headers, &notification)
                .await
                .starts_with("HTTP/1.1 202")
        );
        let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string();
        let listed = unified(port, "POST", "/mcp", rpc_headers, &list).await;
        assert!(listed.contains("wispnet_settings"));
        assert!(!listed.contains("wispnet_set_service")); // No node socket in this fixture.
        let bad=json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wispnet_set_port","arguments":{"port":80}}}).to_string();
        assert!(
            unified(port, "POST", "/mcp", rpc_headers, &bad)
                .await
                .contains("-32602")
        );
        assert_eq!(Settings::load(state.path()).unwrap().registry_port, port);
        async fn fetch(port: u16, authority: &str) -> String {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            socket
                .write_all(
                    format!("GET / HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).await.unwrap();
            reply
        }
        let gike = format!("j-strix-{}-gike.localhost:{port}", &own.to_string()[..12]);
        let llama = format!("j-strix-{}-llama.localhost:{port}", &own.to_string()[..12]);
        // Opening the app's online access is sufficient; no tile or resolve first.
        let (a, b) = tokio::join!(fetch(port, &gike), fetch(port, &llama));
        assert!(a.ends_with("gike"), "{a}");
        assert!(b.ends_with("llama"), "{b}");
        let link = format!("wispnet://{own}/gike");
        let resolved = client
            .command(&json!({"op":"resolve_link","link":link}))
            .await
            .unwrap();
        assert_eq!(resolved["url"], format!("http://{gike}/"));
        let registry = call(
            port,
            "POST",
            "/v1/resolve",
            "Content-Type: application/json\r\n",
            &json!({"link":link}).to_string(),
        )
        .await;
        assert!(registry.starts_with("HTTP/1.1 200"));
        assert!(registry.contains(&format!("http://{gike}/")));
        assert!(
            fetch(port, &format!("unknown.localhost:{port}"))
                .await
                .starts_with("HTTP/1.1 421")
        );
        assert!(
            fetch(port, &format!("{gike}\r\nHost: {llama}"))
                .await
                .starts_with("HTTP/1.1 421")
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        let changed = unused_port();
        let change=json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"wispnet_set_port","arguments":{"port":changed}}}).to_string();
        let acknowledgement = unified(port, "POST", "/mcp", rpc_headers, &change).await;
        assert!(
            acknowledgement.starts_with("HTTP/1.1 200"),
            "{acknowledgement}"
        );
        assert!(
            acknowledgement.contains("\"isError\":false"),
            "{acknowledgement}"
        );
        assert!(!state.path().join("mcp-token").exists());
        let moved = client
            .command(&json!({"op":"resolve_link","link":link}))
            .await
            .unwrap();
        assert_eq!(
            moved["url"],
            format!(
                "http://{}/",
                gike.replace(&format!(":{port}"), &format!(":{changed}"))
            )
        );
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
        store
            .host_mutate(
                own,
                &owner,
                Mutation::Remove {
                    expected_revision: 2,
                    id: "gike".into(),
                },
            )
            .unwrap();
        assert!(
            client
                .command(&json!({"op":"resolve_link","link":link}))
                .await
                .is_err()
        );
        assert!(
            fetch(
                changed,
                &llama.replace(&format!(":{port}"), &format!(":{changed}"))
            )
            .await
            .ends_with("llama")
        );
        let stop=json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"wispnet_set_access","arguments":{"enabled":false}}}).to_string();
        let acknowledgement = unified(changed, "POST", "/mcp", rpc_headers, &stop).await;
        assert!(
            acknowledgement.contains("\"isError\":false"),
            "{acknowledgement}"
        );
        assert!(TcpStream::connect(("127.0.0.1", changed)).await.is_err());
        client.gateway.stop().await;
        client.endpoint.close().await;
        for server in servers {
            server.abort();
        }
    }
    #[tokio::test]
    async fn registry_ports_tokenless_origins_policy_and_lifecycle() {
        use wispnet_registry::host::{self, Authority, Mutation};
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = wispnet_core::identity::SecretKey::generate();
        let own = key.public();
        host::initialize(state.path(), own).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend_hits = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = backend.accept().await else {
                    break;
                };
                let head = headers(&mut socket).await.unwrap();
                backend_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(!String::from_utf8_lossy(&head).contains("X-Wispnet-Access"));
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
            }
        });
        let (mut store, owner) = host::owner_store(state.path(), own).unwrap();
        store
            .host_mutate(
                own,
                &owner,
                Mutation::Service {
                    expected_revision: 0,
                    id: "llama".into(),
                    backend: address,
                    allow: vec![],
                    display_name: None,
                    kind: None,
                },
            )
            .unwrap();
        let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .unwrap();
        let client = Client::from_endpoint(
            endpoint,
            key,
            state.path().into(),
            config,
            Authority::open(state.path(), own).unwrap(),
        );
        let registry = unused_port();
        let service_port = registry;
        let settings = Settings {
            registry_port: registry,
            allowed_origins: vec!["https://chat.example".into()],
            service_ports: vec![ServicePort {
                host: own,
                service: "llama".into(),
                port: service_port,
            }],
        };
        settings.save(state.path()).unwrap();
        client.command(&json!({"op":"enable"})).await.unwrap();
        let list = call(registry, "GET", "/v1/services", "", "").await;
        assert!(list.contains("llama"));
        let directory: Value =
            serde_json::from_str(list.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let listed_url = directory["services"][0]["url"]
            .as_str()
            .expect("Listing must create the route without an earlier open or resolve");
        assert_eq!(
            url::Url::parse(listed_url).unwrap().port(),
            Some(service_port)
        );

        for site in ["none", "same-origin", "same-site", "cross-site"] {
            let navigation = format!(
                "Sec-Fetch-Site: {site}\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n"
            );
            let reply = call(registry, "GET", "/v1/services", &navigation, "").await;
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            assert!(reply.contains("Content-Type: application/json"));
            assert!(reply.contains("llama"));
        }
        let duplicate = "Sec-Fetch-Site: none\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n";
        assert!(
            call(registry, "GET", "/v1/services", duplicate, "")
                .await
                .starts_with("HTTP/1.1 403")
        );

        assert!(!list.contains(&address.to_string()));
        assert!(
            call(
                registry,
                "GET",
                "/v1/services",
                "Origin: https://unlisted.example\r\n",
                ""
            )
            .await
            .starts_with("HTTP/1.1 200")
        );
        assert!(
            call(registry, "GET", "/v1/services", "Origin: null\r\n", "")
                .await
                .starts_with("HTTP/1.1 200")
        );
        let preflight = call(
            registry,
            "OPTIONS",
            "/v1/resolve",
            "Origin: https://chat.example\r\nAccess-Control-Request-Method: POST\r\n",
            "",
        )
        .await;
        assert!(preflight.contains("Access-Control-Allow-Origin: https://chat.example"));
        let body = json!({"host_id":own,"service_id":"llama"}).to_string();
        let result = call(
            registry,
            "POST",
            "/v1/resolve",
            "Content-Type: application/json\r\nOrigin: https://chat.example\r\n",
            &body,
        )
        .await;
        assert!(result.starts_with("HTTP/1.1 200"), "{result}");
        let resolved: Value =
            serde_json::from_str(result.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let url = url::Url::parse(resolved["url"].as_str().unwrap()).unwrap();
        assert_eq!(url.port(), Some(service_port));
        assert!(resolved.get("access_token").is_none());
        assert!(resolved.get("header_name").is_none());
        assert!(resolved.get("expires_in_seconds").is_none());
        async fn service(port: u16, authority: &str, extra: &str) -> String {
            request_service(port, authority, "GET", extra).await
        }
        async fn request_service(port: u16, authority: &str, method: &str, extra: &str) -> String {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            socket
                .write_all(
                    format!(
                        "{method} / HTTP/1.1\r\nHost: {authority}\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).await.unwrap();
            reply
        }
        let authority = format!("{}:{service_port}", url.host_str().unwrap());
        let mut direct = TcpStream::connect(("127.0.0.1", service_port))
            .await
            .unwrap();
        direct.write_all(format!("POST /_wispnet/access HTTP/1.1\r\nHost: {authority}\r\nOrigin: https://chat.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut direct_reply = String::new();
        direct.read_to_string(&mut direct_reply).await.unwrap();
        assert!(direct_reply.starts_with("HTTP/1.1 404"), "{direct_reply}");
        let auth = String::from("Origin: https://chat.example\r\n");
        let reply = service(service_port, &authority, &auth).await;
        assert!(reply.ends_with("ok"), "{reply}");
        assert!(reply.contains("Access-Control-Allow-Origin: https://chat.example"));
        assert!(
            service(service_port, &authority, "Origin: https://chat.example\r\n")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert!(
            service(service_port, &authority, "Origin: https://evil.example\r\n")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert!(
            request_service(service_port, &authority, "POST", "")
                .await
                .ends_with("ok")
        );
        assert!(
            service(service_port, &authority, "X-Wispnet-Access: retired\r\n")
                .await
                .ends_with("ok")
        );
        for origin in ["https://unlisted.example", "http://localhost:18765", "null"] {
            let headers = format!("Origin: {origin}\r\n");
            let reply = request_service(service_port, &authority, "POST", &headers).await;
            assert!(reply.ends_with("ok"), "{origin}: {reply}");
            assert!(reply.contains(&format!("Access-Control-Allow-Origin: {origin}\r\n")));
            let preflight = call(registry, "OPTIONS", "/v1/resolve", &headers, "").await;
            assert!(preflight.starts_with("HTTP/1.1 204"), "{preflight}");
            assert!(preflight.contains(&format!("Access-Control-Allow-Origin: {origin}\r\n")));
        }
        let before = hits.load(std::sync::atomic::Ordering::SeqCst);
        for extra in [
            "Origin: https://chat.example\r\nOrigin: https://evil.example\r\n",
            "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\n",
            "Origin: https://chat.example\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Site: same-origin\r\n",
        ] {
            for method in ["GET", "POST", "OPTIONS"] {
                let reply = request_service(service_port, &authority, method, extra).await;
                assert!(
                    reply.starts_with("HTTP/1.1 403"),
                    "{method} {extra}: {reply}"
                );
            }
            assert!(
                call(registry, "GET", "/v1/services", extra, "")
                    .await
                    .starts_with("HTTP/1.1 403")
            );
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "Denied requests reached backend"
        );
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut bad = settings.clone();
        bad.registry_port = occupied.local_addr().unwrap().port();
        assert!(client.gateway.configure(client.clone(), bad).await.is_err());
        assert_eq!(Settings::load(state.path()).unwrap(), settings);
        assert!(
            call(registry, "GET", "/v1/services", "", "")
                .await
                .starts_with("HTTP/1.1 200")
        );
        let new_port = unused_port();
        let mut changed = settings.clone();
        changed.registry_port = new_port;
        changed.service_ports[0].port = unused_port();
        client
            .gateway
            .configure(client.clone(), changed.clone())
            .await
            .unwrap();
        assert!(TcpStream::connect(("127.0.0.1", registry)).await.is_err());
        assert!(
            TcpStream::connect(("127.0.0.1", service_port))
                .await
                .is_err()
        );
        client.command(&json!({"op":"stop"})).await.unwrap();
        assert!(TcpStream::connect(("127.0.0.1", new_port)).await.is_err());
        client.command(&json!({"op":"enable"})).await.unwrap();
        assert_eq!(Settings::load(state.path()).unwrap(), changed);
        store
            .host_mutate(
                own,
                &owner,
                Mutation::Remove {
                    expected_revision: 1,
                    id: "llama".into(),
                },
            )
            .unwrap();
        assert!(
            !call(
                new_port,
                "POST",
                "/v1/resolve",
                "Content-Type: application/json\r\n",
                &body
            )
            .await
            .starts_with("HTTP/1.1 200")
        );
        client.gateway.stop().await;
        client.endpoint.close().await;
        server.abort();
    }
}

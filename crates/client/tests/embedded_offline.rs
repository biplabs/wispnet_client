#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use wispnet_agent::{
    control,
    tunnel::{self, TunnelHost},
};
use wispnet_client::{
    Client,
    public_access::{ALPN, Registration, hostname, read_json, valid_domain, write_json},
};
#[allow(dead_code)]
#[path = "../src/test_support/public_ingress.rs"]
mod ingress;
use ingress::Ingress;
use wispnet_core::{
    config::{ServiceConfig, TransportConfig},
    identity::SecretKey,
};

struct PlatformStorage;
impl wispnet_files::Publisher for PlatformStorage {
    fn publish(
        &self,
        _: wispnet_files::Publication<'_>,
        _: std::fs::File,
        _: Arc<AtomicBool>,
    ) -> io::Result<()> {
        unreachable!("this test transfers HTTP/TCP data only")
    }
}

async fn mutate(client: &Arc<Client>, operation: Value) {
    client
        .command(&json!({"op":"local_owner_mutate","operation":operation}))
        .await
        .unwrap();
}

async fn read_through(reader: &mut (impl AsyncRead + Unpin), suffix: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(suffix) {
            let mut byte = [0];
            reader.read_exact(&mut byte).await.unwrap();
            bytes.extend(byte);
            assert!(bytes.len() < 8192);
        }
        bytes
    })
    .await
    .unwrap()
}

async fn assert_closed(reader: &mut (impl AsyncRead + Unpin)) {
    let mut tail = Vec::new();
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        reader.take(1024).read_to_end(&mut tail),
    )
    .await
    .expect("paused host must close the active stream");
    assert!(tail.len() < 1024, "paused host must not continue streaming");
}

async fn public_request(port: u16, name: &str) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn platform_offline_stops_http_tcp_and_public_streams_without_mutating_grants() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = SecretKey::generate();
        let own = key.public();
        let remote_key = SecretKey::generate();
        let remote_id = remote_key.public();
        let config: TransportConfig = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let client = Client::new_peer_with_publisher(key, state.path().into(), config.clone(), Some(Arc::new(PlatformStorage))).await.unwrap();
        assert!(!client.services_enabled());

        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            while let Ok((stream, _)) = echo.accept().await {
                tokio::spawn(async move { let (mut reader, mut writer) = stream.into_split(); let _ = tokio::io::copy(&mut reader, &mut writer).await; });
            }
        });
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http.local_addr().unwrap();
        let http_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = http.accept().await {
                tokio::spawn(async move {
                    let request = read_through(&mut stream, b"\r\n\r\n").await;
                    let response: &[u8] = if request.starts_with(b"GET /hold ") {
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n"
                    } else { b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello" };
                    let _ = stream.write_all(response).await;
                    std::future::pending::<()>().await;
                });
            }
        });
        let remote = TunnelHost::bind(&remote_key, &config, &[ServiceConfig {
            id: "echo".into(), backend: echo_addr, allow: vec![own],
        }]).await.unwrap();
        mutate(&client, json!({"op":"approve","expected_revision":0,"peer":remote_id,"name":"laptop"})).await;
        for (revision, id, kind, backend) in [(1, "website", "http", http_addr), (2, "database", "tcp", echo_addr)] {
            mutate(&client, json!({"op":"service","expected_revision":revision,"id":id,"backend":backend,"kind":kind,"allow":[remote_id]})).await;
        }
        let address = client.endpoint.bound_sockets()[0];
        assert!(tunnel::connect(remote.endpoint(), own, address).await.is_err());
        assert!(control::connect(remote.endpoint(), own, address).await.is_err());

        let gateway = tunnel::bind_configured_client(&SecretKey::generate(), Some(&config)).await.unwrap();
        let gateway_address = gateway.bound_sockets()[0];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = listener.local_addr().unwrap().port();
        let ingress = Ingress::new("wispnet.test".into(), vec![own]).unwrap();
        let gateway_task = tokio::spawn(ingress.run(gateway.clone(), listener));
        let before = client.command(&json!({"op":"local_owner_status"})).await.unwrap();
        let services = before["host"]["services"].as_array().unwrap();
        let web = services.iter().find(|s| s["id"] == "website").unwrap();
        let database = services.iter().find(|s| s["id"] == "database").unwrap();
        client.command(&json!({"op":"configure_public_gateway","gateway":{"endpoint_id":gateway.id(),"relay_url":"https://relay.invalid","domain":"wispnet.test","direct_addr":gateway_address}})).await.unwrap();
        client.command(&json!({"op":"configure_public","service":"website","incarnation":web["incarnation"],"enabled":true,"name":"phone"})).await.unwrap();
        assert_eq!(client.command(&json!({"op":"public_status"})).await.unwrap()["services"][0]["state"], "offline");
        let publication_config = std::fs::read(state.path().join("public-services.json")).unwrap();
        let registry = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let registry_port = registry.local_addr().unwrap().port();
        drop(registry);
        client.command(&json!({"op":"configure_access","settings":{"registry_port":registry_port}})).await.unwrap();
        client.command(&json!({"op":"enable"})).await.unwrap();
        let connection = tunnel::connect(remote.endpoint(), own, address).await.unwrap();
        let mut direct_http = tunnel::open_service_bound(&connection, "website", web["incarnation"].as_str().unwrap()).await.unwrap();
        direct_http.write_all(b"GET /hold HTTP/1.1\r\nHost: test\r\n\r\n").await.unwrap();
        read_through(&mut direct_http, b"x\r\n").await;
        let mut direct_tcp = tunnel::open_service_bound(&connection, "database", database["incarnation"].as_str().unwrap()).await.unwrap();
        direct_tcp.write_all(b"ping").await.unwrap();
        read_through(&mut direct_tcp, b"ping").await;
        for _ in 0..100 {
            if client.command(&json!({"op":"public_status"})).await.unwrap()["services"][0]["state"] == "online" { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(client.command(&json!({"op":"public_status"})).await.unwrap()["services"][0]["state"], "online");
        let name = hostname("phone", own, "website", "wispnet.test");
        let mut public_http = tokio::net::TcpStream::connect(("127.0.0.1", gateway_port)).await.unwrap();
        public_http.write_all(format!("GET /hold HTTP/1.1\r\nHost: {name}\r\n\r\n").as_bytes()).await.unwrap();
        read_through(&mut public_http, b"x\r\n").await;

        client.command(&json!({"op":"stop"})).await.unwrap();
        assert!(!client.services_enabled());
        assert_closed(&mut direct_http).await;
        assert_closed(&mut direct_tcp).await;
        assert_closed(&mut public_http).await;
        tokio::time::timeout(Duration::from_secs(3), connection.closed()).await.unwrap();
        assert!(tunnel::connect(remote.endpoint(), own, address).await.is_err());
        assert!(control::connect(remote.endpoint(), own, address).await.is_err());
        for _ in 0..3 {
            let offline = client.command(&json!({"op":"local_owner_status"})).await.unwrap();
            assert_eq!(offline["host"], before["host"]);
            assert_eq!(offline["public_access"]["services"][0]["state"], "offline");
            assert_eq!(std::fs::read(state.path().join("public-services.json")).unwrap(), publication_config);
        }
        assert!(!public_request(gateway_port, &name).await.ends_with(b"hello"), "owner status must not restart an offline public service");
        // The endpoint remains usable for outgoing connections while hosting is paused.
        let outgoing = tunnel::connect(&client.endpoint, remote_id, remote.endpoint().bound_sockets()[0]).await.unwrap();
        let mut outgoing_stream = tunnel::open_service(&outgoing, "echo").await.unwrap();
        outgoing_stream.write_all(b"outbound").await.unwrap();
        read_through(&mut outgoing_stream, b"outbound").await;
        client.command(&json!({"op":"enable"})).await.unwrap();
        let restored = tunnel::connect(remote.endpoint(), own, address).await.unwrap();
        let mut stream = tunnel::open_service_bound(&restored, "database", database["incarnation"].as_str().unwrap()).await.unwrap();
        stream.write_all(b"restored").await.unwrap();
        read_through(&mut stream, b"restored").await;
        assert_eq!(client.command(&json!({"op":"local_owner_status"})).await.unwrap()["host"], before["host"]);
        for _ in 0..100 {
            if client.command(&json!({"op":"public_status"})).await.unwrap()["services"][0]["state"] == "online" { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(client.command(&json!({"op":"public_status"})).await.unwrap()["services"][0]["state"], "online");
        assert!(public_request(gateway_port, &name).await.ends_with(b"hello"));
        assert_eq!(std::fs::read(state.path().join("public-services.json")).unwrap(), publication_config);
        client.command(&json!({"op":"stop"})).await.unwrap();
        client.endpoint.close().await;
        remote.shutdown().await;
        gateway_task.abort();
        gateway.close().await;
        echo_task.abort();
        http_task.abort();
    }).await.unwrap();
}

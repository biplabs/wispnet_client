#![cfg(unix)]

use serde_json::json;
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Barrier, Notify},
    task::{JoinHandle, JoinSet},
};
use wispnet_agent::tunnel::{ActiveCounts, TunnelHost};
use wispnet_client::{Client, access_settings::Settings};
use wispnet_core::{config::TransportConfig, identity::SecretKey, pairing::Bootstrap};
use wispnet_registry::host::{self, Authority, Mutation};

const NAVIGATION: &str =
    "Sec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n";

struct Fixture {
    host: TunnelHost,
    client: Arc<Client>,
    host_state: tempfile::TempDir,
    _client_state: tempfile::TempDir,
    backend: JoinHandle<()>,
    release_cancelled: Arc<Notify>,
    release_survivor: Arc<Notify>,
    incarnation: String,
}

impl Fixture {
    async fn new() -> Self {
        let host_state = tempfile::tempdir().unwrap();
        let client_state = tempfile::tempdir().unwrap();
        for state in [host_state.path(), client_state.path()] {
            std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let host_key = SecretKey::generate();
        let client_key = SecretKey::generate();
        host::initialize(host_state.path(), host_key.public()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_address = listener.local_addr().unwrap();
        let release_cancelled = Arc::new(Notify::new());
        let release_survivor = Arc::new(Notify::new());
        let cancelled = release_cancelled.clone();
        let survivor = release_survivor.clone();
        let backend = tokio::spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = requests.join_next(), if !requests.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let cancelled = cancelled.clone();
                        let survivor = survivor.clone();
                        requests.spawn(async move {
                            let Ok(head) = read_headers(&mut stream).await else { return };
                            if stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.is_err() {
                                return;
                            }
                            if head.starts_with(b"GET /cancel ") {
                                cancelled.notified().await;
                            } else if head.starts_with(b"GET /survivor ") {
                                survivor.notified().await;
                            }
                            let _ = stream.write_all(b"done").await;
                        });
                    }
                }
            }
        });
        {
            let (mut store, owner) =
                host::owner_store(host_state.path(), host_key.public()).unwrap();
            store
                .host_mutate(
                    host_key.public(),
                    &owner,
                    Mutation::Approve {
                        expected_revision: 0,
                        peer: client_key.public(),
                        name: "browser-client".into(),
                    },
                )
                .unwrap();
            store
                .host_mutate(
                    host_key.public(),
                    &owner,
                    Mutation::Service {
                        expected_revision: 1,
                        id: "site".into(),
                        backend: backend_address,
                        allow: vec![client_key.public()],
                        display_name: None,
                        kind: None,
                    },
                )
                .unwrap();
        }
        let authority = Authority::open(host_state.path(), host_key.public()).unwrap();
        let incarnation = authority
            .services()
            .await
            .unwrap()
            .into_iter()
            .find(|service| service.id == "site")
            .unwrap()
            .incarnation;
        let config: TransportConfig = serde_json::from_value(json!({
            "bind_addr":"127.0.0.1:0", "max_connections":4,
            "max_streams":32, "max_streams_per_connection":4
        }))
        .unwrap();
        let host = TunnelHost::bind_host(&host_key, &config, authority)
            .await
            .unwrap();
        wispnet_agent::peers::save(
            client_state.path(),
            &Bootstrap {
                version: 1,
                host: host_key.public(),
                relay_url: "https://fixture.invalid".into(),
                direct_hint: Some(host.endpoint().bound_sockets()[0]),
            },
        )
        .unwrap();
        let browser_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        Settings {
            registry_port: browser_port.local_addr().unwrap().port(),
            ..Settings::default()
        }
        .save(client_state.path())
        .unwrap();
        drop(browser_port);
        let client = Client::new(client_key, client_state.path().into(), config)
            .await
            .unwrap();
        Self {
            host,
            client,
            host_state,
            _client_state: client_state,
            backend,
            release_cancelled,
            release_survivor,
            incarnation,
        }
    }

    async fn browser_url(&self) -> String {
        self.client.command(&json!({"op":"enable"})).await.unwrap();
        self.client
            .gateway
            .open(
                self.client.clone(),
                self.host.endpoint().id(),
                "site".into(),
                self.incarnation.clone(),
            )
            .await
            .unwrap()
    }

    fn last_connection(&self) -> u64 {
        self.host.counts().peer_transport()[self.client.endpoint.id().to_string()]["connection"]
            ["connection_id"]
            .as_u64()
            .expect("host observed an authenticated peer connection")
    }

    async fn mutate(&self, operation: impl FnOnce(u64) -> Mutation) {
        let own = self.host.endpoint().id();
        let revision = Authority::open(self.host_state.path(), own)
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .revision;
        let (mut store, owner) = host::owner_store(self.host_state.path(), own).unwrap();
        store.host_mutate(own, &owner, operation(revision)).unwrap();
    }

    async fn close(self) {
        self.client.command(&json!({"op":"stop"})).await.unwrap();
        self.client.endpoint.close().await;
        self.host.shutdown().await;
        self.backend.abort();
        let _ = self.backend.await;
    }
}

async fn read_headers(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let byte = stream.read_u8().await?;
        bytes.push(byte);
        assert!(bytes.len() <= 32768, "fixture HTTP header exceeded bound");
    }
    Ok(bytes)
}

async fn browser_request(url: &str, path: &str) -> TcpStream {
    let url = url::Url::parse(url).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {}:{}\r\n{NAVIGATION}\r\n",
                url.host_str().unwrap(),
                url.port().unwrap()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream
}

async fn response(url: &str, path: &str) -> String {
    let mut stream = browser_request(url, path).await;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8(bytes).unwrap()
}

async fn wait_counts(counts: &ActiveCounts, connections: usize, streams: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counts.connections() != connections || counts.streams() != streams {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {connections} connections/{streams} streams; observed {}/{}",
            counts.connections(),
            counts.streams()
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_catalogs_and_http_requests_reuse_one_session_per_protocol() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fixture = Fixture::new().await;
        let peer = fixture.host.endpoint().id();
        let barrier = Arc::new(Barrier::new(9));
        let mut calls = JoinSet::new();
        for _ in 0..8 {
            let client = fixture.client.clone();
            let barrier = barrier.clone();
            calls.spawn(async move {
                barrier.wait().await;
                client
                    .command(&json!({"op":"host_device_id", "host":peer}))
                    .await
                    .unwrap()
            });
        }
        barrier.wait().await;
        while let Some(result) = calls.join_next().await {
            assert!(result.unwrap()["device_id"].is_string());
        }
        wait_counts(&fixture.host.counts(), 1, 0).await;
        let control_id = fixture.last_connection();
        for _ in 0..3 {
            fixture.client.catalog(peer).await.unwrap();
            assert_eq!(fixture.last_connection(), control_id);
        }
        let barrier = Arc::new(Barrier::new(9));
        let mut calls = JoinSet::new();
        for _ in 0..8 {
            let client = fixture.client.clone();
            let barrier = barrier.clone();
            calls.spawn(async move {
                barrier.wait().await;
                match client.catalog(peer).await {
                    Ok(catalog) => Some(catalog),
                    Err(error) => {
                        // Snapshot acceptance is independently serialized. Pooling must
                        // preserve its existing contention result without retrying RPCs.
                        assert_eq!(error, "Catalog refresh busy; retry this host");
                        None
                    }
                }
            });
        }
        barrier.wait().await;
        let mut accepted = 0;
        while let Some(result) = calls.join_next().await {
            if let Some(catalog) = result.unwrap() {
                assert_eq!(catalog["host"], json!(peer));
                accepted += 1;
            }
        }
        assert!(accepted > 0);
        wait_counts(&fixture.host.counts(), 1, 0).await;
        assert_eq!(fixture.last_connection(), control_id);

        let url = fixture.browser_url().await;
        assert!(response(&url, "/").await.ends_with("done"));
        wait_counts(&fixture.host.counts(), 2, 0).await;
        let http_id = fixture.last_connection();
        assert_ne!(http_id, control_id);
        let barrier = Arc::new(Barrier::new(9));
        let mut requests = JoinSet::new();
        for _ in 0..8 {
            let url = url.clone();
            let barrier = barrier.clone();
            requests.spawn(async move {
                barrier.wait().await;
                response(&url, "/parallel").await
            });
        }
        barrier.wait().await;
        while let Some(result) = requests.join_next().await {
            let reply = result.unwrap();
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            assert!(reply.ends_with("done"));
        }
        wait_counts(&fixture.host.counts(), 2, 0).await;
        assert_eq!(fixture.last_connection(), http_id);
        fixture.client.catalog(peer).await.unwrap();
        assert_eq!(fixture.last_connection(), control_id);
        fixture.close().await;
    })
    .await
    .expect("connection reuse fixture timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_browser_stream_and_revoking_grants_preserve_session_isolation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fixture = Fixture::new().await;
        let url = fixture.browser_url().await;
        let mut cancelled = browser_request(&url, "/cancel").await;
        assert!(
            read_headers(&mut cancelled)
                .await
                .unwrap()
                .starts_with(b"HTTP/1.1 200")
        );
        let mut survivor = browser_request(&url, "/survivor").await;
        assert!(
            read_headers(&mut survivor)
                .await
                .unwrap()
                .starts_with(b"HTTP/1.1 200")
        );
        wait_counts(&fixture.host.counts(), 2, 2).await;
        let http_id = fixture.last_connection();
        drop(cancelled);
        fixture.release_cancelled.notify_one();
        wait_counts(&fixture.host.counts(), 2, 1).await;
        fixture.release_survivor.notify_one();
        let mut body = Vec::new();
        survivor.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"done");
        wait_counts(&fixture.host.counts(), 2, 0).await;
        assert_eq!(fixture.last_connection(), http_id);

        let caller = fixture.client.endpoint.id();
        fixture
            .mutate(|revision| Mutation::RevokeService {
                expected_revision: revision,
                service: "site".into(),
                peer: caller,
            })
            .await;
        assert!(response(&url, "/revoked").await.starts_with("HTTP/1.1 403"));
        assert_eq!(fixture.last_connection(), http_id);
        fixture
            .mutate(|revision| Mutation::GrantService {
                expected_revision: revision,
                service: "site".into(),
                peer: caller,
            })
            .await;
        assert!(response(&url, "/restored").await.ends_with("done"));
        assert_eq!(fixture.last_connection(), http_id);
        fixture
            .mutate(|revision| Mutation::Revoke {
                expected_revision: revision,
                peer: caller,
            })
            .await;
        assert!(
            fixture
                .client
                .catalog(fixture.host.endpoint().id())
                .await
                .is_err()
        );
        assert!(
            response(&url, "/revoked-peer")
                .await
                .starts_with("HTTP/1.1 403")
        );
        fixture.close().await;
    })
    .await
    .expect("stream isolation fixture timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_stop_and_unpair_invalidate_reusable_sessions() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fixture = Fixture::new().await;
        let peer = fixture.host.endpoint().id();
        let url = fixture.browser_url().await;
        assert!(response(&url, "/").await.ends_with("done"));
        let first_http = fixture.last_connection();
        let mut interrupted = browser_request(&url, "/survivor").await;
        assert!(
            read_headers(&mut interrupted)
                .await
                .unwrap()
                .starts_with(b"HTTP/1.1 200")
        );
        wait_counts(&fixture.host.counts(), 2, 1).await;
        let changed = fixture
            .client
            .command(&json!({"op":"network"}))
            .await
            .unwrap();
        assert!(changed["interrupted_requests"].as_u64().unwrap() >= 1);
        let mut old_body = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            interrupted.read_to_end(&mut old_body),
        )
        .await
        .expect("network recovery left the old browser socket open");
        assert!(
            old_body.is_empty(),
            "interrupted request was resumed or replayed"
        );
        fixture.release_survivor.notify_one();
        wait_counts(&fixture.host.counts(), 0, 0).await;
        fixture.client.catalog(peer).await.unwrap();
        assert!(response(&url, "/after-network").await.ends_with("done"));
        assert_ne!(fixture.last_connection(), first_http);

        let second_http = fixture.last_connection();
        fixture.client.command(&json!({"op":"stop"})).await.unwrap();
        wait_counts(&fixture.host.counts(), 0, 0).await;
        assert!(!fixture.client.gateway.is_enabled());
        fixture
            .client
            .command(&json!({"op":"enable"}))
            .await
            .unwrap();
        assert!(response(&url, "/after-stop").await.ends_with("done"));
        assert_ne!(fixture.last_connection(), second_http);
        wait_counts(&fixture.host.counts(), 2, 0).await;

        fixture
            .client
            .command(&json!({"op":"unpair", "host":peer}))
            .await
            .unwrap();
        wait_counts(&fixture.host.counts(), 0, 0).await;
        assert!(fixture.client.saved_host(peer).await.unwrap().is_none());
        assert!(fixture.client.catalog(peer).await.is_err());
        fixture.close().await;
    })
    .await
    .expect("session invalidation fixture timed out");
}

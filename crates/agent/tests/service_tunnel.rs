use iroh::{Endpoint, endpoint::Connection};
use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use wispnet_agent::tunnel::{self, TunnelError, TunnelHost};
use wispnet_core::{
    config::{ServiceConfig, TransportConfig},
    identity::SecretKey,
    protocol::{self, Status},
};

struct Live(Arc<AtomicUsize>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Backend {
    address: SocketAddr,
    total: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    worker: JoinHandle<()>,
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
async fn backend<F, Fut>(handler: F) -> Backend
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let total = Arc::new(AtomicUsize::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let (total_inner, live_inner) = (total.clone(), live.clone());
    let handler = Arc::new(handler);
    let worker = tokio::spawn(async move {
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break };
                    total_inner.fetch_add(1, Ordering::SeqCst);
                    live_inner.fetch_add(1, Ordering::SeqCst);
                    let guard = Live(live_inner.clone());
                    let handler = handler.clone();
                    handlers.spawn(async move { let _guard = guard; handler(socket).await; });
                },
                Some(result) = handlers.join_next(), if !handlers.is_empty() => { result.unwrap(); }
            }
        }
    });
    Backend {
        address,
        total,
        live,
        worker,
    }
}
fn limits() -> TransportConfig {
    TransportConfig {
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: None,
        relay_only: false,
        relay_ca_file: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        max_connections: 8,
        max_streams: 16,
        max_streams_per_connection: 8,
    }
}
fn service(id: &str, backend: &Backend, peer: &SecretKey) -> ServiceConfig {
    ServiceConfig {
        id: id.into(),
        backend: backend.address,
        allow: vec![peer.public()],
    }
}
async fn connected(host: &TunnelHost, key: &SecretKey) -> (Endpoint, Connection) {
    let endpoint = tunnel::bind_client(key).await.unwrap();
    let connection = tunnel::connect(
        &endpoint,
        host.endpoint().id(),
        host.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    (endpoint, connection)
}
async fn until(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("resource state did not settle");
}
async fn http_header(socket: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
        request.push(socket.read_u8().await.unwrap());
    }
    request
}
async fn raw_status(connection: &Connection, frame: &[u8], finish: bool) -> Status {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(frame).await.unwrap();
    if finish {
        send.finish().unwrap();
    }
    let mut reply = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(7), recv.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    protocol::decode_response(&reply[2..]).unwrap()
}

#[tokio::test]
async fn http_page_and_stream_arrive_before_backend_eof() {
    let release = Arc::new(tokio::sync::Notify::new());
    let release_inner = release.clone();
    let backend = backend(move |mut socket| {
        let release = release_inner.clone();
        async move {
            let request = http_header(&mut socket).await;
            if request.starts_with(b"GET /events ") {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\n\n").await.unwrap();
                release.notified().await;
                socket.write_all(b"data: second\n\n").await.unwrap();
            } else {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\n<h1>page</h1>").await.unwrap();
            }
        }
    }).await;
    let key = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[service("website", &backend, &key)],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &key).await;
    let mut stream = tunnel::open_service(&connection, "website").await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: website\r\n\r\n")
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    let mut page = Vec::new();
    stream.read_to_end(&mut page).await.unwrap();
    assert!(page.ends_with(b"<h1>page</h1>"));
    let mut stream = tunnel::open_service(&connection, "website").await.unwrap();
    stream
        .write_all(b"GET /events HTTP/1.1\r\nHost: website\r\n\r\n")
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    let header = http_header(&mut stream).await;
    assert!(header.starts_with(b"HTTP/1.1 200"));
    let mut first = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&first, b"data: first\n\n");
    release.notify_one();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"data: second\n\n");
    drop(stream);
    endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn unauthorized_unknown_and_forged_requests_never_open_backend() {
    let backend = backend(|mut socket| async move {
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    })
    .await;
    let allowed = SecretKey::generate();
    let denied = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[
            service("private", &backend, &allowed),
            service("other", &backend, &denied),
        ],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &denied).await;
    assert!(matches!(
        tunnel::open_service(&connection, "private").await,
        Err(TunnelError::Status(Status::Denied))
    ));
    assert!(matches!(
        tunnel::open_service(&connection, "missing").await,
        Err(TunnelError::Status(Status::UnknownService))
    ));
    let (allowed_endpoint, allowed_connection) = connected(&host, &allowed).await;
    assert_eq!(
        raw_status(
            &allowed_connection,
            &[
                0, 17, 0, 1, b'h', b't', b't', b'p', b':', b'/', b'/', b'1', b'2', b'7', b'.',
                b'0', b'.', b'0', b'.'
            ],
            true
        )
        .await,
        Status::Malformed
    );
    let mut forged = vec![0, 0, 0, 1];
    forged.extend_from_slice(format!("private\0{}", allowed.public()).as_bytes());
    let len = (forged.len() - 2) as u16;
    forged[..2].copy_from_slice(&len.to_be_bytes());
    assert_eq!(
        raw_status(&connection, &forged, true).await,
        Status::Malformed
    );
    let unknown = tunnel::bind_client(&SecretKey::generate()).await.unwrap();
    if let Ok(connection) = tunnel::connect(
        &unknown,
        host.endpoint().id(),
        host.endpoint().bound_sockets()[0],
    )
    .await
    {
        assert!(tunnel::open_service(&connection, "private").await.is_err());
    }
    assert_eq!(backend.total.load(Ordering::SeqCst), 0);
    assert!(
        host.counts().recent_peers().is_empty(),
        "Denied/malformed requests must not refresh recent activity"
    );
    unknown.close().await;
    endpoint.close().await;
    allowed_endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn malformed_oversized_truncated_versions_and_timeout_fail_without_backend() {
    let backend = backend(|_| async {}).await;
    let key = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[service("website", &backend, &key)],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &key).await;
    for (frame, status) in [
        (vec![255, 255], Status::Malformed),
        (vec![0, 0], Status::Malformed),
        (vec![0, 3, 0, 2, b'a'], Status::UnsupportedVersion),
        (vec![0, 3, 0, 1, 255], Status::Malformed),
        (vec![0, 4, 0, 1, b'a'], Status::Malformed),
    ] {
        assert_eq!(raw_status(&connection, &frame, true).await, status);
    }
    assert_eq!(
        raw_status(&connection, &[0, 3, 0], false).await,
        Status::Timeout
    );
    let bad_alpn = endpoint
        .connect(host.endpoint().addr(), b"wipsnet/service/999")
        .await;
    assert!(bad_alpn.is_err());
    assert_eq!(backend.total.load(Ordering::SeqCst), 0);
    endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn stream_and_connection_limits_deny_without_extra_backend_and_release_capacity() {
    let backend = backend(|mut socket| async move {
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    })
    .await;
    let key = SecretKey::generate();
    let mut config = limits();
    config.max_streams = 1;
    config.max_connections = 1;
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &config,
        &[service("website", &backend, &key)],
    )
    .await
    .unwrap();
    let counts = host.counts();
    let (endpoint, connection) = connected(&host, &key).await;
    let stream = tunnel::open_service(&connection, "website").await.unwrap();
    until(|| backend.total.load(Ordering::SeqCst) == 1).await;
    assert!(matches!(
        tunnel::open_service(&connection, "website").await,
        Err(TunnelError::Status(Status::Busy))
    ));
    let other = tunnel::bind_client(&key).await.unwrap();
    assert!(
        tunnel::connect(
            &other,
            host.endpoint().id(),
            host.endpoint().bound_sockets()[0]
        )
        .await
        .is_err()
    );
    assert_eq!(backend.total.load(Ordering::SeqCst), 1);
    drop(stream);
    until(|| counts.streams() == 0 && backend.live.load(Ordering::SeqCst) == 0).await;
    for _ in 0..12 {
        let stream = tunnel::open_service(&connection, "website").await.unwrap();
        drop(stream);
        until(|| counts.streams() == 0 && backend.live.load(Ordering::SeqCst) == 0).await;
    }
    endpoint.close().await;
    other.close().await;
    host.shutdown().await;
    assert_eq!(counts.connections(), 0);
    assert_eq!(counts.streams(), 0);
}

#[tokio::test]
async fn slow_reader_applies_backpressure_and_cancellation_releases_backend() {
    // A response must exceed both 64 MiB QUIC windows and the TCP buffers to
    // demonstrate backpressure. Reuse one chunk to keep fixture memory bounded.
    const RESPONSE_BYTES: usize = 256 * 1024 * 1024;
    let written = Arc::new(AtomicUsize::new(0));
    let written_inner = written.clone();
    let backend = backend(move |mut socket| {
        let written = written_inner.clone();
        async move {
            let chunk = [7u8; 16384];
            for _ in 0..RESPONSE_BYTES / chunk.len() {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
                written.fetch_add(chunk.len(), Ordering::SeqCst);
            }
        }
    })
    .await;
    let key = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[service("large", &backend, &key)],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &key).await;
    let stream = tunnel::open_service(&connection, "large").await.unwrap();
    until(|| written.load(Ordering::SeqCst) > 0).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut previous = written.load(Ordering::SeqCst);
        let mut unchanged_since = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let current = written.load(Ordering::SeqCst);
            assert!(
                current < RESPONSE_BYTES,
                "whole response was consumed while client did not read"
            );
            assert!(
                backend.live.load(Ordering::SeqCst) > 0,
                "backend writer exited before backpressure was observed"
            );
            if current != previous {
                previous = current;
                unchanged_since = tokio::time::Instant::now();
            } else if unchanged_since.elapsed() >= Duration::from_millis(200) {
                break;
            }
        }
    })
    .await
    .expect("backend writes did not stall while the client was not reading");
    drop(stream);
    until(|| host.counts().streams() == 0 && backend.live.load(Ordering::SeqCst) == 0).await;
    endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn backend_half_close_keeps_upload_open_and_shutdown_drains_active_tasks() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_inner = received.clone();
    let backend = backend(move |mut socket| {
        let received = received_inner.clone();
        async move {
            socket.write_all(b"before").await.unwrap();
            socket.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await.unwrap();
            *received.lock().unwrap() = bytes;
        }
    })
    .await;
    let key = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[service("half", &backend, &key)],
    )
    .await
    .unwrap();
    let counts = host.counts();
    let address = host.endpoint().bound_sockets()[0];
    let (endpoint, connection) = connected(&host, &key).await;
    let mut stream = tunnel::open_service(&connection, "half").await.unwrap();
    let mut message = [0; 6];
    stream.read_exact(&mut message).await.unwrap();
    assert_eq!(&message, b"before");
    assert_eq!(stream.read(&mut [0; 1]).await.unwrap(), 0);
    let payload = vec![9u8; 256 * 1024];
    stream.write_all(&payload).await.unwrap();
    stream.shutdown().await.unwrap();
    // Close the client endpoint immediately after shutdown, rather than waiting
    // for backend processing: buffered upload bytes must already be acknowledged.
    endpoint.close().await;
    until(|| backend.live.load(Ordering::SeqCst) == 0 && counts.streams() == 0).await;
    assert!(
        *received.lock().unwrap() == payload,
        "half-closed upload was truncated"
    );
    let (second_endpoint, second_connection) = connected(&host, &key).await;
    let _active = tunnel::open_service(&second_connection, "half")
        .await
        .unwrap();
    host.shutdown().await;
    assert_eq!(counts.connections(), 0);
    assert_eq!(counts.streams(), 0);
    until(|| backend.live.load(Ordering::SeqCst) == 0).await;
    // Iroh drains connections before its final async transport teardown. Verify
    // a finite release bound instead of racing the final socket-drop poll.
    let _rebound = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            match tokio::net::UdpSocket::bind(address).await {
                Ok(socket) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(1)).await
                }
                Err(error) => panic!("cannot rebind host UDP socket: {error}"),
            }
        }
    })
    .await
    .expect("host UDP port not released within 250 ms");
    second_endpoint.close().await;
}

#[tokio::test]
async fn websocket_standard_handshake_and_masked_frame_pass_unchanged() {
    let backend = backend(|mut socket| async move {
        let request = http_header(&mut socket).await;
        assert!(request.windows(24).any(|w| w==b"dGhlIHNhbXBsZSBub25jZQ=="));
        socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
        let mut frame=[0;11];
        socket.read_exact(&mut frame).await.unwrap();
        // RFC 6455 section 5.7: masked client text message "Hello".
        assert_eq!(frame, [0x81,0x85,0x37,0xfa,0x21,0x3d,0x7f,0x9f,0x4d,0x51,0x58]);
        socket.write_all(b"\x81\x05Hello").await.unwrap();
    }).await;
    let key = SecretKey::generate();
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &limits(),
        &[service("ws", &backend, &key)],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &key).await;
    let mut stream = tunnel::open_service(&connection, "ws").await.unwrap();
    stream.write_all(b"GET /ws HTTP/1.1\r\nHost: website\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    assert!(http_header(&mut stream).await.starts_with(b"HTTP/1.1 101"));
    stream
        .write_all(&[
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ])
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    let mut frame = Vec::new();
    stream.read_to_end(&mut frame).await.unwrap();
    assert_eq!(frame, b"\x81\x05Hello");
    endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn per_connection_quic_credit_is_bounded_and_backend_failure_is_structured() {
    let backend = backend(|mut socket| async move {
        let _ = socket.read(&mut [0; 1]).await;
    })
    .await;
    let key = SecretKey::generate();
    let mut config = limits();
    config.max_streams_per_connection = 1;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_address = dead.local_addr().unwrap();
    drop(dead);
    let host = TunnelHost::bind(
        &SecretKey::generate(),
        &config,
        &[
            service("website", &backend, &key),
            ServiceConfig {
                id: "offline".into(),
                backend: dead_address,
                allow: vec![key.public()],
            },
        ],
    )
    .await
    .unwrap();
    let (endpoint, connection) = connected(&host, &key).await;
    assert!(matches!(
        tunnel::open_service(&connection, "offline").await,
        Err(TunnelError::Status(Status::BackendUnavailable))
    ));
    let stream = tunnel::open_service(&connection, "website").await.unwrap();
    until(|| backend.total.load(Ordering::SeqCst) == 1).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            tunnel::open_service(&connection, "website")
        )
        .await
        .is_err()
    );
    assert_eq!(backend.total.load(Ordering::SeqCst), 1);
    drop(stream);
    until(|| host.counts().streams() == 0 && backend.live.load(Ordering::SeqCst) == 0).await;
    let stream = tunnel::open_service(&connection, "website").await.unwrap();
    drop(stream);
    endpoint.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn both_agents_host_and_access_using_their_own_shared_endpoints() {
    let a_key = SecretKey::generate();
    let b_key = SecretKey::generate();
    let a_backend = backend(|mut socket| async move {
        let mut body = Vec::new();
        socket.read_to_end(&mut body).await.unwrap();
        socket.write_all(b"from-a").await.unwrap();
    })
    .await;
    let b_backend = backend(|mut socket| async move {
        let mut body = Vec::new();
        socket.read_to_end(&mut body).await.unwrap();
        socket.write_all(b"from-b").await.unwrap();
    })
    .await;
    let a = TunnelHost::bind(&a_key, &limits(), &[service("a", &a_backend, &b_key)])
        .await
        .unwrap();
    let b = TunnelHost::bind(&b_key, &limits(), &[service("b", &b_backend, &a_key)])
        .await
        .unwrap();
    let ab = tunnel::connect(
        a.endpoint(),
        b.endpoint().id(),
        b.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    let ba = tunnel::connect(
        b.endpoint(),
        a.endpoint().id(),
        a.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    for (connection, id, expected) in [(&ab, "b", b"from-b"), (&ba, "a", b"from-a")] {
        let mut stream = tunnel::open_service(connection, id).await.unwrap();
        stream.write_all(b"request").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.unwrap();
        assert_eq!(&reply, expected);
    }
    a.shutdown().await;
    b.shutdown().await;
}

use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wispnet_agent::{control, tunnel};
use wispnet_core::{
    config::TransportConfig,
    identity::{EndpointId, Identity},
};
use wispnet_registry::host::{self, Authority, Mutation};
fn dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}
fn mutate(directory: &std::path::Path, host: EndpointId, operation: Mutation) {
    let (mut store, secret) = host::owner_store(directory, host).unwrap();
    store.host_mutate(host, &secret, operation).unwrap();
}
fn approve(directory: &std::path::Path, host: EndpointId, peer: EndpointId, rev: u64) {
    mutate(
        directory,
        host,
        Mutation::Approve {
            expected_revision: rev,
            peer,
            name: "client".into(),
        },
    );
}
fn service(
    directory: &std::path::Path,
    host: EndpointId,
    peer: EndpointId,
    backend: std::net::SocketAddr,
    rev: u64,
) {
    mutate(
        directory,
        host,
        Mutation::Service {
            display_name: None,
            kind: None,
            expected_revision: rev,
            id: "llama".into(),
            backend,
            allow: vec![peer],
        },
    );
}
fn config() -> TransportConfig {
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
        max_streams: 8,
        max_streams_per_connection: 4,
    }
}

#[tokio::test]
async fn public_links_are_filtered_read_only_and_do_not_change_catalog_baselines() {
    use wispnet_core::host::PublicLink;
    let directory = dir();
    let host_key = Identity::generate();
    let client = Identity::generate();
    let own = host_key.endpoint_id();
    let peer = client.endpoint_id();
    host::initialize(directory.path(), own).unwrap();
    approve(directory.path(), own, peer, 0);
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service(
        directory.path(),
        own,
        peer,
        backend.local_addr().unwrap(),
        1,
    );
    mutate(
        directory.path(),
        own,
        Mutation::Service {
            expected_revision: 2,
            id: "private".into(),
            backend: backend.local_addr().unwrap(),
            allow: vec![],
            display_name: None,
            kind: None,
        },
    );
    let authority = Authority::open(directory.path(), own).unwrap();
    let catalog = authority.catalog(peer).await.unwrap();
    let links: Vec<_> = authority
        .services()
        .await
        .unwrap()
        .into_iter()
        .map(|s| PublicLink {
            service: s.id.clone(),
            incarnation: s.incarnation,
            url: format!("https://desktop-{}.wispnet.test/", s.id),
            state: "online".into(),
        })
        .collect();
    authority.set_public_links(links).unwrap();
    let server = tunnel::TunnelHost::bind_host(host_key.secret_key(), &config(), authority.clone())
        .await
        .unwrap();
    let endpoint = tunnel::bind_client(client.secret_key()).await.unwrap();
    let connection = control::connect(&endpoint, own, server.endpoint().bound_sockets()[0])
        .await
        .unwrap();
    let links = control::public_links_on(&connection, own).await.unwrap();
    assert_eq!(links.services.len(), 1);
    assert_eq!(links.services[0].service, "llama");
    assert_eq!(links.services[0].url, "https://desktop-llama.wispnet.test/");
    assert!(control::public_links_on(&connection, peer).await.is_err());
    assert_eq!(
        catalog,
        control::catalog_on(&connection, own).await.unwrap()
    );
    authority.set_public_links(vec![]).unwrap();
    assert!(
        control::public_links_on(&connection, own)
            .await
            .unwrap()
            .services
            .is_empty()
    );
    assert_eq!(catalog, authority.catalog(peer).await.unwrap());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    let stranger = tunnel::bind_client(Identity::generate().secret_key())
        .await
        .unwrap();
    if let Ok(connection) =
        control::connect(&stranger, own, server.endpoint().bound_sockets()[0]).await
    {
        assert!(control::public_links_on(&connection, own).await.is_err());
    }
    mutate(
        directory.path(),
        own,
        Mutation::Revoke {
            expected_revision: 3,
            peer,
        },
    );
    assert!(control::public_links_on(&connection, own).await.is_err());
    endpoint.close().await;
    stranger.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn recreated_same_binding_rejects_old_and_unbound_streams_and_cancels_active() {
    use tokio::{
        net::TcpListener,
        time::{Duration, timeout},
    };
    let directory = dir();
    let host_key = Identity::generate();
    let client = Identity::generate();
    host::initialize(directory.path(), host_key.endpoint_id()).unwrap();
    approve(
        directory.path(),
        host_key.endpoint_id(),
        client.endpoint_id(),
        0,
    );
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    service(
        directory.path(),
        host_key.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        1,
    );
    let authority = Authority::open(directory.path(), host_key.endpoint_id()).unwrap();
    let first = authority
        .catalog(client.endpoint_id())
        .await
        .unwrap()
        .services[0]
        .incarnation
        .clone();
    let server = tunnel::TunnelHost::bind_host(host_key.secret_key(), &config(), authority.clone())
        .await
        .unwrap();
    let endpoint = tunnel::bind_client(client.secret_key()).await.unwrap();
    let connection = tunnel::connect(
        &endpoint,
        host_key.endpoint_id(),
        server.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    assert!(tunnel::open_service(&connection, "llama").await.is_err());
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    let mut stream = tunnel::open_service_bound(&connection, "llama", &first)
        .await
        .unwrap();
    let (mut tcp, _) = backend.accept().await.unwrap();
    tcp.write_all(b"old").await.unwrap();
    let mut initial = [0; 3];
    stream.read_exact(&mut initial).await.unwrap();
    mutate(
        directory.path(),
        host_key.endpoint_id(),
        Mutation::Remove {
            expected_revision: 2,
            id: "llama".into(),
        },
    );
    service(
        directory.path(),
        host_key.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        3,
    );
    let new = authority
        .catalog(client.endpoint_id())
        .await
        .unwrap()
        .services[0]
        .incarnation
        .clone();
    assert_ne!(first, new);
    let mut byte = [0; 1];
    let ended = timeout(Duration::from_secs(3), stream.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(ended, Ok(0) | Err(_)));
    assert!(
        tunnel::open_service_bound(&connection, "llama", &first)
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    let _fresh = tunnel::open_service_bound(&connection, "llama", &new)
        .await
        .unwrap();
    timeout(Duration::from_secs(2), backend.accept())
        .await
        .unwrap()
        .unwrap();
    drop(tcp);
    connection.close(0u32.into(), b"done");
    endpoint.close().await;
    server.shutdown().await;
}
#[tokio::test]
async fn symmetric_hosts_share_protocol_endpoint_and_isolate_local_authority() {
    let a = Identity::generate();
    let b = Identity::generate();
    let client = Identity::generate();
    let da = dir();
    let db = dir();
    host::initialize(da.path(), a.endpoint_id()).unwrap();
    host::initialize(db.path(), b.endpoint_id()).unwrap();
    approve(da.path(), a.endpoint_id(), client.endpoint_id(), 0);
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service(
        da.path(),
        a.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        1,
    );
    let ah = tunnel::TunnelHost::bind_host(
        a.secret_key(),
        &config(),
        Authority::open(da.path(), a.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let bh = tunnel::TunnelHost::bind_host(
        b.secret_key(),
        &config(),
        Authority::open(db.path(), b.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let outgoing = tunnel::bind_client(client.secret_key()).await.unwrap();
    let aa = ah.endpoint().bound_sockets()[0];
    let ba = bh.endpoint().bound_sockets()[0];
    let cc = control::connect(&outgoing, a.endpoint_id(), aa)
        .await
        .unwrap();
    let catalog = control::catalog_on(&cc, a.endpoint_id()).await.unwrap();
    assert_eq!(catalog.host, a.endpoint_id());
    assert_eq!(catalog.services[0].id, "llama");
    assert_eq!(catalog.revision, 2);
    let denied = control::connect(&outgoing, b.endpoint_id(), ba)
        .await
        .unwrap();
    assert!(control::catalog_on(&denied, b.endpoint_id()).await.is_err());
    // Control and service coexist at exactly the same endpoint/UDP address.
    let sc = tunnel::connect(&outgoing, a.endpoint_id(), aa)
        .await
        .unwrap();
    let (parallel_catalog, parallel_stream) = tokio::join!(
        control::catalog_on(&cc, a.endpoint_id()),
        tunnel::open_service_bound(&sc, "llama", &catalog.services[0].incarnation)
    );
    assert_eq!(parallel_catalog.unwrap().host, a.endpoint_id());
    let mut stream = parallel_stream.unwrap();
    let (mut tcp, _) = backend.accept().await.unwrap();
    tcp.write_all(b"hello").await.unwrap();
    let mut data = [0; 5];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"hello");
    drop(stream);
    drop(tcp);
    // Owner credentials are local and host-scoped, not delegated by peer approval.
    let (_, a_owner) = host::owner_store(da.path(), a.endpoint_id()).unwrap();
    let (mut b_store, _) = host::owner_store(db.path(), b.endpoint_id()).unwrap();
    assert!(
        b_store
            .host_mutate(
                b.endpoint_id(),
                &a_owner,
                Mutation::Approve {
                    expected_revision: 0,
                    peer: client.endpoint_id(),
                    name: "transitive".into()
                }
            )
            .is_err()
    );
    // A goes offline. B can approve/register/serve and restart using only B state.
    ah.shutdown().await;
    approve(db.path(), b.endpoint_id(), client.endpoint_id(), 0);
    service(
        db.path(),
        b.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        1,
    );
    let bc = control::connect(&outgoing, b.endpoint_id(), ba)
        .await
        .unwrap();
    let catalog = control::catalog_on(&bc, b.endpoint_id()).await.unwrap();
    assert_eq!(catalog.host, b.endpoint_id());
    assert_eq!(catalog.revision, 2);
    assert_eq!(catalog.services[0].id, "llama");
    let bs = tunnel::connect(&outgoing, b.endpoint_id(), ba)
        .await
        .unwrap();
    let mut stream = tunnel::open_service_bound(&bs, "llama", &catalog.services[0].incarnation)
        .await
        .unwrap();
    let (mut tcp, _) = backend.accept().await.unwrap();
    tcp.write_all(b"active").await.unwrap();
    let mut initial = [0; 6];
    stream.read_exact(&mut initial).await.unwrap();
    assert_eq!(&initial, b"active");
    // Revocation is read again on an existing control/service connection.
    mutate(
        db.path(),
        b.endpoint_id(),
        Mutation::Revoke {
            expected_revision: 2,
            peer: client.endpoint_id(),
        },
    );
    assert!(matches!(
        control::catalog_on(&bc, b.endpoint_id()).await,
        Err(tunnel::TunnelError::Status(
            wispnet_core::protocol::Status::Denied
        ))
    ));
    let mut byte = [0];
    let ended = tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut byte))
        .await
        .expect("revocation must interrupt active stream");
    assert!(matches!(ended, Ok(0) | Err(_)));
    let backend_ended =
        tokio::time::timeout(std::time::Duration::from_secs(3), tcp.read(&mut byte))
            .await
            .expect("revocation must close backend");
    assert!(matches!(backend_ended, Ok(0) | Err(_)));
    assert!(tunnel::open_service(&bs, "llama").await.is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), backend.accept())
            .await
            .is_err()
    );
    bh.shutdown().await;
    let b_restart = tunnel::TunnelHost::bind_host(
        b.secret_key(),
        &config(),
        Authority::open(db.path(), b.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let denied = control::connect(
        &outgoing,
        b.endpoint_id(),
        b_restart.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    assert!(control::catalog_on(&denied, b.endpoint_id()).await.is_err());
    b_restart.shutdown().await;
    outgoing.close().await;
}
#[tokio::test]
async fn malformed_control_cannot_mutate_or_open_backends_and_shutdown_drains() {
    let host_key = Identity::generate();
    let client = Identity::generate();
    let d = dir();
    host::initialize(d.path(), host_key.endpoint_id()).unwrap();
    approve(d.path(), host_key.endpoint_id(), client.endpoint_id(), 0);
    let host = tunnel::TunnelHost::bind_host(
        host_key.secret_key(),
        &config(),
        Authority::open(d.path(), host_key.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let counts = host.counts();
    let endpoint = tunnel::bind_client(client.secret_key()).await.unwrap();
    let connection = control::connect(
        &endpoint,
        host_key.endpoint_id(),
        host.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    for bytes in [
        br#"{"op":"approve","peer":"forged"}"#.as_slice(),
        br#"{"op":"catalog","host":"forged"}"#.as_slice(),
    ] {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        send.write_all(bytes).await.unwrap();
        send.finish().unwrap();
        assert!(recv.read_to_end(1024).await.is_err());
    }
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    send.finish().unwrap();
    assert!(recv.read_to_end(1024).await.is_err());
    assert_eq!(
        control::catalog_on(&connection, host_key.endpoint_id())
            .await
            .unwrap()
            .revision,
        1
    );
    let (mut stalled, _) = connection.open_bi().await.unwrap();
    stalled.write_all(&[0, 0]).await.unwrap();
    host.shutdown().await;
    assert_eq!(counts.connections(), 0);
    assert_eq!(counts.streams(), 0);
    endpoint.close().await;
}

async fn tls_relay(
    certificate: &rcgen::CertifiedKey<rcgen::KeyPair>,
    address: std::net::SocketAddr,
) -> iroh_relay::server::Server {
    use iroh_relay::server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig};
    let tls = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.cert.der().clone()],
        rustls_pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let mut relay = RelayConfig::new("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap());
    relay.tls = Some(TlsConfig::new(
        address,
        CertConfig::Manual { server_config: tls },
    ));
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(relay);
    Server::spawn(cfg).await.unwrap()
}

#[tokio::test]
async fn explicit_tls_relay_without_udp_pins_identity_enforces_grants_and_recovers() {
    use std::time::Duration;
    use tokio::time::timeout;
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let relay = tls_relay(&certificate, "127.0.0.1:0".parse().unwrap()).await;
    let relay_addr = relay.https_addr().unwrap();
    let trust = dir();
    let ca = trust.path().join("relay.pem");
    std::fs::write(&ca, certificate.cert.pem()).unwrap();
    std::fs::set_permissions(&ca, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut cfg = config();
    cfg.relay_url = Some(format!("https://{relay_addr}"));
    cfg.relay_only = true;
    cfg.relay_ca_file = Some(ca);
    let key = Identity::generate();
    let client = Identity::generate();
    let state = dir();
    host::initialize(state.path(), key.endpoint_id()).unwrap();
    approve(state.path(), key.endpoint_id(), client.endpoint_id(), 0);
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service(
        state.path(),
        key.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        1,
    );
    let host = tunnel::TunnelHost::bind_host(
        key.secret_key(),
        &cfg,
        Authority::open(state.path(), key.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let outgoing = tunnel::bind_configured_client(client.secret_key(), Some(&cfg))
        .await
        .unwrap();
    assert!(host.endpoint().bound_sockets().is_empty());
    assert!(outgoing.bound_sockets().is_empty());
    timeout(Duration::from_secs(10), async {
        tokio::join!(host.endpoint().online(), outgoing.online());
    })
    .await
    .unwrap();
    let target = tunnel::peer_target(key.endpoint_id(), Some(&cfg), None).unwrap();
    let connection = control::connect_target(&outgoing, target.clone())
        .await
        .unwrap();
    assert!(connection.paths().iter().any(|p| p.is_relay()));
    assert_eq!(
        control::catalog_on(&connection, key.endpoint_id())
            .await
            .unwrap()
            .services
            .len(),
        1
    );
    assert!(
        control::catalog_on(&connection, client.endpoint_id())
            .await
            .is_err()
    );
    let service_connection = tunnel::connect_target(&outgoing, target.clone())
        .await
        .unwrap();
    let mut stream = tunnel::open_service_bound(
        &service_connection,
        "llama",
        &control::catalog_on(&connection, key.endpoint_id())
            .await
            .unwrap()
            .services[0]
            .incarnation,
    )
    .await
    .unwrap();
    let (mut tcp, _) = timeout(Duration::from_secs(2), backend.accept())
        .await
        .unwrap()
        .unwrap();
    stream.write_all(b"relay-only").await.unwrap();
    let mut data = [0; 10];
    timeout(Duration::from_secs(2), tcp.read_exact(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&data, b"relay-only");
    tcp.write_all(b"reply").await.unwrap();
    let mut reply = [0; 5];
    timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"reply");
    drop(stream);
    drop(tcp);
    // Admission at the relay is deliberately AllowAll, but host approval is separate.
    let stranger_key = Identity::generate();
    let stranger = tunnel::bind_configured_client(stranger_key.secret_key(), Some(&cfg))
        .await
        .unwrap();
    timeout(Duration::from_secs(10), stranger.online())
        .await
        .unwrap();
    let hello = wispnet_agent::pairing::hello(&stranger, target.clone())
        .await
        .unwrap();
    assert_eq!(hello.host, key.endpoint_id());
    assert_eq!(hello.version, wispnet_core::pairing::VERSION);
    if let Ok(denied) = control::connect_target(&stranger, target.clone()).await {
        assert!(
            control::catalog_on(&denied, key.endpoint_id())
                .await
                .is_err()
        );
    }
    let (mut store, owner) = host::owner_store(state.path(), key.endpoint_id()).unwrap();
    let invitation = store
        .pairing_invite(
            key.endpoint_id(),
            &owner,
            cfg.relay_url.as_deref().unwrap(),
            wispnet_registry::enrollment::now(),
            300,
        )
        .unwrap();
    let pending = wispnet_agent::pairing::request_enrollment(
        &stranger,
        key.endpoint_id(),
        &invitation,
        &cfg,
        stranger_key.secret_key(),
        "new-tablet",
    )
    .await
    .unwrap();
    assert_eq!(pending.peer, stranger_key.endpoint_id());
    let denied = control::connect_target(&stranger, target.clone())
        .await
        .unwrap();
    assert_eq!(
        control::catalog_on(&denied, key.endpoint_id())
            .await
            .unwrap_err()
            .to_string(),
        "awaiting approval"
    );
    store
        .host_mutate(
            key.endpoint_id(),
            &owner,
            Mutation::ApproveInvitation {
                services: vec![],
                expected_revision: 2,
                invitation: invitation.invitation.clone(),
            },
        )
        .unwrap();
    let approved = control::connect_target(&stranger, target.clone())
        .await
        .unwrap();
    assert!(
        control::catalog_on(&approved, key.endpoint_id())
            .await
            .unwrap()
            .services
            .is_empty()
    );
    let denied_service = tunnel::connect_target(&stranger, target.clone())
        .await
        .unwrap();
    assert!(
        tunnel::open_service(&denied_service, "llama")
            .await
            .is_err()
    );
    store
        .host_mutate(
            key.endpoint_id(),
            &owner,
            Mutation::Revoke {
                expected_revision: 3,
                peer: stranger_key.endpoint_id(),
            },
        )
        .unwrap();
    assert!(
        wispnet_agent::pairing::request_enrollment(
            &stranger,
            key.endpoint_id(),
            &invitation,
            &cfg,
            stranger_key.secret_key(),
            "new-tablet"
        )
        .await
        .is_err()
    );
    drop(store);
    drop(owner);
    // One unavailable pinned peer never holds up an independent live-peer request.
    let absent = tunnel::peer_target(Identity::generate().endpoint_id(), Some(&cfg), None).unwrap();
    let (missing, live) = tokio::join!(
        control::connect_target(&outgoing, absent),
        timeout(
            Duration::from_secs(2),
            control::catalog_on(&connection, key.endpoint_id())
        )
    );
    assert!(missing.is_err());
    assert!(live.unwrap().is_ok());
    // Rebuild a stateless relay at the same address; no endpoint/key/grant replacement.
    connection.close(0u32.into(), b"test reconnect");
    service_connection.close(0u32.into(), b"test reconnect");
    relay.shutdown().await.unwrap();
    assert!(
        control::connect_target(&outgoing, target.clone())
            .await
            .is_err()
    );
    let relay = tls_relay(&certificate, relay_addr).await;
    timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(c) = control::connect_target(&outgoing, target.clone()).await
                && control::catalog_on(&c, key.endpoint_id()).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(outgoing.id(), client.endpoint_id());
    assert_eq!(host.endpoint().id(), key.endpoint_id());
    // Relay replacement cannot resurrect a host-locally revoked client, even
    // after that client restarts with its original cryptographic identity.
    stranger.close().await;
    let revoked = tunnel::bind_configured_client(stranger_key.secret_key(), Some(&cfg))
        .await
        .unwrap();
    timeout(Duration::from_secs(10), revoked.online())
        .await
        .unwrap();
    assert_eq!(revoked.id(), stranger_key.endpoint_id());
    let denied = control::connect_target(&revoked, target.clone())
        .await
        .unwrap();
    assert_eq!(
        control::catalog_on(&denied, key.endpoint_id())
            .await
            .unwrap_err()
            .to_string(),
        "access denied"
    );
    let current = control::connect_target(&outgoing, target.clone())
        .await
        .unwrap();
    let incarnation = control::catalog_on(&current, key.endpoint_id())
        .await
        .unwrap()
        .services[0]
        .incarnation
        .clone();
    let denied_service = tunnel::connect_target(&revoked, target.clone())
        .await
        .unwrap();
    assert!(
        tunnel::open_service_bound(&denied_service, "llama", &incarnation)
            .await
            .is_err()
    );
    revoked.close().await;

    // No replay of the previous service operation during relay recovery.
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    // The explicit certificate is required; no insecure TLS fallback.
    cfg.relay_ca_file = None;
    let untrusted = tunnel::bind_configured_client(Identity::generate().secret_key(), Some(&cfg))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(1), untrusted.online())
            .await
            .is_err()
    );
    untrusted.close().await;
    stranger.close().await;
    outgoing.close().await;
    host.shutdown().await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_pairing_hello_is_bounded_strict_and_never_approves_a_client() {
    use std::time::Duration;
    use tokio::time::timeout;
    let key = Identity::generate();
    let client_key = Identity::generate();
    let state = dir();
    host::initialize(state.path(), key.endpoint_id()).unwrap();
    let host = tunnel::TunnelHost::bind_host(
        key.secret_key(),
        &config(),
        Authority::open(state.path(), key.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let client = tunnel::bind_client(client_key.secret_key()).await.unwrap();
    let target =
        iroh::EndpointAddr::new(key.endpoint_id()).with_ip_addr(host.endpoint().bound_sockets()[0]);
    let hello = wispnet_agent::pairing::hello(&client, target.clone())
        .await
        .unwrap();
    assert_eq!(hello.host, key.endpoint_id());
    assert_eq!(hello.version, 1);
    for bytes in [
        br#"{"op":"hello","owner":"forged"}"#.as_slice(),
        br#"{"op":"approve"}"#,
        br#"{"op":"hello"}trailing"#,
        &[b'x'; wispnet_core::pairing::MAX_REQUEST + 1],
    ] {
        let connection = client
            .connect(target.clone(), wispnet_core::pairing::ALPN)
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        send.write_all(bytes).await.unwrap();
        send.finish().unwrap();
        assert!(
            timeout(Duration::from_secs(2), recv.read_to_end(512))
                .await
                .unwrap()
                .is_err()
        );
        connection.close(0u32.into(), b"test complete");
    }
    // A stalled pre-approval RPC cannot occupy a connection indefinitely.
    let stalled = client
        .connect(target.clone(), wispnet_core::pairing::ALPN)
        .await
        .unwrap();
    timeout(Duration::from_secs(7), stalled.closed())
        .await
        .unwrap();
    let control = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    assert!(
        control::catalog_on(&control, key.endpoint_id())
            .await
            .is_err()
    );
    let service = tunnel::connect_target(&client, target).await.unwrap();
    assert!(tunnel::open_service(&service, "llama").await.is_err());
    let (mut store, _) = host::owner_store(state.path(), key.endpoint_id()).unwrap();
    let snapshot = store.host_snapshot(key.endpoint_id()).unwrap();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.clients.is_empty());
    assert!(snapshot.services.iter().all(|s| matches!(
        s.kind,
        wispnet_core::host::ServiceKind::Shell | wispnet_core::host::ServiceKind::Files
    )));
    client.close().await;
    let counts = host.counts();
    host.shutdown().await;
    assert_eq!(counts.connections(), 0);
    assert_eq!(counts.streams(), 0);
}

#[tokio::test]
async fn existing_direct_path_survives_configured_relay_outage() {
    use std::time::Duration;
    use tokio::time::timeout;
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let relay = tls_relay(&certificate, "127.0.0.1:0".parse().unwrap()).await;
    let trust = dir();
    let ca = trust.path().join("relay.pem");
    std::fs::write(&ca, certificate.cert.pem()).unwrap();
    let mut cfg = config();
    cfg.relay_url = Some(format!("https://{}", relay.https_addr().unwrap()));
    cfg.relay_ca_file = Some(ca);
    let key = Identity::generate();
    let client = Identity::generate();
    let state = dir();
    host::initialize(state.path(), key.endpoint_id()).unwrap();
    approve(state.path(), key.endpoint_id(), client.endpoint_id(), 0);
    let host = tunnel::TunnelHost::bind_host(
        key.secret_key(),
        &cfg,
        Authority::open(state.path(), key.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let outgoing = tunnel::bind_configured_client(client.secret_key(), Some(&cfg))
        .await
        .unwrap();
    let target = tunnel::peer_target(
        key.endpoint_id(),
        Some(&cfg),
        Some(host.endpoint().bound_sockets()[0]),
    )
    .unwrap();
    let connection = control::connect_target(&outgoing, target).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if connection
                .paths()
                .iter()
                .any(|p| p.is_ip() && p.is_selected())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    relay.shutdown().await.unwrap();
    assert_eq!(
        control::catalog_on(&connection, key.endpoint_id())
            .await
            .unwrap()
            .host,
        key.endpoint_id()
    );
    outgoing.close().await;
    host.shutdown().await;
}

#[tokio::test]
async fn udp_enabled_hosts_fall_back_to_relay_with_stale_hint() {
    use std::time::Duration;
    use tokio::time::timeout;
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let relay = tls_relay(&certificate, "127.0.0.1:0".parse().unwrap()).await;
    let trust = dir();
    let ca = trust.path().join("relay.pem");
    std::fs::write(&ca, certificate.cert.pem()).unwrap();
    let mut cfg = config();
    cfg.relay_url = Some(format!("https://{}", relay.https_addr().unwrap()));
    cfg.relay_ca_file = Some(ca);
    let client_key = Identity::generate();
    let hosts = [Identity::generate(), Identity::generate()];
    // Fresh local authorities; only one host exists on the network at a time.
    for key in &hosts {
        let state = dir();
        host::initialize(state.path(), key.endpoint_id()).unwrap();
        approve(state.path(), key.endpoint_id(), client_key.endpoint_id(), 0);
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        service(
            state.path(),
            key.endpoint_id(),
            client_key.endpoint_id(),
            backend.local_addr().unwrap(),
            1,
        );
        let host = tunnel::TunnelHost::bind_host(
            key.secret_key(),
            &cfg,
            Authority::open(state.path(), key.endpoint_id()).unwrap(),
        )
        .await
        .unwrap();
        let client = tunnel::bind_configured_client(client_key.secret_key(), Some(&cfg))
            .await
            .unwrap();
        assert!(!host.endpoint().bound_sockets().is_empty());
        assert!(!client.bound_sockets().is_empty());
        timeout(Duration::from_secs(10), async {
            tokio::join!(host.endpoint().online(), client.online());
        })
        .await
        .unwrap();
        // The isolated audit repeats this test with kernel UDP blocking. The
        // normal suite also covers the stale hint without firewall privileges.
        let target = tunnel::peer_target(
            key.endpoint_id(),
            Some(&cfg),
            Some("127.0.0.9:9".parse().unwrap()),
        )
        .unwrap();
        let hello = wispnet_agent::pairing::hello(&client, target.clone())
            .await
            .unwrap();
        assert_eq!(hello.host, key.endpoint_id());
        let control = control::connect_target(&client, target.clone())
            .await
            .unwrap();
        assert_eq!(
            control::catalog_on(&control, key.endpoint_id())
                .await
                .unwrap()
                .services
                .len(),
            1
        );
        let connection = tunnel::connect_target(&client, target).await.unwrap();
        if std::env::var("WISPNET_AUDIT_DROP_UDP").as_deref() == Ok("1") {
            assert!(
                connection
                    .paths()
                    .iter()
                    .any(|p| p.is_relay() && p.is_selected())
            );
        }
        let mut stream = tunnel::open_service_bound(
            &connection,
            "llama",
            &control::catalog_on(&control, key.endpoint_id())
                .await
                .unwrap()
                .services[0]
                .incarnation,
        )
        .await
        .unwrap();
        let (mut tcp, _) = timeout(Duration::from_secs(2), backend.accept())
            .await
            .unwrap()
            .unwrap();
        stream.write_all(b"request").await.unwrap();
        let mut data = [0; 7];
        timeout(Duration::from_secs(2), tcp.read_exact(&mut data))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&data, b"request");
        tcp.write_all(b"reply").await.unwrap();
        let mut reply = [0; 5];
        timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"reply");
        drop(stream);
        drop(tcp);
        client.close().await;
        host.shutdown().await;
    }
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn catalog_rejects_mismatched_pin_before_sending_a_stream() {
    use std::time::Duration;
    use tokio::time::timeout;
    let server_key = Identity::generate();
    let client_key = Identity::generate();
    let unrelated = Identity::generate();
    let server = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(server_key.secret_key().clone())
        .alpns(vec![wispnet_core::host::ALPN.to_vec()])
        .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap();
    let client = tunnel::bind_client(client_key.secret_key()).await.unwrap();
    let (outgoing, incoming) = tokio::join!(
        control::connect(&client, server_key.endpoint_id(), server.bound_sockets()[0]),
        async { server.accept().await.unwrap().await.unwrap() }
    );
    let outgoing = outgoing.unwrap();
    let result = timeout(
        Duration::from_secs(1),
        control::catalog_on(&outgoing, unrelated.endpoint_id()),
    )
    .await
    .expect("a wrong pin must fail without waiting for a remote response");
    assert!(matches!(
        result,
        Err(tunnel::TunnelError::Transport(
            "control target identity mismatch"
        ))
    ));
    assert!(
        timeout(Duration::from_millis(250), incoming.accept_bi())
            .await
            .is_err()
    );
    assert!(matches!(
        control::diagnostic_on(&outgoing, unrelated.endpoint_id()).await,
        Err(tunnel::TunnelError::Transport(
            "diagnostic target identity mismatch"
        ))
    ));
    assert!(
        timeout(Duration::from_millis(100), incoming.accept_bi())
            .await
            .is_err()
    );
    // Rejection is local: it neither closes nor poisons the shared connection.
    let (mut send, _recv) = outgoing.open_bi().await.unwrap();
    send.write_all(b"still usable").await.unwrap();
    send.finish().unwrap();
    let (_send, mut recv) = timeout(Duration::from_secs(1), incoming.accept_bi())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recv.read_to_end(32).await.unwrap(), b"still usable");
    client.close().await;
    server.close().await;
}

#[tokio::test]
async fn authenticated_device_name_updates_only_own_label_and_preserves_access() {
    let directory = dir();
    let key = Identity::generate();
    let device = Identity::generate();
    let other = Identity::generate();
    host::initialize(directory.path(), key.endpoint_id()).unwrap();
    approve(directory.path(), key.endpoint_id(), device.endpoint_id(), 0);
    approve(directory.path(), key.endpoint_id(), other.endpoint_id(), 1);
    service(
        directory.path(),
        key.endpoint_id(),
        device.endpoint_id(),
        "127.0.0.1:8201".parse().unwrap(),
        2,
    );
    let authority = Authority::open(directory.path(), key.endpoint_id())
        .unwrap()
        .with_device_id("j-strix");
    let server = tunnel::TunnelHost::bind_host(key.secret_key(), &config(), authority.clone())
        .await
        .unwrap();
    let endpoint = tunnel::bind_client(device.secret_key()).await.unwrap();
    let connection = control::connect(
        &endpoint,
        key.endpoint_id(),
        server.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    assert_eq!(
        control::device_id_on(&connection, key.endpoint_id())
            .await
            .unwrap(),
        "j-strix"
    );
    assert!(
        control::device_id_on(&connection, other.endpoint_id())
            .await
            .is_err()
    );
    let initial_revision = authority.snapshot().await.unwrap().revision;
    authority.set_device_id("j-strix-").unwrap();
    assert!(authority.set_device_id("bad id").is_err());
    assert_eq!(
        control::device_id_on(&connection, key.endpoint_id())
            .await
            .unwrap(),
        "j-strix-"
    );
    assert_eq!(
        authority.snapshot().await.unwrap().revision,
        initial_revision
    );
    let before = control::catalog_on(&connection, key.endpoint_id())
        .await
        .unwrap();
    control::set_name_on(&connection, key.endpoint_id(), "Johnny’s phone")
        .await
        .unwrap();
    let snapshot = authority.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .clients
            .iter()
            .find(|c| c.peer == device.endpoint_id())
            .unwrap()
            .name,
        "johnny-s-phone"
    );
    assert_eq!(
        snapshot
            .clients
            .iter()
            .find(|c| c.peer == other.endpoint_id())
            .unwrap()
            .name,
        "client"
    );
    assert_eq!(
        snapshot
            .services
            .iter()
            .find(|s| s.id == "llama")
            .unwrap()
            .allow,
        vec![device.endpoint_id()]
    );
    let after = control::catalog_on(&connection, key.endpoint_id())
        .await
        .unwrap();
    assert_eq!(before.services, after.services);
    assert!(after.follows(&before));
    control::set_name_on(&connection, key.endpoint_id(), "Johnny’s phone")
        .await
        .unwrap();
    assert_eq!(
        authority.snapshot().await.unwrap().revision,
        snapshot.revision
    );
    for bad in ["", "bad\nname", &"a".repeat(129)] {
        assert!(
            control::set_name_on(&connection, key.endpoint_id(), bad)
                .await
                .is_err()
        );
        assert!(
            authority
                .set_client_name(device.endpoint_id(), bad.into())
                .await
                .is_err()
        );
    }
    assert!(
        control::set_name_on(&connection, other.endpoint_id(), "Forged")
            .await
            .is_err()
    );
    assert!(
        authority
            .set_client_name(Identity::generate().endpoint_id(), "Unknown".into())
            .await
            .is_err()
    );
    let reopened = Authority::open(directory.path(), key.endpoint_id()).unwrap();
    assert_eq!(
        reopened
            .snapshot()
            .await
            .unwrap()
            .clients
            .iter()
            .find(|c| c.peer == device.endpoint_id())
            .unwrap()
            .name,
        "johnny-s-phone"
    );
    mutate(
        directory.path(),
        key.endpoint_id(),
        Mutation::Revoke {
            expected_revision: snapshot.revision,
            peer: device.endpoint_id(),
        },
    );
    assert!(
        control::set_name_on(&connection, key.endpoint_id(), "Revoked")
            .await
            .is_err()
    );
    assert!(
        control::device_id_on(&connection, key.endpoint_id())
            .await
            .is_err()
    );
    assert!(!authority.approved(device.endpoint_id()).await);
    endpoint.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn diagnostic_is_approved_peer_only_and_does_not_open_a_backend_or_change_policy() {
    use tokio::time::{Duration, timeout};
    let directory = dir();
    let host_key = Identity::generate();
    let client = Identity::generate();
    host::initialize(directory.path(), host_key.endpoint_id()).unwrap();
    approve(
        directory.path(),
        host_key.endpoint_id(),
        client.endpoint_id(),
        0,
    );
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service(
        directory.path(),
        host_key.endpoint_id(),
        client.endpoint_id(),
        backend.local_addr().unwrap(),
        1,
    );
    let authority = Authority::open(directory.path(), host_key.endpoint_id()).unwrap();
    let before = serde_json::to_value(authority.snapshot().await.unwrap()).unwrap();
    let server = tunnel::TunnelHost::bind_host(host_key.secret_key(), &config(), authority.clone())
        .await
        .unwrap();
    let endpoint = tunnel::bind_client(client.secret_key()).await.unwrap();
    let connection = control::connect(
        &endpoint,
        host_key.endpoint_id(),
        server.endpoint().bound_sockets()[0],
    )
    .await
    .unwrap();
    let report = control::diagnostic_on(&connection, host_key.endpoint_id())
        .await
        .unwrap();
    assert!(report.valid(host_key.endpoint_id()));
    assert_eq!(report.paths.selected_path, "direct");
    assert!(!report.network.fresh); // No relay discovery is configured in this fixture.
    let mapping = control::router_mapping_on(&connection, host_key.endpoint_id())
        .await
        .unwrap();
    assert!(mapping.valid(host_key.endpoint_id()));
    let mapping_json = serde_json::to_string(&mapping).unwrap();
    assert!(!mapping_json.contains("127.0.0.1"));
    assert!(!mapping_json.contains("192.168."));
    assert!(
        control::router_mapping_on(&connection, client.endpoint_id())
            .await
            .is_err()
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("127.0.0.1"));
    assert!(!json.contains("llama"));
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    assert_eq!(
        before,
        serde_json::to_value(authority.snapshot().await.unwrap()).unwrap()
    );
    let stranger = tunnel::bind_client(Identity::generate().secret_key())
        .await
        .unwrap();
    if let Ok(connection) = control::connect(
        &stranger,
        host_key.endpoint_id(),
        server.endpoint().bound_sockets()[0],
    )
    .await
    {
        assert!(
            control::diagnostic_on(&connection, host_key.endpoint_id())
                .await
                .is_err()
        );
        assert!(
            control::router_mapping_on(&connection, host_key.endpoint_id())
                .await
                .is_err()
        );
    }
    endpoint.close().await;
    stranger.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn old_host_without_diagnostic_support_keeps_the_local_report() {
    use iroh::{Endpoint, EndpointAddr, endpoint::presets};
    use wispnet_core::host::ALPN;
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap();
    let host = server.id();
    let accepted = server.clone();
    let worker = tokio::spawn(async move {
        let connection = accepted.accept().await.unwrap().await.unwrap();
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let mut length = [0u8; 4];
            if recv.read_exact(&mut length).await.is_err() {
                break;
            }
            let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
            recv.read_exact(&mut bytes).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if request["op"] != "catalog" {
                let _ = send.reset(1u32.into());
                continue;
            }
            let reply = serde_json::to_vec(&serde_json::json!({"version":1,"complete":true,
                "host":host,"generation":"a".repeat(64),"revision":0,"service_count":0,"services":[]})).unwrap();
            send.write_all(&(reply.len() as u32).to_be_bytes())
                .await
                .unwrap();
            send.write_all(&reply).await.unwrap();
            send.finish().unwrap();
        }
    });
    let endpoint = tunnel::bind_client(Identity::generate().secret_key())
        .await
        .unwrap();
    let report = wispnet_agent::diagnostics::explain_path(
        &endpoint,
        host,
        EndpointAddr::new(host).with_ip_addr(server.bound_sockets()[0]),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(report["local"]["paths"]["selected_path"], "direct");
    assert!(report["remote"].is_null());
    assert!(
        report["remote_error"]
            .as_str()
            .unwrap()
            .contains("may need an update")
    );
    endpoint.close().await;
    server.close().await;
    worker.abort();
    let _ = worker.await;
}

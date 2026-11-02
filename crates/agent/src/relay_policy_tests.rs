//! Exercise independent relay policy choices through the normal configured builder.
use super::*;
use iroh::Watcher;
use iroh_relay::server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig};
use std::{net::SocketAddr, os::unix::fs::PermissionsExt};
use wispnet_core::identity::Identity;

async fn relay(cert: &rcgen::CertifiedKey<rcgen::KeyPair>) -> Server {
    let tls = rustls::ServerConfig::builder_with_provider(iroh_relay::tls::default_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls_pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut relay = RelayConfig::new("127.0.0.1:0".parse::<SocketAddr>().unwrap());
    relay.tls = Some(TlsConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        CertConfig::Manual { server_config: tls },
    ));
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    Server::spawn(config).await.unwrap()
}

async fn delayed_copy<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W) {
    let mut bytes = [0; 65536];
    while let Ok(size) = r.read(&mut bytes).await {
        if size == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if w.write_all(&bytes[..size]).await.is_err() {
            break;
        }
    }
    let _ = w.shutdown().await;
}

async fn check_policy(prefer: bool, warm: bool) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let primary = relay(&cert).await;
    let backup = relay(&cert).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_url: iroh::RelayUrl = format!("https://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let backup_url: iroh::RelayUrl = format!("https://{}", backup.https_addr().unwrap())
        .parse()
        .unwrap();
    let upstream = primary.https_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let mut workers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (downstream, _) = accepted.unwrap();
                    workers.spawn(async move {
                        let upstream = TcpStream::connect(upstream).await.unwrap();
                        let (mut dr, mut dw) = downstream.into_split();
                        let (mut ur, mut uw) = upstream.into_split();
                        tokio::join!(delayed_copy(&mut dr, &mut uw), delayed_copy(&mut ur, &mut dw));
                    });
                }
                _ = workers.join_next(), if !workers.is_empty() => {}
            }
        }
    });
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ca = state.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let config: TransportConfig = serde_json::from_value(serde_json::json!({
        "bind_addr":"127.0.0.1:0", "relay_only":true,"relay_ca_file":ca,
        "relay_url":primary_url,"backup_relay_urls":[backup_url],
        "prefer_primary_relay":prefer,"keep_backup_relays_warm":warm,
    }))
    .unwrap();
    let host_key = Identity::generate();
    let client_key = Identity::generate();
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = TunnelHost::bind(
        host_key.secret_key(),
        &config,
        &[ServiceConfig {
            id: "echo".into(),
            backend: backend.local_addr().unwrap(),
            allow: vec![client_key.endpoint_id()],
        }],
    )
    .await
    .unwrap();
    let client = bind_configured_client(client_key.secret_key(), Some(&config))
        .await
        .unwrap();
    let expected = if prefer { &primary_url } else { &backup_url };
    timeout(Duration::from_secs(20), async {
        loop {
            let sessions = client.relay_connections();
            let home_matches = client
                .home_relay_status()
                .get()
                .iter()
                .any(|s| s.url() == expected && s.is_connected());
            if home_matches && (!warm || sessions.iter().filter(|s| s.is_connected()).count() == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("relay home/session policy failed: prefer={prefer}, warm={warm}"));
    if prefer && !warm {
        // A healthy preferred primary needs no standby packet session before
        // any peer is dialed. Allow another relay-policy tick to catch an
        // accidentally enabled warm-backup policy.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            client
                .relay_connections()
                .iter()
                .filter(|s| s.is_connected())
                .count(),
            1
        );
    }

    let target = peer_target(host_key.endpoint_id(), Some(&config), None).unwrap();
    assert_eq!(target.relay_urls().count(), 2);
    let connection = connect_target(&client, target).await.unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            if connection.paths().iter().any(|p| {
                p.is_selected() && p.remote_addr() == &iroh::TransportAddr::Relay(expected.clone())
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("relay data-path policy failed: prefer={prefer}, warm={warm}"));
    assert_eq!(connection.remote_id(), host_key.endpoint_id());
    let mut stream = open_service(&connection, "echo").await.unwrap();
    let (mut backend, _) = backend.accept().await.unwrap();
    stream.write_all(b"policy").await.unwrap();
    let mut bytes = [0; 6];
    timeout(Duration::from_secs(5), backend.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"policy");
    connection.close(0u32.into(), b"relay policy fixture complete");
    client.close().await;
    host.shutdown().await;
    proxy.abort();
    let _ = proxy.await;
    primary.shutdown().await.unwrap();
    backup.shutdown().await.unwrap();
}

#[tokio::test]
async fn relay_preference_and_warm_standby_are_independent() {
    tokio::join!(
        check_policy(true, true),
        check_policy(true, false),
        check_policy(false, true),
        check_policy(false, false),
    );
}

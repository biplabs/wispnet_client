//! Real DNS/TLS recovery fixtures; no hosts-file edits or live DNS dependencies.
use super::*;
use crate::control;
use iroh::dns::{DnsResolver, NameserverConfig};
use n0_future::StreamExt;
use std::{
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::PermissionsExt,
    sync::Mutex,
};
use wispnet_core::identity::Identity;
use wispnet_registry::host::{self, Authority, Mutation};

struct DnsFixture {
    address: SocketAddr,
    replacement: Arc<std::sync::atomic::AtomicBool>,
    questions: Arc<Mutex<Vec<String>>>,
    worker: JoinHandle<()>,
}
impl DnsFixture {
    async fn start() -> Self {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let replacement = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let questions = Arc::new(Mutex::new(Vec::new()));
        let replaced = replacement.clone();
        let log = questions.clone();
        let worker = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            loop {
                let (size, source) = socket.recv_from(&mut buffer).await.unwrap();
                let packet = &buffer[..size];
                assert!(size >= 17 && packet[4..6] == [0, 1]);
                let mut cursor = 12;
                let mut labels = Vec::new();
                while packet[cursor] != 0 {
                    let length = packet[cursor] as usize;
                    cursor += 1;
                    labels.push(std::str::from_utf8(&packet[cursor..cursor + length]).unwrap());
                    cursor += length;
                }
                cursor += 1;
                let kind = u16::from_be_bytes([packet[cursor], packet[cursor + 1]]);
                let name = labels.join(".");
                log.lock().unwrap().push(name.clone());
                assert_eq!(name, "relay.test", "unexpected node discovery DNS query");
                let mut response = packet[..2].to_vec();
                let ipv6 = replaced.load(Ordering::SeqCst);
                let answers = (!ipv6 && kind == 1) || (ipv6 && kind == 28);
                response.extend_from_slice(&[0x81, 0x80, 0, 1, 0, u8::from(answers), 0, 0, 0, 0]);
                response.extend_from_slice(&packet[12..cursor + 4]);
                if answers {
                    // Switch loopback IP families without privileged interface
                    // aliases: macOS does not bind arbitrary 127/8 addresses.
                    // One-second TTL still exercises ordinary cache expiry.
                    let address = if ipv6 {
                        std::net::Ipv6Addr::LOCALHOST.octets().to_vec()
                    } else {
                        Ipv4Addr::LOCALHOST.octets().to_vec()
                    };
                    response.extend_from_slice(&[0xc0, 0x0c]);
                    response.extend_from_slice(&kind.to_be_bytes());
                    response.extend_from_slice(&[0, 1, 0, 0, 0, 1, 0, address.len() as u8]);
                    response.extend_from_slice(&address);
                }
                socket.send_to(&response, source).await.unwrap();
            }
        });
        Self {
            address,
            replacement,
            questions,
            worker,
        }
    }
    fn resolver(&self) -> DnsResolver {
        DnsResolver::builder()
            .add_nameserver_config(
                NameserverConfig::udp(self.address.ip()).with_port(self.address.port()),
            )
            .disable_fallback()
            .build()
    }
}
impl Drop for DnsFixture {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn relay(
    cert: &rcgen::CertifiedKey<rcgen::KeyPair>,
    address: SocketAddr,
) -> iroh_relay::server::Server {
    relay_with_access(cert, address, Arc::new(iroh_relay::server::AllowAll)).await
}

async fn relay_with_access(
    cert: &rcgen::CertifiedKey<rcgen::KeyPair>,
    address: SocketAddr,
    access: Arc<dyn iroh_relay::server::DynAccessControl>,
) -> iroh_relay::server::Server {
    use iroh_relay::server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig};
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        rustls_pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let mut config = RelayConfig::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    config.access = access;
    config.tls = Some(TlsConfig::new(
        address,
        CertConfig::Manual { server_config: tls },
    ));
    let mut server = ServerConfig::default();
    server.relay = Some(config);
    Server::spawn(server).await.unwrap()
}

#[tokio::test]
async fn relay_dns_ip_replacement_recovers_without_endpoint_or_policy_reset() {
    let dns = DnsFixture::start().await;
    let cert = rcgen::generate_simple_self_signed(vec!["relay.test".into()]).unwrap();
    let original = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let port = original.https_addr().unwrap().port();
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ca = state.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let config = TransportConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: Some(format!("https://relay.test:{port}")),
        relay_only: true,
        relay_ca_file: Some(ca),
        max_connections: 8,
        max_streams: 8,
        max_streams_per_connection: 4,
    };
    let host_key = Identity::generate();
    let client_key = Identity::generate();
    host::initialize(state.path(), host_key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(state.path(), host_key.endpoint_id()).unwrap();
    store
        .host_mutate(
            host_key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: client_key.endpoint_id(),
                name: "client".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let authority = Authority::open(state.path(), host_key.endpoint_id()).unwrap();
    let builder = configured_builder(host_key.secret_key(), Some(&config))
        .unwrap()
        .dns_resolver(dns.resolver());
    let host = TunnelHost::bind_builder(
        &config,
        Services::host(authority.clone()).unwrap(),
        builder,
        true,
    )
    .await
    .unwrap();
    let client = configured_builder(client_key.secret_key(), Some(&config))
        .unwrap()
        .dns_resolver(dns.resolver())
        .bind()
        .await
        .unwrap();
    timeout(Duration::from_secs(10), async {
        tokio::join!(host.endpoint().online(), client.online());
    })
    .await
    .unwrap();
    // A stale direct hint must not prevent reaching the pinned relay-only host.
    let target = peer_target(
        host_key.endpoint_id(),
        Some(&config),
        Some("127.0.0.9:9".parse().unwrap()),
    )
    .unwrap();
    let diagnostic = crate::diagnostics::explain_path(
        &client,
        host_key.endpoint_id(),
        target.clone(),
        true,
        true,
    )
    .await
    .unwrap();
    assert_eq!(diagnostic["selected_path"], "relay");
    assert_eq!(
        diagnostic["reason"],
        "This device is configured to use relay only."
    );
    assert!(diagnostic["network_report"].is_null() || diagnostic["network_report"].is_object());
    assert!(
        crate::diagnostics::explain_path(
            &client,
            client_key.endpoint_id(),
            target.clone(),
            false,
            true,
        )
        .await
        .is_err()
    );
    let before_connection = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    let before = control::catalog_on(&before_connection, host_key.endpoint_id())
        .await
        .unwrap();
    let observation = tokio::time::Instant::now();
    crate::diagnostics::settle_path(&before_connection).await;
    assert!(observation.elapsed() >= Duration::from_secs(1));
    assert_eq!(
        crate::diagnostics::selected_path(&before_connection),
        "relay"
    );
    before_connection.close(0u32.into(), b"fixture reconnect");
    // Once closed, observation must return promptly instead of waiting again.
    timeout(
        Duration::from_millis(100),
        crate::diagnostics::settle_path(&before_connection),
    )
    .await
    .unwrap();
    original.shutdown().await.unwrap();
    let changed = relay(
        &cert,
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    )
    .await;
    dns.replacement.store(true, Ordering::SeqCst);
    // Do not reset endpoints, resolver caches, or peer pins. Recovery must use
    // ordinary TTL expiry plus Iroh's independent outbound reconnect/backoff.
    timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(connection) = control::connect_target(&client, target.clone()).await
                && let Ok(after) = control::catalog_on(&connection, host_key.endpoint_id()).await
            {
                assert_eq!(
                    serde_json::to_value(&after).unwrap(),
                    serde_json::to_value(&before).unwrap()
                );
                assert!(connection.paths().iter().any(|p| p.is_relay()));
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(host.endpoint().id(), host_key.endpoint_id());
    assert_eq!(client.id(), client_key.endpoint_id());
    assert!(authority.approved(client_key.endpoint_id()).await);
    {
        let questions = dns.questions.lock().unwrap();
        assert!(questions.len() >= 4);
        assert!(questions.iter().all(|name| name == "relay.test"));
    }
    assert!(
        !dns.worker.is_finished(),
        "DNS fixture failed while observing queries"
    );
    client.close().await;
    host.shutdown().await;
    changed.shutdown().await.unwrap();
}

#[derive(Debug)]
struct DenyAdmission;
impl iroh_relay::server::AccessControl for DenyAdmission {
    async fn on_connect(
        &self,
        _: &iroh_relay::server::ClientRequest,
    ) -> iroh_relay::server::Access {
        iroh_relay::server::Access::Deny {
            reason: Some("SECRET-SENTINEL untrusted relay reason".into()),
        }
    }
}

#[tokio::test]
async fn relay_admission_diagnostic_is_distinct_and_redacts_server_reason() {
    use iroh::Watcher;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let relay = relay_with_access(
        &cert,
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(DenyAdmission),
    )
    .await;
    let trust = tempfile::tempdir().unwrap();
    let ca = trust.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let config = TransportConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: Some(format!("https://{}", relay.https_addr().unwrap())),
        relay_only: true,
        relay_ca_file: Some(ca),
        max_connections: 8,
        max_streams: 8,
        max_streams_per_connection: 4,
    };
    let key = Identity::generate();
    let endpoint = bind_configured_client(key.secret_key(), Some(&config))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if endpoint
                .home_relay_status()
                .get()
                .iter()
                .any(|r| r.auth_denied_reason().is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let target = peer_target(Identity::generate().endpoint_id(), Some(&config), None).unwrap();
    let error = control::connect_target(&endpoint, target)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        TunnelError::Transport("configured relay denied endpoint admission")
    );
    assert!(!format!("{error} {error:?}").contains("SECRET-SENTINEL"));
    endpoint.close().await;
    relay.shutdown().await.unwrap();
}

#[test]
fn outside_discovery_is_not_a_packet_relay_destination() {
    let mut config: TransportConfig = serde_json::from_value(serde_json::json!({
        "bind_addr":"0.0.0.0:0", "relay_url":"https://home.example"
    }))
    .unwrap();
    let legacy = configured_relay_mode(&config).unwrap().unwrap().relay_map();
    let home: iroh::RelayUrl = "https://home.example".parse().unwrap();
    let outside: iroh::RelayUrl = "https://outside.example".parse().unwrap();
    assert!(legacy.get(&home).unwrap().quic.is_some());
    assert_eq!(legacy.len(), 1);
    config.discovery_relay_url = Some(outside.to_string());
    let map = configured_relay_mode(&config).unwrap().unwrap().relay_map();
    assert_eq!(map.len(), 1);
    assert!(map.contains(&home));
    assert!(!map.contains(&outside));
    let discovery = configured_discovery_relay(&config).unwrap().unwrap();
    assert_eq!(discovery.url, outside);
    assert_ne!(discovery.url, home);
    assert_eq!(discovery.quic.as_ref().unwrap().port, 7842);
    // Pairing/bootstrap still names the primary, regardless of relay selection.
    let target = peer_target(Identity::generate().endpoint_id(), Some(&config), None).unwrap();
    assert_eq!(target.relay_urls().next(), Some(&home));
    config.backup_relay_urls.push(outside.to_string());
    let map = configured_relay_mode(&config).unwrap().unwrap().relay_map();
    assert!(map.contains(&outside));
    assert_eq!(map.len(), 2);
    let target = peer_target(Identity::generate().endpoint_id(), Some(&config), None).unwrap();
    assert_eq!(target.relay_urls().count(), 2);
    assert!(target.relay_urls().any(|url| url == &outside));
}

#[tokio::test]
async fn primary_discovery_timeout_uses_same_socket_fallback_without_changing_home_relay() {
    use iroh::Watcher;
    use iroh_relay::server::{
        CertConfig, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig,
    };
    let cert = rcgen::generate_simple_self_signed(vec!["relay.test".into()]).unwrap();
    let primary = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let primary_url: iroh::RelayUrl = format!(
        "https://relay.test:{}",
        primary.https_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        rustls_pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let mut relay_config = RelayConfig::new("127.0.0.1:0".parse::<SocketAddr>().unwrap());
    relay_config.tls = Some(TlsConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        CertConfig::Manual { server_config: tls },
    ));
    let mut server_config = ServerConfig::default();
    server_config.relay = Some(relay_config);
    server_config.quic = Some(QuicConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
    ));
    let observer = Server::spawn(server_config).await.unwrap();
    let observer_url: iroh::RelayUrl = format!(
        "https://relay.test:{}",
        observer.https_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    let fallback = iroh::RelayConfig::new(
        observer_url.clone(),
        Some(iroh_relay::RelayQuicConfig::new(
            observer.quic_addr().unwrap().port(),
        )),
    );
    let unused = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let unused_port = unused.local_addr().unwrap().port();
    drop(unused);
    let primary_config = iroh::RelayConfig::new(
        primary_url.clone(),
        Some(iroh_relay::RelayQuicConfig::new(unused_port)),
    );
    let dns = DnsFixture::start().await;
    let endpoint = Endpoint::builder(presets::Minimal)
        .dns_resolver(dns.resolver())
        .relay_mode(iroh::RelayMode::Custom(
            [primary_config].into_iter().collect(),
        ))
        .ca_tls_config(iroh_relay::tls::CaTlsConfig::custom_roots([cert
            .cert
            .der()
            .clone()]))
        .net_report_config(
            iroh::endpoint::NetReportConfig::default().quic_discovery_fallback(fallback),
        )
        .bind()
        .await
        .unwrap();
    let report = timeout(Duration::from_secs(8), async {
        let mut reports = endpoint.net_report().stream();
        while let Some(report) = reports.next().await {
            if let Some(report) = report
                && report.has_udp()
                && report.preferred_relay.as_ref() == Some(&primary_url)
            {
                return report;
            }
        }
        panic!("network report stream ended");
    })
    .await
    .unwrap();
    // Primary has no QAD listener: this response can only come from the observer.
    // Its observed source port must belong to this endpoint, not a helper socket.
    let observed_port = report.global_v4.unwrap().port();
    assert!(
        endpoint
            .bound_sockets()
            .iter()
            .any(|a| a.is_ipv4() && a.port() == observed_port)
    );
    timeout(Duration::from_secs(5), endpoint.online())
        .await
        .unwrap();
    let homes = endpoint.home_relay_status().get();
    assert!(
        homes
            .iter()
            .any(|r| r.url() == &primary_url && r.is_connected())
    );
    assert!(homes.iter().all(|r| r.url() != &observer_url));
    endpoint.close().await;
    observer.shutdown().await.unwrap();
    primary.shutdown().await.unwrap();
}

// Exercise the exact generic policy used by the vendored dependency without
// pulling its unrelated upstream development dependencies into this workspace.
#[path = "../../../vendor/iroh/src/net_report/fallback.rs"]
mod discovery_fallback_policy;

#[tokio::test]
async fn backup_relay_supports_cold_start_and_primary_loss_without_repairing() {
    use iroh::Watcher;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let primary = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let primary_addr = primary.https_addr().unwrap();
    let backup = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let backup_url = format!("https://{}", backup.https_addr().unwrap());
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ca = state.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let mut config: TransportConfig = serde_json::from_value(serde_json::json!({
        "bind_addr":"127.0.0.1:0", "relay_url":format!("https://{primary_addr}"),
        "backup_relay_urls":[backup_url], "relay_only":true, "relay_ca_file":ca,
    }))
    .unwrap();
    let host_key = Identity::generate();
    let client_key = Identity::generate();
    host::initialize(state.path(), host_key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(state.path(), host_key.endpoint_id()).unwrap();
    store
        .host_mutate(
            host_key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: client_key.endpoint_id(),
                name: "phone".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let authority = Authority::open(state.path(), host_key.endpoint_id()).unwrap();
    // Primary offline before either endpoint has ever seen the backup.
    primary.shutdown().await.unwrap();
    let host = TunnelHost::bind_host(host_key.secret_key(), &config, authority.clone())
        .await
        .unwrap();
    let client = bind_configured_client(client_key.secret_key(), Some(&config))
        .await
        .unwrap();
    timeout(Duration::from_secs(20), async {
        tokio::join!(host.endpoint().online(), client.online());
    })
    .await
    .unwrap();
    let target = peer_target(host_key.endpoint_id(), Some(&config), None).unwrap();
    assert_eq!(target.relay_urls().count(), 2);
    let connection = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    let before = control::catalog_on(&connection, host_key.endpoint_id())
        .await
        .unwrap();
    assert!(connection.paths().iter().all(|p| p.is_relay()));
    assert_eq!(
        client.home_relay_status().get()[0].url().to_string(),
        format!("{backup_url}/")
    );
    // Bring primary back, then remove the currently selected backup. Existing
    // endpoints must discover the remaining relay, without clearing peer state.
    let primary = relay(&cert, primary_addr).await;
    connection.close(0u32.into(), b"fixture failover");
    backup.shutdown().await.unwrap();
    timeout(Duration::from_secs(90), async {
        loop {
            if let Ok(c) = control::connect_target(&client, target.clone()).await
                && let Ok(after) = control::catalog_on(&c, host_key.endpoint_id()).await
            {
                assert_eq!(
                    serde_json::to_value(&before).unwrap(),
                    serde_json::to_value(&after).unwrap()
                );
                assert!(c.paths().iter().all(|p| p.is_relay()));
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap();
    client.close().await;
    host.shutdown().await;
    // A fresh process with the same identity and approvals also works with
    // the backup offline. No discovery cache or old connection is required.
    config.discovery_relay_url = None;
    let host = TunnelHost::bind_host(host_key.secret_key(), &config, authority.clone())
        .await
        .unwrap();
    let client = bind_configured_client(client_key.secret_key(), Some(&config))
        .await
        .unwrap();
    let c = control::connect_target(&client, target).await.unwrap();
    let after = control::catalog_on(&c, host_key.endpoint_id())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    assert!(authority.approved(client_key.endpoint_id()).await);
    client.close().await;
    host.shutdown().await;
    primary.shutdown().await.unwrap();
}

#[derive(Debug, Default)]
struct RecordAdmission(Mutex<std::collections::BTreeSet<iroh::EndpointId>>);
impl iroh_relay::server::AccessControl for RecordAdmission {
    async fn on_connect(
        &self,
        request: &iroh_relay::server::ClientRequest,
    ) -> iroh_relay::server::Access {
        self.0.lock().unwrap().insert(request.endpoint_id());
        iroh_relay::server::Access::Allow
    }
}

#[tokio::test]
async fn packet_priority_ignores_discovery_and_latency_and_requires_stable_failback() {
    use iroh::Watcher;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    // Neither fixture offers UDP discovery. Packet relay health must suffice.
    let primary = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let address = primary.https_addr().unwrap();
    let primary_url: iroh::RelayUrl = format!("https://{address}").parse().unwrap();
    let backup = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let backup_url: iroh::RelayUrl = format!("https://{}", backup.https_addr().unwrap())
        .parse()
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ca = state.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let config: TransportConfig = serde_json::from_value(serde_json::json!({
        "bind_addr":"127.0.0.1:0", "relay_only":true, "relay_ca_file":ca,
        "relay_url":primary_url, "backup_relay_urls":[backup_url],
    }))
    .unwrap();
    let host_key = Identity::generate();
    let client_key = Identity::generate();
    host::initialize(state.path(), host_key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(state.path(), host_key.endpoint_id()).unwrap();
    store
        .host_mutate(
            host_key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: client_key.endpoint_id(),
                name: "phone".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let authority = Authority::open(state.path(), host_key.endpoint_id()).unwrap();
    let builder = |key: &SecretKey| {
        configured_builder(key, Some(&config))
            .unwrap()
            .net_report_config(
                iroh::endpoint::NetReportConfig::minimal()
                    .keep_all_relays(true)
                    .prefer_relay(
                        primary_url.clone(),
                        Duration::from_secs(1),
                        Duration::from_secs(4),
                    ),
            )
    };
    let host = TunnelHost::bind_builder(
        &config,
        Services::host(authority.clone()).unwrap(),
        builder(host_key.secret_key()),
        true,
    )
    .await
    .unwrap();
    let client = builder(client_key.secret_key()).bind().await.unwrap();
    async fn wait_home(endpoint: &Endpoint, url: &iroh::RelayUrl) {
        timeout(Duration::from_secs(20), async {
            loop {
                if endpoint
                    .home_relay_status()
                    .get()
                    .iter()
                    .any(|s| s.url() == url && s.is_connected())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
    tokio::join!(
        wait_home(host.endpoint(), &primary_url),
        wait_home(&client, &primary_url)
    );
    // Multiple net reports and a usable backup cannot displace healthy primary.
    host.endpoint().network_change().await;
    client.network_change().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(client.home_relay_status().get()[0].url(), &primary_url);
    let target = peer_target(host_key.endpoint_id(), Some(&config), None).unwrap();
    let c = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    let before = control::catalog_on(&c, host_key.endpoint_id())
        .await
        .unwrap();
    c.close(0u32.into(), b"priority fixture");
    primary.shutdown().await.unwrap();
    tokio::join!(
        wait_home(host.endpoint(), &backup_url),
        wait_home(&client, &backup_url)
    );
    let c = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(
            control::catalog_on(&c, host_key.endpoint_id())
                .await
                .unwrap()
        )
        .unwrap()
    );
    c.close(0u32.into(), b"priority fixture");
    // A brief recovery must not trigger failback.
    let admissions = Arc::new(RecordAdmission::default());
    let recovering = relay_with_access(&cert, address, admissions.clone()).await;
    timeout(Duration::from_secs(10), async {
        loop {
            if admissions
                .0
                .lock()
                .unwrap()
                .contains(&client_key.endpoint_id())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(client.home_relay_status().get()[0].url(), &backup_url);
    recovering.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(client.home_relay_status().get()[0].url(), &backup_url);
    let restored = relay(&cert, address).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(client.home_relay_status().get()[0].url(), &backup_url);
    tokio::join!(
        wait_home(host.endpoint(), &primary_url),
        wait_home(&client, &primary_url)
    );
    // Verify data still works and the authenticated catalog/approvals survived.
    let c = control::connect_target(&client, target).await.unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(
            control::catalog_on(&c, host_key.endpoint_id())
                .await
                .unwrap()
        )
        .unwrap()
    );
    assert!(authority.approved(client_key.endpoint_id()).await);
    client.close().await;
    host.shutdown().await;
    restored.shutdown().await.unwrap();
    backup.shutdown().await.unwrap();
}

#[tokio::test]
async fn different_home_relays_connect_without_first_contact_through_primary() {
    use iroh::Watcher;
    let phone_dns = DnsFixture::start().await;
    let host_dns = DnsFixture::start().await;
    // Same primary URL, but the host's network can only resolve an unreachable
    // IPv6 listener. The phone can reach its IPv4 listener. Backup is shared.
    host_dns.replacement.store(true, Ordering::SeqCst);
    let cert =
        rcgen::generate_simple_self_signed(vec!["relay.test".into(), "127.0.0.1".into()]).unwrap();
    let primary = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let backup = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let primary_url: iroh::RelayUrl = format!(
        "https://relay.test:{}",
        primary.https_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    let backup_url: iroh::RelayUrl = format!("https://{}", backup.https_addr().unwrap())
        .parse()
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ca = state.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let config: TransportConfig = serde_json::from_value(serde_json::json!({
        "bind_addr":"127.0.0.1:0", "relay_only":true, "relay_ca_file":ca,
        "relay_url":primary_url, "backup_relay_urls":[backup_url],
    }))
    .unwrap();
    let host_key = Identity::generate();
    let phone_key = Identity::generate();
    host::initialize(state.path(), host_key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(state.path(), host_key.endpoint_id()).unwrap();
    store
        .host_mutate(
            host_key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: phone_key.endpoint_id(),
                name: "phone".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let authority = Authority::open(state.path(), host_key.endpoint_id()).unwrap();
    let builder = |key: &SecretKey, resolver: DnsResolver| {
        configured_builder(key, Some(&config))
            .unwrap()
            .dns_resolver(resolver)
            .net_report_config(
                iroh::endpoint::NetReportConfig::minimal()
                    .keep_all_relays(true)
                    .prefer_relay(
                        primary_url.clone(),
                        Duration::from_secs(1),
                        Duration::from_secs(4),
                    ),
            )
    };
    let host = TunnelHost::bind_builder(
        &config,
        Services::host(authority.clone()).unwrap(),
        builder(host_key.secret_key(), host_dns.resolver()),
        true,
    )
    .await
    .unwrap();
    let phone = builder(phone_key.secret_key(), phone_dns.resolver())
        .bind()
        .await
        .unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let host_ready = host
                .endpoint()
                .home_relay_status()
                .get()
                .iter()
                .any(|s| s.url() == &backup_url && s.is_connected());
            let phone_ready = phone
                .home_relay_status()
                .get()
                .iter()
                .any(|s| s.url() == &primary_url && s.is_connected());
            if host_ready && phone_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    // This is their first peer connection: no previous exchange can have told
    // the phone the host's home relay. Both candidates came from local config.
    let target = peer_target(host_key.endpoint_id(), Some(&config), None).unwrap();
    let connection = control::connect_target(&phone, target).await.unwrap();
    let catalog = control::catalog_on(&connection, host_key.endpoint_id())
        .await
        .unwrap();
    assert_eq!(catalog.host, host_key.endpoint_id());
    assert!(connection.paths().iter().all(|p| p.is_relay()));
    assert_eq!(phone.home_relay_status().get()[0].url(), &primary_url);
    assert_eq!(
        host.endpoint().home_relay_status().get()[0].url(),
        &backup_url
    );
    assert!(authority.approved(phone_key.endpoint_id()).await);
    phone.close().await;
    host.shutdown().await;
    primary.shutdown().await.unwrap();
    backup.shutdown().await.unwrap();
}

async fn qad_relay(cert: &rcgen::CertifiedKey<rcgen::KeyPair>) -> iroh_relay::server::Server {
    use iroh_relay::server::{
        CertConfig, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig,
    };
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        rustls_pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let mut config = RelayConfig::new("127.0.0.1:0".parse::<SocketAddr>().unwrap());
    config.tls = Some(TlsConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        CertConfig::Manual { server_config: tls },
    ));
    let mut server = ServerConfig::default();
    server.relay = Some(config);
    server.quic = Some(QuicConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
    ));
    Server::spawn(server).await.unwrap()
}

#[tokio::test]
async fn parallel_discovery_retains_all_candidates_and_idle_relay_sessions() {
    use iroh::Watcher;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let primary = qad_relay(&cert).await;
    let backup = qad_relay(&cert).await;
    let primary_url: iroh::RelayUrl = format!("https://{}", primary.https_addr().unwrap())
        .parse()
        .unwrap();
    let backup_url: iroh::RelayUrl = format!("https://{}", backup.https_addr().unwrap())
        .parse()
        .unwrap();
    // This simple UDP NAT makes the second observer see a different source port.
    // It exercises real QUIC discovery and proves neither candidate is discarded.
    let proxy = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let upstream = backup.quic_addr().unwrap();
    let worker = tokio::spawn(async move {
        let mut client = None;
        let mut bytes = [0; 65536];
        loop {
            let (n, source) = proxy.recv_from(&mut bytes).await.unwrap();
            if source == upstream {
                if let Some(destination) = client {
                    proxy.send_to(&bytes[..n], destination).await.unwrap();
                }
            } else if client.is_none() || client == Some(source) {
                client = Some(source);
                proxy.send_to(&bytes[..n], upstream).await.unwrap();
            }
        }
    });
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Custom(
            [
                iroh::RelayConfig::new(
                    primary_url.clone(),
                    Some(iroh_relay::RelayQuicConfig::new(
                        primary.quic_addr().unwrap().port(),
                    )),
                ),
                iroh::RelayConfig::new(
                    backup_url.clone(),
                    Some(iroh_relay::RelayQuicConfig::new(proxy_addr.port())),
                ),
            ]
            .into_iter()
            .collect(),
        ))
        .ca_tls_config(iroh_relay::tls::CaTlsConfig::custom_roots([cert
            .cert
            .der()
            .clone()]))
        .net_report_config(
            iroh::endpoint::NetReportConfig::default()
                .keep_all_relays(true)
                .prefer_relay(
                    primary_url.clone(),
                    Duration::from_secs(1),
                    Duration::from_secs(3),
                ),
        )
        .bind()
        .await
        .unwrap();
    async fn complete(
        endpoint: &Endpoint,
    ) -> std::collections::BTreeMap<iroh::RelayUrl, std::collections::BTreeSet<SocketAddr>> {
        timeout(Duration::from_secs(12), async {
            loop {
                if let Some(report) = endpoint.net_report().get()
                    && report.discovery_addresses.len() == 2
                    && endpoint
                        .relay_connections()
                        .iter()
                        .filter(|s| s.is_connected())
                        .count()
                        == 2
                {
                    return report.discovery_addresses;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap()
    }
    let report = complete(&endpoint).await;
    let first = report[&primary_url].iter().find(|a| a.is_ipv4()).unwrap();
    let second = report[&backup_url].iter().find(|a| a.is_ipv4()).unwrap();
    assert_ne!(first, second);
    assert_eq!(second.port(), proxy_addr.port());
    timeout(Duration::from_secs(3), async {
        loop {
            let addresses: std::collections::BTreeSet<_> =
                endpoint.addr().ip_addrs().copied().collect();
            if addresses.contains(first) && addresses.contains(second) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // No peer traffic: both TCP sessions and both independent QAD sessions must
    // survive beyond upstream's 60-second non-home idle cleanup boundary.
    tokio::time::sleep(Duration::from_secs(65)).await;
    endpoint.network_change().await;
    // This notification does not change any interfaces. Netmon may correctly
    // suppress it, so a new report event is not guaranteed. Check that both
    // discoveries and relay sessions survived the idle interval instead.
    let report = complete(&endpoint).await;
    assert_eq!(
        endpoint
            .relay_connections()
            .iter()
            .filter(|s| s.is_connected())
            .count(),
        2
    );
    assert_eq!(report.len(), 2);
    assert!(
        report[&backup_url]
            .iter()
            .any(|a| a.port() == proxy_addr.port())
    );
    assert_eq!(endpoint.home_relay_status().get()[0].url(), &primary_url);
    // Bootstrap through relay only, exchange the full candidate set, then
    // validate a real direct path despite the additional translated candidate.
    endpoint.set_alpns(vec![protocol::ALPN.to_vec()]);
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::custom([
            primary_url.clone(),
            backup_url.clone(),
        ]))
        .ca_tls_config(iroh_relay::tls::CaTlsConfig::custom_roots([cert
            .cert
            .der()
            .clone()]))
        .net_report_config(
            iroh::endpoint::NetReportConfig::minimal()
                .keep_all_relays(true)
                .prefer_relay(
                    primary_url.clone(),
                    Duration::from_secs(1),
                    Duration::from_secs(3),
                ),
        )
        .bind()
        .await
        .unwrap();
    let target = EndpointAddr::new(endpoint.id())
        .with_relay_url(primary_url.clone())
        .with_relay_url(backup_url.clone());
    let receive = async {
        let conn = endpoint.accept().await.unwrap().await.unwrap();
        assert_eq!(conn.remote_id(), client.id());
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let bytes = recv.read_to_end(128).await.unwrap();
        send.write_all(&bytes).await.unwrap();
        send.finish().unwrap();
        let _ = send.stopped().await;
        conn
    };
    let send = async {
        let conn = connect_target(&client, target).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"parallel candidates").await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(128).await.unwrap(), b"parallel candidates");
        timeout(Duration::from_secs(15), async {
            while !conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        conn
    };
    let (server_conn, client_conn) = timeout(Duration::from_secs(25), async {
        tokio::join!(receive, send)
    })
    .await
    .unwrap();
    assert!(
        client_conn
            .paths()
            .iter()
            .any(|p| p.is_selected() && p.is_ip())
    );
    client.close().await;
    drop(server_conn);
    endpoint.close().await;
    worker.abort();
    let _ = worker.await;
    primary.shutdown().await.unwrap();
    backup.shutdown().await.unwrap();
}

#[tokio::test]
async fn parallel_relays_prefer_our_data_path_even_when_backup_is_faster() {
    use iroh::Watcher;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let primary = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let backup = relay(&cert, "127.0.0.1:0".parse().unwrap()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_url: iroh::RelayUrl = format!("https://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let backup_url: iroh::RelayUrl = format!("https://{}", backup.https_addr().unwrap())
        .parse()
        .unwrap();
    let upstream = primary.https_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (downstream, _) = accepted.unwrap();
                    workers.spawn(async move {
                        let upstream = tokio::net::TcpStream::connect(upstream).await.unwrap();
                        let (mut dr, mut dw) = downstream.into_split();
                        let (mut ur, mut uw) = upstream.into_split();
                        async fn delayed<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(r: &mut R, w: &mut W) {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let mut bytes = [0; 65536];
                            while let Ok(n) = r.read(&mut bytes).await {
                                if n == 0 { break; }
                                tokio::time::sleep(Duration::from_millis(25)).await;
                                if w.write_all(&bytes[..n]).await.is_err() { break; }
                            }
                            let _ = w.shutdown().await;
                        }
                        tokio::join!(delayed(&mut dr, &mut uw), delayed(&mut ur, &mut dw));
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
    }))
    .unwrap();
    let host_key = Identity::generate();
    let client_key = Identity::generate();
    host::initialize(state.path(), host_key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(state.path(), host_key.endpoint_id()).unwrap();
    store
        .host_mutate(
            host_key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: client_key.endpoint_id(),
                name: "phone".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let authority = Authority::open(state.path(), host_key.endpoint_id()).unwrap();
    let builder = |key: &SecretKey| {
        configured_builder(key, Some(&config))
            .unwrap()
            .net_report_config(
                iroh::endpoint::NetReportConfig::minimal()
                    .keep_all_relays(true)
                    .prefer_relay(
                        primary_url.clone(),
                        Duration::from_secs(1),
                        Duration::from_secs(4),
                    ),
            )
    };
    let host = TunnelHost::bind_builder(
        &config,
        Services::host(authority).unwrap(),
        builder(host_key.secret_key()),
        true,
    )
    .await
    .unwrap();
    let client = builder(client_key.secret_key()).bind().await.unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            if client
                .relay_connections()
                .iter()
                .filter(|s| s.is_connected())
                .count()
                == 2
                && host
                    .endpoint()
                    .relay_connections()
                    .iter()
                    .filter(|s| s.is_connected())
                    .count()
                    == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let target = peer_target(host_key.endpoint_id(), Some(&config), None).unwrap();
    let connection = control::connect_target(&client, target.clone())
        .await
        .unwrap();
    control::catalog_on(&connection, host_key.endpoint_id())
        .await
        .unwrap();
    let selection_result = timeout(Duration::from_secs(15), async {
        loop {
            let paths = connection.paths();
            let own = paths
                .iter()
                .find(|p| p.remote_addr() == &iroh::TransportAddr::Relay(primary_url.clone()));
            let other = paths
                .iter()
                .find(|p| p.remote_addr() == &iroh::TransportAddr::Relay(backup_url.clone()));
            if let (Some(own), Some(other)) = (own, other)
                && own.is_selected()
                && other.rtt() < own.rtt()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        selection_result.is_ok(),
        "homes={:?}; paths={:?}",
        client.home_relay_status().get(),
        connection
            .paths()
            .iter()
            .map(|p| (p.remote_addr().clone(), p.is_selected(), p.rtt()))
            .collect::<Vec<_>>()
    );
    assert_eq!(client.home_relay_status().get()[0].url(), &primary_url);
    // Abrupt loss of our relay still allows authenticated data through n0.
    connection.close(0u32.into(), b"primary failure fixture");
    proxy.abort();
    let _ = proxy.await;
    primary.shutdown().await.unwrap();
    let backup_connection = timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(c) = control::connect_target(&client, target.clone()).await
                && control::catalog_on(&c, host_key.endpoint_id())
                    .await
                    .is_ok()
                && c.paths().iter().any(|p| {
                    p.is_selected()
                        && p.remote_addr() == &iroh::TransportAddr::Relay(backup_url.clone())
                })
            {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(10), async {
        while client.home_relay_status().get()[0].url() != &backup_url {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let restored = relay(
        &cert,
        primary_url
            .to_string()
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .parse()
            .unwrap(),
    )
    .await;
    timeout(Duration::from_secs(20), async {
        loop {
            if client.home_relay_status().get()[0].url() == &primary_url
                && backup_connection.paths().iter().any(|p| {
                    p.is_selected()
                        && p.remote_addr() == &iroh::TransportAddr::Relay(primary_url.clone())
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    control::catalog_on(&backup_connection, host_key.endpoint_id())
        .await
        .unwrap();
    restored.shutdown().await.unwrap();
    client.close().await;
    host.shutdown().await;
    backup.shutdown().await.unwrap();
}

#[tokio::test]
async fn wildcard_host_binds_both_families_and_explicit_bind_stays_restricted() {
    // Require IPv6 on this test host rather than silently passing without testing it.
    let probe = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    drop(probe);
    for (address, expect_ipv6) in [("0.0.0.0:0", true), ("127.0.0.1:0", false)] {
        let config: TransportConfig = serde_json::from_value(serde_json::json!({
            "bind_addr": address
        }))
        .unwrap();
        let host = TunnelHost::bind(&SecretKey::generate(), &config, &[])
            .await
            .unwrap();
        let sockets = host.endpoint.bound_sockets();
        assert!(sockets.iter().any(|a| a.is_ipv4()));
        assert_eq!(sockets.iter().any(|a| a.is_ipv6()), expect_ipv6);
        if expect_ipv6 {
            // Exercise actual QUIC over IPv6, not just socket creation.
            let client = bind_client(&SecretKey::generate()).await.unwrap();
            let port = sockets.iter().find(|a| a.is_ipv6()).unwrap().port();
            let target = EndpointAddr::new(host.endpoint.id()).with_ip_addr(
                std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
            );
            let connection = timeout(
                Duration::from_secs(5),
                client.connect(target, protocol::ALPN),
            )
            .await
            .unwrap()
            .unwrap();
            connection.close(0u32.into(), b"test complete");
            client.close().await;
        }
        host.shutdown().await;
    }
}

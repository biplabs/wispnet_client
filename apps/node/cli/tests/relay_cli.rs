use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    time::timeout,
};
use wispnet_core::{config::TransportConfig, identity::Identity};
use wispnet_registry::host::{self, Authority, Mutation};

async fn command(config: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wispnet"))
        .arg(args[0])
        .arg("--config")
        .arg(config)
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).await.unwrap();
    drop(stdin);
    let output = timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn native_catalog_and_service_need_no_peer_address() {
    use irelay::server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig};
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
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
    let mut relay = RelayConfig::new("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap());
    relay.tls = Some(TlsConfig::new(
        "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
        CertConfig::Manual { server_config: tls },
    ));
    let mut server = ServerConfig::default();
    server.relay = Some(relay);
    let relay = Server::spawn(server).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ca = tmp.path().join("relay.pem");
    std::fs::write(&ca, cert.cert.pem()).unwrap();
    let relay_url = format!("https://{}", relay.https_addr().unwrap());
    let client_config = tmp.path().join("client.toml");
    std::fs::write(&client_config, format!("schema_version=1\ndevice_name='client'\nstate_dir='{}'\nlog_level='info'\n[transport]\nbind_addr='127.0.0.1:0'\nrelay_url='{relay_url}'\nrelay_only=true\nrelay_ca_file='{}'\n", tmp.path().join("client").display(), ca.display())).unwrap();
    let initialized = command(&client_config, &["identity-init"], b"").await;
    let peer = initialized
        .split("Endpoint ID: ")
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let host_state = tmp.path().join("host");
    std::fs::create_dir(&host_state).unwrap();
    std::fs::set_permissions(&host_state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = Identity::generate();
    host::initialize(&host_state, identity.endpoint_id()).unwrap();
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut store, owner) = host::owner_store(&host_state, identity.endpoint_id()).unwrap();
    store
        .host_mutate(
            identity.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer,
                name: "client".into(),
            },
        )
        .unwrap();
    store
        .host_mutate(
            identity.endpoint_id(),
            &owner,
            Mutation::Service {
                display_name: None,
                kind: None,
                expected_revision: 1,
                id: "echo".into(),
                backend: backend.local_addr().unwrap(),
                allow: vec![peer],
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let cfg = TransportConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: Some(relay_url),
        relay_only: false,
        relay_ca_file: Some(ca),
        max_connections: 8,
        max_streams: 8,
        max_streams_per_connection: 4,
    };
    let host = wispnet_agent::tunnel::TunnelHost::bind_host(
        identity.secret_key(),
        &cfg,
        Authority::open(&host_state, identity.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(10), host.endpoint().online())
        .await
        .unwrap();
    let pin = identity.endpoint_id().to_string();
    let hello = command(&client_config, &["peer-hello", "--peer", &pin], b"").await;
    let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(hello, serde_json::json!({"host": pin, "version": 1}));
    let invitation_path = tmp.path().join("invitation.json");
    let invitation = serde_json::json!({"version": 1, "host": pin, "relay_url": cfg.relay_url,
        "invitation": "a".repeat(64), "code": "b".repeat(64), "scope": "request_access",
        "expires_at": wispnet_registry::enrollment::now() + 120});
    std::fs::write(&invitation_path, serde_json::to_vec(&invitation).unwrap()).unwrap();
    std::fs::set_permissions(&invitation_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let checked = command(
        &client_config,
        &[
            "pairing-check",
            "--peer",
            &pin,
            "--invitation",
            invitation_path.to_str().unwrap(),
        ],
        b"",
    )
    .await;
    let checked: serde_json::Value = serde_json::from_str(&checked).unwrap();
    assert_eq!(
        checked,
        serde_json::json!({"host": pin, "version": 1,
        "state": "bootstrap_verified", "approval_checked": false, "capability_checked": false})
    );
    let (mut store, owner) = host::owner_store(&host_state, identity.endpoint_id()).unwrap();
    let mut issued = store
        .pairing_invite(
            identity.endpoint_id(),
            &owner,
            cfg.relay_url.as_deref().unwrap(),
            wispnet_registry::enrollment::now(),
            300,
        )
        .unwrap();
    issued.direct_hint = Some(host.endpoint().bound_sockets()[0]);
    std::fs::write(&invitation_path, serde_json::to_vec(&issued).unwrap()).unwrap();
    let pending = command(
        &client_config,
        &[
            "pairing-request",
            "--peer",
            &pin,
            "--invitation",
            invitation_path.to_str().unwrap(),
        ],
        b"",
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pending).unwrap()["state"],
        "pending_owner_approval"
    );
    let pin_path = tmp
        .path()
        .join("client/peer-pins")
        .join(format!("{pin}.json"));
    let persisted = std::fs::read_to_string(&pin_path).unwrap();
    assert!(!persisted.contains(&issued.code));
    assert!(!persisted.contains(&issued.invitation));
    assert_eq!(
        std::fs::metadata(&pin_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&invitation_path).unwrap();
    drop(store);
    drop(owner);
    // A fresh CLI process reuses the saved direct hint without an invitation or
    // explicit address. This local config explicitly disables the saved relay.
    let direct_config = tmp.path().join("direct.toml");
    std::fs::write(&direct_config, format!("schema_version=1\ndevice_name='client'\nstate_dir='{}'\nlog_level='info'\n[transport]\nbind_addr='127.0.0.1:0'\n",
        tmp.path().join("client").display())).unwrap();
    let direct = command(&direct_config, &["peer-probe", "--peer", &pin], b"").await;
    let direct: serde_json::Value = serde_json::from_str(&direct).unwrap();
    assert_eq!(direct["selected_path"], "direct");
    assert_eq!(direct["relay_status"], "disabled");
    let catalog = command(&client_config, &["host-catalog", "--peer", &pin], b"").await;
    let catalog: serde_json::Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["host"], pin);
    assert_eq!(catalog["services"][0]["id"], "echo");
    let echo = async {
        let (mut socket, _) = backend.accept().await.unwrap();
        let mut request = Vec::new();
        socket.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        socket.write_all(b"response").await.unwrap();
        socket.shutdown().await.unwrap();
    };
    let tunnel_args = ["tunnel", "--peer", &pin, "--service", "echo"];
    let (output, ()) = timeout(Duration::from_secs(10), async {
        tokio::join!(command(&client_config, &tunnel_args, b"request"), echo)
    })
    .await
    .unwrap();
    assert_eq!(output, "response");
    let direct_echo = async {
        let (mut socket, _) = backend.accept().await.unwrap();
        let mut request = Vec::new();
        socket.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"direct-request");
        socket.write_all(b"direct-response").await.unwrap();
        socket.shutdown().await.unwrap();
    };
    let (direct_output, ()) = timeout(Duration::from_secs(10), async {
        tokio::join!(
            command(&direct_config, &tunnel_args, b"direct-request"),
            direct_echo
        )
    })
    .await
    .unwrap();
    assert_eq!(direct_output, "direct-response");

    let probe = command(&client_config, &["peer-probe", "--peer", &pin], b"").await;
    let probe: serde_json::Value = serde_json::from_str(&probe).unwrap();
    assert_eq!(probe["selected_path"], "relay");
    assert_eq!(probe["relay_status"], "connected");
    assert_eq!(probe["generation"], catalog["generation"]);
    assert_eq!(probe["service_count"], 1);
    assert_eq!(probe["client"], peer.to_string());
    let mut watcher = tokio::process::Command::new(env!("CARGO_BIN_EXE_wispnet"))
        .args(["peer-watch", "--config"])
        .arg(&client_config)
        .args(["--peer", &pin])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watcher.stdout.take().unwrap()).lines();
    let first = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["ok"], true);
    assert_eq!(first["client"], probe["client"]);
    let status = tokio::process::Command::new("kill")
        .args(["-USR1", &watcher.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let changed = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let changed: serde_json::Value = serde_json::from_str(&changed).unwrap();
    assert_eq!(changed["ok"], true);
    assert_eq!(changed["client"], first["client"]);
    assert_eq!(changed["generation"], first["generation"]);
    assert_eq!(changed["revision"], first["revision"]);
    let (mut store, owner) = host::owner_store(&host_state, identity.endpoint_id()).unwrap();
    store
        .host_mutate(
            identity.endpoint_id(),
            &owner,
            Mutation::Revoke {
                expected_revision: 2,
                peer,
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let denied = timeout(Duration::from_secs(8), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let denied: serde_json::Value = serde_json::from_str(&denied).unwrap();
    assert_eq!(denied["ok"], false);
    assert!(denied.get("generation").is_none());
    assert!(denied.get("services").is_none());
    assert_eq!(denied["client"], probe["client"]);
    assert_eq!(denied["retry_after_ms"], 1000);
    for expected in [2000, 4000] {
        let started = std::time::Instant::now();
        let next = timeout(Duration::from_secs(8), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let next: serde_json::Value = serde_json::from_str(&next).unwrap();
        assert_eq!(next["ok"], false);
        assert_eq!(next["retry_after_ms"], expected);
        assert!(started.elapsed() >= Duration::from_millis(expected / 2 - 100));
    }
    // A network notification interrupts the four-second backoff immediately.
    let status = tokio::process::Command::new("kill")
        .args(["-USR1", &watcher.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let reset = timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reset: serde_json::Value = serde_json::from_str(&reset).unwrap();
    assert_eq!(reset["ok"], false);
    assert_eq!(reset["retry_after_ms"], 1000);
    assert_eq!(reset["client"], probe["client"]);
    // Recovery resets pacing without replacing the endpoint or identity.
    let (mut store, owner) = host::owner_store(&host_state, identity.endpoint_id()).unwrap();
    store
        .host_mutate(
            identity.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 3,
                peer,
                name: "reapproved-client".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let recovered = timeout(Duration::from_secs(8), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let recovered: serde_json::Value = serde_json::from_str(&recovered).unwrap();
    assert_eq!(recovered["ok"], true);
    assert_eq!(recovered["retry_after_ms"], 1000);
    assert_eq!(recovered["client"], probe["client"]);
    assert_eq!(recovered["generation"], probe["generation"]);
    // Approval does not restore the service grant removed by revocation.
    assert_eq!(recovered["service_count"], 0);
    let status = tokio::process::Command::new("kill")
        .args(["-TERM", &watcher.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert!(
        timeout(Duration::from_secs(2), watcher.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    // Graceful cancellation releases this installation's exclusive key lease.
    let initialized = command(&client_config, &["identity-init"], b"").await;
    assert!(initialized.contains(&peer.to_string()));
    host.shutdown().await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_direct_probe_reports_path_without_backend_or_relay() {
    let temporary = tempfile::tempdir().unwrap();
    let client_config = temporary.path().join("client.toml");
    std::fs::write(
        &client_config,
        format!(
            "schema_version=1\ndevice_name='direct-client'\nstate_dir='{}'\nlog_level='info'\n",
            temporary.path().join("client").display()
        ),
    )
    .unwrap();
    let initialized = command(&client_config, &["identity-init"], b"").await;
    let client = initialized
        .split("Endpoint ID: ")
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let state = temporary.path().join("host");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let key = Identity::generate();
    host::initialize(&state, key.endpoint_id()).unwrap();
    let (mut store, owner) = host::owner_store(&state, key.endpoint_id()).unwrap();
    store
        .host_mutate(
            key.endpoint_id(),
            &owner,
            Mutation::Approve {
                expected_revision: 0,
                peer: client,
                name: "direct-client".into(),
            },
        )
        .unwrap();
    drop(store);
    drop(owner);
    let config = TransportConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: None,
        relay_only: false,
        relay_ca_file: None,
        max_connections: 8,
        max_streams: 8,
        max_streams_per_connection: 4,
    };
    let host = wispnet_agent::tunnel::TunnelHost::bind_host(
        key.secret_key(),
        &config,
        Authority::open(&state, key.endpoint_id()).unwrap(),
    )
    .await
    .unwrap();
    let pin = key.endpoint_id().to_string();
    let address = host.endpoint().bound_sockets()[0].to_string();
    let outsider = Identity::generate();
    let endpoint = wispnet_agent::tunnel::bind_client(outsider.secret_key())
        .await
        .unwrap();
    let target =
        wispnet_agent::tunnel::peer_target(key.endpoint_id(), None, Some(address.parse().unwrap()))
            .unwrap();
    let mismatch =
        wispnet_agent::diagnostics::probe(&endpoint, outsider.endpoint_id(), target, false)
            .await
            .unwrap_err();
    assert_eq!(
        mismatch,
        wispnet_agent::tunnel::TunnelError::Transport("diagnostic target identity mismatch")
    );
    endpoint.close().await;
    let result = command(
        &client_config,
        &["peer-probe", "--peer", &pin, "--address", &address],
        b"",
    )
    .await;
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["selected_path"], "direct");
    assert_eq!(result["relay_status"], "disabled");
    assert_eq!(result["service_count"], 0);
    assert_eq!(result["host"], pin);
    assert!(result.get("services").is_none());
    host.shutdown().await;
}

use std::time::Duration;
use tokio::time::timeout;
use wispnet_agent::{pairing, tunnel};
use wispnet_core::identity::Identity;

#[tokio::test]
async fn hello_checks_transport_pin_before_request_and_rejects_false_responses() {
    let server_key = Identity::generate();
    let client_key = Identity::generate();
    let wrong = Identity::generate();
    let server = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(server_key.secret_key().clone())
        .alpns(vec![wispnet_core::pairing::ALPN.to_vec()])
        .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap();
    let client = tunnel::bind_client(client_key.secret_key()).await.unwrap();
    let target =
        iroh::EndpointAddr::new(server_key.endpoint_id()).with_ip_addr(server.bound_sockets()[0]);
    let (outgoing, incoming) = tokio::join!(
        client.connect(target.clone(), wispnet_core::pairing::ALPN),
        async { server.accept().await.unwrap().await.unwrap() }
    );
    let outgoing = outgoing.unwrap();
    assert!(
        pairing::hello_on(&outgoing, wrong.endpoint_id())
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(250), incoming.accept_bi())
            .await
            .is_err()
    );
    outgoing.close(0u32.into(), b"test complete");
    let responses = [
        serde_json::json!({"version": 1, "host": wrong.endpoint_id()}),
        serde_json::json!({"version": 2, "host": server_key.endpoint_id()}),
        serde_json::json!({"version": 1, "host": server_key.endpoint_id(), "approved": true}),
    ];
    for response in responses {
        let responder = async {
            let connection = server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let request = recv.read_to_end(260).await.unwrap();
            assert_eq!(&request[4..], br#"{"op":"hello"}"#);
            let bytes = serde_json::to_vec(&response).unwrap();
            send.write_all(&(bytes.len() as u32).to_be_bytes())
                .await
                .unwrap();
            send.write_all(&bytes).await.unwrap();
            send.finish().unwrap();
            let _ = send.stopped().await;
            connection.close(0u32.into(), b"test complete");
        };
        let (result, ()) = timeout(Duration::from_secs(3), async {
            tokio::join!(pairing::hello(&client, target.clone()), responder)
        })
        .await
        .unwrap();
        assert!(result.is_err());
    }
    client.close().await;
    server.close().await;
}

#[test]
fn invitation_files_and_targets_fail_closed_without_exposing_capabilities() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use wispnet_core::{config::TransportConfig, pairing::Invitation};
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("invite.json");
    let host = Identity::generate().endpoint_id();
    let code = "c".repeat(64);
    let value = serde_json::json!({"version": 1, "host": host, "relay_url": "https://relay.example",
        "invitation": "a".repeat(64), "code": code, "scope": "request_access", "expires_at": 200});
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let invitation = pairing::read_invitation(&path).unwrap();
    let mut config = TransportConfig {
        backup_relay_urls: Vec::new(),
        prefer_primary_relay: true,
        keep_backup_relays_warm: true,
        discovery_relay_url: None,
        relay_url: Some("https://relay.example/".into()),
        relay_only: true,
        relay_ca_file: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        max_connections: 8,
        max_streams: 8,
        max_streams_per_connection: 4,
    };
    assert!(pairing::invitation_target(&invitation, host, &config, 100).is_ok());
    assert!(
        pairing::invitation_target(
            &invitation,
            Identity::generate().endpoint_id(),
            &config,
            100
        )
        .is_err()
    );
    assert!(pairing::invitation_target(&invitation, host, &config, 200).is_err());
    config.relay_url = Some("https://different.example".into());
    assert!(pairing::invitation_target(&invitation, host, &config, 100).is_err());
    config.relay_url = None;
    assert!(pairing::invitation_target(&invitation, host, &config, 100).is_err());
    let link = temporary.path().join("symlink");
    symlink(&path, &link).unwrap();
    assert!(pairing::read_invitation(&link).is_err());
    let link = temporary.path().join("hardlink");
    std::fs::hard_link(&path, &link).unwrap();
    assert!(pairing::read_invitation(&path).is_err());
    std::fs::remove_file(link).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(pairing::read_invitation(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, [b'x'; 8193]).unwrap();
    assert!(pairing::read_invitation(&path).is_err());
    std::fs::write(&path, format!("{{\"code\":\"{code}\"}}")).unwrap();
    let error = match pairing::read_invitation(&path) {
        Ok(_) => panic!("malformed invitation accepted"),
        Err(error) => error,
    };
    assert!(!error.to_string().contains(&code));
    assert!(pairing::read_invitation(temporary.path()).is_err());
    assert!(pairing::read_invitation(std::path::Path::new("relative.json")).is_err());
    let fifo = temporary.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(pairing::read_invitation(&fifo).is_err());
    // No permissive unknown scope/version or legacy controller import.
    for field in ["version", "scope", "controller"] {
        let mut changed = value.clone();
        changed[field] = serde_json::json!("admin");
        assert!(serde_json::from_value::<Invitation>(changed).is_err());
    }
}

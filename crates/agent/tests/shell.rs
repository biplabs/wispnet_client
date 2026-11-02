#![cfg(unix)]
use std::{path::Path, time::Duration};
use wispnet_agent::tunnel::{self, TunnelHost};
use wispnet_core::{
    config::TransportConfig,
    identity::{EndpointId, Identity},
    protocol::Status,
};
use wispnet_registry::host::{self, Authority, Mutation};
use wispnet_shell::{ClientFrame, ServerFrame, read_server, write_client};
fn mutate(dir: &Path, host: EndpointId, op: Mutation) {
    let (mut store, owner) = host::owner_store(dir, host).unwrap();
    store.host_mutate(host, &owner, op).unwrap();
}
fn config() -> TransportConfig {
    TransportConfig {
        backup_relay_urls: vec![],
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
async fn attach(connection: &iroh::endpoint::Connection, inc: &str) -> tunnel::ServiceStream {
    let mut stream = tunnel::open_service_bound(connection, "shell", inc)
        .await
        .unwrap();
    write_client(
        &mut stream,
        &ClientFrame::Attach {
            session: "main".into(),
            cols: 80,
            rows: 24,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_server(&mut stream).await.unwrap(),
        Some(ServerFrame::Attached { .. })
    ));
    stream
}
async fn ping(stream: &mut tunnel::ServiceStream) {
    write_client(stream, &ClientFrame::Ping).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                read_server(stream).await.unwrap().unwrap(),
                ServerFrame::Pong
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn irohshell_grants_reconnect_revocation_and_incarnation_are_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = Identity::generate();
    let own = identity.endpoint_id();
    let allowed = Identity::generate();
    let denied = Identity::generate();
    host::initialize(dir.path(), own).unwrap();
    for (revision, peer) in [allowed.endpoint_id(), denied.endpoint_id()]
        .into_iter()
        .enumerate()
    {
        mutate(
            dir.path(),
            own,
            Mutation::Approve {
                expected_revision: revision as u64,
                peer,
                name: "client".into(),
            },
        );
    }
    let service = |revision, allow| Mutation::Shell {
        expected_revision: revision,
        allow,
    };
    mutate(dir.path(), own, service(2, vec![allowed.endpoint_id()]));
    let authority = Authority::open(dir.path(), own).unwrap();
    let incarnation = authority
        .services()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == wispnet_core::host::ServiceKind::Shell)
        .unwrap()
        .incarnation;
    let host = TunnelHost::bind_host(identity.secret_key(), &config(), authority.clone())
        .await
        .unwrap();
    let address = host.endpoint().bound_sockets()[0];
    let endpoint = tunnel::bind_client(allowed.secret_key()).await.unwrap();
    let connection = tunnel::connect(&endpoint, own, address).await.unwrap();
    let other = tunnel::bind_client(denied.secret_key()).await.unwrap();
    let other_connection = tunnel::connect(&other, own, address).await.unwrap();
    assert!(matches!(
        tunnel::open_service_bound(&other_connection, "shell", &incarnation).await,
        Err(tunnel::TunnelError::Status(Status::Denied))
    ));
    assert!(matches!(
        tunnel::open_service(&connection, "shell").await,
        Err(tunnel::TunnelError::Status(Status::Denied))
    ));
    let mut stream = attach(&connection, &incarnation).await;
    ping(&mut stream).await;
    write_client(&mut stream, &ClientFrame::Detach)
        .await
        .unwrap();
    drop(stream);
    let mut stream = attach(&connection, &incarnation).await;
    ping(&mut stream).await;
    mutate(dir.path(), own, service(3, vec![]));
    tokio::time::timeout(Duration::from_secs(4), async {
        while matches!(read_server(&mut stream).await, Ok(Some(_))) {}
    })
    .await
    .unwrap();
    drop(stream);
    mutate(dir.path(), own, service(4, vec![allowed.endpoint_id()]));
    let current = authority
        .services()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == "shell")
        .unwrap()
        .incarnation;
    assert_eq!(current, incarnation);
    assert!(matches!(
        tunnel::open_service_bound(&connection, "shell", &"ab".repeat(32)).await,
        Err(tunnel::TunnelError::Status(Status::Denied))
    ));
    let mut stream = attach(&connection, &current).await;
    ping(&mut stream).await;
    drop(stream);
    endpoint.close().await;
    other.close().await;
    host.shutdown().await;
}

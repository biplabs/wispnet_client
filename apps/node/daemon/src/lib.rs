//! Private local UI API sharing the host's existing identity lease and endpoint.
mod firewall;
pub mod invitation;
use iroh::Watcher;
use serde_json::{Value, json};
use std::{
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::PathBuf,
    pin::pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
pub use wispnet_client::access_settings::{save_enabled, saved_enabled};
use wispnet_core::{config::Config, identity::IdentityStore};
struct State {
    state_dir: PathBuf,
    device_name: String,
    client: Arc<wispnet_client::Client>,
    authority: wispnet_registry::host::Authority,
    counts: wispnet_agent::tunnel::ActiveCounts,
    relay_configured: bool,
    config: Config,
    access_lock: tokio::sync::Mutex<()>,
}
impl State {
    async fn request(&self, mut value: Value) -> Result<Value, String> {
        let op = value["op"].as_str().ok_or("Missing operation")?.to_owned();
        match op.as_str() {
            "enable" | "stop" => {
                let _lock = self.access_lock.lock().await;
                if op == "stop" {
                    // Persist the pause before asynchronously tearing down listeners.
                    save_enabled(&self.state_dir, false)?;
                    return self.client.command(&value).await;
                }
                let was_enabled = self.client.gateway.is_enabled();
                if value["if_unset"] == true && saved_enabled(&self.state_dir)?.is_some() {
                    return Ok(json!({"enabled": was_enabled}));
                }
                let response = self.client.command(&value).await?;
                if let Err(error) = save_enabled(&self.state_dir, true) {
                    if !was_enabled {
                        let _ = self.client.command(&json!({"op":"stop"})).await;
                    }
                    return Err(error);
                }
                Ok(response)
            }
            "invite_qr" => {
                let config = self.config.clone();
                tokio::task::spawn_blocking(move || {
                    invitation::issue(&config).map_err(|e| e.to_string())
                })
                .await
                .map_err(|_| "Cannot generate pairing QR".to_owned())?
            }
            "transport_status" => {
                let snapshot = self.authority.snapshot().await.map_err(|e| e.to_string())?;
                let mut peers = self.counts.peer_transport();
                if let Some(peers) = peers.as_object_mut() {
                    peers.retain(|peer, _| {
                        snapshot.clients.iter().any(|c| c.peer.to_string() == *peer)
                    });
                }
                let endpoint = &self.client.endpoint;
                let network = endpoint.net_report().get().map(|r| {
                    json!({
                        "udp_v4":r.udp_v4,"udp_v6":r.udp_v6,
                        "public_ipv4_discovered":r.global_v4.is_some_and(|a|wispnet_agent::diagnostics::public_ip((*a.ip()).into())),
                        "observed_ipv4_is_private":r.global_v4.map(|a|a.ip().is_private()),
                        "public_ipv6_discovered":r.global_v6.is_some_and(|a|wispnet_agent::diagnostics::public_ip((*a.ip()).into())),
                    })
                });
                let router_mapping =
                    wispnet_agent::diagnostics::router_mapping_report(endpoint).await;
                Ok(
                    json!({"host":endpoint.id(),"network":network,"router_mapping":router_mapping,
                    "local_ip_addresses":endpoint.addr().ip_addrs().count(),
                    "public_ip_addresses":endpoint.addr().ip_addrs().filter(|a|wispnet_agent::diagnostics::public_ip(a.ip())).count(),
                    "relays":wispnet_agent::diagnostics::transport_observations(endpoint)["relays"],
                    "discovery":wispnet_agent::diagnostics::transport_observations(endpoint)["discovery"],
                    "peers":peers}),
                )
            }
            "status" => {
                if let Err(error) = self.client.publications.reconcile(&self.client).await {
                    tracing::warn!("Public access reconciliation failed: {error}");
                }
                let snapshot = self.authority.snapshot().await.map_err(|e| e.to_string())?;
                let peers = self.counts.peers();
                let mut recent = self.counts.recent_peers();
                // A recent request is display metadata, never durable authority.
                recent
                    .retain(|peer, _| snapshot.clients.iter().any(|c| c.peer.to_string() == *peer));
                let mut pins = Vec::new();
                if let Ok(entries) = std::fs::read_dir(self.state_dir.join("peer-pins")) {
                    for entry in entries.take(129) {
                        let entry = entry.map_err(|_| "Cannot inspect peer pins")?;
                        let name = entry.file_name();
                        let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                            continue;
                        };
                        let Ok(id) = id.parse() else { continue };
                        if self.client.saved_host(id).await?.is_some() {
                            pins.push(id.to_string());
                        }
                    }
                }
                for record in self
                    .authority
                    .paired_hosts()
                    .await
                    .map_err(|e| e.to_string())?
                {
                    let id = record.host.to_string();
                    if !pins.contains(&id) {
                        pins.push(id);
                    }
                }
                if value["refresh_clients"] == true {
                    self.client.refresh_client_pairings(
                        snapshot
                            .clients
                            .iter()
                            .filter(|c| !pins.contains(&c.peer.to_string()))
                            .map(|c| c.peer)
                            .collect(),
                    );
                }
                let (invitations, host_name) = self.owner(None).await?;
                let mut telemetry = serde_json::Map::new();
                for (peer, (path, rtt_ms)) in self.client.telemetry_map() {
                    if pins.contains(&peer) {
                        telemetry.insert(peer, json!({"path": path, "rtt_ms": rtt_ms}));
                    }
                }
                for (peer, (path, rtt_ms)) in self.counts.peer_telemetry() {
                    if snapshot.clients.iter().any(|c| c.peer.to_string() == peer)
                        || peers.contains_key(&peer)
                    {
                        telemetry.insert(peer, json!({"path": path, "rtt_ms": rtt_ms}));
                    }
                }
                Ok(
                    json!({"api_version":1,"host":snapshot,"host_name":host_name,"connected_peers":peers,"recent_peers":recent,"peer_telemetry":telemetry,"presence_timeout_seconds":wispnet_agent::tunnel::PRESENCE_TIMEOUT.as_secs(),"transport_streams":self.counts.streams(),"invitations":invitations,"peer_pins":pins,"relay":wispnet_agent::diagnostics::relay_state(&self.client.endpoint,self.relay_configured),"access_enabled":self.client.gateway.is_enabled(),"public_access":self.client.publications.status(&self.client).await.unwrap_or_else(|e|json!({"error":e,"services":[]}))}),
                )
            }
            "apply_operation" => {
                let host = value["host"].as_str().ok_or("Missing target host")?;
                if host != self.client.endpoint.id().to_string() {
                    return Err("Wrong target host".into());
                }
                let id = value["id"]
                    .as_str()
                    .ok_or("Missing operation ID")?
                    .to_owned();
                let action = serde_json::from_value(value["action"].clone())
                    .map_err(|_| "Invalid combined action")?;
                let directory = self.state_dir.clone();
                let host = self.client.endpoint.id();
                let operation_id = id.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let (mut store, secret) =
                        wispnet_registry::host::owner_store(&directory, host)?;
                    store.host_apply_operation(host, &secret, &operation_id, action)
                })
                .await;
                use wispnet_registry::control::ControlError;
                let mut receipt = match outcome {
                    Ok(Ok(receipt)) => {
                        json!({"state":receipt.state,"revision":receipt.revision,"reason":receipt.reason})
                    }
                    Ok(Err(
                        error @ (ControlError::Invalid
                        | ControlError::Forbidden
                        | ControlError::Conflict
                        | ControlError::RateLimited),
                    )) => {
                        json!({"state":"rejected","revision":null,"reason":error.to_string()})
                    }
                    // A commit may have happened before an I/O/handler failure.
                    // Only a replayed durable receipt can resolve that uncertainty.
                    _ => {
                        json!({"state":"pending","revision":null,"reason":"Target storage unavailable or acknowledgement unconfirmed"})
                    }
                };
                receipt["host"] = json!(host.to_string());
                receipt["id"] = json!(id);
                Ok(receipt)
            }
            "mutate" => {
                let operation = value
                    .get("operation")
                    .ok_or("Missing owner operation")?
                    .clone();
                let revision = self.owner(Some(operation)).await?.0;
                Ok(json!({"revision":revision}))
            }
            "open" => {
                // Local routes use the node's configured computer name, even when
                // a caller omits the name or retains an old display label.
                if value["host"].as_str() == Some(&self.client.endpoint.id().to_string()) {
                    value["host_name"] = json!(self.device_name);
                }
                self.client.command(&value).await
            }
            "pair" => {
                value["name"] = json!(self.device_name);
                self.client.command(&value).await
            }
            "public_status"
            | "shell_open"
            | "shell_input"
            | "shell_resize"
            | "shell_poll"
            | "shell_close"
            | "file_put"
            | "file_list"
            | "file_get"
            | "file_stat"
            | "file_workspaces"
            | "file_status"
            | "file_cancel"
            | "configure_public"
            | "configure_public_gateway"
            | "gateway_settings"
            | "configure_access"
            | "resolve_link"
            | "resolve_service"
            | "unpair"
            | "preview_invitation"
            | "catalog"
            | "host_device_id"
            | "host_public_links"
            | "diagnose_connection"
            | "set_name"
            | "telemetry"
            | "access_status" => self.client.command(&value).await,
            _ => Err("Unsupported local operation".into()),
        }
    }
    async fn owner(&self, operation: Option<Value>) -> Result<(Value, String), String> {
        let host = self.client.endpoint.id();
        let directory = self.state_dir.clone();
        let name = self.device_name.clone();
        tokio::task::spawn_blocking(move || {
            let (mut store, secret) =
                wispnet_registry::host::owner_store(&directory, host).map_err(|e| e.to_string())?;
            let result = if let Some(operation) = operation {
                let op =
                    serde_json::from_value(operation).map_err(|_| "Invalid owner operation")?;
                json!(
                    store
                        .host_mutate(host, &secret, op)
                        .map_err(|e| e.to_string())?
                )
            } else {
                json!(
                    store
                        .pairing_list(host, &secret, wispnet_registry::enrollment::now())
                        .map_err(|e| e.to_string())?
                )
            };
            Ok((result, name))
        })
        .await
        .map_err(|_| "Local owner operation failed".to_owned())?
    }
}
fn request_timeout(value: &Value) -> std::time::Duration {
    // The shared path diagnostic can observe both ends for up to 55 seconds.
    // Caller-supplied timeout fields never extend ordinary owner operations.
    std::time::Duration::from_secs(if value["op"] == "diagnose_connection" {
        60
    } else {
        25
    })
}

const LOCAL_REQUEST_LIMIT: usize = 32;
const LOCAL_SHELL_LIMIT: usize = 32;

struct LocalRequests {
    // Track both classes for shutdown. Only their own permits limit admission.
    tasks: JoinSet<()>,
    requests: Arc<Semaphore>,
    shells: Arc<Semaphore>,
}

impl LocalRequests {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            requests: Arc::new(Semaphore::new(LOCAL_REQUEST_LIMIT)),
            shells: Arc::new(Semaphore::new(LOCAL_SHELL_LIMIT)),
        }
    }

    fn spawn<R, S>(
        &mut self,
        stream: UnixStream,
        request: impl FnOnce(Value) -> R + Send + 'static,
        shell: impl FnOnce(UnixStream, Value) -> S + Send + 'static,
    ) where
        R: Future<Output = Result<Value, String>> + Send + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        if let Ok(request_slot) = self.requests.clone().try_acquire_owned() {
            self.tasks.spawn(serve(
                stream,
                request_slot,
                self.shells.clone(),
                request,
                shell,
            ));
        }
    }
}

async fn accept_local<F>(accept: impl Fn() -> F) -> io::Result<(UnixStream, SocketAddr)>
where
    F: Future<Output = io::Result<(UnixStream, SocketAddr)>>,
{
    loop {
        let error = match accept().await {
            Ok(connection) => return Ok(connection),
            Err(error) => error,
        };
        let delay = match error.raw_os_error() {
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => 100,
            Some(libc::EINTR | libc::EAGAIN | libc::ECONNABORTED | libc::EPROTO) => 10,
            _ => return Err(error),
        };
        tracing::warn!(%error, "Local API accept failed, retrying");
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

async fn serve<R, S>(
    mut stream: UnixStream,
    request_slot: OwnedSemaphorePermit,
    shells: Arc<Semaphore>,
    request: impl FnOnce(Value) -> R,
    shell: impl FnOnce(UnixStream, Value) -> S,
) where
    R: Future<Output = Result<Value, String>>,
    S: Future<Output = ()>,
{
    let started = std::time::Instant::now();
    let parsed = tokio::time::timeout(std::time::Duration::from_secs(25), async {
        if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other("Wrong local user"));
        }
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while request.len() <= 16384 {
            if stream.read(&mut byte).await? == 0 {
                break;
            }
            if byte[0] == b'\n' {
                break;
            }
            request.push(byte[0]);
        }
        if request.len() > 16384 {
            Err(std::io::Error::other("Local request too large"))
        } else {
            serde_json::from_slice::<Value>(&request)
                .map_err(|_| std::io::Error::other("Invalid local request"))
        }
    })
    .await;
    let result = match parsed {
        Ok(Ok(value)) if value["op"] == "shell_stream" => match shells.try_acquire_owned() {
            Ok(_shell_slot) => {
                drop(request_slot);
                shell(stream, value).await;
                return;
            }
            Err(_) => Err("Local shell session limit reached".into()),
        },
        Ok(Ok(value)) => {
            let remaining = request_timeout(&value).saturating_sub(started.elapsed());
            tokio::time::timeout(remaining, request(value))
                .await
                .unwrap_or_else(|_| Err("Local operation timed out".into()))
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("Local request timed out".into()),
    };
    let response = match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    if let Ok(mut bytes) = serde_json::to_vec(&response) {
        bytes.push(b'\n');
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.write_all(&bytes)).await;
    }
}

/// Upgrade this owner-only connection to the native binary Shell protocol.
/// Optional frontends never read identity secrets or create another endpoint.
async fn serve_shell_stream(mut stream: UnixStream, state: Arc<State>, value: Value) {
    let opened = async {
        let host = value["host"]
            .as_str()
            .ok_or("Missing host pin")?
            .parse::<wispnet_core::identity::EndpointId>()
            .map_err(|_| "Invalid host pin")?;
        let native = state.client.open_shell_stream(host).await?;
        Ok::<_, String>((host, native))
    };
    let opened = tokio::time::timeout(std::time::Duration::from_secs(20), opened)
        .await
        .unwrap_or_else(|_| Err("Shell stream setup timed out".into()));
    let (response, native) = match opened {
        Ok((host, native)) => (json!({"ok":true,"result":{"host":host}}), Some(native)),
        Err(error) => (json!({"ok":false,"error":error}), None),
    };
    let mut bytes = serde_json::to_vec(&response).expect("JSON response");
    bytes.push(b'\n');
    if !matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.write_all(&bytes)).await,
        Ok(Ok(()))
    ) {
        return;
    }
    if let Some(mut native) = native {
        let (mut local_read, mut local_write) = stream.split();
        let (mut native_read, mut native_write) = tokio::io::split(&mut native);
        // Shell has explicit StdinEof. A closed owner socket cancels the whole
        // stream, including a process blocked on input or an idle output pipe.
        tokio::select! {
            _ = tokio::io::copy(&mut local_read, &mut native_write) => {},
            _ = tokio::io::copy(&mut native_read, &mut local_write) => {},
        }
    }
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub async fn run(
    config: &Config,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    if config.schema_version != 2 {
        return wispnet_agent::run(config, shutdown).await;
    }
    let store = wispnet_agent::identity::FileIdentityStore::new(&config.state_dir)?;
    let _lease = store.lease()?;
    let identity = store.load()?;
    let authority =
        wispnet_registry::host::Authority::open(&config.state_dir, identity.endpoint_id())?
            .with_device_id(&config.device_name);
    // Older installations kept outbound pins in private files rather than the policy DB.
    // Index their authenticated identities for deletion, without adding any clients/grants.
    if let Ok(entries) = std::fs::read_dir(config.state_dir.join("peer-pins")) {
        let (mut policy, owner) =
            wispnet_registry::host::owner_store(&config.state_dir, identity.endpoint_id())?;
        for entry in entries.take(129) {
            let entry = entry?;
            let name = entry.file_name();
            let Some(peer) = name
                .to_str()
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|s| s.parse().ok())
            else {
                continue;
            };
            if wispnet_agent::peers::load(&config.state_dir, peer)?.is_some() {
                policy.index_legacy_pin(identity.endpoint_id(), &owner, peer)?;
            }
        }
    }
    let transport = config.transport.as_ref().ok_or("Host transport required")?;
    let host = wispnet_agent::tunnel::TunnelHost::bind_host(
        identity.secret_key(),
        transport,
        authority.clone(),
    )
    .await?;
    let client = wispnet_client::Client::from_endpoint(
        host.endpoint().clone(),
        identity.secret_key().clone(),
        config.state_dir.clone(),
        transport.clone(),
        authority.clone(),
    );
    let restore_public = match saved_enabled(&config.state_dir) {
        Ok(Some(true)) => match client.restore_access().await {
            Ok(()) => false,
            Err(error) => {
                tracing::warn!("Saved access setting could not be restored: {error}");
                true
            }
        },
        Ok(Some(false)) => false,
        Ok(None) => true,
        Err(error) => {
            tracing::warn!("Saved access setting unavailable: {error}");
            false
        }
    };
    if restore_public && let Err(error) = client.publications.restore(client.clone()).await {
        tracing::warn!("Public services unavailable: {error}");
    }
    let path = config.state_dir.join("node.sock");
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err("Refusing to replace non-socket node API path".into());
        }
        if UnixStream::connect(&path).await.is_ok() {
            return Err("Node API already running".into());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let _socket = SocketPath(path);
    let state = Arc::new(State {
        state_dir: config.state_dir.clone(),
        device_name: config.device_name.clone(),
        client: client.clone(),
        authority,
        counts: host.counts(),
        relay_configured: transport.relay_url.is_some(),
        config: config.clone(),
        access_lock: tokio::sync::Mutex::new(()),
    });
    let mut firewall_reporter = firewall::Reporter::new();
    let mut firewall_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut local = LocalRequests::new();
    let accept = || listener.accept();
    // Keep any retry delay across firewall ticks and task completions.
    let mut accepted = pin!(accept_local(accept));
    tokio::pin!(shutdown);
    tracing::info!(component="agent",event="started",networking=true,mode="independent_host",endpoint_id=%host.endpoint().id(),bind_addr=%host.endpoint().bound_sockets().first().map(ToString::to_string).unwrap_or_else(||"relay-only".into()));
    let outcome = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            _ = firewall_tick.tick() => {
                if let Some(reporter) = &mut firewall_reporter
                    && reporter.update(host.endpoint()).is_err()
                {
                    tracing::warn!("Cannot publish local firewall socket report");
                }
            }
            _ = local.tasks.join_next(), if !local.tasks.is_empty() => {
                while local.tasks.try_join_next().is_some() {}
            },
            result = &mut accepted => {
                let stream = match result {
                    Ok((stream, _)) => stream,
                    Err(error) => break Err(error),
                };
                accepted.set(accept_local(accept));
                let control_state = state.clone();
                let shell_state = state.clone();
                local.spawn(
                    stream,
                    move |value| async move { control_state.request(value).await },
                    move |stream, value| serve_shell_stream(stream, shell_state, value),
                );
            }
        }
    };
    local.tasks.shutdown().await;
    client.publications.stop().await;
    client.gateway.stop().await;
    drop(state);
    drop(client);
    host.shutdown().await;
    tracing::info!(component = "agent", event = "stopped");
    outcome?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connection_diagnostics_get_the_full_probe_budget() {
        assert_eq!(
            request_timeout(&json!({"op":"diagnose_connection"})),
            std::time::Duration::from_secs(60),
        );
        for operation in ["status", "mutate", "transport_status", "unsupported"] {
            assert_eq!(
                request_timeout(&json!({"op":operation,"timeout":600})),
                std::time::Duration::from_secs(25),
            );
        }
    }

    #[tokio::test]
    async fn local_accept_recovers_and_propagates_permanent_errors() {
        use std::{cell::Cell, future::ready};

        let attempts = Cell::new(0);
        let started = tokio::time::Instant::now();
        let (mut client, server) = UnixStream::pair().unwrap();
        let next = Cell::new(Some(server));
        let (mut server, _) = tokio::time::timeout(
            Duration::from_secs(5),
            accept_local(|| {
                let attempt = attempts.get();
                attempts.set(attempt + 1);
                ready(match attempt {
                    0 => Err(io::Error::from_raw_os_error(libc::ECONNABORTED)),
                    1 => Err(io::Error::from_raw_os_error(libc::EMFILE)),
                    2 => {
                        let stream = next.take().unwrap();
                        let address = stream.peer_addr().unwrap();
                        Ok((stream, address))
                    }
                    _ => panic!("unexpected accept attempt"),
                })
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(attempts.get(), 3);
        assert!(started.elapsed() >= Duration::from_millis(100));
        client.write_all(b"x").await.unwrap();
        assert_eq!(server.read_u8().await.unwrap(), b'x');

        let error = accept_local(|| ready(Err(io::Error::from_raw_os_error(libc::EBADF))))
            .await
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    async fn local_request(local: &mut LocalRequests, op: &str) -> (UnixStream, Value) {
        let (mut client, server) = UnixStream::pair().unwrap();
        local.spawn(
            server,
            |value| async move { Ok(value) },
            |mut stream, _| async move {
                stream.write_all(b"{\"ok\":true}\n").await.unwrap();
                // Keep the session open until its client disconnects.
                let _ = stream.read_u8().await;
            },
        );
        client
            .write_all(format!("{}\n", json!({"op": op})).as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        use tokio::io::AsyncBufReadExt;
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::BufReader::new(&mut client).read_line(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        (client, serde_json::from_str(&response).unwrap())
    }

    #[tokio::test]
    async fn shell_capacity_leaves_local_management_available() {
        let mut local = LocalRequests::new();
        let mut clients = Vec::new();
        for _ in 0..LOCAL_SHELL_LIMIT {
            let (client, response) = local_request(&mut local, "shell_stream").await;
            assert_eq!(response["ok"], true);
            clients.push(client);
        }
        let (_, response) = local_request(&mut local, "shell_stream").await;
        assert_eq!(response["error"], "Local shell session limit reached");
        for op in ["status", "mutate", "stop"] {
            let (_, response) = local_request(&mut local, op).await;
            assert_eq!(response, json!({"ok": true, "result": {"op": op}}));
        }

        drop(clients.pop().unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            // Completed management tasks may still be queued ahead of the shell.
            while local.shells.available_permits() == 0 {
                local.tasks.join_next().await.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
        let (_replacement, response) = local_request(&mut local, "shell_stream").await;
        assert_eq!(response["ok"], true);
        local.tasks.shutdown().await;
        assert_eq!(local.shells.available_permits(), LOCAL_SHELL_LIMIT);
    }
}

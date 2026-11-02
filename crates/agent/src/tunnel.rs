//! Authenticated direct/explicit-relay service transport. Browser access protection belongs to #10.
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, presets},
};
use std::{
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::TcpStream,
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use wispnet_core::{
    config::{ServiceConfig, TransportConfig, validate_services},
    identity::{EndpointId, SecretKey},
    protocol::{self, Status},
};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const BACKEND_TIMEOUT: Duration = Duration::from_secs(3);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// UI activity indication only; never an authorization/session lease.
pub const PRESENCE_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_SESSION: Duration = Duration::from_secs(3600);
const BUFFER: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum TunnelError {
    Status(Status),
    Transport(&'static str),
}
impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => f.write_str(status.as_str()),
            Self::Transport(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for TunnelError {}

type PeerTransportHistory =
    std::collections::BTreeMap<String, (std::time::Instant, serde_json::Value)>;

pub type PeerTelemetry = std::collections::BTreeMap<String, (&'static str, Option<u64>)>;

#[derive(Clone, Default)]
pub struct ActiveCounts {
    seen: Arc<std::sync::Mutex<std::collections::BTreeMap<String, std::time::Instant>>>,
    connections: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
    peers: Arc<std::sync::Mutex<std::collections::BTreeMap<String, usize>>>,
    telemetry: Arc<std::sync::Mutex<PeerTelemetry>>,
    transport: Arc<std::sync::Mutex<PeerTransportHistory>>,
}
impl ActiveCounts {
    pub(crate) fn record_activity(&self, peer: EndpointId) {
        self.record_activity_at(peer, std::time::Instant::now());
    }
    pub(crate) fn record_telemetry(&self, peer: EndpointId, connection: &Connection) {
        let path = crate::diagnostics::selected_path(connection);
        let rtt = crate::diagnostics::selected_rtt_ms(connection);
        let mut telemetry = self.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        let peer = peer.to_string();
        if telemetry.len() >= 1024
            && !telemetry.contains_key(&peer)
            && let Some(first) = telemetry.keys().next().cloned()
        {
            telemetry.remove(&first);
        }
        telemetry.insert(peer.clone(), (path, rtt));
        drop(telemetry);
        let mut transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
        if transport.len() >= 1024
            && !transport.contains_key(&peer)
            && let Some(first) = transport.keys().next().cloned()
        {
            transport.remove(&first);
        }
        transport.insert(
            peer,
            (
                std::time::Instant::now(),
                crate::diagnostics::connection_counters(connection),
            ),
        );
    }
    /// Last authenticated incoming connection per peer, including closed RPCs.
    /// Counters belong to that connection; these are not endpoint totals.
    pub fn peer_transport(&self) -> serde_json::Value {
        let transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
        transport
            .iter()
            .map(|(peer, (at, counters))| {
                (
                    peer.clone(),
                    serde_json::json!({"age_ms":at.elapsed().as_millis(),"connection":counters}),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
    pub fn peer_telemetry(&self) -> PeerTelemetry {
        self.telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn record_activity_at(&self, peer: EndpointId, now: std::time::Instant) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, last| now.duration_since(*last) < PRESENCE_TIMEOUT);
        let peer = peer.to_string();
        if seen.len() >= 1024
            && !seen.contains_key(&peer)
            && let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, last)| **last)
                .map(|(peer, _)| peer.clone())
        {
            seen.remove(&oldest);
        }
        seen.entry(peer)
            .and_modify(|last| *last = (*last).max(now))
            .or_insert(now);
    }
    pub fn recent_peers(&self) -> std::collections::BTreeMap<String, u64> {
        self.recent_peers_at(std::time::Instant::now())
    }
    fn recent_peers_at(&self, now: std::time::Instant) -> std::collections::BTreeMap<String, u64> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, last| now.duration_since(*last) < PRESENCE_TIMEOUT);
        seen.iter()
            .map(|(peer, last)| (peer.clone(), now.duration_since(*last).as_secs()))
            .collect()
    }

    pub fn peers(&self) -> std::collections::BTreeMap<String, usize> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
    pub fn streams(&self) -> usize {
        self.streams.load(Ordering::Relaxed)
    }
}
struct CountGuard(Arc<AtomicUsize>);
impl CountGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for CountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
struct PeerGuard {
    counts: ActiveCounts,
    peer: String,
}
impl PeerGuard {
    fn new(counts: ActiveCounts, peer: EndpointId) -> Self {
        let peer = peer.to_string();
        *counts
            .peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(peer.clone())
            .or_default() += 1;
        Self { counts, peer }
    }
}
impl Drop for PeerGuard {
    fn drop(&mut self) {
        let mut peers = self.counts.peers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = peers.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}
struct CloseConnection(Connection);
impl Drop for CloseConnection {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"tunnel closed");
    }
}

// A 1 Gbit/s path at 200 ms RTT needs 25 MB in flight. These bounded
// windows leave room for bulk Files traffic and concurrent control streams.
fn quic_config(streams: usize) -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_concurrent_bidi_streams((streams as u32).into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window((64u32 * 1024 * 1024).into())
        .receive_window((128u32 * 1024 * 1024).into())
        .send_window(64 * 1024 * 1024)
        .build()
}

#[cfg(not(target_os = "android"))]
type ShellSessions = wispnet_shell::SessionManager;
#[cfg(target_os = "android")]
#[derive(Clone, Default)]
struct ShellSessions;

#[derive(Clone)]
enum Services {
    Static(Arc<Vec<ServiceConfig>>),
    Host(
        wispnet_registry::host::Authority,
        ShellSessions,
        wispnet_files::FileService,
    ),
}
impl Services {
    fn host(authority: wispnet_registry::host::Authority) -> Result<Self, TunnelError> {
        let files = wispnet_files::FileService::new(authority.state_dir().join("files"))
            .map_err(|_| TunnelError::Transport("cannot initialize file service"))?;
        Ok(Self::Host(authority, ShellSessions::default(), files))
    }
    async fn load(&self) -> Result<Vec<wispnet_registry::host::LocalService>, Status> {
        match self {
            Self::Static(services) => Ok(services
                .iter()
                .map(|s| wispnet_registry::host::LocalService {
                    id: s.id.clone(),
                    incarnation: String::new(),
                    display_name: s.id.clone(),
                    kind: wispnet_core::host::ServiceKind::Tcp,
                    backend: s.backend,
                    allow: s.allow.clone(),
                    enabled: true,
                })
                .collect()),
            Self::Host(host, _, _) => host.services().await.map_err(|_| Status::Denied),
        }
    }
    async fn pending(&self, peer: EndpointId) -> bool {
        match self {
            Self::Static(_) => false,
            Self::Host(host, _, _) => host.pairing_pending(peer).await.unwrap_or(false),
        }
    }
    async fn approved(&self, peer: EndpointId) -> bool {
        match self {
            Self::Static(services) => services.iter().any(|s| s.allow.contains(&peer)),
            Self::Host(host, _, _) => host.approved(peer).await,
        }
    }
}

pub struct TunnelHost {
    endpoint: Endpoint,
    stop: watch::Sender<bool>,
    services_enabled: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
    counts: ActiveCounts,
    files: Option<wispnet_files::FileService>,
}
impl TunnelHost {
    pub async fn bind(
        secret: &SecretKey,
        config: &TransportConfig,
        services: &[ServiceConfig],
    ) -> Result<Self, TunnelError> {
        config
            .validate()
            .map_err(|_| TunnelError::Transport("invalid transport limits"))?;
        validate_services(services)
            .map_err(|_| TunnelError::Transport("invalid host service configuration"))?;
        Self::bind_services(
            secret,
            config,
            Services::Static(Arc::new(services.to_vec())),
            true,
        )
        .await
    }
    pub async fn bind_host(
        secret: &SecretKey,
        config: &TransportConfig,
        authority: wispnet_registry::host::Authority,
    ) -> Result<Self, TunnelError> {
        config
            .validate()
            .map_err(|_| TunnelError::Transport("invalid transport limits"))?;
        if authority.host() != secret.public() {
            return Err(TunnelError::Transport("host authority identity mismatch"));
        }
        Self::bind_services(secret, config, Services::host(authority)?, true).await
    }
    pub async fn bind_host_with_files(
        secret: &SecretKey,
        config: &TransportConfig,
        authority: wispnet_registry::host::Authority,
        files: wispnet_files::FileService,
    ) -> Result<Self, TunnelError> {
        Self::bind_host_with_files_enabled(secret, config, authority, files, true).await
    }
    /// Bind an embedded host with its initial admission state already applied.
    pub async fn bind_host_with_files_enabled(
        secret: &SecretKey,
        config: &TransportConfig,
        authority: wispnet_registry::host::Authority,
        files: wispnet_files::FileService,
        enabled: bool,
    ) -> Result<Self, TunnelError> {
        config
            .validate()
            .map_err(|_| TunnelError::Transport("invalid transport limits"))?;
        if authority.host() != secret.public() {
            return Err(TunnelError::Transport("host authority identity mismatch"));
        }
        Self::bind_services(
            secret,
            config,
            Services::Host(authority, ShellSessions::default(), files),
            enabled,
        )
        .await
    }
    async fn bind_services(
        secret: &SecretKey,
        config: &TransportConfig,
        services: Services,
        enabled: bool,
    ) -> Result<Self, TunnelError> {
        Self::bind_builder(
            config,
            services,
            configured_builder(secret, Some(config))?,
            enabled,
        )
        .await
    }
    async fn bind_builder(
        config: &TransportConfig,
        services: Services,
        builder: iroh::endpoint::Builder,
        enabled: bool,
    ) -> Result<Self, TunnelError> {
        let mut alpns = vec![protocol::ALPN.to_vec()];
        if matches!(services, Services::Host(_, _, _)) {
            alpns.push(wispnet_core::host::ALPN.to_vec());
            alpns.push(wispnet_core::pairing::ALPN.to_vec());
        }
        let mut builder = builder.clear_ip_transports();
        if !config.relay_only {
            builder = builder
                .bind_addr(config.bind_addr)
                .map_err(|_| TunnelError::Transport("invalid transport bind address"))?;
            // A wildcard IPv4 bind means all interfaces. Add IPv6 too, but keep
            // explicit interface/loopback binds restricted to their chosen address.
            // IPv6 is optional so hosts without IPv6 support can still start.
            if config.bind_addr.ip().is_unspecified() && config.bind_addr.is_ipv4() {
                let ipv6 = std::net::SocketAddr::from((
                    std::net::Ipv6Addr::UNSPECIFIED,
                    config.bind_addr.port(),
                ));
                builder = builder
                    .bind_addr_with_opts(
                        ipv6,
                        iroh::endpoint::BindOpts::default().set_is_required(false),
                    )
                    .map_err(|_| TunnelError::Transport("invalid IPv6 transport bind address"))?;
            }
        }
        let endpoint = builder
            .alpns(alpns)
            .transport_config(quic_config(config.max_streams_per_connection))
            .bind()
            .await
            .map_err(|_| TunnelError::Transport("cannot bind Iroh endpoint"))?;
        let counts = ActiveCounts::default();
        let (stop, receiver) = watch::channel(false);
        let (services_enabled, service_state) = watch::channel(enabled);
        let files = match &services {
            Services::Host(_, _, files) => Some(files.clone()),
            _ => None,
        };
        if let Some(files) = &files {
            files.set_enabled(enabled);
        }
        let worker = tokio::spawn(serve(
            endpoint.clone(),
            config.clone(),
            services,
            counts.clone(),
            receiver,
            service_state,
        ));
        Ok(Self {
            endpoint,
            stop,
            services_enabled,
            worker: Some(worker),
            counts,
            files,
        })
    }
    pub fn set_files_enabled(&self, enabled: bool) {
        if let Some(files) = &self.files {
            files.set_enabled(enabled);
        }
    }
    /// Pause admission and terminate existing inbound streams without changing
    /// owner grants or closing the endpoint used by outbound client sessions.
    pub fn set_services_enabled(&self, enabled: bool) {
        self.set_files_enabled(enabled);
        self.services_enabled.send_if_modified(|current| {
            if *current == enabled {
                return false;
            }
            *current = enabled;
            true
        });
    }
    pub fn services_enabled(&self) -> bool {
        *self.services_enabled.borrow()
    }
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn counts(&self) -> ActiveCounts {
        self.counts.clone()
    }
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        self.endpoint.close().await;
        // Iroh keeps the UDP socket until the final Endpoint owner is dropped.
        // Drop our owner before yielding to finish canceled transport cleanup,
        // rather than returning while a queued task still holds the socket.
        drop(self);
        tokio::task::yield_now().await;
    }
}
impl Drop for TunnelHost {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

async fn serve(
    endpoint: Endpoint,
    config: TransportConfig,
    services: Services,
    counts: ActiveCounts,
    mut stop: watch::Receiver<bool>,
    mut services_enabled: watch::Receiver<bool>,
) {
    let connections = Arc::new(Semaphore::new(config.max_connections));
    let streams = Arc::new(Semaphore::new(config.max_streams));
    let mut tasks = JoinSet::new();
    let mut shell_policy_tick = tokio::time::interval(Duration::from_secs(1));
    let mut files_cleanup_tick = tokio::time::interval(Duration::from_secs(60));
    files_cleanup_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let cleanup_slot = Arc::new(Semaphore::new(1));
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = services_enabled.changed() => {
                // Cancel on either transition, including a rapid off/on pair
                // coalesced by watch, so no pre-pause connection survives.
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                #[cfg(not(target_os = "android"))]
                if !*services_enabled.borrow()
                    && let Services::Host(_, sessions, _) = &services {
                    sessions.retain_scopes(|_| false);
                }
            },
            _ = files_cleanup_tick.tick() => {
                if let Services::Host(_, _, files) = &services
                    && let Ok(permit) = cleanup_slot.clone().try_acquire_owned()
                {
                    let files = files.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        if files.cleanup_expired().await.is_err() {
                            tracing::warn!("Files retention cleanup could not complete");
                        }
                    });
                }
            },
            _ = shell_policy_tick.tick() => {
                #[cfg(not(target_os = "android"))]
                if let Services::Host(_, sessions, _) = &services {
                    // Disconnected phones no longer have a stream monitor. The
                    // host still owns their PTYs and rechecks durable authority.
                    let allowed = timeout(HANDSHAKE_TIMEOUT, services.load()).await;
                    let mut scopes = std::collections::HashSet::new();
                    if let Ok(Ok(current)) = allowed {
                        for service in current.into_iter().filter(|s| s.kind == wispnet_core::host::ServiceKind::Shell) {
                            for peer in service.allow { scopes.insert(format!("{}/{peer}", service.incarnation)); }
                        }
                    }
                    sessions.retain_scopes(|scope| scopes.contains(scope));
                }
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if !*services_enabled.borrow() { incoming.refuse(); continue; }
                let Ok(permit) = connections.clone().try_acquire_owned() else { incoming.refuse(); continue };
                let services = services.clone();
                let streams = streams.clone();
                let counts = counts.clone();
                let max = config.max_streams_per_connection;
                let diagnostics = crate::diagnostics::Context {
                    endpoint:endpoint.clone(), relay_only:config.relay_only,
                    relay_configured:config.relay_url.is_some(),
                };
                // Count before spawning so it includes in-flight handshakes.
                let guard = CountGuard::new(counts.connections.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let _guard = guard;
                    let Ok(Ok(connection)) = timeout(HANDSHAKE_TIMEOUT, incoming).await else { return };
                    let _close = CloseConnection(connection.clone());
                    // Only the completed transport handshake establishes peer authority.
                    let peer = connection.remote_id();
                    if connection.alpn() == wispnet_core::pairing::ALPN {
                        // Public bootstrap has no access to authority. One RPC,
                        // bounded idle/read time, and the same global budgets.
                        let _close = CloseConnection(connection.clone());
                        let Ok(Ok((send, recv))) = timeout(HANDSHAKE_TIMEOUT, connection.accept_bi()).await else { return };
                        let Ok(_stream_permit) = streams.try_acquire_owned() else { return };
                        let _stream_guard = CountGuard::new(counts.streams.clone());
                        if let Services::Host(authority, _, _) = services {
                            crate::pairing::serve_stream(send, recv, peer, authority).await;
                        }
                        return;
                    }
                    if !timeout(HANDSHAKE_TIMEOUT,services.approved(peer)).await.unwrap_or(false) {
                        let reason: &[u8] = if timeout(HANDSHAKE_TIMEOUT, services.pending(peer)).await.unwrap_or(false) {
                            b"awaiting approval"
                        } else {
                            b"access denied"
                        };
                        connection.close((Status::Denied as u32).into(), reason);
                        return;
                    }
                    let _peer = PeerGuard::new(counts.clone(), peer);
                    serve_connection(connection, peer, services, streams, max, counts, diagnostics).await;
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn serve_connection(
    connection: Connection,
    peer: EndpointId,
    services: Services,
    streams: Arc<Semaphore>,
    max: usize,
    counts: ActiveCounts,
    diagnostics: crate::diagnostics::Context,
) {
    let connection_control = connection.alpn() == wispnet_core::host::ALPN;
    let _close = CloseConnection(connection.clone());
    counts.record_telemetry(peer, &connection);
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = connection.closed() => break,
            _ = tokio::time::sleep(IDLE_TIMEOUT), if tasks.is_empty() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {
                counts.record_telemetry(peer, &connection);
            },
            stream = connection.accept_bi() => {
                let Ok((mut send, mut recv)) = stream else { break };
                counts.record_telemetry(peer, &connection);
                let permit = if tasks.len() < max { streams.clone().try_acquire_owned().ok() } else { None };
                let Some(permit) = permit else {
                    let _ = reply(&mut send, Status::Busy).await;
                    let _ = recv.stop(1u32.into());
                    continue;
                };
                let guard = CountGuard::new(counts.streams.clone());
                let services = services.clone();
                let counts = counts.clone();
                let diagnostics = diagnostics.clone();
                let observed_connection = connection.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _guard = guard;
                    if connection_control {
                        if let Services::Host(host, _, _)=services {crate::control::serve_stream(send,recv,peer,host,counts,diagnostics,observed_connection).await;}
                    } else {
                        serve_stream(send, recv, peer, &services, &counts).await;
                    }
                });
            }
        }
    }
    counts.record_telemetry(peer, &connection);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>, Status> {
    let mut length = [0u8; 2];
    recv.read_exact(&mut length)
        .await
        .map_err(|_| Status::Malformed)?;
    let length = u16::from_be_bytes(length) as usize;
    if length == 0 || length > protocol::MAX_FRAME {
        return Err(Status::Malformed);
    }
    let mut data = vec![0u8; length];
    recv.read_exact(&mut data)
        .await
        .map_err(|_| Status::Malformed)?;
    Ok(data)
}
async fn reply(send: &mut SendStream, status: Status) -> Result<(), TunnelError> {
    timeout(HANDSHAKE_TIMEOUT, async {
        send.write_all(&protocol::response(status))
            .await
            .map_err(|_| TunnelError::Transport("response stream closed"))?;
        send.finish()
            .map_err(|_| TunnelError::Transport("response stream closed"))
    })
    .await
    .map_err(|_| TunnelError::Status(Status::Timeout))?
}
async fn serve_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    peer: EndpointId,
    services: &Services,
    counts: &ActiveCounts,
) {
    let opened = async {
        let frame = read_frame(&mut recv).await?;
        let (id, incarnation) = protocol::decode_bound_request(&frame)?;
        let services = services.load().await?;
        let service = services
            .iter()
            .find(|service| service.id == id)
            .ok_or(Status::UnknownService)?;
        if (!service.incarnation.is_empty()
            && incarnation.as_deref() != Some(service.incarnation.as_str()))
            || (service.incarnation.is_empty() && incarnation.is_some())
        {
            return Err(Status::Denied);
        }
        if !service.allow.contains(&peer) {
            return Err(Status::Denied);
        }
        counts.record_activity(peer);
        // This is deliberately after parsing + authenticating + checking the host-local grant.
        let backend = if service.kind == wispnet_core::host::ServiceKind::Shell {
            #[cfg(target_os = "android")]
            return Err(Status::BackendUnavailable);
            #[cfg(not(target_os = "android"))]
            None
        } else if matches!(
            service.kind,
            wispnet_core::host::ServiceKind::Files
                | wispnet_core::host::ServiceKind::FileShare
                | wispnet_core::host::ServiceKind::FileInbox
        ) {
            None
        } else {
            Some(
                timeout(BACKEND_TIMEOUT, TcpStream::connect(service.backend))
                    .await
                    .map_err(|_| Status::BackendUnavailable)?
                    .map_err(|_| Status::BackendUnavailable)?,
            )
        };
        Ok((
            backend,
            id.to_owned(),
            service.backend,
            service.incarnation.clone(),
            service.kind,
        ))
    };
    let (backend, service_id, binding, incarnation, kind) =
        match timeout(HANDSHAKE_TIMEOUT, opened).await {
            Ok(Ok(backend)) => backend,
            outcome => {
                let status = match outcome {
                    Ok(Err(status)) => status,
                    _ => Status::Timeout,
                };
                let _ = reply(&mut send, status).await;
                let _ = recv.stop(1u32.into());
                return;
            }
        };
    if !matches!(
        timeout(
            HANDSHAKE_TIMEOUT,
            send.write_all(&protocol::response(Status::Ok)),
        )
        .await,
        Ok(Ok(()))
    ) {
        return;
    }
    let mut tunnel = ServiceStream::new(send, recv);
    // Every built-in stream has the same durable grant/incarnation guard
    // as TCP services. Revocation cancels the attached stream, not the daemon.
    let policy = async {
        if matches!(services, Services::Static(_)) {
            std::future::pending::<()>().await;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let allowed = timeout(std::time::Duration::from_secs(1), services.load()).await;
            if !matches!(allowed, Ok(Ok(ref current)) if current.iter().any(|s| s.id == service_id && s.incarnation == incarnation && s.kind == kind && s.backend == binding && s.allow.contains(&peer)))
            {
                break;
            }
        }
    };
    if kind == wispnet_core::host::ServiceKind::Shell {
        #[cfg(not(target_os = "android"))]
        if let Services::Host(_, sessions, _) = services {
            let scope = format!("{incarnation}/{peer}");
            tokio::select! {
                _ = timeout(MAX_SESSION, sessions.serve(tunnel, scope.clone())) => {},
                _ = policy => { sessions.close_scope(&scope); },
            }
        }
        counts.record_activity(peer);
        return;
    }
    if kind == wispnet_core::host::ServiceKind::FileShare {
        if let Services::Host(authority, _, files) = services {
            // Read the selected root and its grant from one current snapshot.
            // A concurrent root/grant edit must not bind the old handshake to
            // new contents before the periodic active-stream guard runs.
            let mut snapshot = match authority.snapshot().await {
                Ok(snapshot) => snapshot,
                _ => return,
            };
            if !snapshot.services.iter().any(|service| {
                service.id == service_id
                    && service.kind == kind
                    && service.incarnation == incarnation
                    && service.enabled
                    && service.allow.contains(&peer)
            }) {
                return;
            }
            let Some(root) = snapshot.file_shares.remove(&service_id) else {
                return;
            };
            let (reader, writer) = tokio::io::split(tunnel);
            let peer_id = peer.to_string();
            tokio::select! {
                biased;
                _ = policy => {},
                _ = files.serve_share(&peer_id, &incarnation, root, reader, writer) => {},
            }
        }
        counts.record_activity(peer);
        return;
    }
    if kind == wispnet_core::host::ServiceKind::FileInbox {
        if let Services::Host(authority, _, files) = services {
            let mut snapshot = match authority.snapshot().await {
                Ok(snapshot) => snapshot,
                _ => return,
            };
            if !snapshot.services.iter().any(|service| {
                service.id == service_id
                    && service.kind == kind
                    && service.incarnation == incarnation
                    && service.enabled
                    && service.allow.contains(&peer)
            }) {
                return;
            }
            let Some(root) = snapshot.file_inboxes.remove(&service_id) else {
                return;
            };
            let exports = std::collections::BTreeMap::from([("incoming".into(), root)]);
            let (reader, writer) = tokio::io::split(tunnel);
            let peer_id = peer.to_string();
            tokio::select! {
                biased;
                _ = policy => {},
                _ = files.serve(&peer_id,&incarnation,exports,reader,writer) => {},
            }
        }
        counts.record_activity(peer);
        return;
    }
    if kind == wispnet_core::host::ServiceKind::Files {
        if let Services::Host(authority, _, files) = services {
            let exports = match authority.file_exports().await {
                Ok(exports) => exports,
                Err(_) => return,
            };
            let (reader, writer) = tokio::io::split(tunnel);
            let peer_id = peer.to_string();
            // Long transfers are bounded by activity and permissions, not the
            // interactive Shell session lifetime. A disconnect retains progress.
            tokio::select! {
                biased;
                _ = policy => {},
                _ = files.serve(&peer_id, &incarnation, exports, reader, writer) => {},
            }
        }
        counts.record_activity(peer);
        return;
    }
    let Some(backend) = backend else { return };
    let (mut reader, mut writer) = backend.into_split();
    let (mut tunnel_read, mut tunnel_write) = tokio::io::split(&mut tunnel);
    let activity = AtomicUsize::new(0);
    let transfer = timeout(MAX_SESSION, async {
        tokio::try_join!(
            pump(&mut tunnel_read, &mut writer, &activity, IDLE_TIMEOUT),
            pump(&mut reader, &mut tunnel_write, &activity, IDLE_TIMEOUT)
        )
    });
    tokio::select! { _ = transfer => {}, _ = policy => {} }
    counts.record_activity(peer);
}

/// Fixed per-direction buffer. Read idleness is shared across the session so
/// one-way streams survive while either direction makes progress. Slow writes
/// still have their own timeout to bound backpressure.
/// EOF half-closes the destination and permits the opposite direction to finish.
async fn pump(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    activity: &AtomicUsize,
    idle: Duration,
) -> io::Result<()> {
    let mut buffer = [0u8; BUFFER];
    loop {
        let observed = activity.load(Ordering::Relaxed);
        let n = match timeout(idle, reader.read(&mut buffer)).await {
            Ok(result) => result?,
            Err(_) if activity.load(Ordering::Relaxed) != observed => continue,
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "tunnel idle")),
        };
        if n == 0 {
            return timeout(idle, writer.shutdown()).await.map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "tunnel half-close timeout")
            })?;
        }
        activity.fetch_add(1, Ordering::Relaxed);
        timeout(idle, writer.write_all(&buffer[..n]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "tunnel slow writer"))??;
        activity.fetch_add(1, Ordering::Relaxed);
    }
}

/// Dropping an unfinished stream cancels both directions. A clean EOF/shutdown
/// preserves QUIC half-close semantics and does not reset a completed direction.
pub struct ServiceStream {
    send: SendStream,
    recv: RecvStream,
    send_finished: bool,
    recv_finished: bool,
    completion: Option<Pin<Box<dyn Future<Output = io::Result<()>> + Send>>>,
}
impl ServiceStream {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send,
            recv,
            send_finished: false,
            recv_finished: false,
            completion: None,
        }
    }
}
impl AsyncRead for ServiceStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let capacity = buf.remaining();
        let result = Pin::new(&mut self.recv).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && capacity > 0 && buf.filled().len() == before {
            self.recv_finished = true;
        }
        result
    }
}
impl AsyncWrite for ServiceStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.send_finished {
            return Poll::Ready(Ok(()));
        }
        if self.completion.is_none() {
            if self.send.finish().is_err() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tunnel send closed",
                )));
            }
            let stopped = self.send.stopped();
            self.completion = Some(Box::pin(async move {
                match stopped.await {
                    Ok(None) => Ok(()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "tunnel send not acknowledged",
                    )),
                }
            }));
        }
        let result = self
            .completion
            .as_mut()
            .expect("completion was just initialized")
            .as_mut()
            .poll(cx);
        if result.is_ready() {
            self.completion = None;
        }
        if matches!(result, Poll::Ready(Ok(()))) {
            self.send_finished = true;
        }
        result
    }
}
impl Drop for ServiceStream {
    fn drop(&mut self) {
        if !self.send_finished {
            let _ = self.send.reset(1u32.into());
        }
        if !self.recv_finished {
            let _ = self.recv.stop(1u32.into());
        }
    }
}

fn configured_relay_mode(config: &TransportConfig) -> Result<Option<iroh::RelayMode>, TunnelError> {
    config
        .relay_url
        .as_ref()
        .map(|url| {
            let relay = url
                .parse()
                .map_err(|_| TunnelError::Transport("invalid relay URL"))?;
            let mut relays: Vec<iroh::RelayUrl> = vec![relay];
            for backup in &config.backup_relay_urls {
                relays.push(
                    backup
                        .parse()
                        .map_err(|_| TunnelError::Transport("invalid backup relay URL"))?,
                );
            }
            Ok(iroh::RelayMode::custom(relays))
        })
        .transpose()
}

fn configured_discovery_relay(
    config: &TransportConfig,
) -> Result<Option<iroh::RelayConfig>, TunnelError> {
    config
        .discovery_relay_url
        .as_ref()
        .map(|url| {
            let relay: iroh::RelayUrl = url
                .parse()
                .map_err(|_| TunnelError::Transport("invalid discovery relay URL"))?;
            Ok(iroh::RelayConfig::from(relay))
        })
        .transpose()
}

fn configured_builder(
    secret: &SecretKey,
    config: Option<&TransportConfig>,
) -> Result<iroh::endpoint::Builder, TunnelError> {
    // Android native CLI has no JVM. getaddrinfo delegates to Android's OS
    // resolver without JNI or library-selected public DNS fallback. It also
    // works in the future app without installing a second DNS policy.
    #[cfg(target_os = "android")]
    let resolver = crate::os_dns::resolver();
    #[cfg(not(target_os = "android"))]
    let resolver = iroh::dns::DnsResolver::builder()
        .with_system_defaults()
        .disable_fallback()
        .build();
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret.clone())
        .dns_resolver(resolver);
    if let Some(config) = config {
        config
            .validate()
            .map_err(|_| TunnelError::Transport("invalid relay configuration"))?;
        if let Some(mode) = configured_relay_mode(config)? {
            builder = builder.relay_mode(mode);
        }
        let mut reports = iroh::endpoint::NetReportConfig::default();
        if let Some(observer) = configured_discovery_relay(config)? {
            reports = reports.quic_discovery_fallback(observer);
        }
        if !config.backup_relay_urls.is_empty() {
            reports = reports.keep_all_relays(config.keep_backup_relays_warm);
            if config.prefer_primary_relay {
                reports = reports.prefer_relay(
                    config
                        .relay_url
                        .as_ref()
                        .expect("validated primary")
                        .parse()
                        .map_err(|_| TunnelError::Transport("invalid relay URL"))?,
                    Duration::from_secs(5),
                    Duration::from_secs(30),
                );
            }
        }
        builder = builder.net_report_config(reports);
        if let Some(path) = &config.relay_ca_file {
            use rustls_pki_types::pem::PemObject;
            use std::io::Read;
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|_| TunnelError::Transport("cannot read relay trust root"))?;
            let meta = file
                .metadata()
                .map_err(|_| TunnelError::Transport("cannot inspect relay trust root"))?;
            if !meta.is_file()
                || meta.len() > 16384
                || meta.nlink() != 1
                || meta.mode() & 0o022 != 0
                || (meta.uid() != 0 && meta.uid() != unsafe { libc::geteuid() })
            {
                return Err(TunnelError::Transport("unsafe relay trust root file"));
            }
            let mut bytes = Vec::new();
            file.take(16385)
                .read_to_end(&mut bytes)
                .map_err(|_| TunnelError::Transport("cannot read relay trust root"))?;
            let cert = rustls_pki_types::CertificateDer::from_pem_slice(&bytes)
                .map_err(|_| TunnelError::Transport("invalid relay trust root"))?;
            builder = builder.ca_tls_config(iroh_relay::tls::CaTlsConfig::custom_roots([cert]));
        }
        if config.relay_only {
            builder = builder.clear_ip_transports();
        }
    }
    Ok(builder)
}

/// A pinned peer plus explicit relay and optional direct hint. No lookup service.
pub fn peer_target(
    peer: EndpointId,
    config: Option<&TransportConfig>,
    address: Option<std::net::SocketAddr>,
) -> Result<EndpointAddr, TunnelError> {
    if let Some(config) = config {
        config
            .validate()
            .map_err(|_| TunnelError::Transport("invalid relay configuration"))?;
    }
    let mut target = EndpointAddr::new(peer);
    if let Some(url) = config.and_then(|c| c.relay_url.as_ref()) {
        target = target.with_relay_url(
            url.parse()
                .map_err(|_| TunnelError::Transport("invalid relay URL"))?,
        );
    }
    if let Some(config) = config {
        for url in &config.backup_relay_urls {
            target = target.with_relay_url(
                url.parse()
                    .map_err(|_| TunnelError::Transport("invalid backup relay URL"))?,
            );
        }
    }
    if let Some(address) = address {
        target = target.with_ip_addr(address);
    }
    if address.is_none() && config.and_then(|c| c.relay_url.as_ref()).is_none() {
        return Err(TunnelError::Transport(
            "target requires configured relay or direct address",
        ));
    }
    Ok(target)
}
pub async fn bind_client(secret: &SecretKey) -> Result<Endpoint, TunnelError> {
    bind_configured_client(secret, None).await
}
pub async fn bind_configured_client(
    secret: &SecretKey,
    config: Option<&TransportConfig>,
) -> Result<Endpoint, TunnelError> {
    configured_builder(secret, config)?
        .transport_config(quic_config(8))
        .bind()
        .await
        .map_err(|_| TunnelError::Transport("cannot bind client endpoint"))
}
/// Only fixed diagnostics are exposed; relay-supplied errors may contain untrusted text.
pub(crate) fn connection_failure(endpoint: &Endpoint, fallback: &'static str) -> TunnelError {
    use iroh::Watcher;
    let relays = endpoint.home_relay_status().get();
    if relays.iter().any(|r| r.auth_denied_reason().is_some()) {
        TunnelError::Transport("configured relay denied endpoint admission")
    } else if !relays.is_empty() && !relays.iter().any(|r| r.is_connected()) {
        TunnelError::Transport("configured relay unavailable; direct peer path also failed")
    } else {
        TunnelError::Transport(fallback)
    }
}
pub async fn connect_target(
    endpoint: &Endpoint,
    target: EndpointAddr,
) -> Result<Connection, TunnelError> {
    timeout(HANDSHAKE_TIMEOUT, endpoint.connect(target, protocol::ALPN))
        .await
        .map_err(|_| connection_failure(endpoint, "service peer connection timeout"))?
        .map_err(|_| {
            connection_failure(
                endpoint,
                "service peer unavailable or identity/ALPN mismatch",
            )
        })
}
pub async fn connect(
    endpoint: &Endpoint,
    peer: EndpointId,
    address: std::net::SocketAddr,
) -> Result<Connection, TunnelError> {
    timeout(
        HANDSHAKE_TIMEOUT,
        endpoint.connect(
            EndpointAddr::new(peer).with_ip_addr(address),
            protocol::ALPN,
        ),
    )
    .await
    .map_err(|_| TunnelError::Status(Status::Timeout))?
    .map_err(|_| TunnelError::Transport("peer unavailable or identity/ALPN mismatch"))
}
pub async fn open_service(connection: &Connection, id: &str) -> Result<ServiceStream, TunnelError> {
    open_request(
        connection,
        protocol::request(id).map_err(TunnelError::Status)?,
    )
    .await
}
pub async fn open_service_bound(
    connection: &Connection,
    id: &str,
    incarnation: &str,
) -> Result<ServiceStream, TunnelError> {
    open_request(
        connection,
        protocol::request_bound(id, incarnation).map_err(TunnelError::Status)?,
    )
    .await
}
async fn open_request(
    connection: &Connection,
    request: Vec<u8>,
) -> Result<ServiceStream, TunnelError> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|_| TunnelError::Transport("peer closed the connection"))?;
        send.write_all(&request)
            .await
            .map_err(|_| TunnelError::Transport("request stream closed"))?;
        let payload = read_frame(&mut recv).await.map_err(TunnelError::Status)?;
        let status = protocol::decode_response(&payload).map_err(TunnelError::Status)?;
        if status != Status::Ok {
            return Err(TunnelError::Status(status));
        }
        Ok(ServiceStream::new(send, recv))
    })
    .await
    .map_err(|_| TunnelError::Status(Status::Timeout))?
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod relay_tests;

#[cfg(test)]
#[path = "relay_policy_tests.rs"]
mod relay_policy_tests;

#[cfg(test)]
mod idle_tests {
    use super::*;
    #[test]
    fn recent_activity_refreshes_and_expires_independently_of_live_connections() {
        let counts = ActiveCounts::default();
        let peer = SecretKey::generate().public();
        assert!(counts.recent_peers().is_empty());
        counts.record_activity(peer);
        let first = counts.seen.lock().unwrap()[&peer.to_string()];
        assert_eq!(
            counts.recent_peers_at(first + Duration::from_secs(29))[&peer.to_string()],
            29
        );
        assert!(counts.peers().is_empty());
        // Refresh the timestamp with a later real request; old expiry must not win.
        let later = first + Duration::from_secs(30);
        counts.record_activity_at(peer, later);
        // Concurrent requests may obtain the map lock out of timestamp order.
        counts.record_activity_at(peer, first + Duration::from_secs(10));
        assert!(
            counts
                .recent_peers_at(first + PRESENCE_TIMEOUT)
                .contains_key(&peer.to_string())
        );
        assert!(counts.recent_peers_at(later + PRESENCE_TIMEOUT).is_empty());
    }

    #[tokio::test]
    async fn one_way_progress_keeps_silent_direction_alive_but_total_idle_still_ends() {
        let (mut client, tunnel) = tokio::io::duplex(64);
        let (backend, mut server) = tokio::io::duplex(64);
        let transfer = tokio::spawn(async move {
            let (mut up_read, mut down_write) = tokio::io::split(tunnel);
            let (mut down_read, mut up_write) = tokio::io::split(backend);
            let activity = AtomicUsize::new(0);
            let idle = Duration::from_millis(50);
            tokio::try_join!(
                pump(&mut up_read, &mut up_write, &activity, idle),
                pump(&mut down_read, &mut down_write, &activity, idle)
            )
        });
        for _ in 0..25 {
            server.write_all(b"x").await.unwrap();
            let mut byte = [0];
            client.read_exact(&mut byte).await.unwrap();
            assert_eq!(&byte, b"x");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = timeout(Duration::from_millis(200), transfer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}

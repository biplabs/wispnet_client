//! Shared peer browser client. One identity/endpoint shared by pairing, catalogs and gateway.
pub mod access_settings;
mod connections;
pub mod files;
pub mod gateway;
mod mcp;
mod owner;
pub mod public_access;
mod service_registry;
mod shell;
mod shell_exec;
use iroh::Endpoint;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::runtime::Runtime;
use wispnet_core::{
    config::TransportConfig,
    identity::{EndpointId, SecretKey},
};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);
fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("native runtime")
    })
}
pub struct Client {
    connections: connections::Pool,
    shells: shell::Shells,
    files: files::Files,
    pub endpoint: Endpoint,
    key: SecretKey,
    config: TransportConfig,
    state: PathBuf,
    pub gateway: gateway::Gateways,
    pub publications: public_access::Publications,
    pub(crate) local_authority: Option<wispnet_registry::host::Authority>,
    telemetry: Mutex<wispnet_agent::tunnel::PeerTelemetry>,
    _host: Option<wispnet_agent::tunnel::TunnelHost>,
    client_checks: Mutex<std::collections::BTreeMap<String, std::time::Instant>>,
    check_slots: Arc<tokio::sync::Semaphore>,
}
fn token() -> String {
    SecretKey::generate()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn argument<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing app argument".into())
}
fn peer(value: &Value) -> Result<EndpointId, String> {
    argument(value, "host")?
        .parse()
        .map_err(|_| "Invalid host pin".into())
}
impl Client {
    async fn service_host_name(&self, host: EndpointId) -> Result<String, String> {
        if host == self.endpoint.id() {
            return self
                .local_authority
                .as_ref()
                .map(|a| a.device_id())
                .ok_or("Local host unavailable".into());
        }
        if self.saved_host(host).await?.is_none() {
            return Err("Pair with this host first".into());
        }
        let connection = self
            .peer_connection(host, connections::Protocol::Control)
            .await?;
        let result = wispnet_agent::control::device_id_on(&connection, host).await;
        result.map_err(|e| e.to_string())
    }
    pub(crate) async fn open_route(self: &Arc<Self>, value: &Value) -> Result<String, String> {
        let host = peer(value)?;
        let id = argument(value, "service")?;
        let incarnation = argument(value, "incarnation")?;
        if !wispnet_core::enrollment::valid_code(incarnation) {
            return Err("Refresh this service before opening".into());
        }
        if host == self.endpoint.id()
            && let Some(authority) = &self.local_authority
        {
            // Only the owner-only node API constructs a local authority client.
            // Local owner access needs neither a self-pin nor a self-grant.
            let services = authority.services().await.map_err(|e| e.to_string())?;
            if !services.iter().any(|s| {
                s.id == id
                    && s.incarnation == incarnation
                    && s.kind == wispnet_core::host::ServiceKind::Http
            }) {
                return Err("Service no longer permitted".into());
            }
        } else {
            if self.saved_host(host).await?.is_none() {
                return Err("Pair with this host first".into());
            }
            let catalog = self.catalog(host).await?;
            if !catalog["services"].as_array().is_some_and(|s| {
                s.iter().any(|s| {
                    s["id"] == id && s["incarnation"] == incarnation && s["kind"] == "http"
                })
            }) {
                return Err("Service no longer permitted".into());
            }
        }
        let host_name = self.service_host_name(host).await?;
        let url = self
            .gateway
            .open_named(
                self.clone(),
                host,
                id.to_owned(),
                incarnation.to_owned(),
                Some(&host_name),
            )
            .await?;
        Ok(url)
    }
    pub fn from_endpoint(
        endpoint: Endpoint,
        key: SecretKey,
        state: PathBuf,
        config: TransportConfig,
        authority: wispnet_registry::host::Authority,
    ) -> Arc<Self> {
        Arc::new(Self {
            connections: connections::Pool::default(),
            shells: shell::Shells::default(),
            files: files::Files::default(),
            endpoint,
            key,
            state,
            config,
            gateway: gateway::Gateways::default(),
            publications: public_access::Publications::new(),
            local_authority: Some(authority),
            telemetry: Mutex::new(std::collections::BTreeMap::new()),
            _host: None,
            client_checks: Mutex::new(std::collections::BTreeMap::new()),
            check_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    pub async fn new(
        key: SecretKey,
        state: PathBuf,
        config: TransportConfig,
    ) -> Result<Arc<Self>, String> {
        config
            .validate()
            .map_err(|_| "Invalid relay configuration")?;
        let endpoint = wispnet_agent::tunnel::bind_configured_client(&key, Some(&config))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            connections: connections::Pool::default(),
            shells: shell::Shells::default(),
            files: files::Files::default(),
            endpoint,
            key,
            config,
            state,
            gateway: gateway::Gateways::default(),
            publications: public_access::Publications::new(),
            local_authority: None,
            telemetry: Mutex::new(std::collections::BTreeMap::new()),
            _host: None,
            client_checks: Mutex::new(std::collections::BTreeMap::new()),
            check_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        }))
    }
    /// Android shares its existing protected identity with a status/catalog endpoint.
    /// Files receiving is configured explicitly by the local Android owner.
    pub async fn new_peer(
        key: SecretKey,
        state: PathBuf,
        config: TransportConfig,
    ) -> Result<Arc<Self>, String> {
        Self::new_peer_with_publisher(key, state, config, None).await
    }
    pub async fn new_peer_with_publisher(
        key: SecretKey,
        state: PathBuf,
        config: TransportConfig,
        publisher: Option<Arc<dyn wispnet_files::Publisher>>,
    ) -> Result<Arc<Self>, String> {
        let directory = state.clone();
        let own = key.public();
        let authority = tokio::task::spawn_blocking(move || {
            wispnet_registry::host::initialize(&directory, own).map_err(|e| e.to_string())?;
            if let Ok(entries) = std::fs::read_dir(directory.join("peer-pins")) {
                let (mut store, owner) = wispnet_registry::host::owner_store(&directory, own)
                    .map_err(|e| e.to_string())?;
                for entry in entries.take(129) {
                    let entry = entry.map_err(|_| "Cannot inspect saved hosts")?;
                    let name = entry.file_name();
                    let Some(peer) = name
                        .to_str()
                        .and_then(|s| s.strip_suffix(".json"))
                        .and_then(|s| s.parse().ok())
                    else {
                        continue;
                    };
                    if wispnet_agent::peers::load(&directory, peer)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        store
                            .index_legacy_pin(own, &owner, peer)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            wispnet_registry::host::Authority::open(&directory, own).map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "Cannot prepare pairing state")??;
        let platform_storage = publisher.is_some();
        let files = wispnet_files::FileService::with_publisher(
            state.join("files"),
            wispnet_files::Limits::default(),
            publisher,
        )
        .map_err(|e| e.to_string())?;
        files.set_enabled(!platform_storage);
        let client_files = files::Files::with_local(files.clone());
        let host = wispnet_agent::tunnel::TunnelHost::bind_host_with_files_enabled(
            &key,
            &config,
            authority.clone(),
            files,
            !platform_storage,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            connections: connections::Pool::default(),
            shells: shell::Shells::default(),
            files: client_files,
            endpoint: host.endpoint().clone(),
            key,
            config,
            state,
            gateway: gateway::Gateways::default(),
            publications: public_access::Publications::new(),
            local_authority: Some(authority),
            telemetry: Mutex::new(std::collections::BTreeMap::new()),
            _host: Some(host),
            client_checks: Mutex::new(std::collections::BTreeMap::new()),
            check_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        }))
    }
    /// Embedded hosts follow the local access switch. Daemon-owned endpoints
    /// remain admitted until their owning runtime stops them.
    pub fn services_enabled(&self) -> bool {
        self._host
            .as_ref()
            .is_none_or(|host| host.services_enabled())
    }
    /// Contact client-only legacy devices while their directory is being refreshed.
    /// No queue, no daemon timer, and at most four probes plus one attempt per 30s.
    pub fn refresh_client_pairings(self: &Arc<Self>, peers: Vec<EndpointId>) {
        let now = std::time::Instant::now();
        let mut checks = self.client_checks.lock().unwrap_or_else(|e| e.into_inner());
        checks.retain(|peer, _| peers.iter().any(|p| p.to_string() == *peer));
        for peer in peers {
            if checks
                .get(&peer.to_string())
                .is_some_and(|last| now.duration_since(*last) < std::time::Duration::from_secs(30))
            {
                continue;
            }
            let Ok(permit) = self.check_slots.clone().try_acquire_owned() else {
                break;
            };
            checks.insert(peer.to_string(), now);
            let client = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = client.check_pairing(peer).await;
            });
        }
    }
    pub fn record_telemetry(&self, peer: EndpointId, connection: &iroh::endpoint::Connection) {
        let path = wispnet_agent::diagnostics::selected_path(connection);
        let rtt = wispnet_agent::diagnostics::selected_rtt_ms(connection);
        let mut map = self.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        let key = peer.to_string();
        if map.len() >= 1024
            && !map.contains_key(&key)
            && let Some(first) = map.keys().next().cloned()
        {
            map.remove(&first);
        }
        map.insert(key, (path, rtt));
    }
    pub fn telemetry_map(&self) -> wispnet_agent::tunnel::PeerTelemetry {
        self.telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub async fn catalog(&self, host: EndpointId) -> Result<Value, String> {
        self.check_pairing(host).await?;
        let state = self.state.clone();
        let epoch =
            tokio::task::spawn_blocking(move || wispnet_agent::catalogs::begin(&state, host))
                .await
                .map_err(|_| "Catalog baseline task failed")??;
        let connection = self
            .peer_connection(host, connections::Protocol::Control)
            .await?;
        let result = wispnet_agent::control::catalog_on(&connection, host)
            .await
            .map_err(|e| e.to_string());
        self.record_telemetry(host, &connection);
        let catalog = result?;
        let state = self.state.clone();
        let accepted = catalog.clone();
        tokio::task::spawn_blocking(move || {
            wispnet_agent::catalogs::accept(&state, host, &epoch, &accepted)
        })
        .await
        .map_err(|_| "Catalog baseline task failed")??;
        serde_json::to_value(catalog).map_err(|_| "Invalid catalog".into())
    }
    pub async fn saved_host(
        &self,
        host: EndpointId,
    ) -> Result<Option<wispnet_core::pairing::Bootstrap>, String> {
        if let Some(authority) = &self.local_authority
            && let Some(record) = authority
                .paired_hosts()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|b| b.host == host)
        {
            return Ok(Some(record));
        }
        if let Some(authority) = &self.local_authority
            && authority
                .pairing_blocked(host)
                .await
                .map_err(|e| e.to_string())?
        {
            return Ok(None);
        }
        wispnet_agent::peers::load(&self.state, host).map_err(|_| "Invalid saved host".into())
    }
    pub async fn target(&self, host: EndpointId) -> Result<iroh::EndpointAddr, String> {
        wispnet_agent::tunnel::peer_target(
            host,
            Some(&self.config),
            self.saved_host(host).await?.and_then(|p| p.direct_hint),
        )
        .map_err(|e| e.to_string())
    }
    async fn peer_connection(
        &self,
        host: EndpointId,
        protocol: connections::Protocol,
    ) -> Result<connections::Lease, String> {
        if self.saved_host(host).await?.is_none() {
            self.connections.forget(Some(host));
            return Err("Pair with this host first".into());
        }
        self.connections
            .get(
                &self.endpoint,
                host,
                protocol,
                self.config.max_connections,
                self.config.max_streams_per_connection,
                self.target(host),
            )
            .await
    }
    async fn check_pairing(&self, host: EndpointId) -> Result<(), String> {
        let Some(authority) = &self.local_authority else {
            return Ok(());
        };
        let Some(pairing) = authority
            .pairing_session(host)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        let target = self.target(host).await?;
        // Older peers and transport failures do not prove removal. Normal catalog
        // authorization still applies, including when no status can be obtained.
        let Ok(status) =
            wispnet_agent::pairing::pairing_status(&self.endpoint, target, &pairing).await
        else {
            return Ok(());
        };
        if status.state != wispnet_core::pairing::PairingState::Removed {
            return Ok(());
        }
        let state = self.state.clone();
        let own = self.endpoint.id();
        let removed = tokio::task::spawn_blocking(move || {
            let (mut store, owner) =
                wispnet_registry::host::owner_store(&state, own).map_err(|e| e.to_string())?;
            store
                .apply_remote_removal(own, &owner, host, &pairing)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "Cannot apply remote removal")??;
        if removed {
            self.connections.forget(Some(host));
            self.shells.close_host(host).await;
            self.files.cancel_host(Some(host));
            self.gateway.forget(self, host).await?;
            return Err("Pairing removed by remote machine".into());
        }
        Err("Pairing changed during refresh; try again".into())
    }
    pub async fn command(self: &Arc<Self>, value: &Value) -> Result<Value, String> {
        match argument(value, "op")? {
            "local_owner_status" | "local_owner_mutate" | "local_owner_invite"
            | "local_owner_apply" => owner::command(self, value).await,
            "file_workspaces" | "file_put" | "file_status" | "file_cancel" | "file_list"
            | "file_get" | "file_stat" => self.files_command(value).await,
            "shell_open" => self.shell_open(value).await,
            "shell_input" | "shell_resize" | "shell_poll" | "shell_close" => {
                self.shells.command(value).await
            }
            "local_files_status" => {
                let snapshot = self
                    .local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?
                    .snapshot()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(
                    json!({"revision":snapshot.revision,"clients":snapshot.clients,"exports":snapshot.file_exports,
                    "allow":snapshot.services.iter().find(|s| s.id == "files").map(|s| &s.allow),
                    "service":snapshot.services.iter().find(|s| s.id == "files")}),
                )
            }
            "local_shares_status" => {
                let snapshot = self
                    .local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?
                    .snapshot()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(
                    json!({"revision": snapshot.revision, "clients": snapshot.clients,
                    "shares": snapshot.services.iter().filter(|service| service.kind == wispnet_core::host::ServiceKind::FileShare).collect::<Vec<_>>(),
                    "roots": snapshot.file_shares}),
                )
            }
            "local_inboxes_status" => {
                let snapshot = self
                    .local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?
                    .snapshot()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(
                    json!({"revision":snapshot.revision,"clients":snapshot.clients,
                    "inboxes":snapshot.services.iter().filter(|service| service.kind == wispnet_core::host::ServiceKind::FileInbox).collect::<Vec<_>>(),
                    "roots":snapshot.file_inboxes,"legacy_exports":snapshot.file_exports,
                    "legacy_service":snapshot.services.iter().find(|service|service.id=="files")}),
                )
            }
            "local_files_configure"
            | "local_service_enabled"
            | "local_share_configure"
            | "local_share_remove"
            | "local_inbox_configure"
            | "local_inbox_remove" => {
                let directory = self.state.clone();
                let own = self.endpoint.id();
                let target_kind = match value["op"].as_str() {
                    Some("local_share_remove") => Some(wispnet_core::host::ServiceKind::FileShare),
                    Some("local_inbox_remove") => Some(wispnet_core::host::ServiceKind::FileInbox),
                    Some("local_share_configure") if value.get("new_id").is_some() => {
                        Some(wispnet_core::host::ServiceKind::FileShare)
                    }
                    Some("local_inbox_configure") if value.get("new_id").is_some() => {
                        Some(wispnet_core::host::ServiceKind::FileInbox)
                    }
                    _ => None,
                };
                let target_id = target_kind
                    .map(|_| argument(value, "id").map(str::to_owned))
                    .transpose()?;
                let operation = if value["op"] == "local_service_enabled" {
                    json!({"op":"set_service_enabled", "expected_revision":value["expected_revision"],
                        "id":value["id"], "incarnation":value["incarnation"], "enabled":value["enabled"]})
                } else if value["op"] == "local_share_configure" {
                    if value.get("new_id").is_some() {
                        json!({"op":"edit_folder", "expected_revision":value["expected_revision"],
                            "id":value["id"],"new_id":value["new_id"],"root":value["root"],"allow":value["allow"]})
                    } else {
                        json!({"op":"file_share", "expected_revision":value["expected_revision"],
                            "id":value["id"],"root":value["root"],"allow":value["allow"]})
                    }
                } else if value["op"] == "local_share_remove" {
                    json!({"op":"remove", "expected_revision":value["expected_revision"], "id":value["id"]})
                } else if value["op"] == "local_inbox_configure" {
                    if value.get("new_id").is_some() {
                        json!({"op":"edit_folder", "expected_revision":value["expected_revision"],
                            "id":value["id"],"new_id":value["new_id"],"root":value["root"],"allow":value["allow"]})
                    } else {
                        json!({"op":"file_inbox", "expected_revision":value["expected_revision"],
                            "id":value["id"],"root":value["root"],"allow":value["allow"]})
                    }
                } else if value["op"] == "local_inbox_remove" {
                    json!({"op":"remove", "expected_revision":value["expected_revision"], "id":value["id"]})
                } else {
                    json!({"op":"files","expected_revision":value["expected_revision"],"exports":value["exports"],"allow":value["allow"]})
                };
                let operation: wispnet_registry::host::Mutation = serde_json::from_value(operation)
                    .map_err(|_| "Invalid local service settings")?;
                let revision = tokio::task::spawn_blocking(move || {
                    let (mut store, owner) = wispnet_registry::host::owner_store(&directory, own)
                        .map_err(|e| e.to_string())?;
                    if let Some(id) = target_id
                        && !store
                            .host_snapshot(own)
                            .map_err(|e| e.to_string())?
                            .services
                            .iter()
                            .any(|service| service.id == id && Some(service.kind) == target_kind)
                    {
                        return Err("Read-only share unavailable".into());
                    }
                    store
                        .host_mutate(own, &owner, operation)
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|_| "Cannot save Files settings")??;
                Ok(json!({"revision":revision}))
            }
            "identity" => Ok(json!({"peer":self.endpoint.id().to_string()})),
            "pairing_state" => {
                let host = peer(value)?;
                let paired = self.saved_host(host).await?.is_some();
                let removed = if let Some(authority) = &self.local_authority {
                    !paired
                        && authority
                            .pairing_blocked(host)
                            .await
                            .map_err(|e| e.to_string())?
                } else {
                    false
                };
                Ok(json!({"paired":paired,"removed":removed}))
            }
            "preview_invitation" => {
                let invitation = wispnet_agent::pairing::decode_invitation(
                    argument(value, "invitation")?.as_bytes(),
                )
                .map_err(|_| "Invalid invitation")?;
                if !invitation.valid(wispnet_registry::enrollment::now()) {
                    return Err("Expired or invalid invitation".into());
                }
                Ok(
                    json!({"host":invitation.host.to_string(),"host_name":invitation.host_name.as_deref().unwrap_or(""),"relay_url":invitation.relay_url,"expires_at":invitation.expires_at}),
                )
            }
            "pair" => {
                let host = peer(value)?;
                let text = argument(value, "invitation")?;
                if text.len() > 8192 {
                    return Err("Invitation too large".into());
                }
                let invitation: wispnet_core::pairing::Invitation =
                    wispnet_agent::pairing::decode_invitation(text.as_bytes())
                        .map_err(|_| "Invalid invitation")?;
                let bootstrap = invitation
                    .bootstrap(host, wispnet_registry::enrollment::now())
                    .ok_or("Expired invitation or wrong host pin")?;
                let reverse =
                    self.local_authority
                        .as_ref()
                        .map(|_| wispnet_core::pairing::Bootstrap {
                            version: wispnet_core::pairing::VERSION,
                            host: self.endpoint.id(),
                            relay_url: bootstrap.relay_url.clone(),
                            direct_hint: self
                                .endpoint
                                .bound_sockets()
                                .into_iter()
                                .find(|a| !a.ip().is_unspecified()),
                        });
                let session = invitation.invitation.clone();
                let pending = wispnet_agent::pairing::request_enrollment_with_bootstrap(
                    &self.endpoint,
                    host,
                    &invitation,
                    &self.config,
                    &self.key,
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Wispnet Android"),
                    reverse,
                )
                .await
                .map_err(|e| e.to_string())?;
                if self.local_authority.is_some() {
                    let state = self.state.clone();
                    let own = self.endpoint.id();
                    let name = invitation
                        .host_name
                        .clone()
                        .unwrap_or_else(|| host.to_string());
                    tokio::task::spawn_blocking(move || {
                        let (mut store, owner) = wispnet_registry::host::owner_store(&state, own)
                            .map_err(|e| e.to_string())?;
                        store
                            .pairing_accept_session(own, &owner, &bootstrap, &name, Some(&session))
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|_| "Cannot save mutual pairing")??;
                } else {
                    wispnet_agent::peers::save(&self.state, &bootstrap)
                        .map_err(|_| "Cannot save host pin")?;
                }
                let state = self.state.clone();
                tokio::task::spawn_blocking(move || wispnet_agent::catalogs::reset(&state, host))
                    .await
                    .map_err(|_| "Cannot reset catalog baseline")??;
                Ok(json!({"state":"pending_owner_approval","host":pending.host.to_string()}))
            }
            "unpair" => {
                let host = peer(value)?;
                if host == self.endpoint.id() {
                    return Err("Cannot unpair this machine".into());
                }
                let saved = self.saved_host(host).await?;
                if let Some(authority) = &self.local_authority {
                    let known = authority
                        .snapshot()
                        .await
                        .map_err(|e| e.to_string())?
                        .clients
                        .iter()
                        .any(|c| c.peer == host);
                    let bootstrap = saved.or_else(|| {
                        if !known {
                            return None;
                        }
                        self.config.relay_url.clone().map(|relay_url| {
                            wispnet_core::pairing::Bootstrap {
                                version: wispnet_core::pairing::VERSION,
                                host,
                                relay_url,
                                direct_hint: None,
                            }
                        })
                    });
                    let state = self.state.clone();
                    let own = self.endpoint.id();
                    let expected = value
                        .get("expected_revision")
                        .map(|v| v.as_u64().ok_or("Invalid policy revision"))
                        .transpose()?;
                    tokio::task::spawn_blocking(move || {
                        let (mut store, owner) = wispnet_registry::host::owner_store(&state, own)
                            .map_err(|e| e.to_string())?;
                        store
                            .delete_pairing(own, &owner, host, bootstrap.as_ref(), expected)
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|_| "Cannot remove pairing")??;
                }
                self.connections.forget(Some(host));
                self.shells.close_host(host).await;
                self.files.cancel_host(Some(host));
                self.gateway.forget(self, host).await?;
                wispnet_agent::peers::remove(&self.state, host).map_err(|e| e.to_string())?;
                Ok(json!({"unpaired":host.to_string()}))
            }
            "set_device_id" => {
                let id = argument(value, "device_id")?;
                self.local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?
                    .set_device_id(id)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"device_id":id}))
            }
            "host_device_id" => {
                Ok(json!({"device_id":self.service_host_name(peer(value)?).await?}))
            }
            "set_name" => {
                let host = peer(value)?;
                let name = argument(value, "name")?;
                if !wispnet_core::enrollment::valid_name(name) {
                    return Err("Invalid device name".into());
                }
                if self.saved_host(host).await?.is_none() {
                    return Err("Pair with this host first".into());
                }
                let connection = self
                    .peer_connection(host, connections::Protocol::Control)
                    .await?;
                let result = wispnet_agent::control::set_name_on(&connection, host, name).await;
                result.map_err(|e| e.to_string())?;
                Ok(json!({"name":name}))
            }
            "catalog" => {
                let host = peer(value)?;
                if self.saved_host(host).await?.is_none() {
                    return Err("Pair with this host first".into());
                }
                self.catalog(host).await
            }
            "host_public_links" => {
                let host = peer(value)?;
                if self.saved_host(host).await?.is_none() {
                    return Err("Pair with this host first".into());
                }
                let connection = self
                    .peer_connection(host, connections::Protocol::Control)
                    .await?;
                let result = wispnet_agent::control::public_links_on(&connection, host).await;
                serde_json::to_value(result.map_err(|e| e.to_string())?)
                    .map_err(|_| "Invalid public links".into())
            }
            "diagnose_connection" => {
                let host = peer(value)?;
                if self.saved_host(host).await?.is_none() {
                    return Err("Pair with this host first".into());
                }
                let target = self.target(host).await?;
                let mut report = tokio::time::timeout(
                    std::time::Duration::from_secs(55),
                    wispnet_agent::diagnostics::explain_path(
                        &self.endpoint,
                        host,
                        target,
                        self.config.relay_only,
                        self.config.relay_url.is_some(),
                    ),
                )
                .await
                .map_err(|_| "Connection diagnostic timed out after 55 seconds")?
                .map_err(|e| e.to_string())?;
                report["transport"] =
                    wispnet_agent::diagnostics::transport_observations(&self.endpoint);
                Ok(report)
            }
            "relay_status" => Ok(
                json!({"relay":wispnet_agent::diagnostics::relay_state(&self.endpoint,self.config.relay_url.is_some())}),
            ),
            "telemetry" => {
                let peers: serde_json::Map<String, Value> = self
                    .telemetry_map()
                    .into_iter()
                    .map(|(k, (path, rtt_ms))| (k, json!({"path": path, "rtt_ms": rtt_ms})))
                    .collect();
                Ok(json!({"peers": peers}))
            }
            "public_status" => self.publications.status(self).await,
            "configure_public" | "configure_public_gateway" => {
                self.publications.configure(self.clone(), value).await
            }
            "gateway_settings" => self.gateway.settings(self).await,
            "configure_access" => {
                let settings = serde_json::from_value(
                    value.get("settings").ok_or("Missing settings")?.clone(),
                )
                .map_err(|_| "Invalid access settings")?;
                self.gateway.configure(self.clone(), settings).await
            }
            "resolve_service" => self.resolve_service(value).await,
            "resolve_link" => {
                let link =
                    wispnet_core::service_link::ServiceLink::parse(argument(value, "link")?)?;
                self.resolve_service(&json!({"host":link.host,"service":link.service}))
                    .await
            }
            "access_status" => Ok(json!({"enabled":self.gateway.is_enabled()})),
            "enable" => {
                let _guard = self.gateway.settings_lock.lock().await;
                let discover = !self.gateway.is_enabled();
                self.gateway.enable();
                if let Err(error) = self.gateway.restore(self.clone()).await {
                    if let Some(host) = &self._host {
                        host.set_services_enabled(false);
                    }
                    self.publications.stop().await;
                    self.gateway.stop().await;
                    return Err(error);
                }
                if discover {
                    // Populate permitted routes so a copied HTTP link also works
                    // before the user opens its tile on this device.
                    let _ = self.service_list().await;
                }
                if let Some(host) = &self._host {
                    host.set_services_enabled(true);
                }
                if discover && let Err(error) = self.publications.restore(self.clone()).await {
                    if let Some(host) = &self._host {
                        host.set_services_enabled(false);
                    }
                    self.gateway.stop().await;
                    return Err(error);
                }
                Ok(json!({"enabled":true}))
            }
            "stop" => {
                self.connections.forget(None);
                let _guard = self.gateway.settings_lock.lock().await;
                if let Some(host) = &self._host {
                    host.set_services_enabled(false);
                }
                self.publications.stop().await;
                self.shells.close_all().await;
                self.files.cancel_host(None);
                self.gateway.stop().await;
                Ok(json!({"enabled":false}))
            }
            "network" => {
                self.shells.close_all().await;
                self.files.cancel_host(None);
                let _guard = self.gateway.settings_lock.lock().await;
                let interrupted = self.gateway.interrupt().await;
                self.connections.forget(None);
                self.endpoint.network_change().await;
                if self.gateway.is_enabled() {
                    self.gateway.restore(self.clone()).await?;
                }
                Ok(json!({"network_changed":true,"interrupted_requests":interrupted}))
            }
            "open" => {
                let url = self.open_route(value).await?;
                Ok(json!({"url":url}))
            }
            _ => Err("Unknown app operation".into()),
        }
    }
}
fn app_transport_config(value: &Value) -> Result<TransportConfig, String> {
    let config: TransportConfig = serde_json::from_value(json!({
        "bind_addr": "0.0.0.0:0",
        "relay_url": argument(value, "relay")?,
        "discovery_relay_url": value.get("discovery_relay"),
        "backup_relay_urls": value.get("backup_relays").cloned().unwrap_or_else(|| json!([])),
        "prefer_primary_relay": value.get("prefer_primary_relay").cloned().unwrap_or(json!(true)),
        "keep_backup_relays_warm": value.get("keep_backup_relays_warm").cloned().unwrap_or(json!(true)),
        "relay_only": false,
    }))
    .map_err(|_| "Invalid relay")?;
    config.validate().map_err(|_| "Invalid relay")?;
    Ok(config)
}

/// Platform capability entry point. Unlike `call`, this consumes an already
/// authorized file object and never resolves a numeric descriptor from JSON.
pub fn put_file_descriptor(text: &str, file: std::fs::File) -> String {
    let outcome = (|| -> Result<Value, String> {
        if text.len() > 16384 {
            return Err("App request too large".into());
        }
        let value: Value = serde_json::from_str(text).map_err(|_| "Invalid app request")?;
        let client = CLIENT
            .lock()
            .map_err(|_| "Native state unavailable")?
            .clone()
            .ok_or("Client not initialized")?;
        runtime().block_on(client.start_file_descriptor_job(&value, file))
    })();
    match outcome {
        Ok(value) => json!({"ok":true,"result":value}).to_string(),
        Err(error) => json!({"ok":false,"error":error,"code":"operation_failed"}).to_string(),
    }
}

pub fn call(text: &str) -> String {
    call_with_publisher(text, None)
}
pub fn call_with_publisher(
    text: &str,
    publisher: Option<Arc<dyn wispnet_files::Publisher>>,
) -> String {
    let outcome = (|| -> Result<Value, String> {
        if text.len() > 16384 {
            return Err("App request too large".into());
        }
        let value: Value = serde_json::from_str(text).map_err(|_| "Invalid app request")?;
        if value["op"] == "new_key" {
            return Ok(json!({"key":token()}));
        }
        if value["op"] == "init" {
            let key: SecretKey = argument(&value, "key")?
                .parse()
                .map_err(|_| "Invalid device key")?;
            let state = PathBuf::from(argument(&value, "state")?);
            let config = app_transport_config(&value)?;
            let mut client = CLIENT.lock().map_err(|_| "Native state unavailable")?;
            if client.is_none() {
                *client = Some(runtime().block_on(Client::new_peer_with_publisher(
                    key, state, config, publisher,
                ))?);
            }
            return Ok(
                json!({"ready":true,"peer":client.as_ref().expect("initialized").endpoint.id().to_string()}),
            );
        }
        let client = CLIENT
            .lock()
            .map_err(|_| "Native state unavailable")?
            .clone()
            .ok_or("Client not initialized")?;
        runtime().block_on(client.command(&value))
    })();
    match outcome {
        Ok(value) => json!({"ok":true,"result":value}).to_string(),
        Err(error) => {
            let code = match error.as_str() {
                "awaiting approval" => "approval_pending",
                "access denied" => "host_denied",
                "Pairing removed by remote machine" => "pairing_removed",
                "catalog host or schema mismatch"
                | "invalid catalog"
                | "control frame truncated"
                | "control trailing data"
                | "control frame too large"
                | "Catalog rollback, changed generation or inconsistent snapshot"
                | "Catalog baseline unavailable or corrupt"
                | "Pairing changed during catalog refresh" => "catalog_rejected",
                "Service no longer permitted" => "service_denied",
                "pairing request denied" => "invitation_denied",
                "Expired invitation or wrong host pin" => "invitation_invalid",
                _ => "operation_failed",
            };
            json!({"ok":false,"error":error,"code":code}).to_string()
        }
    }
}

#[cfg(test)]
mod app_transport_tests {
    use super::*;

    #[test]
    fn android_discovery_fallback_preserves_primary_and_legacy_init() {
        let primary = "https://relay.wispnet.app";
        let outside = "https://usw1-1.relay.n0.iroh.link";
        let config = app_transport_config(
            &json!({"relay": primary, "discovery_relay": outside, "backup_relays":[outside]}),
        )
        .unwrap();
        assert_eq!(config.relay_url.as_deref(), Some(primary));
        assert_eq!(config.discovery_relay_url.as_deref(), Some(outside));
        assert_eq!(config.backup_relay_urls, vec![outside.to_string()]);
        assert!(!config.relay_only);
        assert!(
            app_transport_config(&json!({"relay": primary}))
                .unwrap()
                .discovery_relay_url
                .is_none()
        );
        assert!(
            app_transport_config(
                &json!({"relay": primary, "discovery_relay": "http://insecure.example"})
            )
            .is_err()
        );
    }
}

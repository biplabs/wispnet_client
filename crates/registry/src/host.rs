//! Independent single-writer authority. Remote catalogs never enter these tables.
use crate::{
    control::{self, ControlError},
    store::Store,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use wispnet_core::{
    config::ServiceConfig,
    enrollment::valid_name,
    host::{Catalog, Service, ServiceKind},
    identity::EndpointId,
    protocol::valid_service_id,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mutation {
    Approve {
        expected_revision: u64,
        peer: EndpointId,
        name: String,
    },
    Revoke {
        expected_revision: u64,
        peer: EndpointId,
    },
    ApproveInvitation {
        expected_revision: u64,
        invitation: String,
        /// Additional existing services to grant in the same approval transaction.
        #[serde(default)]
        services: Vec<String>,
    },
    RejectInvitation {
        expected_revision: u64,
        invitation: String,
    },
    /// Built-in shell is permanent; this operation only changes remote grants.
    Shell {
        expected_revision: u64,
        allow: Vec<EndpointId>,
    },
    /// Built-in file exports and their explicit remote grants.
    Files {
        expected_revision: u64,
        allow: Vec<EndpointId>,
        exports: BTreeMap<String, PathBuf>,
    },
    /// A read-only share has its own service identity and device grants.
    FileShare {
        expected_revision: u64,
        id: String,
        root: PathBuf,
        allow: Vec<EndpointId>,
    },
    /// A receiving folder has its own service identity and device grants.
    FileInbox {
        expected_revision: u64,
        id: String,
        root: PathBuf,
        allow: Vec<EndpointId>,
    },
    /// Add one approved peer's access without changing the service or other grants.
    GrantService {
        expected_revision: u64,
        service: String,
        peer: EndpointId,
    },
    /// Remove one approved peer's access without changing pairing or other grants.
    RevokeService {
        expected_revision: u64,
        service: String,
        peer: EndpointId,
    },
    Service {
        expected_revision: u64,
        id: String,
        backend: std::net::SocketAddr,
        allow: Vec<EndpointId>,
        #[serde(default)]
        /// Legacy input accepted for installed clients; never stored.
        display_name: Option<String>,
        #[serde(default)]
        kind: Option<ServiceKind>,
    },
    /// Edit an existing service, optionally renaming it in the same transaction.
    EditService {
        expected_revision: u64,
        id: String,
        new_id: String,
        backend: std::net::SocketAddr,
        /// Omission preserves the current grants; a supplied list replaces them.
        #[serde(default)]
        allow: Option<Vec<EndpointId>>,
        #[serde(default)]
        kind: Option<ServiceKind>,
    },
    /// Atomically edit a folder's name, location and grants, preserving its mode.
    EditFolder {
        expected_revision: u64,
        id: String,
        new_id: String,
        root: PathBuf,
        allow: Vec<EndpointId>,
    },
    /// Pause serving without removing the binding, folders, or remote grants.
    SetServiceEnabled {
        expected_revision: u64,
        id: String,
        incarnation: String,
        enabled: bool,
    },
    Remove {
        expected_revision: u64,
        id: String,
    },
}
impl Mutation {
    fn revision(&self) -> u64 {
        match self {
            Self::Approve {
                expected_revision, ..
            }
            | Self::Revoke {
                expected_revision, ..
            }
            | Self::ApproveInvitation {
                expected_revision, ..
            }
            | Self::RejectInvitation {
                expected_revision, ..
            }
            | Self::Shell {
                expected_revision, ..
            }
            | Self::Files {
                expected_revision, ..
            }
            | Self::FileShare {
                expected_revision, ..
            }
            | Self::FileInbox {
                expected_revision, ..
            }
            | Self::GrantService {
                expected_revision, ..
            }
            | Self::RevokeService {
                expected_revision, ..
            }
            | Self::Service {
                expected_revision, ..
            }
            | Self::EditService {
                expected_revision, ..
            }
            | Self::EditFolder {
                expected_revision, ..
            }
            | Self::SetServiceEnabled {
                expected_revision, ..
            }
            | Self::Remove {
                expected_revision, ..
            } => *expected_revision,
        }
    }
}
#[derive(Serialize)]
pub struct Snapshot {
    pub host: EndpointId,
    pub generation: String,
    pub revision: u64,
    pub clients: Vec<Client>,
    pub services: Vec<LocalService>,
    /// Owner-local configuration; never included in remote catalogs.
    pub file_exports: BTreeMap<String, PathBuf>,
    pub file_shares: BTreeMap<String, PathBuf>,
    pub file_inboxes: BTreeMap<String, PathBuf>,
}
impl Snapshot {
    pub fn enabled_services(&self) -> impl Iterator<Item = &LocalService> {
        self.services.iter().filter(|service| {
            service.enabled && (service.kind != ServiceKind::Files || !self.file_exports.is_empty())
        })
    }
}
#[derive(Serialize)]
pub struct Client {
    pub peer: EndpointId,
    pub name: String,
}
#[derive(Clone, Serialize)]
pub struct LocalService {
    pub id: String,
    pub incarnation: String,
    /// Legacy response field, always derived from id.
    pub display_name: String,
    pub kind: ServiceKind,
    pub backend: std::net::SocketAddr,
    pub allow: Vec<EndpointId>,
    pub enabled: bool,
}
/// Shell has no loopback listener. The reserved address is durable metadata
/// only; ordinary TCP/HTTP services still require a nonzero backend port.
fn validate_local_service(
    id: &str,
    backend: std::net::SocketAddr,
    allow: &[EndpointId],
    kind: ServiceKind,
) -> Result<(), ControlError> {
    let backend = if matches!(
        kind,
        ServiceKind::Shell | ServiceKind::Files | ServiceKind::FileShare | ServiceKind::FileInbox
    ) {
        if backend != std::net::SocketAddr::from(([127, 0, 0, 1], 0)) {
            return Err(ControlError::Invalid);
        }
        #[cfg(target_os = "android")]
        if kind == ServiceKind::Shell {
            return Err(ControlError::Invalid);
        }
        std::net::SocketAddr::from(([127, 0, 0, 1], 1))
    } else {
        backend
    };
    wispnet_core::config::validate_services(&[ServiceConfig {
        id: if matches!(kind, ServiceKind::Shell | ServiceKind::Files) {
            "builtin-service".into()
        } else {
            id.into()
        },
        backend,
        allow: allow.to_vec(),
    }])
    .map_err(|_| ControlError::Invalid)
}
/// Install the singleton only on desktop hosts. Preserve an old user-defined
/// service named shell, including its incarnation and grants, under a free ID.
pub(crate) fn ensure_builtin_shell(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<bool> {
    #[cfg(target_os = "android")]
    {
        let _ = tx;
        Ok(false)
    }
    #[cfg(not(target_os = "android"))]
    {
        let initialized: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM host_authority)", [], |r| {
                r.get(0)
            })?;
        if !initialized {
            return Ok(false);
        }
        let old = tx
            .query_row(
                "SELECT kind,backend FROM host_services WHERE id='shell'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        if old
            .as_ref()
            .is_some_and(|(kind, backend)| kind == "shell" && backend == "127.0.0.1:0")
        {
            return Ok(false);
        }
        if old.is_some() {
            let mut suffix = 0u32;
            let renamed = loop {
                let candidate = if suffix == 0 {
                    "shell-legacy".to_owned()
                } else {
                    format!("shell-legacy-{suffix}")
                };
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&candidate],
                    |r| r.get::<_, bool>(0),
                )? {
                    break candidate;
                }
                suffix += 1;
            };
            tx.execute("INSERT INTO host_services(id,backend,incarnation,kind) SELECT ?1,backend,incarnation,kind FROM host_services WHERE id='shell'",[&renamed])?;
            tx.execute(
                "UPDATE host_grants SET service=?1 WHERE service='shell'",
                [&renamed],
            )?;
            tx.execute("DELETE FROM host_services WHERE id='shell'", [])?;
        }
        tx.execute("INSERT INTO host_services(id,backend,incarnation,kind) VALUES('shell','127.0.0.1:0',lower(hex(randomblob(32))),'shell')",[])?;
        Ok(true)
    }
}
/// Reserve Files without inheriting a same-name custom service's grants.
pub(crate) fn ensure_builtin_files(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<bool> {
    if !tx.query_row("SELECT EXISTS(SELECT 1 FROM host_authority)", [], |r| {
        r.get::<_, bool>(0)
    })? {
        return Ok(false);
    }
    let old = tx
        .query_row(
            "SELECT kind,backend FROM host_services WHERE id='files'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    if old
        .as_ref()
        .is_some_and(|(kind, backend)| kind == "files" && backend == "127.0.0.1:0")
    {
        return Ok(false);
    }
    if old.is_some() {
        let mut suffix = 0u32;
        let renamed = loop {
            let candidate = if suffix == 0 {
                "files-legacy".to_owned()
            } else {
                format!("files-legacy-{suffix}")
            };
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                [&candidate],
                |r| r.get::<_, bool>(0),
            )? {
                break candidate;
            }
            suffix += 1;
        };
        tx.execute("INSERT INTO host_services(id,backend,incarnation,kind) SELECT ?1,backend,incarnation,kind FROM host_services WHERE id='files'", [&renamed])?;
        tx.execute(
            "UPDATE host_grants SET service=?1 WHERE service='files'",
            [&renamed],
        )?;
        tx.execute("DELETE FROM host_services WHERE id='files'", [])?;
    }
    tx.execute("INSERT INTO host_services(id,backend,incarnation,kind) VALUES('files','127.0.0.1:0',lower(hex(randomblob(32))),'files')", [])?;
    Ok(true)
}

/// Export roots must be existing absolute directories with no symlink component.
/// Descriptor-relative confinement is also enforced by the Files engine at use time.
/// Convert legacy global Inbox permission to independently revocable folders.
/// Existing artifacts stay in place; only service bindings/resume identities move.
pub(crate) fn migrate_file_inboxes(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<bool> {
    let exports = file_exports(tx).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let grants: i64 = tx.query_row(
        "SELECT count(*) FROM host_grants WHERE service='files'",
        [],
        |r| r.get(0),
    )?;
    if exports.is_empty() && grants == 0 {
        return Ok(false);
    }
    let enabled: bool = tx.query_row(
        "SELECT enabled FROM host_services WHERE id='files' AND kind='files'",
        [],
        |r| r.get(0),
    )?;
    for (alias, root) in exports {
        let mut id = alias.clone();
        let base = format!("inbox-{}", &alias[..alias.len().min(48)]);
        let mut suffix = 0usize;
        while matches!(id.as_str(), "shell" | "files")
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                [&id],
                |r| r.get::<_, bool>(0),
            )?
        {
            id = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            suffix += 1;
        }
        tx.execute("INSERT INTO host_services(id,backend,incarnation,kind,enabled) VALUES(?1,'127.0.0.1:0',lower(hex(randomblob(32))),'file_inbox',?2)", params![id,enabled])?;
        tx.execute(
            "INSERT INTO host_file_inboxes(service,root) VALUES(?1,?2)",
            params![id, root.to_str().ok_or(rusqlite::Error::InvalidQuery)?],
        )?;
        tx.execute("INSERT INTO host_grants(service,client) SELECT ?1,client FROM host_grants WHERE service='files'", [&id])?;
    }
    tx.execute("DELETE FROM host_grants WHERE service='files'", [])?;
    tx.execute("DELETE FROM host_file_exports", [])?;
    tx.execute(
        "UPDATE host_services SET incarnation=lower(hex(randomblob(32))) WHERE id='files'",
        [],
    )?;
    Ok(true)
}

fn validate_file_exports(
    tx: &rusqlite::Transaction<'_>,
    exports: &BTreeMap<String, PathBuf>,
) -> Result<(), ControlError> {
    if exports.len() > 32 {
        return Err(ControlError::Invalid);
    }
    let database: String = tx.query_row(
        "SELECT file FROM pragma_database_list WHERE name='main'",
        [],
        |r| r.get(0),
    )?;
    let authority_directory = Path::new(&database)
        .parent()
        .ok_or(ControlError::Storage)?
        .canonicalize()
        .map_err(|_| ControlError::Storage)?;
    for (name, root) in exports {
        if !valid_service_id(name) || !root.is_absolute() || root.as_os_str().len() > 4096 {
            return Err(ControlError::Invalid);
        }
        let canonical = root.canonicalize().map_err(|_| ControlError::Invalid)?;
        if canonical.starts_with(&authority_directory)
            || authority_directory.starts_with(&canonical)
        {
            return Err(ControlError::Invalid);
        }
        let mut prefix = PathBuf::new();
        for component in root.components() {
            if matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            ) {
                return Err(ControlError::Invalid);
            }
            prefix.push(component);
            let metadata = std::fs::symlink_metadata(&prefix).map_err(|_| ControlError::Invalid)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(ControlError::Invalid);
            }
        }
    }
    Ok(())
}
fn validate_share_root(tx: &rusqlite::Transaction<'_>, root: &Path) -> Result<(), ControlError> {
    if !root.is_absolute() || root.as_os_str().len() > 4096 {
        return Err(ControlError::Invalid);
    }
    let database: String = tx.query_row(
        "SELECT file FROM pragma_database_list WHERE name='main'",
        [],
        |r| r.get(0),
    )?;
    let private = Path::new(&database)
        .parent()
        .ok_or(ControlError::Storage)?
        .canonicalize()
        .map_err(|_| ControlError::Storage)?;
    let canonical = root.canonicalize().map_err(|_| ControlError::Invalid)?;
    if canonical.starts_with(&private) || private.starts_with(&canonical) {
        return Err(ControlError::Invalid);
    }
    let mut prefix = PathBuf::new();
    for component in root.components() {
        if matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ) {
            return Err(ControlError::Invalid);
        }
        prefix.push(component);
        let metadata = std::fs::symlink_metadata(&prefix).map_err(|_| ControlError::Invalid)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ControlError::Invalid);
        }
    }
    Ok(())
}
fn file_shares(tx: &rusqlite::Transaction<'_>) -> Result<BTreeMap<String, PathBuf>, ControlError> {
    let mut statement = tx.prepare("SELECT service,root FROM host_file_shares ORDER BY service")?;
    statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .map(|r| r.map_err(ControlError::from))
        .collect()
}

fn file_inboxes(tx: &rusqlite::Transaction<'_>) -> Result<BTreeMap<String, PathBuf>, ControlError> {
    let mut statement =
        tx.prepare("SELECT service,root FROM host_file_inboxes ORDER BY service")?;
    statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .map(|r| r.map_err(ControlError::from))
        .collect()
}

fn file_exports(tx: &rusqlite::Transaction<'_>) -> Result<BTreeMap<String, PathBuf>, ControlError> {
    let mut statement = tx.prepare("SELECT name,root FROM host_file_exports ORDER BY name")?;
    let values = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    // Validate durable syntax without requiring removable exports to stay mounted.
    if values.len() > 32
        || values
            .iter()
            .any(|(name, root)| !valid_service_id(name) || !root.is_absolute())
    {
        return Err(ControlError::Storage);
    }
    Ok(values)
}

impl Store {
    pub fn host_initialize(&mut self, host: EndpointId, secret: &str) -> Result<(), ControlError> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM host_authority)", [], |r| {
                r.get(0)
            })?;
        if exists {
            return Err(ControlError::Conflict);
        }
        let legacy:i64=tx.query_row("SELECT (SELECT count(*) FROM controller)+(SELECT count(*) FROM devices)+(SELECT count(*) FROM services)",[],|r|r.get(0))?;
        if legacy != 0 || !control::valid_token(secret) {
            return Err(ControlError::Invalid);
        }
        tx.execute(
            "INSERT INTO host_authority VALUES(1,?1,?2,?3)",
            params![host.to_string(), &*control::token(), control::hash(secret)],
        )?;
        ensure_builtin_shell(&tx)?;
        ensure_builtin_files(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn host_check(&self, host: EndpointId, secret: Option<&str>) -> Result<(), ControlError> {
        let authority: Option<(String, String)> = self
            .0
            .query_row(
                "SELECT endpoint,owner_hash FROM host_authority WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, digest)) = authority else {
            return Err(ControlError::NotReady);
        };
        if id != host.to_string() {
            return Err(ControlError::Forbidden);
        }
        if secret.is_some_and(|s| !control::valid_token(s) || control::hash(s) != digest) {
            return Err(ControlError::Forbidden);
        }
        Ok(())
    }
    pub fn host_snapshot(&mut self, host: EndpointId) -> Result<Snapshot, ControlError> {
        let tx = self.0.transaction()?;
        let (stored, generation): (String, String) = tx.query_row(
            "SELECT endpoint,generation FROM host_authority WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if stored != host.to_string() || !wispnet_core::enrollment::valid_code(&generation) {
            return Err(ControlError::Forbidden);
        }
        let revision: i64 =
            tx.query_row("SELECT value FROM revision WHERE id=1", [], |r| r.get(0))?;
        let mut clients = Vec::new();
        let mut services = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id,name FROM host_clients ORDER BY id")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
                let (peer, name): (String, String) = row?;
                if !valid_name(&name) {
                    return Err(ControlError::Storage);
                }
                clients.push(Client {
                    peer: peer.parse().map_err(|_| ControlError::Storage)?,
                    name: wispnet_core::config::readable_device_id(&name),
                });
            }
            let mut stmt = tx.prepare(
                "SELECT id,backend,incarnation,kind,enabled FROM host_services ORDER BY id",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })? {
                let (id, backend, incarnation, kind, enabled) = row?;
                let kind = match kind.as_str() {
                    "http" => ServiceKind::Http,
                    "tcp" => ServiceKind::Tcp,
                    "shell" => ServiceKind::Shell,
                    "files" => ServiceKind::Files,
                    "file_share" => ServiceKind::FileShare,
                    "file_inbox" => ServiceKind::FileInbox,
                    _ => return Err(ControlError::Storage),
                };
                if !wispnet_core::enrollment::valid_code(&incarnation) {
                    return Err(ControlError::Storage);
                }
                let mut allow = Vec::new();
                let mut grants =
                    tx.prepare("SELECT client FROM host_grants WHERE service=?1 ORDER BY client")?;
                for row in grants.query_map([&id], |r| r.get::<_, String>(0))? {
                    allow.push(row?.parse().map_err(|_| ControlError::Storage)?);
                }
                if allow.iter().any(|p| !clients.iter().any(|c| c.peer == *p)) {
                    return Err(ControlError::Storage);
                }
                let backend = backend.parse().map_err(|_| ControlError::Storage)?;
                if (kind == ServiceKind::Shell) != (id == "shell")
                    || (kind == ServiceKind::Files) != (id == "files")
                {
                    return Err(ControlError::Storage);
                }
                validate_local_service(&id, backend, &allow, kind)
                    .map_err(|_| ControlError::Storage)?;
                services.push(LocalService {
                    // Legacy wire field for installed clients, always the ID.
                    display_name: id.clone(),
                    id,
                    incarnation,
                    kind,
                    backend,
                    allow,
                    enabled,
                });
            }
        }
        let file_exports = file_exports(&tx)?;
        let file_shares = file_shares(&tx)?;
        let file_inboxes = file_inboxes(&tx)?;
        if services.iter().any(|service| {
            (service.kind == ServiceKind::FileShare) != file_shares.contains_key(&service.id)
                || (service.kind == ServiceKind::FileInbox)
                    != file_inboxes.contains_key(&service.id)
        }) {
            return Err(ControlError::Storage);
        }
        tx.commit()?;
        Ok(Snapshot {
            host,
            generation,
            revision: revision.try_into().map_err(|_| ControlError::Storage)?,
            clients,
            services,
            file_exports,
            file_shares,
            file_inboxes,
        })
    }
    pub fn host_mutate(
        &mut self,
        host: EndpointId,
        secret: &str,
        operation: Mutation,
    ) -> Result<u64, ControlError> {
        // Host identity and owner hash are immutable after initialization.
        self.host_check(host, Some(secret))?;
        let expected = operation.revision();
        if expected >= i64::MAX as u64 {
            return Err(ControlError::Invalid);
        }
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: i64 =
            tx.query_row("SELECT value FROM revision WHERE id=1", [], |r| r.get(0))?;
        if revision as u64 != expected {
            return Err(ControlError::Conflict);
        }
        apply_mutation(&tx, operation)?;
        tx.execute("UPDATE revision SET value=value+1 WHERE id=1", [])?;
        tx.commit()?;
        Ok(expected + 1)
    }
}
pub(crate) fn apply_mutation(
    tx: &rusqlite::Transaction<'_>,
    operation: Mutation,
) -> Result<(), ControlError> {
    match operation {
        Mutation::ApproveInvitation {
            invitation,
            services,
            ..
        } => {
            let unique: std::collections::BTreeSet<_> = services.iter().collect();
            if services.len() > 162 || unique.len() != services.len() {
                return Err(ControlError::Invalid);
            }
            for service in &services {
                if !valid_service_id(service)
                    || !tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                        [service],
                        |r| r.get::<_, bool>(0),
                    )?
                {
                    return Err(ControlError::Invalid);
                }
                require_file_export_for_grant(tx, service)?;
            }
            let peer = crate::pairing::approve(tx, &invitation, crate::enrollment::now())?;
            // Preserve other clients' grants and any access this peer already has.
            for service in services {
                tx.execute(
                    "INSERT OR IGNORE INTO host_grants VALUES(?1,?2)",
                    params![service, peer],
                )?;
            }
        }
        Mutation::RejectInvitation { invitation, .. } => {
            crate::pairing::reject(tx, &invitation)?;
        }
        Mutation::Approve { peer, name, .. } => {
            if !valid_name(&name) {
                return Err(ControlError::Invalid);
            }
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM host_clients WHERE id!=?1",
                [peer.to_string()],
                |r| r.get(0),
            )?;
            if count >= 1024 {
                return Err(ControlError::Invalid);
            }
            tx.execute("INSERT INTO host_clients VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET name=excluded.name",params![peer.to_string(),name])?;
        }
        Mutation::Revoke { peer, .. } => {
            if tx.execute("DELETE FROM host_clients WHERE id=?1", [peer.to_string()])? == 0 {
                return Err(ControlError::Invalid);
            }
            tx.execute(
                "UPDATE host_invitations SET state='revoked',challenge=NULL
                    WHERE peer=?1 AND state IN ('issued','challenged','pending')",
                [peer.to_string()],
            )?;
        }
        Mutation::EditService {
            expected_revision,
            id,
            new_id,
            backend,
            allow,
            kind,
        } => {
            if matches!(id.as_str(), "shell" | "files")
                || matches!(new_id.as_str(), "shell" | "files")
                || matches!(
                    kind,
                    Some(
                        ServiceKind::Shell
                            | ServiceKind::Files
                            | ServiceKind::FileShare
                            | ServiceKind::FileInbox
                    )
                )
                || !valid_service_id(&id)
                || !valid_service_id(&new_id)
            {
                return Err(ControlError::Invalid);
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_file_shares WHERE service=?1 UNION ALL SELECT 1 FROM host_file_inboxes WHERE service=?1)",
                [&id],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(ControlError::Invalid);
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                [&id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(ControlError::Invalid);
            }
            if id != new_id {
                let occupied: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&new_id],
                    |r| r.get(0),
                )?;
                if occupied {
                    return Err(ControlError::Conflict);
                }
                // Both parent rows exist while grants move, so immediate foreign
                // keys remain valid without changing the schema or dropping grants.
                tx.execute(
                    "INSERT INTO host_services(id,backend,incarnation,kind,enabled)
                     SELECT ?2,backend,incarnation,kind,enabled FROM host_services WHERE id=?1",
                    params![id, new_id],
                )?;
                tx.execute(
                    "UPDATE host_grants SET service=?2 WHERE service=?1",
                    params![id, new_id],
                )?;
                tx.execute("DELETE FROM host_services WHERE id=?1", [&id])?;
            }
            let allow = match allow {
                Some(allow) => allow,
                None => {
                    let mut statement = tx.prepare(
                        "SELECT client FROM host_grants WHERE service=?1 ORDER BY client",
                    )?;
                    statement
                        .query_map([&new_id], |r| r.get::<_, String>(0))?
                        .map(|row| row?.parse().map_err(|_| ControlError::Storage))
                        .collect::<Result<Vec<_>, ControlError>>()?
                }
            };
            // Reuse the usual backend, peer and protocol checks. Any failure
            // rolls back the rename and grant migration along with the edit.
            apply_mutation(
                tx,
                Mutation::Service {
                    expected_revision,
                    id: new_id,
                    backend,
                    allow,
                    display_name: None,
                    kind,
                },
            )?;
        }
        Mutation::GrantService { service, peer, .. } => {
            if !valid_service_id(&service)
                || !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&service],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Err(ControlError::Invalid);
            }
            require_file_export_for_grant(tx, &service)?;
            let peer = peer.to_string();
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
                [&peer],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(ControlError::Forbidden);
            }
            // Excluding this peer lets an existing grant remain idempotent even
            // when all 128 slots are occupied.
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM host_grants WHERE service=?1 AND client!=?2",
                params![service, peer],
                |r| r.get(0),
            )?;
            if count >= 128 {
                return Err(ControlError::Invalid);
            }
            tx.execute(
                "INSERT OR IGNORE INTO host_grants(service,client) VALUES(?1,?2)",
                params![service, peer],
            )?;
        }
        Mutation::RevokeService { service, peer, .. } => {
            if !valid_service_id(&service)
                || !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&service],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Err(ControlError::Invalid);
            }
            let peer = peer.to_string();
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
                [&peer],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(ControlError::Forbidden);
            }
            tx.execute(
                "DELETE FROM host_grants WHERE service=?1 AND client=?2",
                params![service, peer],
            )?;
        }
        Mutation::Shell { allow, .. } => {
            if allow.len() > 128
                || allow.iter().collect::<std::collections::HashSet<_>>().len() != allow.len()
            {
                return Err(ControlError::Invalid);
            }
            let builtin:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM host_services WHERE id='shell' AND kind='shell' AND backend='127.0.0.1:0')",[],|r|r.get(0))?;
            if !builtin {
                return Err(ControlError::Invalid);
            }
            for peer in &allow {
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
                    [peer.to_string()],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(ControlError::Forbidden);
                }
            }
            tx.execute("DELETE FROM host_grants WHERE service='shell'", [])?;
            for peer in allow {
                tx.execute(
                    "INSERT INTO host_grants VALUES('shell',?1)",
                    [peer.to_string()],
                )?;
            }
        }
        Mutation::Files { allow, exports, .. } => {
            validate_file_exports(tx, &exports)?;
            if (exports.is_empty() && !allow.is_empty())
                || allow.len() > 128
                || allow.iter().collect::<std::collections::HashSet<_>>().len() != allow.len()
            {
                return Err(ControlError::Invalid);
            }
            let builtin: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM host_services WHERE id='files' AND kind='files' AND backend='127.0.0.1:0')", [], |r| r.get(0))?;
            if !builtin {
                return Err(ControlError::Invalid);
            }
            for peer in &allow {
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
                    [peer.to_string()],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(ControlError::Forbidden);
                }
            }
            if file_exports(tx)? != exports {
                tx.execute("DELETE FROM host_file_exports", [])?;
                for (name, root) in exports {
                    tx.execute(
                        "INSERT INTO host_file_exports VALUES(?1,?2)",
                        params![name, root.to_str().ok_or(ControlError::Invalid)?],
                    )?;
                }
                tx.execute("UPDATE host_services SET incarnation=lower(hex(randomblob(32))) WHERE id='files'", [])?;
            }
            tx.execute("DELETE FROM host_grants WHERE service='files'", [])?;
            for peer in allow {
                tx.execute(
                    "INSERT INTO host_grants VALUES('files',?1)",
                    [peer.to_string()],
                )?;
            }
        }
        Mutation::EditFolder {
            id,
            new_id,
            root,
            allow,
            ..
        } => {
            if !valid_service_id(&new_id) || matches!(new_id.as_str(), "shell" | "files") {
                return Err(ControlError::Invalid);
            }
            let spelling = tx
                .query_row("SELECT kind FROM host_services WHERE id=?1", [&id], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?;
            let (kind, table) = match spelling.as_deref() {
                Some("file_inbox") => (ServiceKind::FileInbox, "host_file_inboxes"),
                Some("file_share") => (ServiceKind::FileShare, "host_file_shares"),
                _ => return Err(ControlError::Invalid),
            };
            if id != new_id {
                if tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&new_id],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(ControlError::Conflict);
                }
                tx.execute("INSERT INTO host_services(id,backend,incarnation,kind,enabled) SELECT ?2,backend,lower(hex(randomblob(32))),kind,enabled FROM host_services WHERE id=?1", params![id,new_id])?;
                tx.execute(
                    &format!("UPDATE {table} SET service=?2 WHERE service=?1"),
                    params![id, new_id],
                )?;
                tx.execute(
                    "UPDATE host_grants SET service=?2 WHERE service=?1",
                    params![id, new_id],
                )?;
                tx.execute("DELETE FROM host_services WHERE id=?1", [&id])?;
            }
            apply_folder_service(tx, new_id, root, allow, kind)?;
        }
        Mutation::FileShare {
            id, root, allow, ..
        } => apply_folder_service(tx, id, root, allow, ServiceKind::FileShare)?,
        Mutation::FileInbox {
            id, root, allow, ..
        } => apply_folder_service(tx, id, root, allow, ServiceKind::FileInbox)?,
        Mutation::Service {
            id,
            backend,
            allow,
            display_name,
            kind,
            ..
        } => {
            if matches!(id.as_str(), "shell" | "files")
                || matches!(
                    kind,
                    Some(
                        ServiceKind::Shell
                            | ServiceKind::Files
                            | ServiceKind::FileShare
                            | ServiceKind::FileInbox
                    )
                )
            {
                return Err(ControlError::Invalid);
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_file_shares WHERE service=?1 UNION ALL SELECT 1 FROM host_file_inboxes WHERE service=?1)",
                [&id],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(ControlError::Invalid);
            }
            let effective_kind = match kind {
                Some(kind) => kind,
                None => match tx
                    .query_row("SELECT kind FROM host_services WHERE id=?1", [&id], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?
                    .as_deref()
                {
                    Some("shell") => ServiceKind::Shell,
                    Some("files") => ServiceKind::Files,
                    Some("tcp") => ServiceKind::Tcp,
                    _ => ServiceKind::Http,
                },
            };
            validate_local_service(&id, backend, &allow, effective_kind)?;
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM host_services WHERE id!=?1 AND id NOT IN ('shell','files')",
                [&id],
                |r| r.get(0),
            )?;
            if count >= 128
                && !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_services WHERE id=?1)",
                    [&id],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Err(ControlError::Invalid);
            }
            for peer in &allow {
                let approved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
                    [peer.to_string()],
                    |r| r.get(0),
                )?;
                if !approved {
                    return Err(ControlError::Forbidden);
                }
            }
            if display_name
                .as_deref()
                .is_some_and(|name| !valid_name(name))
            {
                return Err(ControlError::Invalid);
            }
            let kind = kind.map(|k| match k {
                ServiceKind::Http => "http",
                ServiceKind::Tcp => "tcp",
                ServiceKind::Shell => "shell",
                ServiceKind::Files => "files",
                ServiceKind::FileShare => "file_share",
                ServiceKind::FileInbox => "file_inbox",
            });
            // Accept bounded legacy input, but never persist another name.
            tx.execute("INSERT INTO host_services(id,backend,incarnation,kind)
                VALUES(?1,?2,lower(hex(randomblob(32))),coalesce(?3,'http'))
                ON CONFLICT(id) DO UPDATE SET backend=excluded.backend,
                incarnation=CASE WHEN ?3 IS NOT NULL AND ?3!=host_services.kind THEN lower(hex(randomblob(32))) ELSE host_services.incarnation END,
                kind=coalesce(?3,host_services.kind)",
                params![id,backend.to_string(),kind])?;
            tx.execute("DELETE FROM host_grants WHERE service=?1", [&id])?;
            for peer in allow {
                tx.execute(
                    "INSERT INTO host_grants VALUES(?1,?2)",
                    params![id, peer.to_string()],
                )?;
            }
        }
        Mutation::SetServiceEnabled {
            id,
            incarnation,
            enabled,
            ..
        } => {
            if !valid_service_id(&id) || !wispnet_core::enrollment::valid_code(&incarnation) {
                return Err(ControlError::Invalid);
            }
            if tx.execute(
                "UPDATE host_services SET enabled=?1 WHERE id=?2 AND incarnation=?3",
                params![enabled, id, incarnation],
            )? == 0
            {
                return Err(ControlError::Conflict);
            }
        }
        Mutation::Remove { id, .. } => {
            if matches!(id.as_str(), "shell" | "files")
                || !valid_service_id(&id)
                || tx.execute("DELETE FROM host_services WHERE id=?1", [id])? == 0
            {
                return Err(ControlError::Invalid);
            }
        }
    }
    Ok(())
}
fn apply_folder_service(
    tx: &rusqlite::Transaction<'_>,
    id: String,
    root: PathBuf,
    allow: Vec<EndpointId>,
    kind: ServiceKind,
) -> Result<(), ControlError> {
    let (table, spelling) = match kind {
        ServiceKind::FileShare => ("host_file_shares", "file_share"),
        ServiceKind::FileInbox => ("host_file_inboxes", "file_inbox"),
        _ => return Err(ControlError::Invalid),
    };
    if !valid_service_id(&id) || matches!(id.as_str(), "shell" | "files") {
        return Err(ControlError::Invalid);
    }
    validate_share_root(tx, &root)?;
    let backend = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    validate_local_service(&id, backend, &allow, kind)?;
    let previous = tx
        .query_row("SELECT kind FROM host_services WHERE id=?1", [&id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    if previous
        .as_deref()
        .is_some_and(|previous| previous != spelling)
    {
        return Err(ControlError::Conflict);
    }
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM host_services WHERE id!=?1 AND id NOT IN ('shell','files')",
        [&id],
        |r| r.get(0),
    )?;
    if count >= 128 && previous.is_none() {
        return Err(ControlError::Invalid);
    }
    for peer in &allow {
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_clients WHERE id=?1)",
            [peer.to_string()],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(ControlError::Forbidden);
        }
    }
    let roots = if kind == ServiceKind::FileShare {
        file_shares(tx)?
    } else {
        file_inboxes(tx)?
    };
    let changed = roots.get(&id) != Some(&root);
    tx.execute("INSERT INTO host_services(id,backend,incarnation,kind) VALUES(?1,'127.0.0.1:0',lower(hex(randomblob(32))),?3) ON CONFLICT(id) DO UPDATE SET incarnation=CASE WHEN ?2 THEN lower(hex(randomblob(32))) ELSE host_services.incarnation END", params![id,changed,spelling])?;
    tx.execute(&format!("INSERT INTO {table}(service,root) VALUES(?1,?2) ON CONFLICT(service) DO UPDATE SET root=excluded.root"), params![id,root.to_str().ok_or(ControlError::Invalid)?])?;
    tx.execute("DELETE FROM host_grants WHERE service=?1", [&id])?;
    for peer in allow {
        tx.execute(
            "INSERT INTO host_grants VALUES(?1,?2)",
            params![id, peer.to_string()],
        )?;
    }
    Ok(())
}

/// Files grants are standing access to configured folders. They cannot be
/// assigned before the owner has chosen at least one folder to share.
fn require_file_export_for_grant(
    tx: &rusqlite::Transaction<'_>,
    service: &str,
) -> Result<(), ControlError> {
    if service == "files" && file_exports(tx)?.is_empty() {
        return Err(ControlError::Invalid);
    }
    Ok(())
}
#[derive(Clone)]
pub struct Authority {
    pub(crate) host: EndpointId,
    directory: PathBuf,
    pub(crate) store: Arc<Mutex<Store>>,
    device_id: Arc<std::sync::RwLock<String>>,
    public_links: Arc<std::sync::RwLock<Vec<wispnet_core::host::PublicLink>>>,
}
impl Authority {
    pub fn open(directory: &Path, host: EndpointId) -> Result<Self, ControlError> {
        let path = directory.join("host.sqlite3");
        // Agent startup must never create/reconstruct missing authoritative state.
        if !path.try_exists().map_err(|_| ControlError::Storage)? {
            return Err(ControlError::NotReady);
        }
        let mut store = Store::open(&path)?;
        store.host_check(host, None)?;
        if !store.ready()? {
            return Err(ControlError::Storage);
        }
        store.host_snapshot(host)?;
        Ok(Self {
            host,
            directory: directory.to_path_buf(),
            store: Arc::new(Mutex::new(store)),
            device_id: Arc::new(std::sync::RwLock::new(format!(
                "device-{}",
                &host.to_string()[..8]
            ))),
            public_links: Arc::new(std::sync::RwLock::new(Vec::new())),
        })
    }
    pub fn with_device_id(self, id: &str) -> Self {
        *self.device_id.write().unwrap_or_else(|e| e.into_inner()) =
            wispnet_core::config::readable_device_id(id);
        self
    }
    pub fn set_device_id(&self, id: &str) -> Result<(), ControlError> {
        if !valid_service_id(id) {
            return Err(ControlError::Invalid);
        }
        *self.device_id.write().unwrap_or_else(|e| e.into_inner()) = id.to_owned();
        Ok(())
    }
    pub fn device_id(&self) -> String {
        self.device_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn state_dir(&self) -> &Path {
        &self.directory
    }
    pub fn host(&self) -> EndpointId {
        self.host
    }
    /// Runtime advertisements contain no backend addresses or gateway credentials.
    pub fn set_public_links(
        &self,
        services: Vec<wispnet_core::host::PublicLink>,
    ) -> Result<(), ControlError> {
        let links = wispnet_core::host::PublicLinks {
            version: 1,
            host: self.host,
            services,
        };
        if !links.valid(self.host) {
            return Err(ControlError::Invalid);
        }
        *self.public_links.write().unwrap_or_else(|e| e.into_inner()) = links.services;
        Ok(())
    }
    pub async fn public_links(
        &self,
        peer: EndpointId,
    ) -> Result<wispnet_core::host::PublicLinks, ControlError> {
        let catalog = self.catalog(peer).await?;
        let services = self
            .public_links
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|link| {
                catalog.services.iter().any(|s| {
                    s.kind == ServiceKind::Http
                        && s.id == link.service
                        && s.incarnation == link.incarnation
                })
            })
            .cloned()
            .collect();
        Ok(wispnet_core::host::PublicLinks {
            version: 1,
            host: self.host,
            services,
        })
    }
    pub async fn snapshot(&self) -> Result<Snapshot, ControlError> {
        let host = self.host;
        let mut store = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.store.clone().lock_owned(),
        )
        .await
        .map_err(|_| ControlError::RateLimited)?;
        tokio::task::spawn_blocking(move || store.host_snapshot(host))
            .await
            .map_err(|_| ControlError::Storage)?
    }
    /// The transport-authenticated peer may update only its existing display label.
    pub async fn set_client_name(
        &self,
        peer: EndpointId,
        name: String,
    ) -> Result<(), ControlError> {
        if !valid_name(&name) {
            return Err(ControlError::Invalid);
        }
        let host = self.host;
        let mut store = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.store.clone().lock_owned(),
        )
        .await
        .map_err(|_| ControlError::RateLimited)?;
        tokio::task::spawn_blocking(move || {
            store.host_check(host, None)?;
            let tx = store
                .0
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let previous: Option<String> = tx
                .query_row(
                    "SELECT name FROM host_clients WHERE id=?1",
                    [peer.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            let previous = previous.ok_or(ControlError::Forbidden)?;
            if previous != name {
                let revision: i64 =
                    tx.query_row("SELECT value FROM revision WHERE id=1", [], |r| r.get(0))?;
                if revision == i64::MAX {
                    return Err(ControlError::Invalid);
                }
                tx.execute(
                    "UPDATE host_clients SET name=?1 WHERE id=?2",
                    params![name, peer.to_string()],
                )?;
                tx.execute("UPDATE revision SET value=value+1 WHERE id=1", [])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|_| ControlError::Storage)?
    }
    pub async fn approved(&self, peer: EndpointId) -> bool {
        self.snapshot()
            .await
            .is_ok_and(|s| s.clients.iter().any(|c| c.peer == peer))
    }
    pub async fn file_exports(&self) -> Result<BTreeMap<String, PathBuf>, ControlError> {
        Ok(self.snapshot().await?.file_exports)
    }
    pub async fn file_share(&self, id: &str) -> Result<Option<PathBuf>, ControlError> {
        Ok(self.snapshot().await?.file_shares.remove(id))
    }
    /// Serving paths use only enabled bindings; owner snapshots retain paused services.
    pub async fn services(&self) -> Result<Vec<LocalService>, ControlError> {
        Ok(self.snapshot().await?.enabled_services().cloned().collect())
    }
    pub async fn catalog(&self, peer: EndpointId) -> Result<Catalog, ControlError> {
        let snapshot = self.snapshot().await?;
        if !snapshot.clients.iter().any(|c| c.peer == peer) {
            return Err(ControlError::Forbidden);
        }
        let services: Vec<Service> = snapshot
            .enabled_services()
            .filter(|s| s.allow.contains(&peer))
            .cloned()
            .map(|s| Service {
                id: s.id,
                incarnation: s.incarnation,
                display_name: s.display_name,
                kind: s.kind,
            })
            .collect();
        Ok(Catalog {
            version: 1,
            complete: true,
            service_count: services.len() as u16,
            host: self.host,
            generation: snapshot.generation,
            revision: snapshot.revision,
            services,
        })
    }
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Storage => "host state unavailable or corrupt",
            Self::Invalid => "invalid host operation",
            Self::Conflict => "host revision conflict",
            Self::Forbidden => "host operation denied",
            Self::NotReady => {
                "host authority missing; initialize explicitly or restore protected state"
            }
            Self::RateLimited => "host operation rate limited",
        })
    }
}
impl std::error::Error for ControlError {}
/// Explicit initialization only; a surviving credential blocks lost-database reset.
pub fn initialize(directory: &Path, host: EndpointId) -> Result<(), ControlError> {
    use std::io::Write;
    let path = directory.join("host.sqlite3");
    let credential = directory.join("host-owner.token");
    if credential.try_exists().map_err(|_| ControlError::Storage)? {
        if !path.try_exists().map_err(|_| ControlError::Storage)? {
            return Err(ControlError::NotReady);
        }
        let store = Store::open(&path)?;
        store.host_check(host, Some(&control::read_token(&credential)?))?;
        return Ok(());
    }
    if path.try_exists().map_err(|_| ControlError::Storage)? {
        return Err(ControlError::Conflict);
    }
    let secret = control::token();
    let mut temp = control::credential_temp(&credential)?;
    temp.write_all(secret.as_bytes())
        .map_err(|_| ControlError::Storage)?;
    temp.as_file()
        .sync_all()
        .map_err(|_| ControlError::Storage)?;
    temp.persist_noclobber(&credential)
        .map_err(|_| ControlError::Storage)?;
    std::fs::File::open(directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| ControlError::Storage)?;
    let mut store = Store::open(&path)?;
    store.host_initialize(host, &secret)
}
/// Open a verified existing store for protected local owner commands.
pub fn owner_store(
    directory: &Path,
    host: EndpointId,
) -> Result<(Store, zeroize::Zeroizing<String>), ControlError> {
    let secret = control::read_token(&directory.join("host-owner.token"))?;
    let path = directory.join("host.sqlite3");
    if !path.try_exists().map_err(|_| ControlError::Storage)? {
        return Err(ControlError::NotReady);
    }
    let store = Store::open(&path)?;
    store.host_check(host, Some(&secret))?;
    Ok((store, secret))
}

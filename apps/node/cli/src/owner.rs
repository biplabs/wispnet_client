//! Private owner IPC, durable cross-host operations and display-only desktop data.
//!
//! These helpers share the node's existing identity and owner socket; they never
//! open another network endpoint or treat cached data as authorization.
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const REQUEST_LIMIT: usize = 16_384;
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const PRIVATE_LIMIT: usize = 65_536;
const CACHE_LIMIT: usize = 128 * 1024;

pub fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required".into())
}

pub fn identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone)]
pub struct NodeApi {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl NodeApi {
    pub fn new(config: &Path) -> Result<Self, String> {
        let config = fs::canonicalize(config).map_err(|e| e.to_string())?;
        let text = fs::read_to_string(&config).map_err(|e| e.to_string())?;
        let data: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
        let state = PathBuf::from(
            data.get("state_dir")
                .and_then(toml::Value::as_str)
                .ok_or("Missing node state directory")?,
        );
        if !state.is_absolute() {
            return Err("Node state directory must be absolute".into());
        }
        Ok(Self { config, state })
    }

    /// Keep an explicit negative acknowledgement separate from a broken pipe.
    fn response(&self, request: &Value) -> Result<Value, String> {
        if !request.is_object() {
            return Err("Request must be a JSON object".into());
        }
        let path = self.state.join("node.sock");
        let metadata = fs::symlink_metadata(&path).map_err(offline_error)?;
        let uid = unsafe { libc::getuid() };
        if !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            return Err("Node socket must be owner-only".into());
        }
        let mut payload = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        payload.push(b'\n');
        if payload.len() > REQUEST_LIMIT {
            return Err("Request exceeds local API limit".into());
        }
        let mut stream = UnixStream::connect(path).map_err(offline_error)?;
        let timeout = Duration::from_secs(if request["op"] == "diagnose_connection" {
            65
        } else {
            27
        });
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        let verified = {
            let mut peer_uid: libc::uid_t = 0;
            let mut peer_gid: libc::gid_t = 0;
            (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) }) == 0
                && peer_uid == uid
        };
        #[cfg(not(target_os = "macos"))]
        let verified = {
            let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            (unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut length,
                )
            }) == 0
                && length as usize == std::mem::size_of::<libc::ucred>()
                && peer.uid == uid
        };
        if !verified {
            return Err("Unexpected node socket owner".into());
        }
        stream.write_all(&payload).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        BufReader::new(stream)
            .take((RESPONSE_LIMIT + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > RESPONSE_LIMIT || bytes.last() != Some(&b'\n') {
            return Err("Invalid or oversized node response".into());
        }
        let response: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid node response")?;
        if response["ok"] != true {
            return Ok(
                json!({"ok":false,"kind":"rejected", "error":response["error"].as_str().unwrap_or("Node operation failed")}),
            );
        }
        let result = response.get("result").ok_or("Missing node result")?;
        if request["op"] == "status" && result["api_version"] != 1 {
            return Err("Unsupported node API version".into());
        }
        Ok(json!({"ok":true,"result":result}))
    }

    pub fn request(&self, request: Value) -> Result<Value, String> {
        let response = self.response(&request)?;
        if response["ok"] == true {
            Ok(response["result"].clone())
        } else {
            Err(response["error"]
                .as_str()
                .unwrap_or("Node operation failed")
                .into())
        }
    }

    pub fn set_online(&self, online: bool) -> Result<Value, String> {
        crate::tui::api::Api {
            config: self.config.clone(),
            state: self.state.clone(),
        }
        .online(online)
    }
}

fn offline_error(error: std::io::Error) -> String {
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ECONNREFUSED)
    ) {
        format!("Node offline: {error}")
    } else {
        error.to_string()
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| e.to_string())?;
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("Unsafe private directory".into());
    }
    Ok(())
}

fn read_private_limit(path: &Path, limit: usize) -> Result<Value, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > limit as u64
    {
        return Err("Operation files must be bounded, owner-only regular files".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("Private file too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

pub fn read_private(path: &Path) -> Result<Value, String> {
    read_private_limit(path, PRIVATE_LIMIT)
}

pub fn write_private(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("Missing private directory")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| e.to_string())?;
    let metadata = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err("Unsafe private directory".into());
    }
    let mut temporary = tempfile::Builder::new()
        .prefix("snapshot-")
        .tempfile_in(parent)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut temporary, value).map_err(|e| e.to_string())?;
    temporary.as_file().sync_all().map_err(|e| e.to_string())?;
    temporary.persist(path).map_err(|e| e.to_string())?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key)))
}

pub fn validate_plan(value: &Value) -> Result<(), String> {
    if !exact_keys(value, &["action", "targets"]) {
        return Err("Expected action and targets".into());
    }
    let action = &value["action"];
    let Some(op @ ("approve" | "revoke")) = action["op"].as_str() else {
        return Err("Expected an explicit client approval or revocation".into());
    };
    if !action["peer"].as_str().is_some_and(identity) {
        return Err("Invalid client identity".into());
    }
    if !exact_keys(
        action,
        if op == "approve" {
            &["op", "peer", "name"]
        } else {
            &["op", "peer"]
        },
    ) || (op == "approve"
        && !action["name"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.chars().count() <= 80))
    {
        return Err("Invalid client action".into());
    }
    let targets = value["targets"].as_array().ok_or("Expected target hosts")?;
    if targets.is_empty() || targets.len() > 32 {
        return Err("Select between one and 32 target hosts".into());
    }
    let mut seen = BTreeSet::new();
    for target in targets {
        let object = target.as_object().ok_or("Invalid target host")?;
        let host = target["host"]
            .as_str()
            .filter(|s| identity(s))
            .ok_or("Invalid target host")?;
        if object.keys().any(|key| key != "host" && key != "ssh") || !seen.insert(host) {
            return Err("Invalid or duplicate target host".into());
        }
        if let Some(ssh) = object.get("ssh") {
            let ssh = ssh.as_str().ok_or("Invalid explicit SSH owner target")?;
            if ssh.is_empty()
                || ssh.len() > 254
                || !ssh.as_bytes()[0].is_ascii_alphanumeric()
                || !ssh
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b))
            {
                return Err("Invalid explicit SSH owner target".into());
            }
        }
    }
    Ok(())
}

pub struct Combined {
    pub api: NodeApi,
    pub directory: PathBuf,
}

impl Combined {
    pub fn new(config: &Path) -> Result<Self, String> {
        let api = NodeApi::new(config)?;
        let directory = api.state.join("combined-operations");
        Ok(Self { api, directory })
    }
    fn path(&self, operation: &str) -> Result<PathBuf, String> {
        if !identity(operation) {
            return Err("Invalid operation ID".into());
        }
        Ok(self.directory.join(format!("{operation}.json")))
    }
    fn save(&self, value: &Value) -> Result<(), String> {
        private_directory(&self.directory)?;
        write_private(
            &self.path(value["id"].as_str().ok_or("Missing operation ID")?)?,
            value,
        )
    }
    fn load(&self, operation: &str) -> Result<Value, String> {
        let mut value = read_private(&self.path(operation)?)?;
        if value["version"] != 1 || value["id"] != operation {
            return Err("Invalid saved operation".into());
        }
        let targets = value["targets"].as_array().ok_or("Invalid saved targets")?;
        let plan_targets: Vec<_> = targets
            .iter()
            .map(|target| {
                let mut target = target.as_object().cloned().unwrap_or_default();
                target.retain(|key, _| key == "host" || key == "ssh");
                Value::Object(target)
            })
            .collect();
        validate_plan(&json!({"action":value["action"],"targets":plan_targets}))?;
        for target in targets {
            match target["state"].as_str() {
                Some("applied") if target["revision"].as_u64().is_some() => {}
                Some("pending" | "rejected") if target["revision"].is_null() => {}
                _ => return Err("Invalid saved target status or receipt".into()),
            }
        }
        update_completion(&mut value);
        Ok(value)
    }
    fn journals(&self) -> Result<Vec<PathBuf>, String> {
        private_directory(&self.directory)?;
        let mut files = fs::read_dir(&self.directory)
            .map_err(|e| e.to_string())?
            .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        files.retain(|p| p.extension().is_some_and(|e| e == "json"));
        files.sort();
        Ok(files)
    }
    pub fn list(&self) -> Result<Value, String> {
        let files = self.journals()?;
        if files.len() > 128 {
            return Err("Too many operation journals".into());
        }
        files
            .iter()
            .map(|path| {
                self.load(
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or("Invalid journal name")?,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
    }
    pub fn start(&self, plan: &Value) -> Result<Value, String> {
        validate_plan(plan)?;
        if self.journals()?.len() >= 128 {
            return Err(
                "Operation journal limit reached; archive finished journals explicitly".into(),
            );
        }
        let mut random = [0_u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let operation: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let targets: Vec<Value> = plan["targets"]
            .as_array()
            .ok_or("Expected targets")?
            .iter()
            .map(|target| {
                let mut target = target.as_object().cloned().unwrap_or_default();
                target.insert("state".into(), json!("pending"));
                target.insert("reason".into(), json!("Not acknowledged"));
                target.insert("revision".into(), Value::Null);
                Value::Object(target)
            })
            .collect();
        let value = json!({"version":1,"id":operation,"action":plan["action"],"targets":targets,"complete":false,"finished":false});
        // The durable intent is committed before any target can change authority.
        self.save(&value)?;
        self.retry(&operation)
    }
    fn attempt(&self, value: &Value, target: &Value) -> Value {
        let request = json!({"op":"apply_operation","id":value["id"],"host":target["host"],"action":value["action"]});
        let response = if let Some(ssh) = target["ssh"].as_str() {
            let output = bounded_command(
                Command::new("ssh").args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=yes",
                    "-o",
                    "ConnectTimeout=4",
                    ssh,
                    "\"$HOME/.local/bin/wispnet-node\" --response",
                ]),
                Some(request.to_string().as_bytes()),
                8,
            );
            output.and_then(|output| {
                serde_json::from_slice::<Value>(&output.stdout)
                    .map_err(|_| "Owner reply unconfirmed".into())
            })
        } else {
            self.api.response(&request)
        };
        let Ok(response) = response else {
            return pending("Target unavailable or reply unconfirmed");
        };
        if response["ok"] != true {
            return json!({"state":if response["kind"] == "rejected" { "rejected" } else { "pending" },
                "reason":response["error"].as_str().unwrap_or("Reply unconfirmed"),"revision":null});
        }
        let receipt = &response["result"];
        if receipt["host"] != target["host"]
            || receipt["id"] != value["id"]
            || !matches!(
                receipt["state"].as_str(),
                Some("applied" | "pending" | "rejected")
            )
        {
            return pending("Invalid target acknowledgement");
        }
        if receipt["state"] == "applied" && receipt["revision"].as_u64().is_none() {
            return pending("Missing durable applied revision");
        }
        if !receipt["reason"].is_null() && !receipt["reason"].is_string() {
            return pending("Invalid target acknowledgement");
        }
        json!({"state":receipt["state"],"reason":receipt["reason"],"revision":if receipt["state"] == "applied" {receipt["revision"].clone()} else {Value::Null}})
    }
    pub fn retry(&self, operation: &str) -> Result<Value, String> {
        self.path(operation)?;
        private_directory(&self.directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.directory.join(format!("{operation}.lock")))
            .map_err(|e| e.to_string())?;
        let metadata = lock.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("Unsafe operation lock".into());
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("This operation is already running".into());
        }
        let mut value = self.load(operation)?;
        let pending: Vec<_> = value["targets"]
            .as_array()
            .ok_or("Missing targets")?
            .iter()
            .enumerate()
            .filter(|(_, target)| target["state"] == "pending")
            .map(|(index, target)| (index, target.clone()))
            .collect();
        for batch in pending.chunks(4) {
            let snapshot = value.clone();
            std::thread::scope(|scope| -> Result<(), String> {
                let (sender, receiver) = std::sync::mpsc::channel();
                for (index, target) in batch {
                    let sender = sender.clone();
                    let snapshot = &snapshot;
                    scope.spawn(move || {
                        let _ = sender.send((*index, self.attempt(snapshot, target)));
                    });
                }
                drop(sender);
                for (index, receipt) in receiver {
                    let target = value["targets"][index]
                        .as_object_mut()
                        .ok_or("Invalid target")?;
                    target.extend(receipt.as_object().ok_or("Invalid receipt")?.clone());
                    update_completion(&mut value);
                    self.save(&value)?;
                }
                Ok(())
            })?;
        }
        Ok(value)
    }
}

fn pending(reason: &str) -> Value {
    json!({"state":"pending","reason":reason,"revision":null})
}
fn update_completion(value: &mut Value) {
    if let Some(targets) = value["targets"].as_array() {
        let complete = targets.iter().all(|target| target["state"] == "applied");
        let finished = targets.iter().all(|target| target["state"] != "pending");
        value["complete"] = json!(complete);
        value["finished"] = json!(finished);
    }
}

fn valid_catalog(peer: &str, value: &Value) -> bool {
    if !exact_keys(
        value,
        &[
            "version",
            "complete",
            "service_count",
            "host",
            "generation",
            "revision",
            "services",
        ],
    ) || value["version"].as_u64() != Some(1)
        || value["complete"] != true
        || value["host"] != peer
        || !value["generation"].as_str().is_some_and(identity)
        || value["revision"].as_u64().is_none_or(|n| n >= 1_u64 << 63)
    {
        return false;
    }
    let Some(services) = value["services"].as_array() else {
        return false;
    };
    if services.len() > 162 || value["service_count"].as_u64() != Some(services.len() as u64) {
        return false;
    }
    let mut previous = None;
    for service in services {
        if !exact_keys(service, &["id", "incarnation", "display_name", "kind"])
            || !matches!(
                service["kind"].as_str(),
                Some("http" | "tcp" | "shell" | "files" | "file_share" | "file_inbox")
            )
            || !service["incarnation"].as_str().is_some_and(identity)
        {
            return false;
        }
        let Some(name) = service["display_name"].as_str() else {
            return false;
        };
        if name.trim().is_empty()
            || name.len() > 128
            || name
                .chars()
                .any(|c| c < ' ' || ('\u{7f}'..='\u{9f}').contains(&c))
        {
            return false;
        }
        let Some(id) = service["id"]
            .as_str()
            .filter(|s| wispnet_core::protocol::valid_service_id(s))
        else {
            return false;
        };
        if previous.is_some_and(|prior| prior >= id) {
            return false;
        }
        previous = Some(id);
    }
    true
}

pub struct CatalogCache {
    directory: PathBuf,
}
impl CatalogCache {
    pub fn new(state: &Path) -> Self {
        Self {
            directory: state.join("desktop-catalogs"),
        }
    }
    fn path(&self, peer: &str) -> Result<PathBuf, String> {
        if !identity(peer) {
            return Err("Invalid peer ID".into());
        }
        Ok(self.directory.join(format!("{peer}.json")))
    }
    pub fn load(&self, peer: &str) -> Result<Value, String> {
        let path = self.path(peer)?;
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
            Err(e) => return Err(e.to_string()),
            Ok(_) => {}
        }
        private_directory(&self.directory)?;
        let value = read_private_limit(&path, CACHE_LIMIT)?;
        if !exact_keys(&value, &["version", "catalog", "fetched"])
            || value["version"].as_u64() != Some(1)
            || !valid_catalog(peer, &value["catalog"])
            || !value["fetched"]
                .as_f64()
                .is_some_and(|f| f.is_finite() && f >= 0.)
        {
            return Err("Invalid catalog cache".into());
        }
        Ok(
            json!({"catalog":value["catalog"],"fetched":value["fetched"],"state":"Cached; refresh required"}),
        )
    }
    pub fn save(&self, peer: &str, entry: &Value) -> Result<(), String> {
        let path = self.path(peer)?;
        if !valid_catalog(peer, &entry["catalog"])
            || !entry["fetched"]
                .as_f64()
                .is_some_and(|f| f.is_finite() && f >= 0.)
        {
            return Err("Invalid catalog".into());
        }
        let value = json!({"version":1,"catalog":entry["catalog"],"fetched":entry["fetched"]});
        if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > CACHE_LIMIT {
            return Err("Catalog too large".into());
        }
        private_directory(&self.directory)?;
        write_private(&path, &value)
    }
    pub fn remove(&self, peer: &str) -> Result<(), String> {
        let path = self.path(peer)?;
        match fs::symlink_metadata(&self.directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
            Ok(_) => private_directory(&self.directory)?,
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub fn desktop_data(payload: &Value, config: &Path) -> Result<Value, String> {
    let manager = Combined::new(config)?;
    let cache = CatalogCache::new(&manager.api.state);
    let host = || {
        payload["host"]
            .as_str()
            .ok_or_else(|| "Missing host".to_owned())
    };
    match payload["op"].as_str() {
        Some("operations") => manager.list(),
        Some("targets") => {
            let path = config
                .parent()
                .ok_or("Missing configuration directory")?
                .join("admin-targets.json");
            let targets = if fs::symlink_metadata(&path).is_ok() {
                read_private(&path)?
            } else {
                json!([{"host":manager.api.request(json!({"op":"status"}))?["host"]["host"]}])
            };
            validate_plan(
                &json!({"action":{"op":"revoke","peer":"0".repeat(64)},"targets":targets}),
            )?;
            Ok(targets)
        }
        Some("start") => manager.start(&payload["plan"]),
        Some("retry") => manager.retry(payload["id"].as_str().ok_or("Missing operation ID")?),
        Some("cache_load") => cache.load(host()?),
        Some("cache_save") => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_secs_f64();
            cache.save(
                host()?,
                &json!({"catalog":payload["catalog"],"fetched":now}),
            )?;
            Ok(Value::Null)
        }
        Some("cache_remove") => {
            cache.remove(host()?)?;
            Ok(Value::Null)
        }
        Some("preferences") => preferences(&manager.api.state),
        _ => Err("Unsupported desktop operation".into()),
    }
}

fn preferences(state: &Path) -> Result<Value, String> {
    let path = state.join("desktop-peer-names.json");
    let mut names = Map::new();
    if fs::symlink_metadata(&path).is_ok() {
        let value = read_private(&path)?;
        if let Some(source) = value.as_object() {
            for (peer, label) in source {
                if identity(peer)
                    && label.as_str().is_some_and(|s| {
                        !s.is_empty() && s.chars().count() <= 128 && !s.chars().any(|c| c < ' ')
                    })
                {
                    names.insert(peer.clone(), label.clone());
                }
            }
        }
    }
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".config"));
    let order = legacy_order(&config.join("wispnet/desktop-order.conf"))?;
    Ok(json!({"names":names,"order":order}))
}

fn legacy_order(path: &Path) -> Result<Value, String> {
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e.to_string()),
    };
    let mut section = String::new();
    let mut order = Map::new();
    for line in source.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            continue;
        };
        if section.is_empty() {
            continue;
        }
        let group = if section == "General" {
            key.trim().to_owned()
        } else {
            format!("{}/{}", section, key.trim())
        };
        order.insert(
            group,
            json!(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            ),
        );
    }
    Ok(Value::Object(order))
}

/// A fixed argv, bounded output and deadline for explicit SSH owner transport.
fn bounded_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: u64,
) -> Result<Output, String> {
    let mut stdout = tempfile::tempfile().map_err(|e| e.to_string())?;
    let mut stderr = tempfile::tempfile().map_err(|e| e.to_string())?;
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(stdout.try_clone().map_err(|e| e.to_string())?)
        .stderr(stderr.try_clone().map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(input) = input {
        let outcome = child
            .stdin
            .take()
            .ok_or_else(|| "Missing child input".to_owned())
            .and_then(|mut pipe| pipe.write_all(input).map_err(|e| e.to_string()));
        if let Err(error) = outcome {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline
            || stdout.metadata().map_err(|e| e.to_string())?.len() > RESPONSE_LIMIT as u64
            || stderr.metadata().map_err(|e| e.to_string())?.len() > PRIVATE_LIMIT as u64
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Owner connection unavailable or reply unconfirmed".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    stderr.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout
        .take((RESPONSE_LIMIT + 1) as u64)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    stderr
        .take((PRIVATE_LIMIT + 1) as u64)
        .read_to_end(&mut err)
        .map_err(|e| e.to_string())?;
    if out.len() > RESPONSE_LIMIT || err.len() > PRIVATE_LIMIT {
        return Err("Owner reply exceeds its output limit".into());
    }
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

pub fn run_args(args: &[OsString]) -> Result<(), String> {
    let mut config = home()?.join(".config/wispnet-node/host.toml");
    let mut response = false;
    let mut operation: Option<(&str, OsString)> = None;
    let mut request_file: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--config") if index + 1 < args.len() => {
                index += 1;
                config = PathBuf::from(&args[index]);
            }
            Some("--response") => response = true,
            Some("--request")
                if index + 1 < args.len() && operation.is_none() && request_file.is_none() =>
            {
                index += 1;
                request_file = Some(PathBuf::from(&args[index]));
            }
            Some(flag @ ("--batch" | "--retry"))
                if index + 1 < args.len() && operation.is_none() && request_file.is_none() =>
            {
                index += 1;
                operation = Some((flag, args[index].clone()));
            }
            Some("--operations") if operation.is_none() && request_file.is_none() => {
                operation = Some(("--operations", OsString::new()))
            }
            Some("--help" | "-h") => {
                println!(
                    "Usage: wispnet node-request [--config PATH] [--response] [--request PATH | --batch PATH | --retry ID | --operations]\nSend a JSON owner request on stdin; an empty request reads status."
                );
                return Ok(());
            }
            _ => return Err("Invalid node-request arguments".into()),
        }
        index += 1;
    }
    let execute = || -> Result<Value, String> {
        if let Some((flag, value)) = &operation {
            let manager = Combined::new(&config)?;
            return match *flag {
                "--batch" => manager.start(&read_private(Path::new(value))?),
                "--retry" => manager.retry(value.to_str().ok_or("Invalid operation ID")?),
                _ => manager.list(),
            };
        }
        if let Some(path) = &request_file {
            let request = read_private_limit(path, REQUEST_LIMIT)?;
            if !request.is_object() || !request["op"].is_string() {
                return Err("Request must be an object with an op".into());
            }
            return NodeApi::new(&config)?.response(&request);
        }
        let mut bytes = Vec::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin()
                .take((REQUEST_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
        }
        if bytes.len() > REQUEST_LIMIT {
            return Err("Request exceeds local API limit".into());
        }
        let request = if bytes.iter().all(u8::is_ascii_whitespace) {
            json!({"op":"status"})
        } else {
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?
        };
        NodeApi::new(&config)?.response(&request)
    };
    match execute() {
        Ok(value) if operation.is_some() => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Ok(value) if response => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
            if value["ok"] == true {
                Ok(())
            } else {
                Err(value["error"]
                    .as_str()
                    .unwrap_or("Node request rejected")
                    .into())
            }
        }
        Ok(value) if value["ok"] == true => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value["result"]).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Ok(value) => Err(value["error"]
            .as_str()
            .unwrap_or("Node request rejected")
            .into()),
        Err(error) => {
            if response {
                println!("{}", json!({"ok":false,"kind":"unavailable","error":error}));
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    };

    fn configuration(root: &Path) -> PathBuf {
        let config = root.join("host.toml");
        fs::write(
            &config,
            format!(
                "state_dir={}\n",
                serde_json::to_string(&root.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
        config
    }

    fn serve(
        root: &Path,
        replies: usize,
        reply: impl Fn(Value) -> Value + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let path = root.join("node.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(path).unwrap();
        fs::set_permissions(root.join("node.sock"), fs::Permissions::from_mode(0o600)).unwrap();
        std::thread::spawn(move || {
            for _ in 0..replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request = serde_json::from_str(&line).unwrap();
                let mut response = reply(request).to_string();
                response.push('\n');
                stream.write_all(response.as_bytes()).unwrap();
            }
        })
    }

    #[test]
    fn owner_ipc_checks_socket_permissions_version_and_explicit_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let api = NodeApi::new(&configuration(root.path())).unwrap();
        assert!(
            api.request(json!({"op":"status"}))
                .unwrap_err()
                .starts_with("Node offline: ")
        );
        let server = serve(root.path(), 3, |request| match request["op"].as_str() {
            Some("status") => json!({"ok":true,"result":{"api_version":1}}),
            Some("denied") => json!({"ok":false,"error":"Denied by owner"}),
            _ => json!({"ok":true,"result":{"api_version":2}}),
        });
        fs::set_permissions(
            root.path().join("node.sock"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(
            api.request(json!({"op":"status"}))
                .unwrap_err()
                .contains("owner-only")
        );
        fs::set_permissions(
            root.path().join("node.sock"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(
            api.request(json!({"op":"status"})).unwrap()["api_version"],
            1
        );
        assert_eq!(
            api.response(&json!({"op":"denied"})).unwrap(),
            json!({"ok":false,"kind":"rejected","error":"Denied by owner"})
        );
        assert_eq!(
            api.request(json!({"op":"other"})).unwrap()["api_version"],
            2
        );
        server.join().unwrap();
        let server = serve(
            root.path(),
            1,
            |_| json!({"ok":true,"result":{"api_version":2}}),
        );
        assert!(
            api.request(json!({"op":"status"}))
                .unwrap_err()
                .contains("API version")
        );
        server.join().unwrap();
    }

    #[test]
    fn owner_ipc_rejects_symlink_and_oversized_requests_before_send() {
        let root = tempfile::tempdir().unwrap();
        let api = NodeApi::new(&configuration(root.path())).unwrap();
        fs::write(root.path().join("target"), b"not a socket").unwrap();
        symlink(root.path().join("target"), root.path().join("node.sock")).unwrap();
        assert!(
            api.request(json!({"op":"status"}))
                .unwrap_err()
                .contains("owner-only")
        );
        fs::remove_file(root.path().join("node.sock")).unwrap();
        let listener = UnixListener::bind(root.path().join("node.sock")).unwrap();
        fs::set_permissions(
            root.path().join("node.sock"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(
            api.request(json!({"op":"status","extra":"x".repeat(REQUEST_LIMIT)}))
                .unwrap_err()
                .contains("limit")
        );
        drop(listener);
    }

    #[test]
    fn private_files_reject_symlinks_public_permissions_fifo_and_oversize() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private.json");
        write_private(&path, &json!({"secret":"fixture"})).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_private(&path).unwrap()["secret"], "fixture");
        let link = root.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read_private(&link).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, vec![b'x'; PRIVATE_LIMIT + 1]).unwrap();
        assert!(read_private(&path).is_err());
        let fifo = root.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_private(&fifo).is_err());
    }

    fn catalog(host: &str) -> Value {
        json!({"version":1,"complete":true,"service_count":1,"host":host,"generation":"b".repeat(64),"revision":2,
            "services":[{"id":"files","incarnation":"c".repeat(64),"display_name":"Files","kind":"files"}]})
    }

    #[test]
    fn catalog_cache_is_bounded_private_host_bound_display_only_and_independent_of_pins() {
        let root = tempfile::tempdir().unwrap();
        let cache = CatalogCache::new(root.path());
        let host = "a".repeat(64);
        fs::create_dir(root.path().join("peer-pins")).unwrap();
        fs::write(root.path().join("peer-pins/pin"), "fixture").unwrap();
        assert!(cache.load(&host).unwrap().is_null());
        let entry = json!({"catalog":catalog(&host),"fetched":123.5});
        cache.save(&host, &entry).unwrap();
        assert_eq!(cache.load(&host).unwrap()["catalog"], entry["catalog"]);
        assert_eq!(
            cache.load(&host).unwrap()["state"],
            "Cached; refresh required"
        );
        assert_eq!(
            fs::metadata(&cache.directory).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(cache.path(&host).unwrap()).unwrap().mode() & 0o777,
            0o600
        );
        let other = "d".repeat(64);
        fs::copy(cache.path(&host).unwrap(), cache.path(&other).unwrap()).unwrap();
        assert!(cache.load(&other).is_err());
        fs::set_permissions(
            cache.path(&host).unwrap(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(cache.load(&host).is_err());
        cache.save(&host, &entry).unwrap();
        fs::write(cache.path(&host).unwrap(), vec![b'x'; CACHE_LIMIT + 1]).unwrap();
        assert!(cache.load(&host).is_err());
        cache.remove(&host).unwrap();
        assert!(cache.load(&host).unwrap().is_null());
        assert_eq!(
            fs::read_to_string(root.path().join("peer-pins/pin")).unwrap(),
            "fixture"
        );
        symlink(
            root.path().join("peer-pins/pin"),
            cache.path(&host).unwrap(),
        )
        .unwrap();
        assert!(cache.load(&host).is_err());
        assert!(cache.load("../peer-pins").is_err());
        let mut malformed = entry.clone();
        malformed["catalog"]["services"][0]["display_name"] = json!("Invalid\u{7f}");
        assert!(cache.save(&host, &malformed).is_err());
    }

    #[test]
    fn combined_intent_is_durable_before_dispatch_and_retry_preserves_finished_targets() {
        let root = tempfile::tempdir().unwrap();
        let config = configuration(root.path());
        let manager = Combined::new(&config).unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let state = root.path().to_path_buf();
        let server = serve(root.path(), 3, move |request| {
            let operation = request["id"].as_str().unwrap();
            let journal =
                read_private(&state.join(format!("combined-operations/{operation}.json"))).unwrap();
            assert_eq!(journal["id"], operation);
            let host = request["host"].as_str().unwrap();
            json!({"ok":true,"result":{"host":host,"id":operation,
                "state":match host.as_bytes()[0] {b'a'=>"applied",b'b'=>"pending",_=>"rejected"},
                "revision":if host.starts_with('a') {json!(2)} else {Value::Null},"reason":null}})
        });
        let plan = json!({"action":{"op":"revoke","peer":"d".repeat(64)},"targets":[{"host":a},{"host":b},{"host":c}]});
        let value = manager.start(&plan).unwrap();
        server.join().unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["finished"], false);
        assert_eq!(value["targets"][0]["state"], "applied");
        assert_eq!(value["targets"][1]["state"], "pending");
        assert_eq!(value["targets"][2]["state"], "rejected");
        let expected = b.clone();
        let server = serve(root.path(), 1, move |request| {
            assert_eq!(request["host"], expected);
            json!({"ok":true,"result":{"host":request["host"],"id":request["id"],"state":"applied","revision":3,"reason":null}})
        });
        let result = manager.retry(value["id"].as_str().unwrap()).unwrap();
        server.join().unwrap();
        assert_eq!(result["finished"], true);
        assert_eq!(result["complete"], false);
        assert_eq!(manager.list().unwrap(), json!([result]));
    }

    #[test]
    fn combined_unknown_acknowledgements_stay_pending_and_explicit_denial_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let manager = Combined::new(&configuration(root.path())).unwrap();
        let value = json!({"id":"a".repeat(64),"action":{"op":"revoke","peer":"b".repeat(64)}});
        let target = json!({"host":"c".repeat(64)});
        assert_eq!(manager.attempt(&value, &target)["state"], "pending");
        let server = serve(
            root.path(),
            1,
            |_| json!({"ok":false,"error":"Wrong target host"}),
        );
        assert_eq!(manager.attempt(&value, &target)["state"], "rejected");
        server.join().unwrap();
        let server = serve(
            root.path(),
            1,
            |request| json!({"ok":true,"result":{"host":"d".repeat(64),"id":request["id"],"state":"applied","revision":2,"reason":null}}),
        );
        assert_eq!(manager.attempt(&value, &target)["state"], "pending");
        server.join().unwrap();
        let server = serve(
            root.path(),
            1,
            |request| json!({"ok":true,"result":{"host":request["host"],"id":request["id"],"state":"applied","reason":null}}),
        );
        assert_eq!(manager.attempt(&value, &target)["state"], "pending");
        server.join().unwrap();
    }

    #[test]
    fn combined_rejects_injected_targets_invalid_receipts_symlink_locks_and_concurrent_retry() {
        let root = tempfile::tempdir().unwrap();
        let manager = Combined::new(&configuration(root.path())).unwrap();
        let valid = json!({"action":{"op":"revoke","peer":"b".repeat(64)},"targets":[{"host":"c".repeat(64)}]});
        validate_plan(&valid).unwrap();
        let mut invalid = valid.clone();
        invalid["targets"][0]["ssh"] = json!("-oProxyCommand=bad");
        assert!(validate_plan(&invalid).is_err());
        invalid = valid.clone();
        invalid["targets"] = json!([valid["targets"][0], valid["targets"][0]]);
        assert!(validate_plan(&invalid).is_err());
        assert!(manager.retry("../escape").is_err());
        private_directory(&manager.directory).unwrap();
        let id = "a".repeat(64);
        let mut journal = json!({"id":id,"version":1,"action":valid["action"],"targets":[{"host":"c".repeat(64),"state":"applied","revision":null}]});
        manager.save(&journal).unwrap();
        assert!(manager.load(&id).is_err());
        journal["targets"][0]["revision"] = json!(2);
        manager.save(&journal).unwrap();
        let target = root.path().join("target");
        fs::write(&target, "private").unwrap();
        let lock = manager.directory.join(format!("{id}.lock"));
        symlink(&target, &lock).unwrap();
        assert!(manager.retry(&id).is_err());
        fs::remove_file(&lock).unwrap();
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(manager.retry(&id).unwrap_err().contains("already running"));
    }

    #[test]
    fn legacy_order_preserves_case_and_groups_without_gui_runtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("order.conf");
        fs::write(
            &path,
            "[General]\nmachines=b, a\n[services]\nMyHost=web, llama\n",
        )
        .unwrap();
        assert_eq!(
            legacy_order(&path).unwrap(),
            json!({"machines":["b","a"],"services/MyHost":["web","llama"]})
        );
    }
}

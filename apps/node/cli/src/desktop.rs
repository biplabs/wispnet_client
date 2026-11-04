//! Private, bounded stdio adapter for Flutter. The node remains the authority.
use crate::owner::{CatalogCache, NodeApi, desktop_data, read_private, write_private};
use crate::tui::api::{command, home};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, mpsc},
    time::{SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, String>;

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing or invalid {key}"))
}
fn ident(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
}
fn readable_device_id(value: &str) -> String {
    if label(value) {
        return value.into();
    }
    let mut result = String::new();
    for c in value.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || (!result.is_empty() && "._-".contains(c)) {
            result.push(c);
        } else if !result.is_empty() && !result.ends_with('-') {
            result.push('-');
        }
        if result.len() == 64 {
            break;
        }
    }
    let result = result.trim_end_matches(['.', '_', '-']);
    if result.is_empty() {
        "device".into()
    } else {
        result.into()
    }
}
fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool> {
    args.get(key).map_or(Ok(default), |v| {
        v.as_bool()
            .ok_or_else(|| format!("{key} must be true or false"))
    })
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// Check the original authority/path too: Url normalizes empty paths, backslashes,
// credentials and default ports, which must not make an unsafe handoff acceptable.
fn parsed_handoff(value: &str) -> Result<(url::Url, &str, &str)> {
    if value.len() > 2048
        || value
            .chars()
            .any(|c| (c as u32) < 33 || (127..=159).contains(&(c as u32)))
        || value.contains(['?', '#', '\\'])
    {
        return Err("Invalid browser handoff".into());
    }
    let (_, rest) = value.split_once("://").ok_or("Invalid browser handoff")?;
    let slash = rest.find('/').ok_or("Invalid browser handoff")?;
    let (authority, path) = rest.split_at(slash);
    if authority.contains('@') || authority.is_empty() || authority.ends_with(':') {
        return Err("Invalid browser handoff".into());
    }
    let parsed = url::Url::parse(value).map_err(|_| "Invalid browser handoff")?;
    Ok((parsed, authority, path))
}
fn browser_url(value: &str) -> Result<&str> {
    let (parsed, authority, path) = parsed_handoff(value)?;
    let host = parsed.host_str().unwrap_or_default();
    let port = authority
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok());
    if parsed.scheme() != "http"
        || !host.ends_with(".localhost")
        || host.len() <= 10
        || port.is_none_or(|p| p == 0)
        || !(path == "/"
            || (host == "registry.wispnet.localhost" && path == "/v1/services")
            || (host == "wispnet.localhost" && path == "/registry"))
    {
        return Err("Invalid local browser handoff".into());
    }
    Ok(value)
}
fn public_browser_url(value: &str) -> Result<&str> {
    let (parsed, authority, path) = parsed_handoff(value)?;
    let host = authority.split(':').next().unwrap_or_default();
    let valid_host = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part.as_bytes()[0].is_ascii_alphanumeric()
                && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        });
    let port_ok = authority
        .split_once(':')
        .is_none_or(|(_, port)| port == "443");
    if parsed.scheme() != "https" || !valid_host || !port_ok || path != "/" {
        return Err("Invalid public browser handoff".into());
    }
    Ok(value)
}
fn launch_browser(value: &str) -> Result<()> {
    command(
        Command::new(if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "xdg-open"
        })
        .arg(value),
        None,
        15,
    )?;
    Ok(())
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn launch_terminal(args: &[String], state: &Path) -> Result<()> {
    let mut file = tempfile::Builder::new()
        .prefix("shell-")
        .suffix(".command")
        .tempfile_in(state)
        .map_err(|e| e.to_string())?;
    writeln!(
        file,
        "#!/bin/sh\nrm -f -- \"$0\"\nexec {}",
        args.iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    )
    .map_err(|e| e.to_string())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let (_, path) = file.keep().map_err(|e| e.to_string())?;
    let result = if cfg!(target_os = "macos") {
        command(
            Command::new("/usr/bin/open")
                .args(["-a", "Terminal"])
                .arg(&path),
            None,
            15,
        )
        .map(|_| ())
    } else {
        let candidates: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-x"]),
            ("kitty", &[]),
            ("alacritty", &[]),
            ("xterm", &["-e"]),
        ];
        let mut result = Err(
            "Install a terminal application, or connect with wispnet shell from a terminal".into(),
        );
        for (terminal, options) in candidates {
            match Command::new(terminal)
                .args(*options)
                .arg("/bin/sh")
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
            {
                Ok(mut child) => {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    result = Ok(());
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    result = Err(e.to_string());
                    break;
                }
            }
        }
        result
    };
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[derive(Default)]
struct UiState {
    records: Map<String, Value>,
    preferences: Value,
    display: Value,
}
struct Bridge {
    config: PathBuf,
    api: NodeApi,
    cache: CatalogCache,
    state: Mutex<UiState>,
}
impl Bridge {
    fn new(config: &Path) -> Result<Self> {
        let api = NodeApi::new(config)?;
        let mut preferences = desktop_data(&json!({"op":"preferences"}), &api.config)?;
        let mut display = json!({});
        let path = api.state.join("desktop-flutter.json");
        if path.exists() {
            let saved = read_private(&path)?;
            if let Some(value) = saved.get("preferences") {
                preferences = value.clone();
            }
            if let Some(value) = saved.get("display") {
                display = value.clone();
            }
        } else {
            migrate_webkit(&mut preferences);
        }
        preferences
            .as_object_mut()
            .ok_or("Invalid saved desktop preferences")?
            .remove("names");
        if !preferences["order"].is_object() {
            preferences["order"] = json!({});
        }
        if let Some(hosts) = display["hosts"].as_object_mut() {
            for row in hosts.values_mut() {
                if let Some(name) = row["name"].as_str() {
                    row["name"] = json!(readable_device_id(name));
                }
            }
        }
        if let Some(name) = display["name"].as_str() {
            display["name"] = json!(readable_device_id(name));
        }
        Ok(Self {
            config: api.config.clone(),
            cache: CatalogCache::new(&api.state),
            api,
            state: Mutex::new(UiState {
                preferences,
                display,
                ..Default::default()
            }),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, UiState>> {
        self.state
            .lock()
            .map_err(|_| "Desktop state unavailable".into())
    }
    fn save(&self, state: &UiState) -> Result<()> {
        let mut display = state.display.clone();
        if let Some(hosts) = display["hosts"].as_object_mut() {
            for row in hosts.values_mut() {
                if let Some(row) = row.as_object_mut() {
                    row.retain(|key, _| key == "name" || key == "local");
                }
            }
        }
        write_private(
            &self.api.state.join("desktop-flutter.json"),
            &json!({"preferences":state.preferences,"display":display}),
        )
    }
    fn lifecycle(&self, enabled: bool) -> Result<()> {
        self.api.set_online(enabled).map(|_| ())
    }
    fn snapshot(&self) -> Result<Value> {
        let status = match self
            .api
            .request(json!({"op":"status","refresh_clients":true}))
        {
            Ok(status) => status,
            Err(error) if error.starts_with("Node offline:") => {
                let state = self.lock()?;
                let mut result = state.display.clone();
                result["enabled"] = json!(false);
                result["desktop"] = json!(true);
                result["order"] = state.preferences["order"].clone();
                result["hosts"] = state.display.get("hosts").cloned().unwrap_or(json!({}));
                result["network"] = json!("Node offline");
                result["version"] = json!(version());
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        if status["access_enabled"] != true {
            self.api.request(json!({"op":"enable"}))?;
        }
        let own = text(&status["host"], "host")?;
        let mut services = Vec::new();
        for service in array(&status["host"]["services"]) {
            if service["kind"] == "files"
                && status["host"]
                    .get("file_exports")
                    .is_none_or(|v| v.as_object().is_some_and(Map::is_empty) || v.is_null())
            {
                continue;
            }
            let mut service = service.clone();
            if let Some(publication) =
                array(&status["public_access"]["services"])
                    .iter()
                    .find(|p| {
                        p["service"] == service["id"] && p["incarnation"] == service["incarnation"]
                    })
            {
                service["public_access"] = publication.clone();
            }
            services.push(service);
        }
        let mut hosts = Map::new();
        hosts.insert(own.into(), json!({"name":status["host_name"],"state":"available","status":"This machine","local":true,"catalog":{"services":services}}));
        let peers: BTreeSet<_> = array(&status["peer_pins"])
            .iter()
            .filter_map(Value::as_str)
            .chain(
                array(&status["host"]["clients"])
                    .iter()
                    .filter_map(|c| c["peer"].as_str()),
            )
            .collect();
        let mut state = self.lock()?;
        for peer in peers {
            if peer == own {
                continue;
            }
            let record = state.records.get(peer).cloned().unwrap_or_else(|| {
                let cached = self.cache.load(peer).unwrap_or(Value::Null);
                json!({"state":"unverified","status":"Cached; refresh required","catalog":cached.get("catalog").cloned().unwrap_or(json!({"services":[]}))})
            });
            let mut record = record;
            let name = state.preferences["device_ids"][peer]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let name = array(&status["host"]["clients"])
                        .iter()
                        .find(|c| c["peer"] == peer)
                        .and_then(|c| c["name"].as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("device-{}", &peer[..peer.len().min(8)]));
                    readable_device_id(&name)
                });
            record["name"] = json!(name);
            hosts.insert(peer.into(), record);
        }
        state.display = json!({"name":status["host_name"],"peer":own,"hosts":hosts,"relay":status["relay"],"telemetry":{"peers":status["peer_telemetry"]}});
        self.save(&state)?;
        let mut result = state.display.clone();
        result["enabled"] = json!(true);
        result["desktop"] = json!(true);
        result["order"] = state.preferences["order"].clone();
        result["network"] = json!("Network available");
        result["version"] = json!(version());
        result["pendingRequests"] = json!(
            array(&status["invitations"])
                .iter()
                .filter(|i| i["state"] == "pending")
                .count()
        );
        result["activeClients"] = json!(
            array(&status["host"]["clients"])
                .iter()
                .filter(|c| c["peer"]
                    .as_str()
                    .is_some_and(|p| status["connected_peers"][p].as_u64().unwrap_or(0) > 0
                        || status["recent_peers"].get(p).is_some()))
                .count()
        );
        Ok(result)
    }
    fn refresh(&self, peer: &str) -> Result<Value> {
        {
            let state = self.lock()?;
            if state.display["peer"] == peer {
                return Ok(json!({}));
            }
            if state.display["hosts"].get(peer).is_none() {
                return Err("Machine was removed".into());
            }
        }
        let fetched = (|| -> Result<Value> {
            let catalog = self.api.request(json!({"op":"catalog","host":peer}))?;
            if catalog["host"] != peer {
                return Err("Wrong catalog host".into());
            }
            self.cache
                .save(peer, &json!({"catalog":catalog,"fetched":now()}))?;
            let mut record = json!({"state":"available","status":"Available","catalog":catalog});
            if let Ok(links) = self
                .api
                .request(json!({"op":"host_public_links","host":peer}))
                && links["version"] == 1
                && links["host"] == peer
                && links["services"].is_array()
            {
                record["public_access"] = links;
            }
            if let Ok(id) = self.api.request(json!({"op":"host_device_id","host":peer}))
                && let Some(id) = id["device_id"].as_str().filter(|id| label(id))
            {
                let mut state = self.lock()?;
                if !state.preferences["device_ids"].is_object() {
                    state.preferences["device_ids"] = json!({});
                }
                state.preferences["device_ids"][peer] = json!(id);
            }
            let name = self.lock()?.display["name"].clone();
            let _ = self
                .api
                .request(json!({"op":"set_name","host":peer,"name":name}));
            Ok(record)
        })();
        let mut state = self.lock()?;
        let record = match fetched {
            Ok(record) => record,
            Err(error) => {
                let mut previous = state
                    .records
                    .get(peer)
                    .or_else(|| state.display["hosts"].get(peer))
                    .cloned()
                    .unwrap_or(json!({}));
                previous["state"] = json!(if error.to_lowercase().contains("forbidden")
                    || error.to_lowercase().contains("denied")
                {
                    "host_denied"
                } else {
                    "unreachable"
                });
                previous["status"] = json!(error);
                previous
            }
        };
        if state.display["hosts"].get(peer).is_some() {
            state.records.insert(peer.into(), record);
        }
        Ok(json!({}))
    }
    fn target(&self, args: &Value, kinds: &[&str], default_service: Option<&str>) -> Result<Value> {
        let host = text(args, "host")?;
        let incarnation = text(args, "incarnation")?;
        let service = args
            .get("service")
            .and_then(Value::as_str)
            .or(default_service)
            .ok_or("Invalid file service")?;
        if !ident(host) || !ident(incarnation) || !label(service) {
            return Err("Invalid Files target; refresh this machine".into());
        }
        let status = self.api.request(json!({"op":"status"}))?;
        let catalog = if status["host"]["host"] == host {
            status["host"].clone()
        } else {
            self.api.request(json!({"op":"catalog","host":host}))?
        };
        if catalog["host"] != host
            || !array(&catalog["services"]).iter().any(|s| {
                s["id"] == service
                    && kinds.iter().any(|kind| s["kind"] == *kind)
                    && s["incarnation"] == incarnation
                    && s["enabled"] != false
            })
        {
            return Err("Files access unavailable or changed; refresh this machine".into());
        }
        Ok(json!({"host":host,"service":service,"incarnation":incarnation}))
    }
    fn open_shell(&self, args: &Value) -> Result<Value> {
        let host = text(args, "host")?;
        let service = text(args, "service")?;
        let incarnation = text(args, "incarnation")?;
        let session = args
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("main");
        if !ident(host) || !ident(incarnation) || !label(service) || !label(session) {
            return Err("Invalid shell session".into());
        }
        self.api.request(json!({"op":"enable"}))?;
        self.target(args, &["shell"], None)?;
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let argv = vec![
            executable.to_string_lossy().into_owned(),
            "shell".into(),
            "--host".into(),
            host.into(),
            "--service".into(),
            service.into(),
            "--incarnation".into(),
            incarnation.into(),
            "--session".into(),
            session.into(),
            "--config".into(),
            self.config.to_string_lossy().into_owned(),
        ];
        launch_terminal(&argv, &self.api.state)?;
        Ok(json!({}))
    }
    fn send_file(&self, args: &Value) -> Result<Value> {
        let workspace = args
            .get("workspace")
            .map_or(Ok(""), |v| v.as_str().ok_or("Choose a receiving folder"))?;
        if !workspace.is_empty() && !label(workspace) {
            return Err("Choose a receiving folder".into());
        }
        let source = absolute_path(text(args, "local_path")?)?;
        if !fs::symlink_metadata(&source)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_file()
        {
            return Err(
                "Choose regular files; folders and symbolic links are not supported".into(),
            );
        }
        let source = source
            .parent()
            .ok_or("Invalid file parent")?
            .canonicalize()
            .map_err(|e| e.to_string())?
            .join(source.file_name().ok_or("Invalid file name")?);
        let remote = text(args, "remote_path")?;
        relative_path(remote, false)?;
        let resume = bool_arg(args, "resume", true)?;
        let mut request = self.target(args, &["files", "file_inbox"], Some("files"))?;
        request["op"] = json!("file_put");
        request["workspace"] = json!(workspace);
        request["local_path"] = json!(source);
        request["remote_path"] = json!(remote);
        request["resume"] = json!(resume);
        self.api.request(request)
    }
    fn shared_files(&self, method: &str, args: &Value) -> Result<Value> {
        let path = args.get("path").map_or(Ok(""), |v| {
            v.as_str().ok_or("Choose a relative path inside the share")
        })?;
        relative_path(path, true)?;
        let mut request = self.target(args, &["file_share"], None)?;
        request["path"] = json!(path);
        if method == "shareList" {
            let cursor = args.get("cursor").cloned().unwrap_or(Value::Null);
            if !cursor.is_null() && cursor.as_str().is_none_or(|v| v.len() > 4096) {
                return Err("Invalid directory cursor".into());
            }
            request["op"] = json!("file_list");
            request["cursor"] = cursor;
        } else {
            let destination = text(args, "local_path")?;
            absolute_path(destination)?;
            let resume = bool_arg(args, "resume", true)?;
            let recursive = bool_arg(args, "recursive", false)?;
            if path.is_empty() && !recursive {
                return Err("Choose a file or download the whole folder".into());
            }
            request["op"] = json!("file_get");
            request["local_path"] = json!(destination);
            request["resume"] = json!(resume);
            if recursive {
                request["recursive"] = json!(true);
            }
        }
        self.api.request(request)
    }
    fn dispatch(&self, method: &str, args: &Value) -> Result<Value> {
        match method {
            "sshStatus" => self.ssh_status(text(args,"host")?),
            "sshSetup" => {
                let host=text(args,"host")?; if !ident(host) { return Err("Invalid Shell host".into()); }
                let out=command(Command::new(std::env::current_exe().map_err(|e| e.to_string())?).args(["ssh-bridge","--host",host,"--config"]).arg(&self.config).arg("--install-config"),None,35)?;
                Ok(json!({"command":String::from_utf8_lossy(&out).trim()}))
            }
            "startDesktop" => { self.lifecycle(true)?; self.api.request(json!({"op":"enable"}))?; Ok(json!({})) }
            "snapshot" => self.snapshot(),
            "resolveLink" => self.api.request(json!({"op":"resolve_link","link":text(args,"link")?})),
            "takeInvitation" => Ok(json!({})),
            "setAccess" => { let enabled=bool_arg(args,"enabled",false)?; self.lifecycle(enabled)?; if enabled { self.api.request(json!({"op":"enable"}))?; } Ok(json!({})) }
            "refreshHost" => self.refresh(text(args,"host")?),
            "connectionDiagnostics" => {
                let host=text(args,"host")?; if !ident(host) { return Err("Choose a paired device to diagnose".into()); }
                let status=self.api.request(json!({"op":"status"}))?;
                if status["host"]["host"]==host || !(array(&status["peer_pins"]).iter().any(|p| p==host) || array(&status["host"]["clients"]).iter().any(|c| c["peer"]==host)) { return Err("Choose a paired remote device to diagnose".into()); }
                let name={ let state=self.lock()?; state.preferences["device_ids"][host].as_str().or_else(|| state.records.get(host).and_then(|r| r["name"].as_str())).map(str::to_owned) }
                    .or_else(|| array(&status["host"]["clients"]).iter().find(|c| c["peer"]==host).and_then(|c| c["name"].as_str()).map(str::to_owned)).unwrap_or_else(|| host[..8].into());
                let mut row=json!({"host":host,"name":name});
                match self.api.request(json!({"op":"diagnose_connection","host":host})) { Ok(report)=>row["report"]=report,Err(error)=>row["error"]=json!(error) }
                Ok(json!({"network":null,"vpn":null,"network_changed":null,"peers":[row]}))
            }
            "preview"|"pair" => {
                let code=text(args,"code")?.chars().filter(|c| !c.is_whitespace()).collect::<String>();
                if !code.starts_with("wispnet1:") || code.len()>8192 { return Err("Paste a complete Wispnet pairing code".into()); }
                self.lifecycle(true)?;
                let preview=self.api.request(json!({"op":"preview_invitation","invitation":code}))?;
                if method=="preview" { return Ok(preview); }
                let host=text(&preview,"host")?;
                let result=self.api.request(json!({"op":"pair","host":host,"invitation":code}))?;
                let name=preview["host_name"].as_str().filter(|n| !n.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("device-{}",&host[..host.len().min(8)]));
                let mut state=self.lock()?; if !state.preferences["device_ids"].is_object() { state.preferences["device_ids"]=json!({}); }
                state.preferences["device_ids"][host]=json!(readable_device_id(&name)); state.records.insert(host.into(),json!({"state":"approval_pending","status":"Pending owner approval"})); self.save(&state)?;
                Ok(result)
            }
            "open" => {
                self.api.request(json!({"op":"enable"}))?;
                let host=text(args,"host")?; let name=self.lock()?.preferences["device_ids"][host].clone();
                let result=self.api.request(json!({"op":"open","host":host,"service":text(args,"service")?,"incarnation":text(args,"incarnation")?,"host_name":name}))?;
                Ok(json!({"url":browser_url(text(&result,"url")?)?}))
            }
            "openShell" => self.open_shell(args),
            "fileWorkspaces" => { let mut request=self.target(args,&["files","file_inbox"],Some("files"))?; request["op"]=json!("file_workspaces"); self.api.request(request) }
            "fileSend" => self.send_file(args),
            "shareList"|"shareGet" => self.shared_files(method,args),
            "fileStatus"|"fileCancel" => {
                let transfer=text(args,"transfer")?; if !ident(transfer) { return Err("Invalid file transfer handle".into()); }
                self.api.request(json!({"op":if method=="fileStatus" {"file_status"}else{"file_cancel"},"transfer":transfer}))
            }
            "shellOpen" => { self.api.request(json!({"op":"enable"}))?; self.api.request(json!({"op":"shell_open","host":text(args,"host")?,"service":args.get("service").unwrap_or(&json!("shell")),"incarnation":text(args,"incarnation")?,"session":args.get("session").unwrap_or(&json!("main")),"cols":integer_arg(args,"cols",80)?,"rows":integer_arg(args,"rows",24)?})) }
            "shellPoll" => self.api.request(json!({"op":"shell_poll","session":text(args,"session")?})),
            "shellInput" => self.api.request(json!({"op":"shell_input","session":text(args,"session")?,"data_b64":text(args,"data_b64")?})),
            "shellResize" => self.api.request(json!({"op":"shell_resize","session":text(args,"session")?,"cols":integer_arg(args,"cols",0)?,"rows":integer_arg(args,"rows",0)?})),
            "shellClose" => self.api.request(json!({"op":"shell_close","session":text(args,"session")?,"terminate":bool_arg(args,"terminate",false)?})),
            "launchPublic" => {
                let status=self.api.request(json!({"op":"status"}))?;
                let publication=array(&status["public_access"]["services"]).iter().find(|p| p["service"]==args["service"] && p["enabled"]==true).ok_or("Public URL unavailable")?;
                let url=public_browser_url(text(publication,"url")?)?; launch_browser(url)?; Ok(json!({"url":url}))
            }
            "launch" => { let url=text(args,"url")?; let url=browser_url(url).or_else(|_| public_browser_url(url))?; launch_browser(url)?; Ok(json!({})) }
            "removeHost" => {
                let host=text(args,"host")?; let status=self.api.request(json!({"op":"status"}))?;
                if status["host"]["host"]==host { return Err("This is the local machine; manage its shared services in Settings".into()); }
                self.api.request(json!({"op":"unpair","host":host,"expected_revision":status["host"]["revision"]}))?;
                let mut state=self.lock()?; state.records.remove(host);
                if let Some(ids)=state.preferences["device_ids"].as_object_mut(){ids.remove(host);}
                if let Some(hosts)=state.display["hosts"].as_object_mut(){hosts.remove(host);}
                self.cache.remove(host)?; self.save(&state)?; Ok(json!({}))
            }
            "saveOrder" => {
                let group=text(args,"group")?; let ids=args["ids"].as_array().ok_or("Invalid card order")?;
                if !(group=="machines" || group.strip_prefix("services/").is_some_and(ident)) || ids.len()>if group=="machines" {128}else{162} || !ids.iter().all(Value::is_string) {return Err("Invalid card order".into());}
                let mut state=self.lock()?; state.preferences["order"][group]=json!(ids); self.save(&state)?; Ok(json!({}))
            }
            "settings"|"saveSettings" => {
                let data=self.api.request(json!({"op":"gateway_settings"}))?; let mut settings=data["settings"].clone();
                let host=args.get("host").filter(|v| !v.is_null());
                if method=="settings" {
                    let url=if let Some(host)=host {array(&data["routes"]).iter().find(|r| r["host"]==*host && r["service"]==args["service"]).and_then(|r| r["url"].as_str()).unwrap_or_default().into()}else{format!("http://wispnet.localhost:{}/registry",settings["registry_port"])};
                    return Ok(json!({"port":settings["registry_port"],"url":url}));
                }
                let port=args["port"].as_u64().filter(|p| (1024..=65535).contains(p)).ok_or("Port must be between 1024 and 65535")?;
                let name=if host.is_none(){let name=text(args,"name")?.trim();if !label(name){return Err("Invalid device ID".into());}Some(name)}else{None};
                settings["registry_port"]=json!(port); settings["service_ports"]=json!([]); self.api.request(json!({"op":"configure_access","settings":settings}))?;
                if let Some(name)=name && self.api.request(json!({"op":"status"}))?["host_name"]!=name {self.rename_device(name)?;}
                Ok(json!({}))
            }
            "setServiceEnabled" => {
                let enabled=args["enabled"].as_bool().ok_or("Invalid service state")?;
                let status=self.api.request(json!({"op":"status"}))?;
                self.api.request(json!({"op":"mutate","operation":{"op":"set_service_enabled","expected_revision":status["host"]["revision"],"id":text(args,"service")?,"incarnation":text(args,"incarnation")?,"enabled":enabled}}))
            }
            "desktopStatus"|"ownerStatus" => self.api.request(json!({"op":"status","refresh_clients":true})),
            "desktopRequest"|"ownerRequest" => {let payload=&args["payload"];if !matches!(payload["op"].as_str(),Some("mutate"|"invite_qr"|"configure_public")){return Err("Unsupported owner operation".into());}self.api.request(payload.clone())}
            "desktopData"|"ownerData" => Ok(json!({"value":desktop_data(args,&self.config)?})),
            _ => Err("Unsupported desktop UI operation".into()),
        }
    }
    fn ssh_status(&self, host: &str) -> Result<Value> {
        if !ident(host) {
            return Err("Invalid Shell host".into());
        }
        let installed = command(
            Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
                .args(["ssh-bridge", "--help"]),
            None,
            5,
        )
        .is_ok_and(|out| String::from_utf8_lossy(&out).contains("--stdio"));
        let path = home()?.join(".ssh/config");
        let mut alias = None;
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                let meta = file.metadata().map_err(|e| e.to_string())?;
                if !meta.is_file()
                    || meta.uid() != unsafe { libc::geteuid() }
                    || meta.mode() & 0o022 != 0
                    || meta.len() > 1024 * 1024
                {
                    return Err("Cannot verify SSH alias: unsafe or oversized configuration".into());
                }
                let mut source = String::new();
                file.take(1024 * 1024 + 1)
                    .read_to_string(&mut source)
                    .map_err(|e| e.to_string())?;
                if source.len() > 1024 * 1024 {
                    return Err("Cannot verify SSH alias: oversized configuration".into());
                }
                let begin = format!("# Wispnet bridge {host} begin\n");
                let end = format!("# Wispnet bridge {host} end\n");
                if let Some((_, rest)) = source.split_once(&begin)
                    && let Some((block, _)) = rest.split_once(&end)
                    && block.contains(&format!("--host {host}"))
                    && block.contains("ssh-bridge")
                {
                    alias = block.lines().find_map(|line| {
                        line.strip_prefix("Host ")
                            .filter(|v| valid_ssh_alias(v))
                            .map(|v| format!("ssh {v}"))
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        Ok(json!({"installed":installed,"alias_configured":alias.is_some(),"command":alias}))
    }
    fn rename_device(&self, name: &str) -> Result<()> {
        let expected = home()?
            .join(".config/wispnet-node/host.toml")
            .canonicalize()
            .map_err(|_| "Rename requires the installed node configuration")?;
        if self.config != expected {
            return Err("Rename requires the installed node configuration".into());
        }
        let source = fs::read_to_string(&self.config).map_err(|e| e.to_string())?;
        // Keep comments and section order, removing only obsolete top-level IDs.
        let mut updated = format!(
            "device_id={}\n",
            serde_json::to_string(name).map_err(|e| e.to_string())?
        );
        let mut top = true;
        for line in source.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                top = false;
            }
            let key = trimmed.split_once('=').map(|(key, _)| key.trim());
            if top && matches!(key, Some("device_name" | "device_id")) {
                continue;
            }
            updated.push_str(line);
        }
        toml::from_str::<toml::Value>(&updated).map_err(|e| e.to_string())?;
        let mut file = tempfile::Builder::new()
            .prefix(".rename-")
            .tempfile_in(self.config.parent().ok_or("Invalid config parent")?)
            .map_err(|e| e.to_string())?;
        file.write_all(updated.as_bytes())
            .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&self.config).map_err(|e| e.to_string())?;
        self.lifecycle(false)?;
        self.lifecycle(true)
    }
}
fn valid_ssh_alias(alias: &str) -> bool {
    alias
        .strip_prefix("shell-")
        .and_then(|s| s.strip_suffix(".localhost"))
        .is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        })
}
fn integer_arg(args: &Value, key: &str, default: i64) -> Result<i64> {
    args.get(key).map_or(Ok(default), |v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| format!("Invalid {key}"))
    })
}
fn absolute_path(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.as_os_str().as_encoded_bytes().contains(&0)
        || !path.is_absolute()
        || path.file_name().is_none()
    {
        return Err("Choose an absolute local file path".into());
    }
    Ok(path)
}
fn relative_path(path: &str, allow_empty: bool) -> Result<()> {
    if path.contains(['\0', '\\'])
        || path.len() > 4096
        || (!allow_empty && path.is_empty())
        || (!path.is_empty()
            && path.split('/').any(|p| {
                p.is_empty() || p == "." || p == ".." || p.starts_with(".wispnet-") || p.len() > 255
            }))
    {
        return Err("Choose a relative path inside the folder".into());
    }
    Ok(())
}
fn version() -> String {
    env!("WISPNET_VERSION").into()
}
fn migrate_webkit(preferences: &mut Value) {
    let Ok(home) = home() else {
        return;
    };
    let directory = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    for path in [directory.join("app.wispnet.desktop/localstorage/tauri_localhost_0.localstorage"),home.join("Library/WebKit/app.wispnet.desktop/WebsiteData/LocalStorage/tauri_localhost_0.localstorage")] {
        let Ok(db)=rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else{continue;};
        let Ok(mut query)=db.prepare("SELECT value FROM ItemTable WHERE key='cardOrder'") else{continue;};
        let Ok(mut rows)=query.query([]) else{continue;};
        while let Ok(Some(row))=rows.next(){
            let source=match row.get_ref(0){
                Ok(rusqlite::types::ValueRef::Text(bytes))=>String::from_utf8(bytes.into()).ok(),
                Ok(rusqlite::types::ValueRef::Blob(bytes)) if bytes.len()%2==0=>String::from_utf16(&bytes.chunks_exact(2).map(|c|u16::from_le_bytes([c[0],c[1]])).collect::<Vec<_>>()).ok(),
                _=>None,
            };
            if let Some(source)=source && let Ok(Value::Object(order))=serde_json::from_str::<Value>(&source) {
                if !preferences["order"].is_object(){preferences["order"]=json!({});}preferences["order"].as_object_mut().unwrap().extend(order);
            }
        }
    }
}

pub fn run_args(args: &[OsString]) -> Result<()> {
    let config = match args {
        [] => home()?.join(".config/wispnet-node/host.toml"),
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            return Err("Usage: wispnet desktop-bridge [--config /absolute/path/host.toml]".into());
        }
    };
    #[cfg(target_os = "macos")]
    if args.is_empty() {
        crate::install::prepare_macos_desktop()?;
    }
    let bridge = Arc::new(Bridge::new(&config)?);
    let (sender, receiver) = mpsc::sync_channel::<Value>(24);
    let receiver = Arc::new(Mutex::new(receiver));
    let mut workers = Vec::new();
    for _ in 0..6 {
        let receiver = receiver.clone();
        let bridge = bridge.clone();
        workers.push(std::thread::spawn(move || {
            loop {
                let payload = match receiver.lock() {
                    Ok(receiver) => receiver.recv(),
                    Err(_) => return,
                };
                let Ok(payload) = payload else {
                    return;
                };
                let result = (|| {
                    let method = text(&payload, "method")?;
                    bridge.dispatch(method, payload.get("arguments").unwrap_or(&json!({})))
                })();
                let value = match result {
                    Ok(result) => json!({"id":payload["id"],"result":result}),
                    Err(error) => json!({"id":payload["id"],"error":error}),
                };
                let mut out = std::io::stdout().lock();
                if serde_json::to_writer(&mut out, &value).is_err()
                    || writeln!(out).is_err()
                    || out.flush().is_err()
                {
                    return;
                }
            }
        }));
    }
    let input = std::io::stdin();
    let mut input = input.lock();
    let outcome = (|| {
        loop {
            let mut line = Vec::new();
            let read = (&mut input)
                .take(65537)
                .read_until(b'\n', &mut line)
                .map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            if read > 65536 {
                return Err("Oversized desktop request".into());
            }
            let payload = serde_json::from_slice(&line).map_err(|_| "Invalid desktop request")?;
            sender
                .send(payload)
                .map_err(|_| "Desktop workers stopped")?;
        }
        Ok(())
    })();
    drop(sender);
    for worker in workers {
        worker.join().map_err(|_| "Desktop worker failed")?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufReader,
        os::unix::net::UnixListener,
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };

    struct Fixture {
        root: tempfile::TempDir,
        bridge: Arc<Bridge>,
        status: Arc<Mutex<Value>>,
        calls: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let state = root.path().join("state");
            fs::create_dir(&state).unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
            let config = root.path().join("host.toml");
            fs::write(&config, format!("state_dir={}\n", json!(state))).unwrap();
            let socket = state.join("node.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let status = Arc::new(Mutex::new(json!({
                "api_version":1,"host_name":"desktop","access_enabled":true,
                "host":{"host":"a".repeat(64),"revision":7,"services":[],
                    "clients":[{"peer":"b".repeat(64),"name":"Other machine"}]},
                "peer_pins":["b".repeat(64)],"relay":"connected","peer_telemetry":{},
                "invitations":[],"public_access":{"services":[]}
            })));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (server_status, server_calls, server_stop) =
                (status.clone(), calls.clone(), stop.clone());
            let worker = std::thread::spawn(move || {
                let mut handlers = Vec::new();
                while !server_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let status = server_status.clone();
                            let calls = server_calls.clone();
                            handlers.push(std::thread::spawn(move || {
                                let mut line = String::new();
                                BufReader::new(stream.try_clone().unwrap())
                                    .read_line(&mut line)
                                    .unwrap();
                                let request: Value = serde_json::from_str(&line).unwrap();
                                calls.lock().unwrap().push(request.clone());
                                let result = match request["op"].as_str().unwrap_or("") {
                                    "status" => status.lock().unwrap().clone(),
                                    "catalog" => json!({"host":request["host"],"services":status.lock().unwrap()["host"]["services"]}),
                                    "open" => json!({"url":"http://web.localhost:18355/"}),
                                    "diagnose_connection" => {
                                        std::thread::sleep(Duration::from_millis(300));
                                        json!({"selected_path":"direct"})
                                    }
                                    "gateway_settings" => json!({"settings":{"registry_port":18355,"service_ports":[{"port":18356}]},"routes":[]}),
                                    _ => json!({}),
                                };
                                writeln!(stream, "{}", json!({"ok":true,"result":result}))
                                    .unwrap();
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                for handler in handlers {
                    handler.join().unwrap();
                }
            });
            let bridge = Arc::new(Bridge::new(&config).unwrap());
            Self {
                root,
                bridge,
                status,
                calls,
                stop,
                worker: Some(worker),
            }
        }
        fn last(&self) -> Value {
            self.calls.lock().unwrap().last().unwrap().clone()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn snapshot_keeps_authoritative_local_publication_and_never_uses_caches_for_open() {
        let fixture = Fixture::new();
        let own = "a".repeat(64);
        let peer = "b".repeat(64);
        let incarnation = "c".repeat(64);
        fixture.status.lock().unwrap()["host"]["services"] = json!([
            {"id":"files","kind":"files","incarnation":incarnation},
            {"id":"web","kind":"http","incarnation":incarnation}
        ]);
        fixture.status.lock().unwrap()["public_access"]["services"] = json!([
            {"service":"web","incarnation":incarnation,"enabled":true,"url":"https://web.test/"}
        ]);
        let snapshot = fixture.bridge.snapshot().unwrap();
        let services = array(&snapshot["hosts"][&own]["catalog"]["services"]);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["public_access"]["url"], "https://web.test/");
        assert_eq!(snapshot["hosts"][&peer]["state"], "unverified");
        fixture
            .bridge
            .dispatch(
                "open",
                &json!({"host":peer,"service":"web","incarnation":incarnation}),
            )
            .unwrap();
        assert_eq!(fixture.last()["op"], "open");
        assert_eq!(fixture.last()["incarnation"], incarnation);
        // Offline state preserves display while a permission error fails closed.
        fs::remove_file(fixture.bridge.api.state.join("node.sock")).unwrap();
        let offline = fixture.bridge.snapshot().unwrap();
        assert_eq!(offline["enabled"], false);
        assert_eq!(offline["peer"], own);
        fs::write(fixture.bridge.api.state.join("node.sock"), "not a socket").unwrap();
        assert!(fixture.bridge.snapshot().is_err());
    }

    #[test]
    fn files_require_fresh_kind_and_incarnation_and_forward_recursive_native_jobs() {
        let fixture = Fixture::new();
        let own = "a".repeat(64);
        let incarnation = "c".repeat(64);
        let target = json!({"host":own,"service":"models","incarnation":incarnation});
        fixture.status.lock().unwrap()["host"]["services"] = json!([
            {"id":"models","kind":"file_share","incarnation":incarnation}
        ]);
        let destination = fixture.root.path().join("chosen folder");
        let mut args = target.clone();
        args["local_path"] = json!(destination);
        args["recursive"] = json!(true);
        fixture.bridge.dispatch("shareGet", &args).unwrap();
        assert_eq!(
            fixture.last(),
            json!({"op":"file_get","host":own,"service":"models","incarnation":incarnation,"path":"","local_path":destination,"resume":true,"recursive":true})
        );
        args["incarnation"] = json!("d".repeat(64));
        assert!(fixture.bridge.dispatch("shareGet", &args).is_err());
        fixture.status.lock().unwrap()["host"]["services"][0]["kind"] = json!("http");
        assert!(fixture.bridge.dispatch("shareList", &target).is_err());
        let source = fixture.root.path().join("data $(literal).bin");
        fs::write(&source, "file bytes stay in the node").unwrap();
        let link = fixture.root.path().join("link.bin");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        fixture.status.lock().unwrap()["host"]["services"] = json!([
            {"id":"incoming","kind":"file_inbox","incarnation":incarnation}
        ]);
        let mut upload = json!({"host":own,"service":"incoming","incarnation":incarnation,"local_path":source,"remote_path":"run/data.bin"});
        fixture.bridge.dispatch("fileSend", &upload).unwrap();
        assert_eq!(fixture.last()["op"], "file_put");
        assert_eq!(fixture.last()["workspace"], "");
        upload["local_path"] = json!(link);
        assert!(fixture.bridge.dispatch("fileSend", &upload).is_err());
        assert_eq!(
            fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c["op"] == "file_put")
                .count(),
            1
        );
    }

    #[test]
    fn owner_mutations_keep_revision_and_diagnostics_do_not_block_snapshot() {
        let fixture = Fixture::new();
        fixture
            .bridge
            .dispatch(
                "setServiceEnabled",
                &json!({"service":"shell","incarnation":"c".repeat(64),"enabled":false}),
            )
            .unwrap();
        assert_eq!(fixture.last()["operation"]["expected_revision"], 7);
        assert_eq!(fixture.last()["operation"]["enabled"], false);
        assert!(
            fixture
                .bridge
                .dispatch("desktopRequest", &json!({"payload":{"op":"shell"}}))
                .is_err()
        );
        let bridge = fixture.bridge.clone();
        let probe = std::thread::spawn(move || {
            bridge
                .dispatch("connectionDiagnostics", &json!({"host":"b".repeat(64)}))
                .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["op"] == "diagnose_connection")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        assert_eq!(fixture.bridge.snapshot().unwrap()["enabled"], true);
        assert!(start.elapsed() < Duration::from_millis(200));
        let report = probe.join().unwrap();
        assert!(report["network"].is_null());
        assert!(report["vpn"].is_null());
        assert_eq!(report["peers"][0]["report"]["selected_path"], "direct");
    }
    #[test]
    fn handoffs_only_allow_fixed_roots_without_credentials_queries_or_normalization() {
        for url in [
            "http://web.localhost:18000/",
            "http://web.localhost:80/",
            "http://registry.wispnet.localhost:18400/v1/services",
            "http://wispnet.localhost:18400/registry",
        ] {
            assert!(browser_url(url).is_ok(), "{url}");
        }
        for url in ["https://web.wispnet.test/", "https://web.wispnet.test:443/"] {
            assert!(public_browser_url(url).is_ok(), "{url}");
        }
        for url in [
            "https://web.wispnet.test",
            "https://user@web.wispnet.test/",
            "https://@web.wispnet.test/",
            "https://web.wispnet.test/?",
            "https://web.wispnet.test/#",
            "https://web.wispnet.test:8443/",
            "https://web.wispnet.test:/",
            "https://bad..test/",
            "https://web.wispnet.test\\evil/",
            "https://web.wispnet.test/\n",
            "http://web.wispnet.test/",
        ] {
            assert!(public_browser_url(url).is_err(), "{url}");
        }
        for url in [
            "http://user@web.localhost:18000/",
            "http://web.localhost:18000/path",
            "http://web.localhost:18000/?token=x",
            "http://web.localhost:18000/a/../",
            "http://web.localhost:18000/\\",
            "http://web.localhost/",
        ] {
            assert!(browser_url(url).is_err(), "{url}");
        }
    }
    #[test]
    fn paths_and_ids_do_not_allow_traversal_links_or_shell_commands() {
        for path in [
            "../private",
            "/etc/passwd",
            "a//b",
            "a/./b",
            "a/../b",
            ".wispnet-state",
            "bad\0path",
            "a\\b",
        ] {
            assert!(relative_path(path, false).is_err());
        }
        assert!(relative_path("run/model.bin", false).is_ok());
        assert!(relative_path("", true).is_ok());
        assert!(relative_path("", false).is_err());
        assert!(!label("main; touch /tmp/injected"));
        assert!(!ident("gggg"));
        assert!(ident(&"a".repeat(64)));
        assert_eq!(readable_device_id(" Desktop machine! "), "desktop-machine");
        assert_eq!(shell_quote("a'b $(literal)"), "'a'\\''b $(literal)'");
        assert!(valid_ssh_alias("shell-desktop.localhost"));
        assert!(!valid_ssh_alias("shell-desktop.localhost injected"));
    }
    #[test]
    fn validation_rejects_bad_types_before_side_effects() {
        assert!(bool_arg(&json!({"resume":"true"}), "resume", true).is_err());
        assert!(bool_arg(&json!({"recursive":1}), "recursive", false).is_err());
        assert!(absolute_path("relative/file").is_err());
        assert!(absolute_path("/").is_err());
        assert!(absolute_path("/tmp/bad\0path").is_err());
    }
}

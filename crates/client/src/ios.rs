//! iOS embedded-client adapter. Identity stays in Keychain in the native runner.
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Preferences {
    #[serde(default)]
    hosts: BTreeMap<String, Value>,
    #[serde(default)]
    order: BTreeMap<String, Value>,
    #[serde(default)]
    operations: Vec<Value>,
    #[serde(default)]
    name: Option<String>,
}

pub(crate) struct Adapter {
    state: PathBuf,
    preferences: Preferences,
    transfers: BTreeMap<String, Value>,
}

fn request(value: Value) -> Result<Value, String> {
    let response: Value = serde_json::from_str(&crate::call(&value.to_string()))
        .map_err(|_| "Invalid native response")?;
    if response["ok"] != true {
        return Err(response["error"]
            .as_str()
            .unwrap_or("Native operation failed")
            .into());
    }
    Ok(response["result"].clone())
}

impl Adapter {
    pub(crate) fn initialize(key: &str, state: PathBuf) -> Result<Self, String> {
        let preferences = match std::fs::read(state.join("ios-ui.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "Cannot read saved devices")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Preferences::default(),
            Err(_) => return Err("Cannot read saved devices".into()),
        };
        request(json!({"op":"init","key":key,"state":state,
            "relay":"https://relay.wispnet.app",
            "backup_relays":["https://usw1-1.relay.n0.iroh.link"]}))?;
        request(json!({"op":"stop"}))?;
        if let Some(name) = &preferences.name {
            request(json!({"op":"set_device_id","device_id":name}))?;
        }
        Ok(Self {
            state,
            preferences,
            transfers: BTreeMap::new(),
        })
    }

    fn save(&self) -> Result<(), String> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let path = self.state.join("ios-ui.json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| "Cannot save device preferences")?;
        file.write_all(&serde_json::to_vec(&self.preferences).map_err(|_| "Invalid preferences")?)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot save device preferences")?;
        std::fs::rename(path, self.state.join("ios-ui.json"))
            .map_err(|_| "Cannot save device preferences".into())
    }

    fn owner_targets(&self) -> Result<Vec<Value>, String> {
        let own = owner_status()?;
        let mut targets = vec![
            json!({"host":own["host"]["host"],"name":own["host_name"],"owner_access":"This device"}),
        ];
        for (host, row) in &self.preferences.hosts {
            if row["catalog"]["host"] == *host
                && row["catalog"]["services"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|s| s["kind"] == "shell" && s["enabled"] != false)
            {
                targets
                    .push(json!({"host":host,"name":row["name"],"owner_access":"Granted Shell"}));
            }
        }
        Ok(targets)
    }
    fn owner_data(&mut self, args: &Value) -> Result<Value, String> {
        let index = match crate::argument(args, "op")? {
            "targets" => return Ok(json!({"value":self.owner_targets()?})),
            "operations" => {
                return Ok(
                    json!({"value":self.preferences.operations.iter().rev().collect::<Vec<_>>()}),
                );
            }
            "start" => {
                let plan = &args["plan"];
                let _: wispnet_registry::combined::Action =
                    serde_json::from_value(plan["action"].clone())
                        .map_err(|_| "Invalid owner action")?;
                let selected = plan["targets"]
                    .as_array()
                    .filter(|v| !v.is_empty() && v.len() <= 32)
                    .ok_or("Choose up to 32 owner targets")?;
                let eligible = self.owner_targets()?;
                let mut included = std::collections::BTreeSet::new();
                let mut targets = vec![];
                for target in selected {
                    let host = crate::argument(target, "host")?;
                    let mut row = eligible
                        .iter()
                        .find(|v| v["host"] == host)
                        .ok_or("Review the available owner targets")?
                        .clone();
                    if !included.insert(host) {
                        return Err("Duplicate owner target".into());
                    }
                    row["state"] = json!("pending");
                    row["reason"] = json!("Not yet applied");
                    targets.push(row);
                }
                if self.preferences.operations.len() >= 100 {
                    return Err("Owner operation history is full".into());
                }
                let created = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                self.preferences.operations.push(json!({"id":crate::token(),"created_at":created,"action":plan["action"],"targets":targets}));
                self.save()?;
                self.preferences.operations.len() - 1
            }
            "retry" => self
                .preferences
                .operations
                .iter()
                .position(|v| v["id"] == args["id"])
                .ok_or("Unknown owner operation")?,
            _ => return Err("Unsupported owner operation".into()),
        };
        let record = self.preferences.operations[index].clone();
        for (target_index, target) in record["targets"]
            .as_array()
            .ok_or("Invalid owner operation")?
            .iter()
            .enumerate()
        {
            if target["state"] != "pending" {
                continue;
            }
            let receipt = request(
                json!({"op":"local_owner_apply","id":record["id"],"host":target["host"],"action":record["action"]}),
            );
            let row = &mut self.preferences.operations[index]["targets"][target_index];
            match receipt {
                Ok(receipt)
                    if receipt["id"] == record["id"]
                        && receipt["host"] == target["host"]
                        && matches!(
                            receipt["state"].as_str(),
                            Some("pending" | "applied" | "rejected")
                        ) =>
                {
                    row["state"] = receipt["state"].clone();
                    row["reason"] = receipt["reason"].clone();
                    row["revision"] = receipt["revision"].clone();
                }
                _ => row["reason"] = json!("Acknowledgement unconfirmed. Retry from history."),
            }
            self.save()?;
        }
        Ok(json!({"value":self.preferences.operations[index]}))
    }

    pub(crate) fn dispatch(&mut self, method: &str, args: Value) -> Result<Value, String> {
        let mut command = args.clone();
        match method {
            "snapshot" => {
                let owned = owner_status()?;
                let own = owned["host"]["host"]
                    .as_str()
                    .ok_or("Missing local identity")?;
                // Only authenticated catalog calls populate remote service tiles.
                let mut hosts = self.preferences.hosts.clone();
                for peer in owned["peer_pins"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    hosts.entry(peer.into()).or_insert_with(
                        || json!({"name":format!("device-{}", &peer[..8]),"state":"unreachable"}),
                    );
                }
                hosts.insert(own.into(), json!({"name":owned["host_name"],"local":true,
                    "state":"available","catalog":{"host":own,"services":owned["host"]["services"]},
                    "public_access":{"host":own,"version":1,"services":owned["public_access"]["services"]}}));
                let enabled = request(json!({"op":"access_status"}))?;
                let relay = request(json!({"op":"relay_status"}))?;
                return Ok(
                    json!({"platform":"ios","peer":own,"name":owned["host_name"],
                    "hosts":hosts,"enabled":enabled["enabled"],"relay":relay["relay"],
                    "network":"Network available","order":self.preferences.order,
                    "pendingRequests":owned["invitations"].as_array().into_iter().flatten().filter(|v|v["state"]=="pending").count(),
                    "telemetry":request(json!({"op":"telemetry"}))?}),
                );
            }
            "setAccess" => {
                command = json!({"op":if args["enabled"] == true {"enable"} else {"stop"}})
            }
            "preview" => {
                command = json!({"op":"preview_invitation","invitation":crate::argument(&args,"code")?.split_whitespace().collect::<String>()})
            }
            "pair" => {
                let invitation = crate::argument(&args, "code")?
                    .split_whitespace()
                    .collect::<String>();
                let target = request(json!({"op":"preview_invitation","invitation":invitation}))?;
                let own = request(json!({"op":"local_owner_status"}))?;
                request(
                    json!({"op":"pair","host":target["host"],"invitation":invitation,"name":own["host_name"]}),
                )?;
                let peer = crate::argument(&target, "host")?;
                self.preferences.hosts.insert(
                    peer.into(),
                    json!({"name":target["host_name"],"state":"unreachable","status":"Paired"}),
                );
                self.save()?;
                return Ok(json!({"host":peer,"name":target["host_name"]}));
            }
            "refreshHost" => {
                let peer = crate::argument(&args, "host")?;
                let row = self.preferences.hosts.entry(peer.into()).or_insert_with(|| json!({"name":format!("device-{}",peer.chars().take(8).collect::<String>())}));
                match request(json!({"op":"catalog","host":peer})) {
                    Ok(catalog) => {
                        row["catalog"] = catalog;
                        row["state"] = json!("available");
                        row["status"] = json!("Available");
                        if let Ok(name) = request(json!({"op":"host_device_id","host":peer})) {
                            row["name"] = name["device_id"].clone();
                        }
                        if let Ok(links) = request(json!({"op":"host_public_links","host":peer})) {
                            row["public_access"] = links;
                        }
                    }
                    Err(error) => {
                        row["state"] = json!("unverified");
                        row["status"] = json!(error);
                    }
                }
                self.save()?;
                return Ok(json!({}));
            }
            "removeHost" => {
                request(json!({"op":"unpair","host":args["host"]}))?;
                self.preferences
                    .hosts
                    .remove(crate::argument(&args, "host")?);
                self.save()?;
                return Ok(json!({}));
            }
            "saveOrder" => {
                let group = crate::argument(&args, "group")?;
                let ids = args["ids"].as_array().ok_or("Invalid card order")?;
                if ids.len() > 128
                    || !ids.iter().all(Value::is_string)
                    || !(group == "machines"
                        || group.strip_prefix("services/").is_some_and(|s| {
                            s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
                        }))
                {
                    return Err("Invalid card order".into());
                }
                self.preferences.order.insert(group.into(), json!(ids));
                self.save()?;
                return Ok(json!({}));
            }
            "launchPublic" => {
                let status = request(json!({"op":"public_status"}))?;
                let publication = status["services"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|p| p["service"] == args["service"] && p["enabled"] == true)
                    .ok_or("Public URL unavailable")?;
                return Ok(json!({"url":publication["url"]}));
            }
            "ownerStatus" => return owner_status(),
            "ownerData" => return self.owner_data(&args),
            "folderServicesStatus" | "sharesStatus" => {
                let status = owner_status()?;
                let services = status["host"]["services"]
                    .as_array()
                    .ok_or("Invalid local services")?;
                return Ok(
                    json!({"revision":status["host"]["revision"],"clients":status["host"]["clients"],
                    "inboxes":services.iter().filter(|s|s["kind"]=="file_inbox" || s["kind"]=="files").collect::<Vec<_>>(),
                    "shares":services.iter().filter(|s|s["kind"]=="file_share").collect::<Vec<_>>(),"file_exports":status["host"]["file_exports"]}),
                );
            }
            "saveShare" | "saveInbox" => {
                command["op"] = json!(if method == "saveShare" {
                    "local_share_configure"
                } else {
                    "local_inbox_configure"
                });
                command["root"] = args["folder"].clone();
            }
            "removeShare" | "removeInbox" => {
                command["op"] = json!(if method == "removeShare" {
                    "local_share_remove"
                } else {
                    "local_inbox_remove"
                })
            }
            "saveReceiving" => {
                let exports = args
                    .get("exports")
                    .cloned()
                    .unwrap_or_else(|| json!({"incoming":args["folder"]}));
                command = json!({"op":"local_files_configure","expected_revision":args["expected_revision"],"exports":exports,"allow":args["allow"]});
            }
            "receivingStatus" => {
                let status = request(json!({"op":"local_files_status"}))?;
                let folder = status["exports"]
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Ok(
                    json!({"revision":status["revision"],"clients":status["clients"],"allow":status["allow"],"folder":folder,"label":folder_label(folder),"available":std::path::Path::new(folder).is_dir()}),
                );
            }
            "ownerRequest" => {
                let payload = &args["payload"];
                command = match payload["op"].as_str() {
                    Some("mutate") => {
                        json!({"op":"local_owner_mutate","operation":payload["operation"]})
                    }
                    Some("invite_qr") => json!({"op":"local_owner_invite"}),
                    Some("configure_public") => {
                        let mut value = payload.clone();
                        value["op"] = json!("configure_public");
                        value["name"] = owner_status()?["host_name"].clone();
                        value
                    }
                    _ => return Err("Unsupported owner operation".into()),
                };
            }
            "setServiceEnabled" => {
                let status = request(json!({"op":"local_owner_status"}))?;
                command = json!({"op":"local_service_enabled","id":args["service"],"incarnation":args["incarnation"],"enabled":args["enabled"],"expected_revision":status["host"]["revision"]});
            }
            "settings" => {
                let data = request(json!({"op":"gateway_settings"}))?;
                let port = &data["settings"]["registry_port"];
                if args["host"].is_string() {
                    let route = data["routes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|r| r["host"] == args["host"] && r["service"] == args["service"]);
                    return Ok(
                        json!({"port":port,"url":route.map(|r|r["url"].clone()).unwrap_or(json!(""))}),
                    );
                }
                return Ok(
                    json!({"port":port,"url":format!("http://127.0.0.1:{port}/v1/services")}),
                );
            }
            "saveSettings" => {
                let port = args["port"]
                    .as_u64()
                    .filter(|p| (1024..=65535).contains(p))
                    .ok_or("Port must be between 1024 and 65535.")?;
                let name = crate::argument(&args, "name")?;
                if !wispnet_core::protocol::valid_service_id(name) {
                    return Err("Enter a valid device ID.".into());
                }
                let mut data = request(json!({"op":"gateway_settings"}))?;
                data["settings"]["registry_port"] = json!(port);
                request(json!({"op":"configure_access","settings":data["settings"]}))?;
                let result = request(json!({"op":"set_device_id","device_id":name}))?;
                self.preferences.name = Some(name.into());
                self.save()?;
                return Ok(result);
            }
            "networkChanged" => command = json!({"op":"network"}),
            "shellOpen" | "shellPoll" | "shellInput" | "shellResize" | "shellClose" => {
                let op = match method {
                    "shellOpen" => "shell_open",
                    "shellPoll" => "shell_poll",
                    "shellInput" => "shell_input",
                    "shellResize" => "shell_resize",
                    _ => "shell_close",
                };
                command["op"] = json!(op);
            }
            "saveTerminalLayout" => return Ok(json!({})),
            "resolveLink" => command["op"] = json!("resolve_link"),
            "open" | "shell_open" | "shell_input" | "shell_resize" | "shell_poll"
            | "shell_close" => command["op"] = json!(method),
            "fileWorkspaces" => command["op"] = json!("file_workspaces"),
            "fileStatus" => command["op"] = json!("file_status"),
            "fileCancel" => command["op"] = json!("file_cancel"),
            "fileList" => {
                let transfers = self
                    .transfers
                    .iter()
                    .filter_map(|(id, args)| {
                        request(json!({"op":"file_status","transfer":id}))
                            .ok()
                            .map(|status| {
                                let mut row = args.clone();
                                if let (Some(row), Some(status)) =
                                    (row.as_object_mut(), status.as_object())
                                {
                                    row.extend(status.clone());
                                }
                                row
                            })
                    })
                    .collect::<Vec<_>>();
                return Ok(json!({"transfers":transfers}));
            }
            "shareList" => command["op"] = json!("file_list"),
            "fileSend" => command["op"] = json!("file_put"),
            "shareGet" => {
                command["op"] = json!("file_get");
                let name = crate::argument(&args, "name")?;
                if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                    return Err("Invalid download name".into());
                }
                command["local_path"] =
                    json!(PathBuf::from(crate::argument(&args, "local_path")?).join(name));
            }
            "connectionDiagnostics" => {
                let mut peers = vec![];
                if let Some(host) = args["host"].as_str() {
                    let report = request(json!({"op":"diagnose_connection","host":host}));
                    peers.push(match report {Ok(report)=>json!({"host":host,"name":self.preferences.hosts.get(host).and_then(|r|r["name"].as_str()).unwrap_or(host),"report":report}),Err(error)=>json!({"host":host,"error":error})});
                }
                return Ok(json!({"network":"Unavailable","vpn":null,"peers":peers}));
            }
            _ => return Err(format!("{method} is not available on iOS yet")),
        }
        let result = request(command)?;
        if matches!(method, "fileSend" | "shareGet")
            && let Some(id) = result["transfer"].as_str()
        {
            self.transfers.insert(id.into(), args);
        }
        Ok(result)
    }
}

fn folder_label(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
fn owner_status() -> Result<Value, String> {
    let mut status = request(json!({"op":"local_owner_status"}))?;
    let host = status["host"].clone();
    let mut services = vec![];
    for mut service in host["services"]
        .as_array()
        .ok_or("Invalid local services")?
        .clone()
    {
        let id = crate::argument(&service, "id")?;
        let root = match service["kind"].as_str() {
            Some("shell") => continue,
            Some("file_inbox") => host["file_inboxes"][id].as_str(),
            Some("file_share") => host["file_shares"][id].as_str(),
            Some("files") => {
                let root = host["file_exports"]
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(Value::as_str);
                if root.is_none() {
                    continue;
                }
                root
            }
            _ => None,
        };
        if let Some(root) = root {
            service["folder"] = json!(root);
            service["label"] = json!(folder_label(root));
            service["available"] = json!(std::path::Path::new(root).is_dir());
        }
        services.push(service);
    }
    status["host"]["services"] = json!(services);
    Ok(status)
}

#[cfg(target_os = "ios")]
mod ffi {
    use super::*;
    use std::{
        ffi::{CStr, CString, c_char},
        sync::Mutex,
    };
    static ADAPTER: Mutex<Option<Adapter>> = Mutex::new(None);

    /// Native runner supplies valid NUL-terminated UTF-8; return is owned until free.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn wispnet_ios_call(input: *const c_char) -> *mut c_char {
        let result = std::panic::catch_unwind(|| -> Result<Value, String> {
            if input.is_null() {
                return Err("Invalid native request".into());
            }
            // SAFETY: the Swift caller holds its C string live throughout this call.
            let text = unsafe { CStr::from_ptr(input) }
                .to_str()
                .map_err(|_| "Invalid request encoding")?;
            if text.len() > 1024 * 1024 {
                return Err("Request too large".into());
            }
            let value: Value = serde_json::from_str(text).map_err(|_| "Invalid native request")?;
            let mut adapter = ADAPTER.lock().map_err(|_| "Native adapter unavailable")?;
            if value["method"] == "initialize" {
                if adapter.is_none() {
                    *adapter = Some(Adapter::initialize(
                        crate::argument(&value, "key")?,
                        PathBuf::from(crate::argument(&value, "state")?),
                    )?);
                }
                return Ok(json!({}));
            }
            adapter
                .as_mut()
                .ok_or("Native adapter not initialized")?
                .dispatch(
                    crate::argument(&value, "method")?,
                    value["arguments"].clone(),
                )
        })
        .unwrap_or_else(|_| Err("Native operation failed".into()));
        let value = match result {
            Ok(result) => json!({"ok":true,"result":result}),
            Err(error) => json!({"ok":false,"error":error}),
        };
        CString::new(value.to_string())
            .expect("JSON has no NUL")
            .into_raw()
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn wispnet_ios_free(value: *mut c_char) {
        if !value.is_null() {
            // SAFETY: the runner returns each allocation exactly once.
            drop(unsafe { CString::from_raw(value) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_preferences_fail_before_identity_initialization() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("ios-ui.json"), b"corrupt").unwrap();
        assert!(Adapter::initialize("invalid", directory.path().into()).is_err());
    }
    #[test]
    fn preferences_survive_restart_and_reject_invalid_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut adapter = Adapter {
            state: directory.path().into(),
            preferences: Preferences::default(),
            transfers: BTreeMap::new(),
        };
        adapter
            .dispatch("saveOrder", json!({"group":"machines","ids":["peer"]}))
            .unwrap();
        let saved: Preferences =
            serde_json::from_slice(&std::fs::read(directory.path().join("ios-ui.json")).unwrap())
                .unwrap();
        assert_eq!(saved.order["machines"], json!(["peer"]));
        assert!(
            adapter
                .dispatch("saveOrder", json!({"group":"../secret","ids":[]}))
                .is_err()
        );
        assert!(
            adapter
                .dispatch("saveOrder", json!({"group":"machines","ids":[42]}))
                .is_err()
        );
    }
}

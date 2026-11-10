//! Scriptable management of the running node, sharing the terminal/desktop API.
use crate::tui::{api::Api, service_links};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

pub const HELP: &str = "Management (uses the running node; --config is optional):
  wispnet node-status
  wispnet online | offline
  wispnet settings [--port 7842] [--device-id my-computer]
  wispnet shell-access [--allow PUBLIC_ID,PUBLIC_ID | --allow none]
  wispnet files-access [--allow PUBLIC_ID,PUBLIC_ID|none] [--export NAME=/absolute/path ... | --clear-exports]
  wispnet inbox [NAME] [--path /absolute/folder] [--allow PUBLIC_ID,PUBLIC_ID|none] [--remove]
  wispnet share [NAME] [--path /absolute/folder] [--allow PUBLIC_ID,PUBLIC_ID|none] [--remove]
  wispnet service-links --host HOST_ID --service SERVICE_ID
  wispnet resolve-link --link wispnet://HOST_ID/SERVICE_ID
  wispnet node-request --request /absolute/path/request.json
All management commands return JSON. node-request accepts the same owner-local
operations as the desktop: catalogs, pairing, revision-checked service/grant
mutations, access settings and public access. Request files must be owner-only.
Service links grant no permissions; pair and obtain access first.
Files requires a receiving folder before grants; permitted devices can send files
into it without confirming each transfer. Clearing all exports revokes access.";

pub fn handles(mode: &str) -> bool {
    matches!(
        mode,
        "node-status"
            | "online"
            | "offline"
            | "settings"
            | "service-links"
            | "resolve-link"
            | "node-request"
            | "shell-access"
            | "files-access"
            | "share"
            | "inbox"
    )
}
pub fn run(args: &[OsString]) -> Result<Value, String> {
    let mode = args[0].to_str().ok_or("Invalid command")?;
    if mode == "share" {
        return share_command(&args[1..]);
    }
    if mode == "inbox" {
        return folder_command(&args[1..], true);
    }
    let mut options = BTreeMap::new();
    let mut exports = BTreeMap::<String, String>::new();
    let mut clear_exports = false;
    let mut pairs = args[1..].iter();
    while let Some(arg) = pairs.next() {
        let key = arg.to_str().ok_or("Invalid option")?;
        if mode == "files-access" && key == "--clear-exports" && !clear_exports {
            clear_exports = true;
            continue;
        }
        let value = pairs.next().and_then(|v| v.to_str()).ok_or(HELP)?;
        if mode == "files-access" && key == "--export" {
            let (name, root) = value
                .split_once('=')
                .ok_or("Export must be NAME=/absolute/path")?;
            if !wispnet_core::protocol::valid_service_id(name)
                || !PathBuf::from(root).is_absolute()
                || exports.insert(name.into(), root.into()).is_some()
            {
                return Err(
                    "Export names must be unique lowercase identifiers and roots absolute paths"
                        .into(),
                );
            }
            continue;
        }
        let valid = key == "--config"
            || match mode {
                "settings" => matches!(key, "--port" | "--device-id"),
                "service-links" => matches!(key, "--host" | "--service"),
                "resolve-link" => key == "--link",
                "node-request" => key == "--request",
                "shell-access" | "files-access" => key == "--allow",
                _ => false,
            };
        if !valid || options.insert(key, value).is_some() {
            return Err(HELP.into());
        }
    }
    if clear_exports && !exports.is_empty() {
        return Err("Use --export or --clear-exports".into());
    }
    let config = options
        .get("--config")
        .map(PathBuf::from)
        .unwrap_or(crate::tui::api::home()?.join(".config/wispnet-node/host.toml"));
    let api = Api::new(config)?;
    let required = |key| {
        options
            .get(key)
            .copied()
            .ok_or_else(|| format!("Missing {key}\n{HELP}"))
    };
    match mode {
        "files-access" => {
            let status = api.request(json!({"op":"status"}))?;
            let service = status["host"]["services"]
                .as_array()
                .and_then(|ss| {
                    ss.iter()
                        .find(|s| s["id"] == "files" && s["kind"] == "files")
                })
                .ok_or("Built-in Files unavailable; update and restart the node")?;
            let current_exports = status["host"]["file_exports"].clone();
            if !options.contains_key("--allow") && exports.is_empty() && !clear_exports {
                return Ok(json!({"service":service,"exports":current_exports}));
            }
            let exports = if clear_exports || !exports.is_empty() {
                json!(exports)
            } else {
                current_exports
            };
            let allow = files_grants(
                options.get("--allow").copied(),
                &service["allow"],
                &exports,
                clear_exports,
            )?;
            let result = api.request(json!({"op":"mutate","operation":{"op":"files","allow":allow,"exports":exports,"expected_revision":status["host"]["revision"]}}))?;
            Ok(
                json!({"service":"files","revision":result["revision"],"allow":allow,"exports":exports}),
            )
        }
        "shell-access" => {
            let status = api.request(json!({"op":"status"}))?;
            let service = status["host"]["services"]
                .as_array()
                .and_then(|ss| {
                    ss.iter()
                        .find(|s| s["id"] == "shell" && s["kind"] == "shell")
                })
                .ok_or("Built-in Shell unavailable; update and restart the node")?;
            let Some(allow) = options.get("--allow") else {
                return Ok(service.clone());
            };
            let allow = if *allow == "none" {
                vec![]
            } else {
                allow
                    .split(',')
                    .map(|id| {
                        id.parse::<wispnet_core::identity::EndpointId>()
                            .map(|p| json!(p))
                            .map_err(|_| "Invalid allowed device ID")
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            let result=api.request(json!({"op":"mutate","operation":{"op":"shell","allow":allow,"expected_revision":status["host"]["revision"]}}))?;
            Ok(json!({"service":"shell","revision":result["revision"],"allow":allow}))
        }
        "node-status" => api.request(json!({"op":"status","refresh_clients":true})),
        "online" => {
            api.online(true)?;
            api.request(json!({"op":"enable"}))
        }
        "offline" => api.online(false),
        "node-request" => {
            let data =
                crate::tui::api::read_private(&PathBuf::from(required("--request")?), 16384)?;
            let request: Value = serde_json::from_str(&data).map_err(|_| "Invalid request JSON")?;
            if !request.is_object() || !request["op"].is_string() {
                return Err("Request must be an object with an op".into());
            }
            api.request(request)
        }
        "settings" => {
            let port = options
                .get("--port")
                .map(|s| {
                    s.parse::<u16>()
                        .map_err(|_| "Port must be between 1024 and 65535")
                })
                .transpose()?;
            if port.is_some_and(|p| p < 1024) {
                return Err("Port must be between 1024 and 65535".into());
            }
            if let Some(name) = options.get("--device-id")
                && !wispnet_core::protocol::valid_service_id(name)
            {
                return Err("Invalid device ID".into());
            }
            if let Some(name) = options.get("--device-id") {
                api.check_rename(name)?;
            }
            if let Some(port) = port {
                let mut settings =
                    api.request(json!({"op":"gateway_settings"}))?["settings"].clone();
                settings["registry_port"] = json!(port);
                settings["service_ports"] = json!([]);
                api.request(json!({"op":"configure_access","settings":settings}))?;
            }
            if let Some(name) = options.get("--device-id") {
                api.rename_device(name)?;
            }
            let mut result = api.request(json!({"op":"gateway_settings"}))?;
            result["device_id"] = api.request(json!({"op":"status"}))?["host_name"].clone();
            Ok(result)
        }
        "resolve-link" => {
            let link = required("--link")?;
            wispnet_core::service_link::ServiceLink::parse(link)?;
            api.request(json!({"op":"enable"}))?;
            api.request(json!({"op":"resolve_link","link":link}))
        }
        "service-links" => {
            let host = required("--host")?;
            let service = required("--service")?;
            wispnet_core::service_link::ServiceLink::parse(&format!("wispnet://{host}/{service}"))?;
            let status = api.request(json!({"op":"status"}))?;
            let catalog = if status["host"]["host"] == host {
                status["host"].clone()
            } else {
                api.request(json!({"op":"catalog","host":host}))?
            };
            let selected = catalog["services"]
                .as_array()
                .and_then(|a| a.iter().find(|s| s["id"] == service))
                .ok_or("Service unavailable or not permitted")?;
            if selected["kind"] == "tcp" {
                api.request(json!({"op":"enable"}))?;
                let route = api.request(json!({"op":"open","host":host,"service":service,"incarnation":selected["incarnation"]}))?;
                return Ok(
                    json!({"kind":"tcp","url":route["url"],"link":format!("wispnet://{host}/{service}")}),
                );
            }
            if selected["kind"] != "http" {
                return Err("Service has no connection links".into());
            }
            service_links(&api, host, selected)
        }
        _ => Err(HELP.into()),
    }
}

/// A folder removal revokes its grants; an explicit grant always requires roots.
fn files_grants(
    requested: Option<&str>,
    current: &Value,
    exports: &Value,
    clear_exports: bool,
) -> Result<Value, String> {
    let allow = match requested {
        None if clear_exports => json!([]),
        None => current.clone(),
        Some("none") => json!([]),
        Some(ids) => json!(
            ids.split(',')
                .map(|id| id
                    .parse::<wispnet_core::identity::EndpointId>()
                    .map_err(|_| "Invalid allowed device ID"))
                .collect::<Result<Vec<_>, _>>()?
        ),
    };
    if !allow.as_array().is_some_and(|peers| peers.is_empty())
        && exports.as_object().is_none_or(|roots| roots.is_empty())
    {
        return Err(
            "Set a shared folder with --export NAME=/absolute/path before granting Files access"
                .into(),
        );
    }
    Ok(allow)
}

/// Each named read-only folder share has independent grants and revocation.
fn share_command(args: &[OsString]) -> Result<Value, String> {
    folder_command(args, false)
}

fn folder_command(args: &[OsString], inbox: bool) -> Result<Value, String> {
    let kind = if inbox { "file_inbox" } else { "file_share" };
    let collection = if inbox { "inboxes" } else { "shares" };
    let roots_key = if inbox { "file_inboxes" } else { "file_shares" };
    let mut name = None;
    let mut root = None;
    let mut allow = None;
    let mut config = None;
    let mut remove = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str().ok_or("Invalid share argument")? {
            "--remove" if !remove => remove = true,
            "--path" if root.is_none() => {
                root = Some(args.next().and_then(|v| v.to_str()).ok_or(HELP)?.to_owned())
            }
            "--allow" if allow.is_none() => {
                allow = Some(args.next().and_then(|v| v.to_str()).ok_or(HELP)?.to_owned())
            }
            "--config" if config.is_none() => {
                config = Some(PathBuf::from(args.next().ok_or(HELP)?))
            }
            value if !value.starts_with('-') && name.is_none() => name = Some(value.to_owned()),
            _ => return Err(HELP.into()),
        }
    }
    if remove && (root.is_some() || allow.is_some()) {
        return Err("Use --remove without --path or --allow".into());
    }
    let api = Api::new(
        config.unwrap_or(crate::tui::api::home()?.join(".config/wispnet-node/host.toml")),
    )?;
    let status = api.request(json!({"op":"status"}))?;
    let shares = status["host"]["services"]
        .as_array()
        .ok_or("Missing host services")?
        .iter()
        .filter(|s| s["kind"] == kind)
        .cloned()
        .collect::<Vec<_>>();
    let Some(name) = name else {
        if root.is_some() || allow.is_some() || remove {
            return Err("A share name is required".into());
        }
        let mut result = json!({"roots":status["host"][roots_key]});
        result[collection] = json!(shares);
        return Ok(result);
    };
    if !wispnet_core::protocol::valid_service_id(&name)
        || matches!(name.as_str(), "shell" | "files")
    {
        return Err("Use a lowercase share identifier other than shell or files".into());
    }
    let existing = shares.iter().find(|s| s["id"] == name);
    if remove {
        if existing.is_none() {
            return Err("Folder service unavailable".into());
        }
        return api.request(json!({"op":"mutate","operation":{"op":"remove","id":name,"expected_revision":status["host"]["revision"]}}));
    }
    if root.is_none() && allow.is_none() {
        return existing
            .cloned()
            .map(|service| json!({"service":service,"root":status["host"][roots_key][&name]}))
            .ok_or_else(|| "Folder service unavailable; set --path to create it".into());
    }
    let root = root
        .or_else(|| status["host"][roots_key][&name].as_str().map(str::to_owned))
        .ok_or("Set --path to create the share")?;
    if !PathBuf::from(&root).is_absolute() {
        return Err("Share folder path must be absolute".into());
    }
    let allow = match allow.as_deref() {
        None => existing
            .map(|s| s["allow"].clone())
            .unwrap_or_else(|| json!([])),
        Some("none") => json!([]),
        Some(ids) => json!(
            ids.split(',')
                .map(|id| id
                    .parse::<wispnet_core::identity::EndpointId>()
                    .map_err(|_| "Invalid allowed device ID"))
                .collect::<Result<Vec<_>, _>>()?
        ),
    };
    let result=api.request(json!({"op":"mutate","operation":{"op":kind,"id":name,"root":root,"allow":allow,"expected_revision":status["host"]["revision"]}}))?;
    Ok(json!({"service":name,"kind":kind,"root":root,"allow":allow,"revision":result["revision"]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn files_grants_require_folders_and_clearing_folders_revokes_access() {
        let peer = wispnet_core::identity::Identity::generate()
            .endpoint_id()
            .to_string();
        let current = json!([peer]);
        assert!(
            files_grants(Some(&peer), &json!([]), &json!({}), false)
                .unwrap_err()
                .contains("shared folder")
        );
        assert_eq!(
            files_grants(None, &current, &json!({}), true).unwrap(),
            json!([])
        );
        assert!(files_grants(Some(&peer), &current, &json!({}), true).is_err());
        assert_eq!(
            files_grants(None, &current, &json!({"jobs":"/srv/jobs"}), false).unwrap(),
            current
        );
        assert_eq!(
            files_grants(Some("none"), &current, &json!({}), false).unwrap(),
            json!([])
        );
    }
}

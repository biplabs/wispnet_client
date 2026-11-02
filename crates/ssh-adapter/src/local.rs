//! The adapter uses the running node's owner-only socket, never another identity.
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
use wispnet_core::{config::Config, identity::EndpointId};

#[derive(Clone)]
pub(crate) struct Local {
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
}
impl Local {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let config = fs::canonicalize(path).map_err(|_| "Cannot find host configuration")?;
        let data = Config::load(&config).map_err(|e| e.to_string())?;
        if data.schema_version != 2 {
            return Err("SSH adapter requires an independent host configuration".into());
        }
        Ok(Self {
            config,
            state: data.state_dir,
        })
    }

    async fn connect(&self) -> Result<UnixStream, String> {
        let path = self.state.join("node.sock");
        let meta = fs::symlink_metadata(&path).map_err(|_| "Local Wispnet node is offline")?;
        let uid = unsafe { libc::geteuid() };
        if !meta.file_type().is_socket() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err("Node socket must be an owner-only socket".into());
        }
        let stream = UnixStream::connect(path).await.map_err(|e| e.to_string())?;
        if stream.peer_cred().map_err(|e| e.to_string())?.uid() != uid {
            return Err("Unexpected local node owner".into());
        }
        Ok(stream)
    }

    pub(crate) async fn request(&self, value: Value) -> Result<Value, String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut stream = self.connect().await?;
            request(&mut stream, &value).await
        })
        .await
        .map_err(|_| "Local node request timed out")?
    }

    pub(crate) async fn stream(&self, host: &str) -> Result<UnixStream, String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut stream = self.connect().await?;
            let opened = request(&mut stream, &json!({"op":"shell_stream","host":host})).await?;
            if opened["host"] != host {
                return Err("Node opened an unexpected Shell peer".into());
            }
            Ok(stream)
        })
        .await
        .map_err(|_| "Shell connection timed out")?
    }

    pub(crate) async fn resolve(&self, target: &str) -> Result<(String, String), String> {
        let status = self.request(json!({"op":"status"})).await?;
        if status["api_version"] != 1 {
            return Err("Unsupported local node API version".into());
        }
        let names = self.names();
        if let Ok(id) = target.parse::<EndpointId>() {
            let host = id.to_string();
            let name = if status["host"]["host"] == host {
                status["host_name"].as_str()
            } else {
                status["host"]["clients"].as_array().and_then(|clients| {
                    clients
                        .iter()
                        .find(|c| c["peer"] == host)
                        .and_then(|c| c["name"].as_str())
                })
            };
            let name = names
                .get(&host)
                .and_then(Value::as_str)
                .or(name)
                .unwrap_or(&host)
                .to_owned();
            return Ok((host, name));
        }
        let mut matches = std::collections::BTreeMap::new();
        if (target == "localhost" || status["host_name"] == target)
            && let Some(host) = status["host"]["host"].as_str()
        {
            matches.insert(
                host.to_owned(),
                status["host_name"]
                    .as_str()
                    .unwrap_or("localhost")
                    .to_owned(),
            );
        }
        if let Some(clients) = status["host"]["clients"].as_array() {
            for client in clients {
                if client["name"] == target
                    && let Some(id) = client["peer"].as_str()
                {
                    matches.insert(id.to_owned(), target.to_owned());
                }
            }
        }
        if let Some(names) = names.as_object() {
            for (id, name) in names {
                if name == target && id.parse::<EndpointId>().is_ok() {
                    matches.insert(id.clone(), target.to_owned());
                }
            }
        }
        if matches.len() != 1 {
            return Err("Device ID unknown or ambiguous; use its full public ID".into());
        }
        Ok(matches.into_iter().next().unwrap())
    }

    fn names(&self) -> Value {
        let mut names = private_json(&self.state.join("desktop-peer-names.json"), 65536)
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(ids) = private_json(&self.state.join("desktop-flutter.json"), 2 * 1024 * 1024)
            .and_then(|value| value["preferences"]["device_ids"].as_object().cloned())
        {
            for (peer, name) in ids {
                if name
                    .as_str()
                    .is_some_and(wispnet_core::protocol::valid_service_id)
                {
                    names[peer] = name;
                }
            }
        }
        names
    }
}

fn private_json(path: &Path, limit: u64) -> Option<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.len() > limit
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

async fn request(stream: &mut UnixStream, value: &Value) -> Result<Value, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > 16 * 1024 {
        return Err("Local request exceeds 16 KiB".into());
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|e| e.to_string())?;
    // Do not read beyond the newline: shell_stream switches to binary framing.
    let mut response = Vec::new();
    loop {
        let byte = stream.read_u8().await.map_err(|e| e.to_string())?;
        if byte == b'\n' {
            break;
        }
        if response.len() >= 2 * 1024 * 1024 {
            return Err("Oversized node response".into());
        }
        response.push(byte);
    }
    let response: Value = serde_json::from_slice(&response).map_err(|_| "Invalid node response")?;
    if response["ok"] != true {
        return Err(response["error"]
            .as_str()
            .unwrap_or("Node rejected the operation")
            .to_owned());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| io::Error::other("Missing node result").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn raw_stream_acknowledgement_does_not_consume_binary_payload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("node.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let local = Local {
            config: root.path().join("config"),
            state: root.path().to_owned(),
        };
        let host = "a".repeat(64);
        let expected = host.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while let Ok(byte) = stream.read_u8().await {
                if byte == b'\n' {
                    break;
                }
                request.push(byte);
            }
            assert_eq!(
                serde_json::from_slice::<Value>(&request).unwrap(),
                json!({"op":"shell_stream","host":expected})
            );
            let mut bytes =
                serde_json::to_vec(&json!({"ok":true,"result":{"host":expected}})).unwrap();
            bytes.extend_from_slice(b"\n\0\xff\nremaining");
            stream.write_all(&bytes).await.unwrap();
        });
        let mut stream = local.stream(&host).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"\0\xff\nremaining");
        server.await.unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            local
                .stream(&host)
                .await
                .unwrap_err()
                .contains("owner-only")
        );
    }
}

//! One-shot commands on the same authenticated service as interactive Shell.
use crate::Client;
use iroh::endpoint::Connection;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, watch},
    task::JoinHandle,
};
use wispnet_core::identity::EndpointId;
use wispnet_shell::{
    ClientFrame, MAX_EXEC_COMMAND_BYTES, MAX_EXEC_OUTPUT_BYTES, MAX_EXEC_TIMEOUT_MS, ServerFrame,
    read_server, write_client,
};

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub(crate) type BoxStream = Box<dyn Stream>;
pub(crate) struct Cancellation {
    pub(crate) host: EndpointId,
    pub(crate) cancel: watch::Sender<bool>,
}
pub(crate) type ExecRegistry = Arc<Mutex<BTreeMap<String, Cancellation>>>;

pub(crate) struct Registration {
    pub(crate) id: String,
    pub(crate) registry: ExecRegistry,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.try_lock() {
            registry.remove(&self.id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let registry = self.registry.clone();
            let id = self.id.clone();
            runtime.spawn(async move {
                registry.lock().await.remove(&id);
            });
        }
    }
}

#[derive(Default)]
pub(crate) struct Resources {
    connection: Option<Connection>,
    local: Option<JoinHandle<()>>,
}
impl Drop for Resources {
    fn drop(&mut self) {
        if let Some(task) = &self.local {
            task.abort();
        }
        if let Some(connection) = &self.connection {
            connection.close(0u32.into(), b"shell command complete or cancelled");
        }
    }
}

struct Options<'a> {
    host: &'a str,
    command: &'a str,
    cwd: Option<&'a str>,
    timeout_ms: u32,
    max_output_bytes: u32,
}
impl<'a> Options<'a> {
    fn parse(value: &'a Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("Invalid shell command arguments")?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "host" | "command" | "cwd" | "timeout_seconds" | "max_output_bytes"
            )
        }) {
            return Err("Unknown shell command argument".into());
        }
        let host = value["host"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 64 && !s.contains('\0'))
            .ok_or("Use a paired device ID or full public ID")?;
        let command = value["command"]
            .as_str()
            .filter(|s| {
                !s.trim().is_empty() && s.len() <= MAX_EXEC_COMMAND_BYTES && !s.contains('\0')
            })
            .ok_or("Command must be nonempty, contain no NUL bytes, and fit in 8192 bytes")?;
        let cwd = value
            .get("cwd")
            .map(|cwd| {
                cwd.as_str()
                    .filter(|s| Path::new(s).is_absolute() && s.len() <= 4096 && !s.contains('\0'))
                    .ok_or("Working directory must be an absolute path on the target")
            })
            .transpose()?;
        let seconds = match value.get("timeout_seconds") {
            Some(value) => value
                .as_u64()
                .filter(|n| *n > 0 && *n <= u64::from(MAX_EXEC_TIMEOUT_MS) / 1000)
                .ok_or("Command timeout must be between 1 and 300 seconds")?,
            None => 30,
        };
        let max_output_bytes = match value.get("max_output_bytes") {
            Some(value) => value
                .as_u64()
                .filter(|n| *n > 0 && *n <= u64::from(MAX_EXEC_OUTPUT_BYTES))
                .ok_or("Output limit must be between 1 and 1048576 bytes")?
                as u32,
            None => 65536,
        };
        Ok(Self {
            host,
            command,
            cwd,
            timeout_ms: seconds as u32 * 1000,
            max_output_bytes,
        })
    }
}

fn named_host(
    target: &str,
    own: EndpointId,
    own_name: Option<&str>,
    paired: impl IntoIterator<Item = (EndpointId, String)>,
) -> Result<EndpointId, String> {
    let mut matches = std::collections::BTreeSet::new();
    if target == "localhost" || own_name == Some(target) {
        matches.insert(own);
    }
    for (host, name) in paired {
        if name == target {
            matches.insert(host);
        }
    }
    if matches.len() != 1 {
        return Err("Device ID unknown or ambiguous; use its full public ID".into());
    }
    Ok(*matches.first().unwrap())
}

impl Client {
    pub(crate) async fn exec_host(&self, target: &str) -> Result<EndpointId, String> {
        let own = self.endpoint.id();
        let host = if let Ok(host) = target.parse() {
            host
        } else {
            let mut paired = Vec::new();
            let own_name = self.local_authority.as_ref().map(|a| a.device_id());
            if let Some(authority) = &self.local_authority {
                for client in authority
                    .snapshot()
                    .await
                    .map_err(|e| e.to_string())?
                    .clients
                {
                    if client.name == target && self.saved_host(client.peer).await?.is_some() {
                        paired.push((client.peer, client.name));
                    }
                }
            }
            named_host(target, own, own_name.as_deref(), paired)?
        };
        if host != own && self.saved_host(host).await?.is_none() {
            return Err("Pair with this host first".into());
        }
        Ok(host)
    }

    pub(crate) async fn run_shell_command(
        self: &Arc<Self>,
        value: &Value,
    ) -> Result<Value, String> {
        let options = Options::parse(value)?;
        let started = std::time::Instant::now();
        let _permit = self.shells.reserve_slot().await?;
        let host = tokio::time::timeout(Duration::from_secs(5), self.exec_host(options.host))
            .await
            .map_err(|_| "Device lookup timed out")??;
        let (cancel, mut cancelled) = watch::channel(false);
        let id = crate::token();
        // Serialize publication with local unpair/stop cleanup. Nothing is sent
        // to the target until the pin has been rechecked under this lock.
        let mut executions = self.shells.execs.lock().await;
        if host != self.endpoint.id() && self.saved_host(host).await?.is_none() {
            return Err("Pairing removed while starting shell command".into());
        }
        executions.insert(id.clone(), Cancellation { host, cancel });
        drop(executions);
        let _registration = Registration {
            id,
            registry: self.shells.execs.clone(),
        };
        let operation = self.exec_selected(host, &options);
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err("Shell command cancelled because the device was removed or access stopped; it may already have run".into()),
            result = tokio::time::timeout(Duration::from_millis(u64::from(options.timeout_ms)) + Duration::from_secs(15), operation) => {
                let mut result = result.map_err(|_| "Shell command connection timed out; the command may have run, so do not retry automatically")??;
                result["duration_ms"] = json!(started.elapsed().as_millis() as u64);
                Ok(result)
            }
        }
    }

    async fn exec_selected(
        self: &Arc<Self>,
        host: EndpointId,
        options: &Options<'_>,
    ) -> Result<Value, String> {
        let mut resources = Resources::default();
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            self.open_exec_stream(host, &mut resources),
        )
        .await
        .map_err(|_| "Shell command connection setup timed out")??;
        write_client(
            &mut stream,
            &ClientFrame::Exec {
                command: options.command.to_owned(),
                cwd: options.cwd.map(str::to_owned),
                timeout_ms: options.timeout_ms,
                max_output_bytes: options.max_output_bytes,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let receive = receive_result(&mut stream, host, options.max_output_bytes as usize);
        let pairing = async {
            if host == self.endpoint.id() {
                std::future::pending::<Result<Value, String>>().await
            } else {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if self.saved_host(host).await?.is_none() {
                        return Err(
                            "Pairing removed during shell command; the command may have run".into(),
                        );
                    }
                }
            }
        };
        tokio::select! { result = receive => result, result = pairing => result }
    }
    pub(crate) async fn open_exec_stream(
        self: &Arc<Self>,
        host: EndpointId,
        resources: &mut Resources,
    ) -> Result<BoxStream, String> {
        if host == self.endpoint.id() {
            #[cfg(target_os = "android")]
            return Err("Shell hosting is available on desktop computers".into());
            #[cfg(not(target_os = "android"))]
            {
                let authority = self
                    .local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?;
                let service = authority
                    .services()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|s| s.id == "shell" && s.kind == wispnet_core::host::ServiceKind::Shell)
                    .ok_or("Shell service unavailable")?;
                let manager = self.shells.local.clone();
                let scope = format!("{}/{}", service.incarnation, host);
                let (stream, server) = tokio::io::duplex(64 * 1024);
                let authority = authority.clone();
                resources.local = Some(tokio::spawn(async move {
                    let guard = async {
                        loop {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if !authority.services().await.is_ok_and(|services| {
                                services.iter().any(|s| {
                                    s.id == "shell" && s.incarnation == service.incarnation
                                })
                            }) {
                                manager.close_scope(&scope);
                                break;
                            }
                        }
                    };
                    tokio::select! { _ = guard => {}, _ = manager.serve(server, scope.clone()) => {} }
                }));
                return Ok(Box::new(stream));
            }
        }
        let catalog = self.catalog(host).await?;
        let service = catalog["services"].as_array().and_then(|services| {
            services.iter().find(|s| s["id"] == "shell" && s["kind"] == "shell")
        }).ok_or("Shell service unavailable or not permitted; grant this device Shell access on the target")?;
        let incarnation = service["incarnation"]
            .as_str()
            .ok_or("Invalid Shell service incarnation")?;
        let connection =
            wispnet_agent::tunnel::connect_target(&self.endpoint, self.target(host).await?)
                .await
                .map_err(|e| e.to_string())?;
        resources.connection = Some(connection.clone());
        let stream = wispnet_agent::tunnel::open_service_bound(&connection, "shell", incarnation)
            .await
            .map_err(|e| e.to_string())?;
        if self.saved_host(host).await?.is_none() {
            return Err("Pairing removed while starting shell command".into());
        }
        Ok(Box::new(stream))
    }
}

async fn receive_result(
    stream: &mut BoxStream,
    host: EndpointId,
    limit: usize,
) -> Result<Value, String> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    loop {
        let frame = read_server(stream).await.map_err(|e| format!("Shell command protocol failed: {e}; update the target if it uses an older Wispnet version"))?
            .ok_or("Shell command connection closed without a result; it may already have run, so do not retry automatically. Update the target if it uses an older Wispnet version")?;
        match frame {
            ServerFrame::Stdout { data } => append_output(&mut stdout, stderr.len(), &data, limit, &mut truncated),
            ServerFrame::Stderr { data } => append_output(&mut stderr, stdout.len(), &data, limit, &mut truncated),
            ServerFrame::ExecFinished { code, timed_out, output_truncated } => return Ok(json!({
                "host": host.to_string(), "stdout": String::from_utf8_lossy(&stdout),
                "stderr": String::from_utf8_lossy(&stderr), "exit_code": code,
                "timed_out": timed_out, "output_truncated": truncated || output_truncated,
            })),
            ServerFrame::Error { message } => return Err(message),
            _ => return Err("Unexpected response to shell command; update the target if it uses an older Wispnet version".into()),
        }
    }
}

fn append_output(
    output: &mut Vec<u8>,
    other_bytes: usize,
    data: &[u8],
    limit: usize,
    truncated: &mut bool,
) {
    let remaining = limit.saturating_sub(output.len() + other_bytes);
    *truncated |= data.len() > remaining;
    output.extend_from_slice(&data[..data.len().min(remaining)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn disabling_local_shell_cancels_commands_and_rejects_new_execution() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let client = Client::new_peer(
            wispnet_core::identity::SecretKey::generate(),
            state.path().into(),
            config,
        )
        .await
        .unwrap();
        let own = client.endpoint.id();
        let mut resources = Resources::default();
        let mut stream = client.open_exec_stream(own, &mut resources).await.unwrap();
        write_client(
            &mut stream,
            &ClientFrame::Exec {
                command: "printf started; sleep 30".into(),
                cwd: None,
                timeout_ms: 30000,
                max_output_bytes: 1024,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_server(&mut stream).await.unwrap(),
            Some(ServerFrame::Stdout { .. })
        ));
        let authority = client.local_authority.as_ref().unwrap();
        let snapshot = authority.snapshot().await.unwrap();
        let shell = snapshot.services.iter().find(|s| s.id == "shell").unwrap();
        client
            .command(
                &json!({"op":"local_service_enabled", "id":"shell", "incarnation":shell.incarnation,
            "expected_revision":snapshot.revision, "enabled":false}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while matches!(read_server(&mut stream).await, Ok(Some(_))) {}
        })
        .await
        .expect("disabled local command remained connected");
        assert!(
            client
                .open_exec_stream(own, &mut Resources::default())
                .await
                .is_err()
        );
        assert!(
            !authority
                .snapshot()
                .await
                .unwrap()
                .services
                .iter()
                .find(|s| s.id == "shell")
                .unwrap()
                .enabled
        );
        client.endpoint.close().await;
    }

    #[test]
    fn names_are_unique_and_local_alias_cannot_hide_a_remote_name() {
        let own = wispnet_core::identity::SecretKey::generate().public();
        let remote = wispnet_core::identity::SecretKey::generate().public();
        let other = wispnet_core::identity::SecretKey::generate().public();
        assert_eq!(
            named_host(
                "j-desktop",
                own,
                Some("j-strix"),
                [(remote, "j-desktop".into())]
            )
            .unwrap(),
            remote
        );
        assert!(
            named_host(
                "j-desktop",
                own,
                None,
                [(remote, "j-desktop".into()), (other, "j-desktop".into())]
            )
            .is_err()
        );
        assert!(named_host("localhost", own, None, [(remote, "localhost".into())]).is_err());
        assert!(named_host("missing", own, None, []).is_err());
        assert_eq!(named_host("localhost", own, None, []).unwrap(), own);
    }
    #[test]
    fn command_options_reject_invalid_bounds_before_execution() {
        let valid = json!({"host":"j-desktop","command":"printf '%s\\n' hello"});
        assert_eq!(Options::parse(&valid).unwrap().timeout_ms, 30000);
        assert_eq!(Options::parse(&valid).unwrap().max_output_bytes, 65536);
        for (key, value) in [
            ("command", json!("")),
            ("command", json!("\0")),
            ("cwd", json!("relative")),
            ("timeout_seconds", json!(0)),
            ("timeout_seconds", json!(301)),
            ("timeout_seconds", json!(1.5)),
            ("max_output_bytes", json!(0)),
            ("max_output_bytes", json!(1048577)),
            ("extra", json!(true)),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(Options::parse(&invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn client_enforces_shared_output_budget_even_if_host_ignores_it() {
        let host = wispnet_core::identity::SecretKey::generate().public();
        let (stream, mut server) = tokio::io::duplex(1024);
        let mut stream: BoxStream = Box::new(stream);
        let sender = tokio::spawn(async move {
            for frame in [
                ServerFrame::Stdout {
                    data: b"out".to_vec(),
                },
                ServerFrame::Stderr {
                    data: vec![255, 10, 65],
                },
                ServerFrame::Stdout { data: vec![0; 300] },
                ServerFrame::ExecFinished {
                    code: 7,
                    timed_out: true,
                    output_truncated: false,
                },
            ] {
                wispnet_shell::write_server(&mut server, &frame)
                    .await
                    .unwrap();
            }
        });
        let value = receive_result(&mut stream, host, 5).await.unwrap();
        assert_eq!(value["stdout"], "out");
        assert_eq!(value["stderr"], "\u{fffd}\n");
        assert_eq!(value["exit_code"], 7);
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["output_truncated"], true);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn missing_command_result_does_not_imply_success_or_trigger_retry() {
        let host = wispnet_core::identity::SecretKey::generate().public();
        let (stream, mut server) = tokio::io::duplex(1024);
        let mut stream: BoxStream = Box::new(stream);
        wispnet_shell::write_server(
            &mut server,
            &ServerFrame::Stdout {
                data: b"maybe ran".to_vec(),
            },
        )
        .await
        .unwrap();
        drop(server);
        assert!(
            receive_result(&mut stream, host, 65536)
                .await
                .unwrap_err()
                .contains("may already have run")
        );
    }
}

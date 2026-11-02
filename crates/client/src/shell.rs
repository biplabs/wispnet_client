//! Bounded native terminal sessions shared by the CLI and Android bridge.
use crate::{Client, argument, peer};
use base64::{Engine, engine::general_purpose::STANDARD};
use iroh::endpoint::Connection;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, mpsc, watch},
};
use wispnet_shell::{ClientFrame, ServerFrame, read_server, write_client};

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type BoxStream = Box<dyn Stream>;

pub(crate) struct Shells {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    slots: Arc<tokio::sync::Semaphore>,
    pub(crate) execs: crate::shell_exec::ExecRegistry,
    #[cfg(not(target_os = "android"))]
    pub(crate) local: wispnet_shell::SessionManager,
    #[cfg(not(target_os = "android"))]
    local_scopes: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    #[cfg(not(target_os = "android"))]
    local_cancel: watch::Sender<bool>,
}
impl Default for Shells {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
            slots: Arc::new(tokio::sync::Semaphore::new(16)),
            execs: Default::default(),
            #[cfg(not(target_os = "android"))]
            local: wispnet_shell::SessionManager::default(),
            #[cfg(not(target_os = "android"))]
            local_scopes: Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new())),
            #[cfg(not(target_os = "android"))]
            local_cancel: watch::channel(false).0,
        }
    }
}
impl Drop for Shells {
    fn drop(&mut self) {
        #[cfg(not(target_os = "android"))]
        {
            let _ = self.local_cancel.send(true);
        }
    }
}
struct Metadata {
    state: &'static str,
    echo: bool,
    canonical: bool,
}
struct Session {
    peer: wispnet_core::identity::EndpointId,
    _permit: tokio::sync::OwnedSemaphorePermit,
    commands: mpsc::Sender<ClientFrame>,
    events: Mutex<mpsc::Receiver<Value>>,
    metadata: Arc<Mutex<Metadata>>,
    last_poll: Arc<std::sync::Mutex<Instant>>,
    cancel: watch::Sender<bool>,
    connection: Option<Connection>,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}
fn size(value: &Value) -> Result<(u16, u16), String> {
    let cols = value["cols"].as_u64().unwrap_or(80);
    let rows = value["rows"].as_u64().unwrap_or(24);
    if cols > u16::MAX as u64
        || rows > u16::MAX as u64
        || !wispnet_shell::valid_size(cols as u16, rows as u16)
    {
        return Err("Terminal size must be between 2 and 1000".into());
    }
    Ok((cols as u16, rows as u16))
}
impl Client {
    pub(crate) async fn shell_open(self: &Arc<Self>, value: &Value) -> Result<Value, String> {
        let host = peer(value)?;
        let service = value["service"].as_str().unwrap_or("shell");
        let incarnation = argument(value, "incarnation")?;
        let name = value["session"].as_str().unwrap_or("main");
        if !wispnet_core::protocol::valid_service_id(service)
            || !wispnet_core::enrollment::valid_code(incarnation)
            || !wispnet_shell::valid_session(name)
        {
            return Err("Invalid shell service or session".into());
        }
        let (cols, rows) = size(value)?;
        // Reserve a slot before any connection/session side effect.
        let permit = self.shells.reserve_slot().await?;
        let connection;
        #[allow(unused_mut)] // Only desktop hosts spawn the local PTY adapter.
        let mut local_worker: Option<tokio::task::JoinHandle<()>> = None;
        let mut stream: BoxStream;
        if host == self.endpoint.id() {
            #[cfg(target_os = "android")]
            {
                return Err("Shell hosting is available on desktop computers".into());
            }
            #[cfg(not(target_os = "android"))]
            {
                connection = None;
                let authority = self
                    .local_authority
                    .as_ref()
                    .ok_or("Local host unavailable")?
                    .clone();
                let services = authority.services().await.map_err(|e| e.to_string())?;
                if !services.iter().any(|s| {
                    s.id == service
                        && s.incarnation == incarnation
                        && s.kind == wispnet_core::host::ServiceKind::Shell
                }) {
                    return Err("Service no longer permitted".into());
                }
                let (client, server) = tokio::io::duplex(64 * 1024);
                let manager = self.shells.local.clone();
                let scope = format!("{incarnation}:{host}");
                let id = service.to_owned();
                let inc = incarnation.to_owned();
                // This monitor outlives a detached client: disabling the service
                // must also terminate its persistent, currently unattended PTYs.
                if self
                    .shells
                    .local_scopes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(scope.clone())
                {
                    let manager = manager.clone();
                    let scope = scope.clone();
                    let scopes = self.shells.local_scopes.clone();
                    let mut stop = self.shells.local_cancel.subscribe();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {_=stop.changed()=>break,_=tokio::time::sleep(Duration::from_millis(500))=>{}}
                            if !authority.services().await.is_ok_and(|ss| {
                                ss.iter().any(|s| {
                                    s.id == id
                                        && s.incarnation == inc
                                        && s.kind == wispnet_core::host::ServiceKind::Shell
                                })
                            }) {
                                break;
                            }
                        }
                        manager.close_scope(&scope);
                        scopes
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&scope);
                    });
                }
                local_worker = Some(tokio::spawn(async move {
                    let _ = manager.serve(server, scope).await;
                }));
                stream = Box::new(client);
            }
        } else {
            if self.saved_host(host).await?.is_none() {
                return Err("Pair with this host first".into());
            }
            let catalog = self.catalog(host).await?;
            if !catalog["services"].as_array().is_some_and(|ss| {
                ss.iter().any(|s| {
                    s["id"] == service && s["incarnation"] == incarnation && s["kind"] == "shell"
                })
            }) {
                return Err("Service no longer permitted".into());
            }
            let conn =
                wispnet_agent::tunnel::connect_target(&self.endpoint, self.target(host).await?)
                    .await
                    .map_err(|e| e.to_string())?;
            let opened =
                wispnet_agent::tunnel::open_service_bound(&conn, service, incarnation).await;
            match opened {
                Ok(opened) => {
                    stream = Box::new(opened);
                    connection = Some(conn);
                }
                Err(e) => {
                    conn.close(0u32.into(), b"shell open failed");
                    return Err(e.to_string());
                }
            }
        }
        let attached = tokio::time::timeout(Duration::from_secs(5), async {
            write_client(
                &mut stream,
                &ClientFrame::Attach {
                    session: name.into(),
                    cols,
                    rows,
                },
            )
            .await?;
            match read_server(&mut stream).await? {
                Some(ServerFrame::Attached { session, username }) => Ok((session, username)),
                Some(ServerFrame::Error { message }) => Err(std::io::Error::other(message)),
                _ => Err(std::io::Error::other(
                    "Shell did not acknowledge attachment",
                )),
            }
        })
        .await;
        let (session_name, username) = match attached {
            Ok(Ok(v)) => v,
            other => {
                if let Some(task) = local_worker {
                    task.abort();
                }
                if let Some(c) = connection {
                    c.close(0u32.into(), b"shell attach failed");
                }
                return Err(match other {
                    Ok(Err(e)) => e.to_string(),
                    _ => "Shell attach timed out".into(),
                });
            }
        };
        // Serialize publication with unpair cleanup. An open that started
        // before removal must not publish a new handle after cleanup finished.
        let mut published = self.shells.sessions.lock().await;
        if host != self.endpoint.id() && self.saved_host(host).await?.is_none() {
            if let Some(task) = local_worker {
                task.abort();
            }
            if let Some(c) = connection {
                c.close(0u32.into(), b"pairing removed during shell open");
            }
            return Err("Pairing removed while opening terminal".into());
        }
        let handle = crate::token();
        let (commands, mut requests) = mpsc::channel(32);
        let (output, events) = mpsc::channel(32);
        let metadata = Arc::new(Mutex::new(Metadata {
            state: "attached",
            echo: false,
            canonical: false,
        }));
        let last_poll = Arc::new(std::sync::Mutex::new(Instant::now()));
        let (cancel, mut cancelled) = watch::channel(false);
        let session = Arc::new(Session {
            peer: host,
            _permit: permit,
            commands,
            events: Mutex::new(events),
            metadata: metadata.clone(),
            last_poll: last_poll.clone(),
            cancel,
            connection: connection.clone(),
        });
        let result = json!({"session":handle,"session_name":session_name,"username":username,"state":"attached","echo_enabled":false,"canonical":false,"path":connection.as_ref().map(wispnet_agent::diagnostics::selected_path).unwrap_or("local"),"latency_ms":connection.as_ref().and_then(wispnet_agent::diagnostics::selected_rtt_ms)});
        published.insert(handle, session);
        drop(published);
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(stream);
            let read_meta = metadata.clone();
            let read_output = output.clone();
            // Dedicated reader: framed reads must not be cancelled between bytes.
            let mut reader = tokio::spawn(async move {
                while let Some(frame) = read_server(&mut read).await? {
                    let mut meta = read_meta.lock().await;
                    let mut final_state = None;
                    let event = match frame {
                        ServerFrame::Output { data } => {
                            json!({"type":"output","data_b64":STANDARD.encode(data)})
                        }
                        ServerFrame::Scrollback { data } => {
                            json!({"type":"scrollback","data_b64":STANDARD.encode(data)})
                        }
                        ServerFrame::TerminalState { echo, canonical } => {
                            meta.echo = echo;
                            meta.canonical = canonical;
                            json!({"type":"terminal_state","echo":echo,"canonical":canonical})
                        }
                        ServerFrame::Exited { code } => {
                            final_state = Some("exited");
                            json!({"type":"exit","code":code})
                        }
                        ServerFrame::Error { message } => {
                            final_state = Some("disconnected");
                            json!({"type":"error","message":message})
                        }
                        ServerFrame::Pong => continue,
                        ServerFrame::Attached { .. }
                        | ServerFrame::ExecStarted
                        | ServerFrame::Stdout { .. }
                        | ServerFrame::Stderr { .. }
                        | ServerFrame::ExecFinished { .. } => {
                            return Err(std::io::Error::other("Unexpected terminal response"));
                        }
                    };
                    drop(meta);
                    if read_output.send(event).await.is_err() {
                        break;
                    }
                    if let Some(state) = final_state {
                        read_meta.lock().await.state = state;
                        break;
                    }
                }
                Ok::<_, std::io::Error>(())
            });
            let mut writer = tokio::spawn(async move {
                let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
                loop {
                    let frame = tokio::select! {
                        frame=requests.recv()=>match frame {Some(f)=>f,None=>break},
                        _=heartbeat.tick()=>{
                            if last_poll.lock().unwrap_or_else(|e|e.into_inner()).elapsed()>Duration::from_secs(120){break;}
                            ClientFrame::Ping
                        }
                    };
                    let closing = matches!(frame, ClientFrame::Detach | ClientFrame::Terminate);
                    tokio::time::timeout(Duration::from_secs(10), write_client(&mut write, &frame))
                        .await
                        .map_err(|_| std::io::Error::other("Shell write timed out"))??;
                    if closing {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            tokio::io::AsyncWriteExt::shutdown(&mut write),
                        )
                        .await;
                        break;
                    }
                }
                Ok::<_, std::io::Error>(())
            });
            let (outcome, wait_reader) = tokio::select! {
                r = &mut reader => (r, false),
                r = &mut writer => (r, true),
                _ = cancelled.changed() => (Ok(Ok(())), false),
            };
            if wait_reader {
                let _ = tokio::time::timeout(Duration::from_millis(200), &mut reader).await;
            }
            reader.abort();
            writer.abort();
            if let Some(task) = local_worker {
                let _ = tokio::time::timeout(Duration::from_millis(200), task).await;
            }
            if let Some(c) = connection {
                c.close(0u32.into(), b"terminal detached");
            }
            let mut meta = metadata.lock().await;
            if meta.state == "attached" {
                meta.state = "disconnected";
                let message = match outcome {
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "Terminal worker stopped".into(),
                    _ => "Terminal connection closed".into(),
                };
                drop(meta);
                let _ = output.try_send(json!({"type":"error","message":message}));
            }
        });
        Ok(result)
    }
}
impl Shells {
    pub(crate) async fn reserve_slot(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        self.sessions.lock().await.retain(|_, session| {
            session
                .last_poll
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                < Duration::from_secs(120)
        });
        self.slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Close a terminal or wait for a running command to finish".into())
    }
    pub(crate) async fn close_host(&self, host: wispnet_core::identity::EndpointId) {
        self.sessions
            .lock()
            .await
            .retain(|_, session| session.peer != host);
        self.execs.lock().await.retain(|_, execution| {
            if execution.host == host {
                let _ = execution.cancel.send(true);
                false
            } else {
                true
            }
        });
    }
    pub(crate) async fn close_all(&self) {
        self.sessions.lock().await.clear();
        self.execs.lock().await.clear();
    }
    pub(crate) async fn command(&self, value: &Value) -> Result<Value, String> {
        let id = argument(value, "session")?;
        if value["op"] == "shell_close" {
            let terminate = value["terminate"].as_bool().unwrap_or(false);
            let session = self.sessions.lock().await.remove(id);
            if let Some(s) = session {
                let frame = if terminate {
                    ClientFrame::Terminate
                } else {
                    ClientFrame::Detach
                };
                if s.commands.try_send(frame).is_ok() {
                    let _ =
                        tokio::time::timeout(Duration::from_millis(250), s.commands.closed()).await;
                }
            }
            return Ok(json!({"closed":true,"terminated":terminate}));
        }
        let session = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or("Terminal session is closed")?;
        *session.last_poll.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        if value["op"] == "shell_poll" {
            let mut events = session.events.lock().await;
            let mut items = Vec::new();
            let mut bytes = 0;
            while let Ok(event) = events.try_recv() {
                bytes += event.to_string().len();
                items.push(event);
                if bytes >= 64 * 1024 {
                    break;
                }
            }
            let metadata = session.metadata.lock().await;
            // Deliver all pending bytes before a native client reacts to EOF.
            let state = if events.is_empty() {
                metadata.state
            } else {
                "attached"
            };
            return Ok(
                json!({"events":items,"state":state,"echo_enabled":metadata.echo,"canonical":metadata.canonical,"path":session.connection.as_ref().map(wispnet_agent::diagnostics::selected_path).unwrap_or("local"),"latency_ms":session.connection.as_ref().and_then(wispnet_agent::diagnostics::selected_rtt_ms)}),
            );
        }
        if session.metadata.lock().await.state != "attached" {
            return Err("Terminal disconnected; reconnect to resume".into());
        }
        let frame = if value["op"] == "shell_input" {
            let encoded = argument(value, "data_b64")?;
            if encoded.len() > 12 * 1024 {
                return Err("Terminal input too large".into());
            }
            let data = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid terminal input")?;
            ClientFrame::Input { data }
        } else {
            let (cols, rows) = size(value)?;
            ClientFrame::Resize { cols, rows }
        };
        tokio::time::timeout(Duration::from_secs(5), session.commands.send(frame))
            .await
            .map_err(|_| "Terminal input timed out")?
            .map_err(|_| "Terminal disconnected; reconnect to resume")?;
        Ok(json!({"accepted":true}))
    }
}

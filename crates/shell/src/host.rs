// SPDX-License-Identifier: AGPL-3.0-only
//! PTY/session mechanics adapted from WispShell. Sessions are owned by the
//! manager, not the client connection, and are scoped by the caller's authority.
use crate::{ClientFrame, ServerFrame, read_client, valid_session, valid_size, write_server};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

const HISTORY_BYTES: usize = 1024 * 1024;
const REPLAY_BYTES: usize = 64 * 1024;
const MAX_SESSIONS: usize = 32;
type SessionKey = (String, String);
#[derive(Clone, Default)]
pub struct SessionManager {
    inner: Arc<ManagerState>,
}
struct ManagerState {
    sessions: Mutex<HashMap<SessionKey, Arc<Session>>>,
    shell: String,
    executors: crate::exec::Executors,
}
impl Default for ManagerState {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            shell: default_shell(),
            executors: crate::exec::Executors::default(),
        }
    }
}
impl Drop for ManagerState {
    fn drop(&mut self) {
        for session in self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            session.terminate();
        }
    }
}
struct Output {
    sequence: u64,
    data: Vec<u8>,
}
#[derive(Default)]
struct State {
    history: VecDeque<Output>,
    bytes: usize,
    next: u64,
    exited: Option<u32>,
    output_done: bool,
    terminated: bool,
    cursors: HashMap<u64, u64>,
    next_attachment: u64,
}
struct Session {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    termination: std::sync::mpsc::Sender<()>,
    state: Arc<Mutex<State>>,
    space: Arc<Condvar>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}
struct Attachment {
    state: Arc<Mutex<State>>,
    space: Arc<Condvar>,
    id: u64,
}
struct EphemeralSession {
    manager: Arc<ManagerState>,
    key: SessionKey,
    session: Arc<Session>,
}
impl Drop for EphemeralSession {
    fn drop(&mut self) {
        let mut sessions = self
            .manager
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if sessions
            .get(&self.key)
            .is_some_and(|session| Arc::ptr_eq(session, &self.session))
        {
            sessions.remove(&self.key);
        }
        self.session.terminate();
    }
}
impl Attachment {
    fn advance(&self, cursor: u64) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cursors
            .insert(self.id, cursor);
        self.space.notify_all();
    }
}
impl Drop for Attachment {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cursors
            .remove(&self.id);
        self.space.notify_all();
    }
}
fn error(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn pty_error(_: impl std::fmt::Display) -> io::Error {
    error("Cannot access shell terminal")
}
async fn send_frame(writer: &mut (impl AsyncWrite + Unpin), frame: &ServerFrame) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), write_server(writer, frame))
        .await
        .map_err(|_| error("Shell client is too slow"))?
}
fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
}
fn default_shell() -> String {
    #[cfg(unix)]
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| std::path::Path::new(s).is_absolute())
        .unwrap_or_else(|| "/bin/sh".into());
    #[cfg(windows)]
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    shell
}
fn shell_command(shell: &str, script: Option<&str>) -> CommandBuilder {
    let mut command = if let Some(script) = script {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command
    } else {
        CommandBuilder::new(shell)
    };
    command.env("TERM", "xterm-256color");
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = std::path::PathBuf::from(home);
        if home.is_absolute() && home.is_dir() {
            command.cwd(home);
        }
    }
    command
}
impl Session {
    fn signal(&self, name: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            let signal = crate::exec::signal_number(name)?;
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.terminated || state.exited.is_some() {
                return Ok(());
            }
            let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
            let group = master
                .process_group_leader()
                .filter(|group| *group > 0)
                .ok_or_else(|| error("Terminal process group unavailable"))?;
            // SAFETY: this is the live foreground group of our owned PTY.
            if unsafe { libc::kill(-group, signal) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(error("Terminal signals require a Unix desktop host"))
        }
    }
    fn terminate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminated {
            return;
        }
        state.terminated = true;
        drop(state);
        self.space.notify_all();
        let _ = self.termination.send(());
    }
    fn spawn(shell: &str, cols: u16, rows: u16, command: Option<&str>) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_error)?;
        let mut child = pair
            .slave
            .spawn_command(shell_command(shell, command))
            .map_err(pty_error)?;
        let (termination, termination_rx) = std::sync::mpsc::channel();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        let state = Arc::new(Mutex::new(State::default()));
        let output_state = state.clone();
        let space = Arc::new(Condvar::new());
        let output_space = space.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                let n = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(n) => n,
                };
                let mut state = output_state.lock().unwrap_or_else(|e| e.into_inner());
                // Preserve every unread byte for attached streams. Once the
                // bounded history fills, blocking this reader backpressures the
                // PTY and therefore the child instead of silently losing output.
                while state.bytes + n > HISTORY_BYTES && !state.terminated {
                    let evict = state.history.front().is_none_or(|old| {
                        state.cursors.values().all(|cursor| *cursor > old.sequence)
                    });
                    if evict {
                        if let Some(old) = state.history.pop_front() {
                            state.bytes -= old.data.len();
                        } else {
                            break;
                        }
                    } else {
                        state = output_space.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                }
                if state.terminated {
                    break;
                }
                let sequence = state.next;
                state.next += 1;
                state.bytes += n;
                state.history.push_back(Output {
                    sequence,
                    data: buffer[..n].to_vec(),
                });
            }
            output_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .output_done = true;
        });
        let exit_state = state.clone();
        std::thread::spawn(move || {
            let code = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status.exit_code(),
                    Err(_) => break 1,
                    Ok(None) => {}
                }
                match termination_rx.recv_timeout(Duration::from_millis(50)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // Keep signaling and reaping with the same child owner.
                        // Its kill implementation escalates ignored hangups;
                        // clone_killer only sends SIGHUP on Unix. This thread
                        // also keeps the grace period off the async runtime.
                        let _ = child.kill();
                        break child.wait().map(|status| status.exit_code()).unwrap_or(1);
                    }
                }
            };
            exit_state.lock().unwrap_or_else(|e| e.into_inner()).exited = Some(code);
        });
        Ok(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            termination,
            state,
            space,
        })
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if !valid_size(cols, rows) {
            return Err(error("Invalid terminal size"));
        }
        self.master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_error)
    }
    fn terminal_state(&self) -> (bool, bool) {
        #[cfg(unix)]
        if let Some(fd) = self
            .master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_raw_fd()
        {
            let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: fd belongs to the live PTY and tcgetattr initializes on success.
            if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } == 0 {
                let termios = unsafe { termios.assume_init() };
                return (
                    termios.c_lflag & libc::ECHO != 0,
                    termios.c_lflag & libc::ICANON != 0,
                );
            }
        }
        (false, false)
    }
}
struct ReaderTask(tokio::task::JoinHandle<()>);
impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl SessionManager {
    /// A configured account shell; the default uses the host account's shell.
    pub fn with_shell(shell: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(ManagerState {
                sessions: Mutex::new(HashMap::new()),
                shell: shell.into(),
                executors: crate::exec::Executors::default(),
            }),
        }
    }
    /// End sessions after owner revocation/removal; an ordinary mobile detach
    /// intentionally does not invoke this operation.
    pub fn close_scope(&self, scope: &str) {
        self.inner.executors.close_scope(scope);
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        sessions.retain(|(namespace, _), session| {
            if namespace == scope {
                session.terminate();
                false
            } else {
                true
            }
        });
    }
    /// Sweep detached as well as attached sessions when the owning service
    /// policy changes. The predicate is evaluated against an authoritative snapshot.
    pub fn retain_scopes(&self, mut allowed: impl FnMut(&str) -> bool) -> usize {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = sessions.len();
        sessions.retain(|(scope, _), session| {
            if allowed(scope) {
                true
            } else {
                session.terminate();
                false
            }
        });
        before - sessions.len() + self.inner.executors.retain_scopes(&mut allowed)
    }
    fn attach(
        &self,
        scope: String,
        name: String,
        cols: u16,
        rows: u16,
    ) -> io::Result<Arc<Session>> {
        self.attach_inner(scope, name, cols, rows, false, None)
    }
    fn attach_inner(
        &self,
        scope: String,
        name: String,
        cols: u16,
        rows: u16,
        ephemeral: bool,
        command: Option<&str>,
    ) -> io::Result<Arc<Session>> {
        if scope.is_empty() || scope.len() > 512 || !valid_session(&name) || !valid_size(cols, rows)
        {
            return Err(error("Invalid shell session"));
        }
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (scope, name);
        if ephemeral && sessions.contains_key(&key) {
            return Err(error("Ephemeral session name already in use"));
        }
        if sessions.get(&key).is_some_and(|s| {
            s.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exited
                .is_some()
        }) {
            sessions.remove(&key);
        }
        if let Some(session) = sessions.get(&key) {
            session.resize(cols, rows)?;
            return Ok(session.clone());
        }
        sessions.retain(|_, s| {
            s.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exited
                .is_none()
        });
        if sessions.len() >= MAX_SESSIONS {
            return Err(error("Shell session limit reached"));
        }
        let session = Arc::new(Session::spawn(&self.inner.shell, cols, rows, command)?);
        sessions.insert(key, session.clone());
        Ok(session)
    }
    /// Serve a single already-authorized shell stream. The caller must include
    /// service incarnation and authenticated peer in scope and monitor revocation.
    pub async fn serve<S>(&self, stream: S, scope: String) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let first = tokio::time::timeout(Duration::from_secs(5), read_client(&mut reader))
            .await
            .map_err(|_| error("Shell attach timeout"))??;
        if let Some(ClientFrame::ExecStream { command, cwd }) = first {
            return self
                .inner
                .executors
                .serve_stream(reader, writer, scope, command, cwd)
                .await;
        }
        if let Some(ClientFrame::Exec {
            command,
            cwd,
            timeout_ms,
            max_output_bytes,
        }) = first
        {
            return self
                .inner
                .executors
                .serve(
                    reader,
                    writer,
                    scope,
                    command,
                    cwd,
                    timeout_ms,
                    max_output_bytes,
                )
                .await;
        }
        let (name, cols, rows, ephemeral, command) = match first {
            Some(ClientFrame::Attach {
                session,
                cols,
                rows,
            }) => (session, cols, rows, false, None),
            Some(ClientFrame::AttachEphemeral {
                session,
                cols,
                rows,
                command,
            }) => {
                if command.as_ref().is_some_and(|command| {
                    command.is_empty()
                        || command.len() > crate::MAX_EXEC_COMMAND_BYTES
                        || command.contains('\0')
                }) {
                    return Err(error("Invalid PTY command"));
                }
                #[cfg(not(unix))]
                if command.is_some() {
                    return Err(error("PTY command execution requires a Unix desktop host"));
                }
                (session, cols, rows, true, command)
            }
            _ => return Err(error("First shell request must attach or start a command")),
        };
        let session = match if ephemeral {
            self.attach_inner(
                scope.clone(),
                name.clone(),
                cols,
                rows,
                true,
                command.as_deref(),
            )
        } else {
            self.attach(scope.clone(), name.clone(), cols, rows)
        } {
            Ok(s) => s,
            Err(e) => {
                send_frame(
                    &mut writer,
                    &ServerFrame::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                return Err(e);
            }
        };
        let session_key = (scope, name.clone());
        let _ephemeral = ephemeral.then(|| EphemeralSession {
            manager: self.inner.clone(),
            key: session_key.clone(),
            session: session.clone(),
        });
        send_frame(
            &mut writer,
            &ServerFrame::Attached {
                session: name,
                username: username(),
            },
        )
        .await?;
        let mut terminal_state = session.terminal_state();
        send_frame(
            &mut writer,
            &ServerFrame::TerminalState {
                echo: terminal_state.0,
                canonical: terminal_state.1,
            },
        )
        .await?;
        let (mut cursor, replay, attachment) = {
            let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut remaining = REPLAY_BYTES;
            let mut replay = Vec::new();
            for output in state.history.iter().rev() {
                if remaining == 0 {
                    break;
                }
                let start = output.data.len().saturating_sub(remaining);
                replay.push(output.data[start..].to_vec());
                remaining = remaining.saturating_sub(output.data.len());
            }
            replay.reverse();
            let id = state.next_attachment;
            state.next_attachment += 1;
            let cursor = state.next;
            state.cursors.insert(id, cursor);
            (
                cursor,
                replay.concat(),
                Attachment {
                    state: session.state.clone(),
                    space: session.space.clone(),
                    id,
                },
            )
        };
        if !replay.is_empty() {
            tokio::time::timeout(
                Duration::from_secs(10),
                send_frame(&mut writer, &ServerFrame::Scrollback { data: replay }),
            )
            .await
            .map_err(|_| error("Shell client is too slow"))??;
        }
        let (tx, mut commands) = mpsc::channel(8);
        // The parser/input worker never shares the output loop: large pasted
        // input must not stop us draining a PTY that is echoing that same input.
        let input_session = session.clone();
        let _reader = ReaderTask(tokio::spawn(async move {
            loop {
                let frame = match read_client(&mut reader).await {
                    Ok(Some(ClientFrame::Input { data })) => {
                        let session = input_session.clone();
                        match tokio::task::spawn_blocking(move || {
                            session
                                .writer
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .write_all(&data)
                        })
                        .await
                        {
                            Ok(Ok(())) => continue,
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(error("Shell input worker failed")),
                        }
                    }
                    Ok(Some(ClientFrame::Resize { cols, rows })) => {
                        match input_session.resize(cols, rows) {
                            Ok(()) => continue,
                            Err(e) => Err(e),
                        }
                    }
                    Ok(Some(ClientFrame::Signal { name })) => match input_session.signal(&name) {
                        Ok(()) => continue,
                        Err(e) => Err(e),
                    },
                    Ok(Some(ClientFrame::StdinEof)) => {
                        // A PTY cannot half-close its input; EOT asks a canonical
                        // terminal to deliver EOF, just as typing Ctrl+D does.
                        let session = input_session.clone();
                        match tokio::task::spawn_blocking(move || {
                            session
                                .writer
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .write_all(&[4])
                        })
                        .await
                        {
                            Ok(Ok(())) => continue,
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(error("Shell input worker failed")),
                        }
                    }
                    frame => frame,
                };
                let done = !matches!(frame, Ok(Some(_)));
                if tx.send(frame).await.is_err() || done {
                    break;
                }
            }
        }));
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut backlog = false;
        let mut state_tick = 0u8;
        loop {
            tokio::select! {
                biased;
                request=commands.recv()=>match request {
                    Some(Ok(Some(ClientFrame::Ping)))=>{send_frame(&mut writer,&ServerFrame::Pong).await?;state_tick=0;},
                    Some(Ok(Some(ClientFrame::Terminate)))=>{
                        self.inner
                            .sessions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&session_key);
                        session.terminate();
                        return Ok(());
                    }
                    Some(Ok(Some(ClientFrame::Detach)))|Some(Ok(None))|None=>return Ok(()),
                    Some(Err(e))=>return Err(e),
                    _=>return Err(error("Shell already attached")),
                },
                _=tokio::task::yield_now(),if backlog=>{},
                _=tick.tick(),if !backlog=>{}
            }
            let state = session.terminal_state();
            if state != terminal_state || state_tick == 0 {
                terminal_state = state;
                send_frame(
                    &mut writer,
                    &ServerFrame::TerminalState {
                        echo: state.0,
                        canonical: state.1,
                    },
                )
                .await?;
            }
            state_tick = state_tick.wrapping_add(1);
            let (data, next_cursor, exit, more) = {
                let state = session.state.lock().unwrap_or_else(|e| e.into_inner());
                let mut data = Vec::new();
                let mut next_cursor = cursor;
                for output in state.history.iter().filter(|o| o.sequence >= cursor) {
                    if data.len() + output.data.len() > crate::MAX_FRAME {
                        break;
                    }
                    data.extend_from_slice(&output.data);
                    next_cursor = output.sequence + 1;
                }
                let more = next_cursor < state.next;
                let exit = if state.output_done && !more {
                    state.exited
                } else {
                    None
                };
                (data, next_cursor, exit, more)
            };
            backlog = more;
            if !data.is_empty() {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    send_frame(&mut writer, &ServerFrame::Output { data }),
                )
                .await
                .map_err(|_| error("Shell client is too slow"))??;
                cursor = next_cursor;
                attachment.advance(cursor);
            }
            if let Some(code) = exit {
                send_frame(&mut writer, &ServerFrame::Exited { code }).await?;
                // Finish and acknowledge the send direction before dropping an
                // Iroh-backed stream; otherwise its cancellation guard resets
                // queued final output and the process exit status.
                tokio::time::timeout(Duration::from_secs(10), writer.shutdown())
                    .await
                    .map_err(|_| error("Shell completion timeout"))??;
                return Ok(());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{read_server, write_client};
    async fn attached(manager: &SessionManager, scope: &str) -> tokio::io::DuplexStream {
        let (mut client, server) = tokio::io::duplex(128 * 1024);
        let manager = manager.clone();
        let scope = scope.to_string();
        tokio::spawn(async move {
            let _ = manager.serve(server, scope).await;
        });
        write_client(
            &mut client,
            &ClientFrame::Attach {
                session: "main".into(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_server(&mut client).await.unwrap(),
            Some(ServerFrame::Attached { .. })
        ));
        client
    }
    async fn output_until(client: &mut tokio::io::DuplexStream, needle: &str) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            loop {
                match read_server(client).await.unwrap().unwrap() {
                    ServerFrame::Output { data } | ServerFrame::Scrollback { data } => {
                        output.extend(data)
                    }
                    ServerFrame::Error { message } => panic!("{message}"),
                    _ => {}
                }
                if String::from_utf8_lossy(&output).contains(needle) {
                    return output;
                }
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn ephemeral_pty_command_preserves_output_and_exit_then_removes_session() {
        let manager = SessionManager::with_shell("/bin/sh");
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move { host.serve(server, "ephemeral/peer".into()).await });
        write_client(
            &mut client,
            &ClientFrame::AttachEphemeral {
                session: "unique".into(),
                cols: 73,
                rows: 21,
                command: Some("stty size; printf 'EPHEMERAL_COMMAND'; exit 9".into()),
            },
        )
        .await
        .unwrap();
        let (output, code) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            loop {
                match read_server(&mut client).await.unwrap().unwrap() {
                    ServerFrame::Output { data } | ServerFrame::Scrollback { data } => {
                        output.extend(data)
                    }
                    ServerFrame::Exited { code } => break (output, code),
                    ServerFrame::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(code, 9);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("21 73"), "{output}");
        assert!(output.contains("EPHEMERAL_COMMAND"), "{output}");
        drop(client);
        worker.await.unwrap().unwrap();
        assert!(manager.inner.sessions.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn ephemeral_pty_disconnect_and_task_abort_terminate_session() {
        for abort in [false, true] {
            let manager = SessionManager::with_shell("/bin/sh");
            let (mut client, server) = tokio::io::duplex(4096);
            let host = manager.clone();
            let worker =
                tokio::spawn(async move { host.serve(server, "ephemeral/peer".into()).await });
            write_client(
                &mut client,
                &ClientFrame::AttachEphemeral {
                    session: "unique".into(),
                    cols: 80,
                    rows: 24,
                    command: Some("printf 'EPHEMERAL_READY'; sleep 30".into()),
                },
            )
            .await
            .unwrap();
            output_until(&mut client, "EPHEMERAL_READY").await;
            let session = manager
                .inner
                .sessions
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
            if abort {
                worker.abort();
                assert!(worker.await.is_err());
                drop(client);
            } else {
                drop(client);
                worker.await.unwrap().unwrap();
            }
            assert!(manager.inner.sessions.lock().unwrap().is_empty());
            tokio::time::timeout(Duration::from_secs(3), async {
                while session.state.lock().unwrap().exited.is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }
    #[tokio::test]
    async fn persistent_shell_resize_and_explicit_exit() {
        let manager = SessionManager::with_shell("/bin/sh");
        let mut client = attached(&manager, "incarnation/peer").await;
        write_client(
            &mut client,
            &ClientFrame::Input {
                data: b"WISP_TEST_VALUE=retained; printf '\\122\\105\\101\\104\\131\\012'\n"
                    .to_vec(),
            },
        )
        .await
        .unwrap();
        output_until(&mut client, "READY").await;
        write_client(&mut client, &ClientFrame::Detach)
            .await
            .unwrap();
        drop(client);
        let mut client = attached(&manager, "incarnation/peer").await;
        write_client(&mut client, &ClientFrame::Resize { cols: 71, rows: 19 })
            .await
            .unwrap();
        write_client(
            &mut client,
            &ClientFrame::Input {
                data: b"printf '\\160\\145\\162\\163\\151\\163\\164\\145\\144:%s\\n' \"$WISP_TEST_VALUE\"; stty size; exit 7\n".to_vec(),
            },
        )
        .await
        .unwrap();
        let (output, exit) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            loop {
                match read_server(&mut client).await.unwrap().unwrap() {
                    ServerFrame::Output { data } | ServerFrame::Scrollback { data } => {
                        output.extend(data)
                    }
                    ServerFrame::Exited { code } => break (output, code),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("persisted:retained"), "{output}");
        assert!(output.contains("19 71"), "{output}");
        assert_eq!(exit, 7);
        let mut client = attached(&manager, "incarnation/peer").await;
        write_client(
            &mut client,
            &ClientFrame::Input {
                data: b"printf 'value:%s\\n' \"${WISP_TEST_VALUE-unset}\"\n".to_vec(),
            },
        )
        .await
        .unwrap();
        output_until(&mut client, "value:unset").await;
    }
    #[tokio::test]
    async fn attached_fast_output_backpressures_without_losing_bytes() {
        const EXPECTED: usize = 3 * 1024 * 1024;
        let manager = SessionManager::with_shell("/bin/sh");
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move { host.serve(server, "fast/peer".into()).await });
        write_client(
            &mut client,
            &ClientFrame::Attach {
                session: "main".into(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_server(&mut client).await.unwrap(),
            Some(ServerFrame::Attached { .. })
        ));
        write_client(
            &mut client,
            &ClientFrame::Input {
                data: b"stty -echo; head -c 3145728 /dev/zero; exit 0\n".to_vec(),
            },
        )
        .await
        .unwrap();
        // Hold the small transport full so the PTY ring must apply backpressure.
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let sessions = manager.inner.sessions.lock().unwrap();
            let session = sessions.values().next().unwrap();
            let state = session.state.lock().unwrap();
            assert!(state.bytes <= HISTORY_BYTES);
            assert_eq!(state.cursors.len(), 1);
        }
        let zeroes = tokio::time::timeout(Duration::from_secs(10), async {
            let mut zeroes = 0;
            loop {
                match read_server(&mut client).await.unwrap().unwrap() {
                    ServerFrame::Output { data } | ServerFrame::Scrollback { data } => {
                        zeroes += data.iter().filter(|b| **b == 0).count();
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    ServerFrame::Exited { code } => {
                        assert_eq!(code, 0);
                        break zeroes;
                    }
                    ServerFrame::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(zeroes, EXPECTED);
        drop(client);
        assert!(worker.await.unwrap().is_ok());
    }
    #[tokio::test]
    async fn large_pasted_script_keeps_input_and_echo_output_flowing() {
        const COMMANDS: usize = 50_000;
        let manager = SessionManager::with_shell("/bin/sh");
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move { host.serve(server, "paste/peer".into()).await });
        write_client(
            &mut client,
            &ClientFrame::Attach {
                session: "main".into(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_server(&mut client).await.unwrap(),
            Some(ServerFrame::Attached { .. })
        ));
        let (mut read, mut write) = tokio::io::split(client);
        let input = tokio::spawn(async move {
            let script = format!(
                "{}exit 0\n",
                "printf '%s%s\\n' REVIEW_ PASTE\n".repeat(COMMANDS)
            );
            assert!(script.len() > 1024 * 1024);
            for data in script.as_bytes().chunks(8192) {
                write_client(
                    &mut write,
                    &ClientFrame::Input {
                        data: data.to_vec(),
                    },
                )
                .await
                .unwrap();
            }
            write
        });
        let output = tokio::time::timeout(Duration::from_secs(20), async {
            let mut output = Vec::new();
            loop {
                match read_server(&mut read).await.unwrap().unwrap() {
                    ServerFrame::Output { data } | ServerFrame::Scrollback { data } => {
                        output.extend(data)
                    }
                    ServerFrame::Exited { code } => {
                        assert_eq!(code, 0);
                        break output;
                    }
                    ServerFrame::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let write = input.await.unwrap();
        assert_eq!(
            output
                .windows(b"REVIEW_PASTE".len())
                .filter(|part| *part == b"REVIEW_PASTE")
                .count(),
            COMMANDS
        );
        drop(read);
        drop(write);
        assert!(worker.await.unwrap().is_ok());
    }
    #[tokio::test]
    async fn disconnect_removes_slow_cursor_and_unblocks_detached_output() {
        let manager = SessionManager::with_shell("/bin/sh");
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move {
            let _ = host.serve(server, "detach/peer".into()).await;
        });
        write_client(
            &mut client,
            &ClientFrame::Attach {
                session: "main".into(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_server(&mut client).await.unwrap(),
            Some(ServerFrame::Attached { .. })
        ));
        write_client(
            &mut client,
            &ClientFrame::Input {
                data: b"head -c 3145728 /dev/zero; exit 0\n".to_vec(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(client);
        worker.await.unwrap();
        let session = manager
            .inner
            .sessions
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                {
                    let state = session.state.lock().unwrap();
                    assert!(state.bytes <= HISTORY_BYTES);
                    if state.output_done && state.exited.is_some() {
                        assert!(state.cursors.is_empty());
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn revocation_sweep_terminates_detached_sessions_even_with_a_live_reference() {
        let manager = SessionManager::with_shell("/bin/sh");
        let session = manager
            .attach("inc/peer".into(), "main".into(), 80, 24)
            .unwrap();
        assert_eq!(manager.retain_scopes(|_| false), 1);
        assert!(manager.inner.sessions.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if session.state.lock().unwrap().exited.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let fresh = manager
            .attach("inc/peer".into(), "main".into(), 80, 24)
            .unwrap();
        assert!(!Arc::ptr_eq(&session, &fresh));
    }
    #[tokio::test]
    async fn revocation_terminates_a_session_that_ignores_hangup() {
        let manager = SessionManager::with_shell("/bin/sh");
        let session = manager
            .attach("inc/peer".into(), "main".into(), 80, 24)
            .unwrap();
        session
            .writer
            .lock()
            .unwrap()
            .write_all(b"trap '' HUP; printf '\\122\\105\\126\\117\\113\\105\\137\\122\\105\\101\\104\\131\\012'; while :; do :; done\n")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let ready = {
                    let state = session.state.lock().unwrap();
                    let output: Vec<_> = state
                        .history
                        .iter()
                        .flat_map(|part| &part.data)
                        .copied()
                        .collect();
                    output
                        .windows(b"REVOKE_READY".len())
                        .any(|part| part == b"REVOKE_READY")
                };
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.retain_scopes(|_| false), 1);
        assert!(manager.inner.sessions.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let finished = {
                    let state = session.state.lock().unwrap();
                    state.exited.is_some() && state.output_done
                };
                if finished {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn scopes_isolate_sessions_and_no_echo_state_is_reported() {
        let manager = SessionManager::with_shell("/bin/sh");
        let mut original = attached(&manager, "one/peer").await;
        write_client(
            &mut original,
            &ClientFrame::Input {
                data: b"WISP_SCOPE_VALUE=private; printf '\\122\\105\\101\\104\\131\\012'\n"
                    .to_vec(),
            },
        )
        .await
        .unwrap();
        output_until(&mut original, "READY").await;
        let mut other = attached(&manager, "two/peer").await;
        write_client(&mut other,&ClientFrame::Input {data:b"printf 'value:%s\\n' \"${WISP_SCOPE_VALUE-unset}\"; stty -echo -icanon; sleep 1; stty echo icanon\n".to_vec()}).await.unwrap();
        output_until(&mut other, "value:unset").await;
        write_client(&mut other, &ClientFrame::Ping).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(
                    read_server(&mut other).await.unwrap(),
                    Some(ServerFrame::TerminalState {
                        echo: false,
                        canonical: false
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn terminate_frame_ends_remote_session() {
        let manager = SessionManager::with_shell("/bin/sh");
        let mut first = attached(&manager, "inc/peer").await;
        write_client(
            &mut first,
            &ClientFrame::Input {
                data: b"WISP_TERMINATED=keep; printf 'SET\\n'\n".to_vec(),
            },
        )
        .await
        .unwrap();
        output_until(&mut first, "SET").await;
        write_client(&mut first, &ClientFrame::Terminate)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while read_server(&mut first).await.unwrap().is_some() {}
        })
        .await
        .unwrap();
        let mut second = attached(&manager, "inc/peer").await;
        write_client(
            &mut second,
            &ClientFrame::Input {
                data: b"printf 'after:%s\\n' \"${WISP_TERMINATED-gone}\"\n".to_vec(),
            },
        )
        .await
        .unwrap();
        output_until(&mut second, "after:gone").await;
    }
}

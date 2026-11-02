// SPDX-License-Identifier: AGPL-3.0-only
//! One-shot account commands. Unix commands use /bin/sh -c, not the interactive
//! account shell. Captured stdout/stderr share one bounded byte budget.
use crate::{
    MAX_EXEC_COMMAND_BYTES, MAX_EXEC_OUTPUT_BYTES, MAX_EXEC_TIMEOUT_MS, ServerFrame, write_server,
};
use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Semaphore, watch},
};

type Jobs = Arc<Mutex<HashMap<u64, (String, watch::Sender<bool>)>>>;
#[cfg(unix)]
pub(crate) fn signal_number(name: &str) -> io::Result<i32> {
    match name {
        "HUP" => Ok(libc::SIGHUP),
        "INT" => Ok(libc::SIGINT),
        "QUIT" => Ok(libc::SIGQUIT),
        "KILL" => Ok(libc::SIGKILL),
        "TERM" => Ok(libc::SIGTERM),
        "WINCH" => Ok(libc::SIGWINCH),
        _ => Err(io::Error::other("Unsupported process signal")),
    }
}
pub(crate) struct Executors {
    slots: Arc<Semaphore>,
    jobs: Jobs,
    next: AtomicU64,
}
impl Default for Executors {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(16)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        }
    }
}
impl Executors {
    pub(crate) fn close_scope(&self, scope: &str) {
        self.retain_scopes(&mut |current| current != scope);
    }
    pub(crate) fn retain_scopes(&self, allowed: &mut impl FnMut(&str) -> bool) -> usize {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let before = jobs.len();
        jobs.retain(|_, (scope, cancel)| {
            if allowed(scope) {
                true
            } else {
                let _ = cancel.send(true);
                false
            }
        });
        before - jobs.len()
    }
    pub(crate) async fn serve_stream<R, W>(
        &self,
        reader: R,
        mut writer: W,
        scope: String,
        command: String,
        cwd: Option<String>,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        if command.is_empty()
            || command.len() > MAX_EXEC_COMMAND_BYTES
            || command.contains('\0')
            || cwd
                .as_ref()
                .is_some_and(|p| p.len() > 4096 || p.contains('\0') || !Path::new(p).is_absolute())
        {
            return fail(&mut writer, "Invalid command or working directory").await;
        }
        let Ok(_permit) = self.slots.clone().try_acquire_owned() else {
            return fail(&mut writer, "Command execution capacity reached").await;
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = watch::channel(false);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (scope, cancel));
        let _registration = Registration {
            jobs: self.jobs.clone(),
            id,
        };
        #[cfg(not(unix))]
        {
            let _ = (reader, command, cwd, cancelled);
            fail(
                &mut writer,
                "Native command execution requires a Unix desktop host",
            )
            .await
        }
        #[cfg(unix)]
        {
            unix::serve_stream(reader, writer, command, cwd, cancelled).await
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn serve<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
        scope: String,
        command: String,
        cwd: Option<String>,
        timeout_ms: u32,
        max_output_bytes: u32,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        if command.is_empty()
            || command.len() > MAX_EXEC_COMMAND_BYTES
            || command.contains('\0')
            || !(1..=MAX_EXEC_TIMEOUT_MS).contains(&timeout_ms)
            || !(1..=MAX_EXEC_OUTPUT_BYTES).contains(&max_output_bytes)
            || cwd
                .as_ref()
                .is_some_and(|p| p.len() > 4096 || p.contains('\0') || !Path::new(p).is_absolute())
        {
            return fail(
                &mut writer,
                "Invalid command, working directory, timeout or output limit",
            )
            .await;
        }
        let Ok(_permit) = self.slots.clone().try_acquire_owned() else {
            return fail(&mut writer, "Command execution capacity reached").await;
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (cancel, mut cancelled) = watch::channel(false);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (scope, cancel));
        let _registration = Registration {
            jobs: self.jobs.clone(),
            id,
        };
        #[cfg(not(unix))]
        {
            let _ = (
                &mut reader,
                command,
                cwd,
                timeout_ms,
                max_output_bytes,
                &mut cancelled,
            );
            fail(
                &mut writer,
                "Native command execution requires a Unix desktop host",
            )
            .await
        }
        #[cfg(unix)]
        {
            let (mut events, job) = match unix::start(command, cwd, timeout_ms, max_output_bytes) {
                Ok(job) => job,
                Err(_) => {
                    return fail(
                        &mut writer,
                        "Cannot start command; check its working directory",
                    )
                    .await;
                }
            };
            let _job = AbortTask(job);
            let forward = async {
                while let Some(frame) = events.recv().await {
                    let done = matches!(frame, ServerFrame::ExecFinished { .. });
                    send(&mut writer, &frame).await?;
                    if done {
                        tokio::time::timeout(Duration::from_secs(10), writer.shutdown())
                            .await
                            .map_err(|_| io::Error::other("Command completion timed out"))??;
                        return Ok(());
                    }
                }
                Err(io::Error::other("Command worker stopped"))
            };
            let mut byte = [0];
            tokio::select! {
                result=forward=>result,
                _=reader.read(&mut byte)=>Err(io::Error::other("Command client disconnected")),
                _=cancelled.changed()=>Err(io::Error::other("Command access revoked")),
            }
        }
    }
}
struct Registration {
    jobs: Jobs,
    id: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}
struct AbortTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn send(writer: &mut (impl AsyncWrite + Unpin), frame: &ServerFrame) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), write_server(writer, frame))
        .await
        .map_err(|_| io::Error::other("Command client is too slow"))?
}
async fn fail(writer: &mut (impl AsyncWrite + Unpin), message: &str) -> io::Result<()> {
    send(
        writer,
        &ServerFrame::Error {
            message: message.into(),
        },
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), writer.shutdown())
        .await
        .map_err(|_| io::Error::other("Command error response timed out"))?
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::process::Stdio;
    use tokio::{
        process::{Child, Command},
        sync::mpsc,
    };
    struct Group(Mutex<i32>);
    impl Group {
        fn kill(&self) {
            let mut group = self.0.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: a positive freshly spawned PID is its dedicated process
            // group ID; the negative argument signals only that owned group.
            // The lock and clearing prevent delayed cleanup or input signaling
            // a subsequently reused PID after the process has been reaped.
            if *group > 0 {
                unsafe {
                    libc::kill(-*group, libc::SIGKILL);
                }
                *group = 0;
            }
        }
        fn signal(&self, name: &str) -> io::Result<()> {
            let signal = signal_number(name)?;
            let group = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if *group > 0 {
                // SAFETY: the group belongs to this live command, and cleanup
                // serializes with this signal before disarming its group ID.
                if unsafe { libc::kill(-*group, signal) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
            }
            Ok(())
        }
    }
    struct Process {
        child: Option<Child>,
        group: Arc<Group>,
    }
    impl Drop for Process {
        fn drop(&mut self) {
            self.group.kill();
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = child.wait().await;
                    });
                }
            }
        }
    }
    struct Capture {
        remaining: usize,
        truncated: bool,
    }
    async fn capture(
        mut pipe: impl AsyncRead + Unpin,
        stderr: bool,
        budget: Arc<Mutex<Capture>>,
        output: mpsc::Sender<ServerFrame>,
    ) {
        let mut buffer = [0; 8192];
        loop {
            let n = match pipe.read(&mut buffer).await {
                Ok(0) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => n,
            };
            let keep = {
                let mut budget = budget.lock().unwrap_or_else(|e| e.into_inner());
                let keep = budget.remaining.min(n);
                budget.remaining -= keep;
                budget.truncated |= keep < n;
                keep
            };
            if keep > 0 {
                let data = buffer[..keep].to_vec();
                let frame = if stderr {
                    ServerFrame::Stderr { data }
                } else {
                    ServerFrame::Stdout { data }
                };
                if output.send(frame).await.is_err() {
                    break;
                }
            }
        }
    }
    async fn stream_capture(
        mut pipe: impl AsyncRead + Unpin,
        stderr: bool,
        output: tokio::sync::mpsc::Sender<ServerFrame>,
    ) -> io::Result<()> {
        let mut bytes = [0; 8192];
        loop {
            let count = match pipe.read(&mut bytes).await {
                Ok(0) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            let data = bytes[..count].to_vec();
            let frame = if stderr {
                ServerFrame::Stderr { data }
            } else {
                ServerFrame::Stdout { data }
            };
            output
                .send(frame)
                .await
                .map_err(|_| io::Error::other("Command output closed"))?;
        }
    }
    pub(super) async fn serve_stream<R, W>(
        mut reader: R,
        mut writer: W,
        command: String,
        cwd: Option<String>,
        mut cancelled: watch::Receiver<bool>,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut builder = Command::new("/bin/sh");
        builder
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        } else if let Some(home) = std::env::var_os("HOME").filter(|p| Path::new(p).is_absolute()) {
            builder.current_dir(home);
        }
        let mut child = match builder.spawn() {
            Ok(child) => child,
            Err(_) => {
                return fail(
                    &mut writer,
                    "Cannot start command; check its working directory",
                )
                .await;
            }
        };
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| io::Error::other("Missing command process group"))?;
        let mut stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Missing command output"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("Missing command error output"))?;
        let mut process = Process {
            child: Some(child),
            group: Arc::new(Group(Mutex::new(group))),
        };
        send(&mut writer, &ServerFrame::ExecStarted).await?;
        let (output, mut events) = mpsc::channel(16);
        let capture_output = output.clone();
        let mut captures = AbortTask(tokio::spawn(async move {
            let (stdout, stderr) = tokio::join!(
                stream_capture(stdout, false, capture_output.clone()),
                stream_capture(stderr, true, capture_output)
            );
            stdout.and(stderr)
        }));
        let input_group = process.group.clone();
        let input_output = output.clone();
        let input = async move {
            loop {
                match crate::read_client(&mut reader).await? {
                    Some(crate::ClientFrame::Input { data }) => {
                        let Some(pipe) = &mut stdin else {
                            return Err(io::Error::other("Command stdin already closed"));
                        };
                        match tokio::time::timeout(Duration::from_secs(10), pipe.write_all(&data))
                            .await
                            .map_err(|_| io::Error::other("Command stdin is not consuming input"))?
                        {
                            Ok(()) => {}
                            // A process may close stdin early while still producing
                            // output; drain later input until the client sends EOF.
                            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Some(crate::ClientFrame::StdinEof) => {
                        stdin.take();
                    }
                    Some(crate::ClientFrame::Signal { name }) => input_group.signal(&name)?,
                    Some(crate::ClientFrame::Ping) => {
                        input_output
                            .send(ServerFrame::Pong)
                            .await
                            .map_err(|_| io::Error::other("Command output closed"))?;
                    }
                    Some(crate::ClientFrame::Terminate) => {
                        input_group.kill();
                        return Err(io::Error::other("Command terminated by client"));
                    }
                    None => return Err(io::Error::other("Command client disconnected")),
                    _ => return Err(io::Error::other("Invalid streaming command request")),
                }
            }
        };
        let supervise = async {
            let status = process.child.as_mut().expect("owned child").wait().await?;
            process.group.kill();
            process.child.take();
            // Once the supervised shell exits, inherited pipes cannot hold the
            // channel open indefinitely, even if a descendant escaped its group.
            tokio::time::timeout(Duration::from_secs(10), &mut captures.0)
                .await
                .map_err(|_| io::Error::other("Command output did not close"))?
                .map_err(|_| io::Error::other("Command output worker stopped"))??;
            use std::os::unix::process::ExitStatusExt;
            let code = status
                .code()
                .map(|code| code as u32)
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1) as u32);
            output
                .send(ServerFrame::ExecFinished {
                    code,
                    timed_out: false,
                    output_truncated: false,
                })
                .await
                .map_err(|_| io::Error::other("Command output closed"))?;
            std::future::pending::<io::Result<()>>().await
        };
        let forward = async {
            while let Some(frame) = events.recv().await {
                let done = matches!(frame, ServerFrame::ExecFinished { .. });
                send(&mut writer, &frame).await?;
                if done {
                    return tokio::time::timeout(Duration::from_secs(10), writer.shutdown())
                        .await
                        .map_err(|_| io::Error::other("Command completion timed out"))?;
                }
            }
            Err(io::Error::other("Command worker stopped"))
        };
        tokio::select! {
            result = forward => result,
            result = input => result,
            result = supervise => result,
            _ = cancelled.changed() => Err(io::Error::other("Command access revoked")),
        }
    }
    pub(super) fn start(
        command: String,
        cwd: Option<String>,
        timeout_ms: u32,
        max_output_bytes: u32,
    ) -> io::Result<(mpsc::Receiver<ServerFrame>, tokio::task::JoinHandle<()>)> {
        let mut builder = Command::new("/bin/sh");
        builder
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        } else if let Some(home) = std::env::var_os("HOME").filter(|p| Path::new(p).is_absolute()) {
            builder.current_dir(home);
        }
        let mut child = builder.spawn()?;
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| io::Error::other("Missing command process group"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Missing command output"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("Missing command error output"))?;
        let mut process = Process {
            child: Some(child),
            group: Arc::new(Group(Mutex::new(group))),
        };
        let budget = Arc::new(Mutex::new(Capture {
            remaining: max_output_bytes as usize,
            truncated: false,
        }));
        let (output, events) = mpsc::channel(16);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.into());
        let task = tokio::spawn(async move {
            let supervise = async {
                let group = &process.group;
                let child = process.child.as_mut().expect("owned child");
                let (status, timed_out) = tokio::select! {
                    status=child.wait()=>(status,false),
                    _=tokio::time::sleep_until(deadline)=>{
                        // Kill the whole group even when a descendant owns the
                        // output pipes or the main shell has already exited.
                        group.kill();
                        (child.wait().await,true)
                    }
                };
                group.kill();
                let code = status
                    .map(|s| {
                        s.code().map(|c| c as u32).unwrap_or_else(|| {
                            use std::os::unix::process::ExitStatusExt;
                            128 + s.signal().unwrap_or(1) as u32
                        })
                    })
                    .unwrap_or(1);
                process.child.take();
                (code, timed_out)
            };
            let captures = async {
                tokio::join!(
                    capture(stdout, false, budget.clone(), output.clone()),
                    capture(stderr, true, budget.clone(), output.clone())
                );
            };
            // The command deadline also bounds pipe draining. A deliberately
            // escaped process group can inherit pipes, but cannot hold this
            // request open indefinitely after the supervised shell has exited.
            let ((code, timed_out), drained) =
                tokio::join!(supervise, tokio::time::timeout_at(deadline, captures));
            let drain_timed_out = drained.is_err();
            let output_truncated =
                budget.lock().unwrap_or_else(|e| e.into_inner()).truncated || drain_timed_out;
            let timed_out = timed_out || drain_timed_out;
            let _ = output
                .send(ServerFrame::ExecFinished {
                    code,
                    timed_out,
                    output_truncated,
                })
                .await;
        });
        Ok((events, task))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{ClientFrame, SessionManager, read_server, write_client};
    async fn open(
        manager: &SessionManager,
        command: &str,
        cwd: Option<String>,
        timeout_ms: u32,
        limit: u32,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move { host.serve(server, "exec/peer".into()).await });
        write_client(
            &mut client,
            &ClientFrame::Exec {
                command: command.into(),
                cwd,
                timeout_ms,
                max_output_bytes: limit,
            },
        )
        .await
        .unwrap();
        (client, worker)
    }
    async fn open_stream(
        manager: &SessionManager,
        command: &str,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let (mut client, server) = tokio::io::duplex(4096);
        let host = manager.clone();
        let worker = tokio::spawn(async move { host.serve(server, "stream/peer".into()).await });
        write_client(
            &mut client,
            &ClientFrame::ExecStream {
                command: command.into(),
                cwd: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            read_server(&mut client).await.unwrap(),
            Some(ServerFrame::ExecStarted)
        );
        (client, worker)
    }
    async fn collect(client: &mut (impl AsyncRead + Unpin)) -> (Vec<u8>, Vec<u8>, u32, bool, bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            loop {
                match read_server(client).await.unwrap().unwrap() {
                    ServerFrame::Stdout { data } => stdout.extend(data),
                    ServerFrame::Stderr { data } => stderr.extend(data),
                    ServerFrame::ExecFinished {
                        code,
                        timed_out,
                        output_truncated,
                    } => return (stdout, stderr, code, timed_out, output_truncated),
                    frame => panic!("unexpected exec frame: {frame:?}"),
                }
            }
        })
        .await
        .unwrap()
    }
    fn running(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with("Z "))
        }) {
            return false;
        }
        unsafe { libc::kill(pid, 0) == 0 }
    }
    async fn stopped(pid: i32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while running(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn pid(client: &mut tokio::io::DuplexStream) -> i32 {
        let frame = tokio::time::timeout(Duration::from_secs(3), read_server(client))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ServerFrame::Stdout { data } = frame else {
            panic!("missing child pid")
        };
        String::from_utf8(data).unwrap().trim().parse().unwrap()
    }
    #[tokio::test]
    async fn native_exec_separates_streams_and_preserves_exit_without_pty() {
        let manager = SessionManager::default();
        let (mut client, worker) = open(
            &manager,
            "printf '\\000hello'; printf 'problem\\n' >&2; exit 7",
            None,
            1000,
            65536,
        )
        .await;
        let (out, err, code, timed_out, truncated) = collect(&mut client).await;
        assert_eq!(out, b"\0hello");
        assert_eq!(err, b"problem\n");
        assert_eq!(code, 7);
        assert!(!timed_out && !truncated);
        drop(client);
        assert!(worker.await.unwrap().is_ok());
    }
    #[tokio::test]
    async fn native_exec_handles_absolute_cwd_and_multiline_quoting() {
        let manager = SessionManager::default();
        let cwd = std::env::temp_dir();
        let (mut client, worker) = open(
            &manager,
            "value='two words'\nprintf '%s\\n' \"$value\"\npwd",
            Some(cwd.to_string_lossy().into()),
            1000,
            65536,
        )
        .await;
        let (out, err, code, _, _) = collect(&mut client).await;
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("two words\n{}\n", cwd.canonicalize().unwrap().display())
        );
        drop(client);
        worker.await.unwrap().unwrap();
        for (command, cwd, timeout, limit) in [
            ("echo no", Some("relative".into()), 1000, 100),
            ("", None, 1000, 100),
            ("true", None, 0, 100),
            ("true", None, 1000, 0),
            ("true", Some("/wispnet-missing-directory".into()), 1000, 100),
        ] {
            let (mut client, worker) = open(&manager, command, cwd, timeout, limit).await;
            assert!(matches!(
                read_server(&mut client).await.unwrap(),
                Some(ServerFrame::Error { .. })
            ));
            drop(client);
            worker.await.unwrap().unwrap();
        }
    }
    #[tokio::test]
    async fn native_exec_bounds_combined_output_and_drains_excess() {
        let manager = SessionManager::default();
        let (mut client, worker) = open(
            &manager,
            "head -c 2097152 /dev/zero; head -c 2097152 /dev/zero >&2",
            None,
            3000,
            12345,
        )
        .await;
        let (out, err, code, timed_out, truncated) = collect(&mut client).await;
        assert_eq!(out.len() + err.len(), 12345);
        assert_eq!(code, 0);
        assert!(!timed_out);
        assert!(truncated);
        drop(client);
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn native_exec_timeout_kills_background_descendants_and_closes_pipes() {
        let manager = SessionManager::default();
        let (mut client, worker) = open(
            &manager,
            "sleep 30 & printf '%s\\n' \"$!\"; wait",
            None,
            100,
            65536,
        )
        .await;
        let (out, _, code, timed_out, _) = collect(&mut client).await;
        let pid = String::from_utf8(out).unwrap().trim().parse().unwrap();
        assert!(timed_out);
        assert!(code >= 128);
        stopped(pid).await;
        drop(client);
        worker.await.unwrap().unwrap();
        // A shell that leaves a pipe-owning background process must not keep a
        // completed command alive until that process happens to close the pipe.
        let (mut client, worker) = open(
            &manager,
            "sleep 30 & printf '%s\\n' \"$!\"",
            None,
            1000,
            65536,
        )
        .await;
        let (out, _, code, timed_out, _) = collect(&mut client).await;
        let pid = String::from_utf8(out).unwrap().trim().parse().unwrap();
        assert_eq!(code, 0);
        assert!(!timed_out);
        stopped(pid).await;
        drop(client);
        worker.await.unwrap().unwrap();
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn escaped_pipe_owner_cannot_hold_request_open_past_deadline() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wispnet-exec-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("pid");
        let command = format!(
            "setsid sh -c 'printf \"%s\\n\" \"$$\" > \"{}\"; sleep 30' & while [ ! -s \"{}\" ]; do sleep 0.01; done",
            path.display(),
            path.display()
        );
        let manager = SessionManager::default();
        let (mut client, worker) = open(&manager, &command, None, 500, 65536).await;
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        struct Cleanup {
            pid: i32,
            dir: std::path::PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                }
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
        let _cleanup = Cleanup { pid, dir };
        let (_, _, code, timed_out, truncated) = collect(&mut client).await;
        assert_eq!(code, 0);
        assert!(timed_out && truncated);
        drop(client);
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn native_exec_disconnect_abort_and_revocation_kill_owned_process_group() {
        for mode in 0..3 {
            let manager = SessionManager::default();
            let (mut client, worker) = open(
                &manager,
                "sleep 30 & printf '%s\\n' \"$!\"; wait",
                None,
                30000,
                65536,
            )
            .await;
            let child = pid(&mut client).await;
            match mode {
                0 => {
                    drop(client);
                    assert!(worker.await.unwrap().is_err());
                }
                1 => {
                    worker.abort();
                    assert!(worker.await.is_err());
                    drop(client);
                }
                _ => {
                    manager.close_scope("exec/peer");
                    assert!(worker.await.unwrap().is_err());
                    drop(client);
                }
            }
            stopped(child).await;
        }
    }
    #[tokio::test]
    async fn streaming_exec_is_full_duplex_binary_and_preserves_stderr_and_exit() {
        let manager = SessionManager::default();
        let (client, worker) =
            open_stream(&manager, "cat; printf 'stream-error' >&2; exit 7").await;
        let bytes: Vec<u8> = (0..2 * 1024 * 1024 + 31).map(|n| n as u8).collect();
        let expected = bytes.clone();
        let (mut read, mut write) = tokio::io::split(client);
        let input = async {
            for data in bytes.chunks(8192) {
                write_client(
                    &mut write,
                    &ClientFrame::Input {
                        data: data.to_vec(),
                    },
                )
                .await
                .unwrap();
            }
            write_client(&mut write, &ClientFrame::StdinEof)
                .await
                .unwrap();
        };
        let (_, (out, err, code, timed_out, truncated)) = tokio::join!(input, collect(&mut read));
        assert_eq!(out, expected);
        assert_eq!(err, b"stream-error");
        assert_eq!(code, 7);
        assert!(!timed_out && !truncated);
        drop((read, write));
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn streaming_exec_disconnect_abort_revocation_and_invalid_control_kill_group() {
        for mode in 0..5 {
            let manager = SessionManager::default();
            let (mut client, worker) =
                open_stream(&manager, "sleep 30 & printf '%s\\n' \"$!\"; wait").await;
            let child = pid(&mut client).await;
            match mode {
                0 => drop(client),
                1 => {
                    worker.abort();
                    drop(client);
                }
                2 => {
                    manager.close_scope("stream/peer");
                    drop(client);
                }
                3 => {
                    write_client(
                        &mut client,
                        &ClientFrame::Signal {
                            name: "SIGKILL".into(),
                        },
                    )
                    .await
                    .unwrap();
                    // Keep the transport alive: invalid controls themselves
                    // must end the channel and terminate its owned processes.
                    stopped(child).await;
                    drop(client);
                }
                _ => {
                    write_client(&mut client, &ClientFrame::Terminate)
                        .await
                        .unwrap();
                    stopped(child).await;
                    drop(client);
                }
            }
            assert!(!matches!(worker.await, Ok(Ok(()))));
            stopped(child).await;
        }
    }
    #[tokio::test]
    async fn streaming_exec_signal_returns_normal_exit_metadata() {
        let manager = SessionManager::default();
        let (mut client, worker) =
            open_stream(&manager, "sleep 30 & printf '%s\\n' \"$!\"; wait").await;
        let child = pid(&mut client).await;
        write_client(
            &mut client,
            &ClientFrame::Signal {
                name: "TERM".into(),
            },
        )
        .await
        .unwrap();
        let (_, _, code, timed_out, truncated) = collect(&mut client).await;
        assert_eq!(code, 128 + libc::SIGTERM as u32);
        assert!(!timed_out && !truncated);
        stopped(child).await;
        drop(client);
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn streaming_exec_stalled_stdin_disconnect_has_bounded_cleanup() {
        let manager = SessionManager::default();
        let (mut client, worker) =
            open_stream(&manager, "sleep 30 & printf '%s\\n' \"$!\"; wait").await;
        let child = pid(&mut client).await;
        let input = tokio::spawn(async move {
            loop {
                if write_client(
                    &mut client,
                    &ClientFrame::Input {
                        data: vec![0; 65536],
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        });
        // The native reader eventually blocks on a child that never reads its
        // stdin. Cancel the client even if its own transport write is blocked.
        tokio::time::sleep(Duration::from_millis(100)).await;
        input.abort();
        assert!(input.await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_secs(12), worker)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        stopped(child).await;
    }
}

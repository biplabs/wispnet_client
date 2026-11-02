//! SSH channel translation lives entirely in this optional crate.
use crate::local::Local;
use russh::{
    Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Pty, Sig,
    keys::PrivateKey,
    server::{self, Msg, Session},
};
use std::{collections::BTreeMap, io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UnixStream,
    sync::oneshot,
    task::JoinHandle,
};
use wispnet_shell::{ClientFrame, ServerFrame, read_server, write_client};

pub(crate) async fn run(local: Local, host: String, key: PrivateKey) -> Result<(), String> {
    let config = Arc::new(server::Config {
        keys: vec![key],
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(100),
        auth_rejection_time_initial: Some(Duration::ZERO),
        max_auth_attempts: 3,
        channel_buffer_size: 8,
        event_buffer_size: 16,
        window_size: 256 * 1024,
        ..Default::default()
    });
    // This is a child process of the local SSH client, with no network listener.
    // Its inherited local-user authority permits auth_none; remote authority is
    // checked by the node separately for every native Shell stream.
    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let handler = Bridge {
        local,
        host,
        channels: BTreeMap::new(),
    };
    server::run_stream(config, stdio, handler)
        .await
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())
}

struct Entry {
    start: Option<oneshot::Sender<(UnixStream, bool)>>,
    pty: Option<(u16, u16)>,
    worker: JoinHandle<()>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
struct Bridge {
    local: Local,
    host: String,
    channels: BTreeMap<ChannelId, Entry>,
}
impl Bridge {
    async fn start(
        &mut self,
        id: ChannelId,
        command: Option<String>,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let Some(entry) = self
            .channels
            .get_mut(&id)
            .filter(|e| e.start.is_some() && !e.worker.is_finished())
        else {
            return session.channel_failure(id);
        };
        if command.as_ref().is_some_and(|command| {
            command.is_empty()
                || command.len() > wispnet_shell::MAX_EXEC_COMMAND_BYTES
                || command.contains('\0')
        }) {
            return session.channel_failure(id);
        }
        let pty = entry.pty.is_some();
        let request = if let Some((cols, rows)) = entry.pty {
            // A fresh ephemeral session belongs to this SSH channel alone.
            let name = format!("ssh-{}-{:x}", std::process::id(), rand::random::<u64>());
            ClientFrame::AttachEphemeral {
                session: name,
                cols,
                rows,
                command,
            }
        } else {
            ClientFrame::ExecStream {
                command: command.unwrap_or_else(|| "exec \"${SHELL:-/bin/sh}\"".into()),
                cwd: None,
            }
        };
        let opened = tokio::time::timeout(Duration::from_secs(30), async {
            let mut native = self.local.stream(&self.host).await?;
            write_client(&mut native, &request)
                .await
                .map_err(|e| e.to_string())?;
            match read_server(&mut native).await.map_err(|e| e.to_string())? {
                Some(ServerFrame::Attached { .. }) if pty => Ok(native),
                Some(ServerFrame::ExecStarted) if !pty => Ok(native),
                Some(ServerFrame::Error { message }) => Err(message),
                _ => Err(
                    "Target did not acknowledge streaming Shell; update its Wispnet node".into(),
                ),
            }
        })
        .await;
        let native = match opened {
            Ok(Ok(native)) => native,
            failure => {
                let message = match failure {
                    Ok(Err(message)) => message,
                    _ => "Shell startup timed out".into(),
                };
                eprintln!("wispnet: {message}");
                return session.channel_failure(id);
            }
        };
        session.channel_success(id)?;
        // The channel worker has drained pre-start control messages since open.
        // A failed receiver means the client already closed this channel.
        let _ = entry.start.take().unwrap().send((native, pty));
        Ok(())
    }
}

impl server::Handler for Bridge {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.retain(|_, entry| !entry.worker.is_finished());
        if self.channels.len() < 16 {
            let id = channel.id();
            let (input, output) = channel.split();
            let (start, ready) = oneshot::channel();
            let worker = tokio::spawn(channel_worker(input, output, ready));
            self.channels.insert(
                id,
                Entry {
                    start: Some(start),
                    pty: None,
                    worker,
                },
            );
            reply.accept().await;
        }
        // Dropping an unaccepted reply rejects the channel.
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        id: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _pixel_width: u32,
        _pixel_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = dimensions(cols, rows);
        if let Some(entry) = self
            .channels
            .get_mut(&id)
            .filter(|e| e.start.is_some() && !e.worker.is_finished() && e.pty.is_none())
            && let Some(size) = size
        {
            entry.pty = Some(size);
            session.channel_success(id)
        } else {
            session.channel_failure(id)
        }
    }

    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(id, None, session).await
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match std::str::from_utf8(data) {
            Ok(command) => self.start(id, Some(command.to_owned()), session).await,
            Err(_) => session.channel_failure(id),
        }
    }

    async fn env_request(
        &mut self,
        id: ChannelId,
        _name: &str,
        _value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(id)
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(id)
    }

    async fn channel_close(
        &mut self,
        id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Aborting the worker closes the native stream; ephemeral PTYs and
        // streaming processes are owned by that stream and terminate remotely.
        self.channels.remove(&id);
        Ok(())
    }
}

fn dimensions(cols: u32, rows: u32) -> Option<(u16, u16)> {
    let cols = u16::try_from(if cols == 0 { 80 } else { cols }).ok()?;
    let rows = u16::try_from(if rows == 0 { 24 } else { rows }).ok()?;
    wispnet_shell::valid_size(cols, rows).then_some((cols, rows))
}

async fn channel_worker(
    mut input: ChannelReadHalf,
    output: ChannelWriteHalf<Msg>,
    mut ready: oneshot::Receiver<(UnixStream, bool)>,
) {
    let (native, pty) = loop {
        tokio::select! {
            biased;
            result = &mut ready => match result {
                Ok(selected) => break selected,
                Err(_) => return,
            },
            message = input.wait() => match message {
                // Requests are also delivered through Handler callbacks. Drain
                // these immediately, including rejected environment requests.
                Some(ChannelMsg::Close) | None => return,
                Some(ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. } | ChannelMsg::Eof) => {
                    let _ = output.close().await;
                    return;
                }
                _ => {},
            },
        }
    };
    let (read, write) = tokio::io::split(native);
    let outcome = {
        let incoming = forward_input(input, write, pty);
        let outgoing = forward_output(read, &output);
        tokio::pin!(incoming, outgoing);
        tokio::select! {
            result = &mut incoming => result,
            result = &mut outgoing => result,
        }
    };
    if let Err(error) = outcome {
        let _ = output
            .extended_data_bytes(1, format!("wispnet: {error}\n").into_bytes())
            .await;
        let _ = output.exit_status(255).await;
    }
    let _ = output.eof().await;
    let _ = output.close().await;
}

async fn forward_input(
    mut channel: ChannelReadHalf,
    mut native: impl AsyncWrite + Unpin,
    pty: bool,
) -> io::Result<()> {
    let mut eof = false;
    while let Some(message) = channel.wait().await {
        let frame = match message {
            ChannelMsg::Data { data } if !eof => ClientFrame::Input {
                data: data.to_vec(),
            },
            ChannelMsg::Data { .. } => return Err(io::Error::other("Input after stdin EOF")),
            ChannelMsg::ExtendedData { .. } => {
                return Err(io::Error::other("Extended stdin is unsupported"));
            }
            ChannelMsg::Eof if !eof => {
                eof = true;
                ClientFrame::StdinEof
            }
            ChannelMsg::Eof => continue,
            ChannelMsg::Close => return Ok(()),
            ChannelMsg::WindowChange {
                col_width,
                row_height,
                ..
            } if pty => {
                let (cols, rows) = dimensions(col_width, row_height)
                    .ok_or_else(|| io::Error::other("Invalid terminal dimensions"))?;
                ClientFrame::Resize { cols, rows }
            }
            ChannelMsg::Signal { signal } => {
                let name = match signal {
                    Sig::HUP => "HUP",
                    Sig::INT => "INT",
                    Sig::KILL => "KILL",
                    Sig::QUIT => "QUIT",
                    Sig::TERM => "TERM",
                    _ => continue,
                };
                ClientFrame::Signal { name: name.into() }
            }
            _ => continue,
        };
        write_client(&mut native, &frame).await?;
    }
    Ok(())
}

async fn forward_output(
    mut native: impl AsyncRead + Unpin,
    channel: &ChannelWriteHalf<Msg>,
) -> io::Result<()> {
    while let Some(frame) = read_server(&mut native).await? {
        let sent = match frame {
            ServerFrame::Output { data }
            | ServerFrame::Scrollback { data }
            | ServerFrame::Stdout { data } => channel.data_bytes(data).await,
            ServerFrame::Stderr { data } => channel.extended_data_bytes(1, data).await,
            ServerFrame::Exited { code } | ServerFrame::ExecFinished { code, .. } => {
                return channel.exit_status(code).await.map_err(io::Error::other);
            }
            ServerFrame::TerminalState { .. } | ServerFrame::Pong => continue,
            ServerFrame::Error { message } => return Err(io::Error::other(message)),
            _ => return Err(io::Error::other("Unexpected Shell response")),
        };
        sent.map_err(io::Error::other)?;
    }
    Err(io::Error::other(
        "Native Shell connection closed without an exit status",
    ))
}

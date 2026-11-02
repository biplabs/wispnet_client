// SPDX-License-Identifier: AGPL-3.0-only
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 64 * 1024;
pub const MAX_EXEC_COMMAND_BYTES: usize = 8192;
pub const MAX_EXEC_TIMEOUT_MS: u32 = 300_000;
pub const MAX_EXEC_OUTPUT_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_EXEC_TIMEOUT_MS: u32 = 30_000;
pub const DEFAULT_EXEC_OUTPUT_BYTES: u32 = 65_536;
const JSON: u8 = 0;
const INPUT: u8 = 1;
const OUTPUT: u8 = 2;
const SCROLLBACK: u8 = 3;
const STDOUT: u8 = 4;
const STDERR: u8 = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientFrame {
    /// One-shot Unix /bin/sh -c execution, without an interactive PTY.
    Exec {
        command: String,
        cwd: Option<String>,
        timeout_ms: u32,
        max_output_bytes: u32,
    },
    /// Full-duplex process channel; its lifetime belongs to this connection.
    ExecStream {
        command: String,
        cwd: Option<String>,
    },
    Attach {
        session: String,
        cols: u16,
        rows: u16,
    },
    /// A PTY that is terminated when this connection ends, rather than retained.
    AttachEphemeral {
        session: String,
        cols: u16,
        rows: u16,
        command: Option<String>,
    },
    Input {
        data: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    StdinEof,
    Signal {
        name: String,
    },
    Detach,
    Terminate,
    Ping,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerFrame {
    ExecStarted,
    Attached {
        session: String,
        username: String,
    },
    Output {
        data: Vec<u8>,
    },
    Stdout {
        data: Vec<u8>,
    },
    Stderr {
        data: Vec<u8>,
    },
    ExecFinished {
        code: u32,
        timed_out: bool,
        output_truncated: bool,
    },
    Scrollback {
        data: Vec<u8>,
    },
    TerminalState {
        echo: bool,
        canonical: bool,
    },
    Exited {
        code: u32,
    },
    Pong,
    Error {
        message: String,
    },
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0];
    if reader.read(&mut kind).await? == 0 {
        return Ok(None);
    }
    let length = reader.read_u32().await? as usize;
    if length > MAX_FRAME {
        return Err(invalid("shell frame too large"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(Some((kind[0], bytes)))
}
async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    data: &[u8],
) -> io::Result<()> {
    if data.len() > MAX_FRAME {
        return Err(invalid("shell frame too large"));
    }
    writer.write_u8(kind).await?;
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(data).await?;
    writer.flush().await
}
pub async fn read_client(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<ClientFrame>> {
    let Some((kind, bytes)) = read_frame(reader).await? else {
        return Ok(None);
    };
    match kind {
        INPUT => Ok(Some(ClientFrame::Input { data: bytes })),
        JSON => {
            let frame: ClientFrame =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid shell request"))?;
            if matches!(frame, ClientFrame::Input { .. }) {
                return Err(invalid("input must use binary frame"));
            }
            Ok(Some(frame))
        }
        _ => Err(invalid("unknown shell request frame")),
    }
}
pub async fn write_client(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &ClientFrame,
) -> io::Result<()> {
    match frame {
        ClientFrame::Input { data } => write_frame(writer, INPUT, data).await,
        _ => {
            write_frame(
                writer,
                JSON,
                &serde_json::to_vec(frame).map_err(|_| invalid("invalid shell request"))?,
            )
            .await
        }
    }
}
pub async fn read_server(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<ServerFrame>> {
    let Some((kind, bytes)) = read_frame(reader).await? else {
        return Ok(None);
    };
    match kind {
        OUTPUT => Ok(Some(ServerFrame::Output { data: bytes })),
        SCROLLBACK => Ok(Some(ServerFrame::Scrollback { data: bytes })),
        STDOUT => Ok(Some(ServerFrame::Stdout { data: bytes })),
        STDERR => Ok(Some(ServerFrame::Stderr { data: bytes })),
        JSON => {
            let frame: ServerFrame =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid shell response"))?;
            if matches!(
                frame,
                ServerFrame::Output { .. }
                    | ServerFrame::Scrollback { .. }
                    | ServerFrame::Stdout { .. }
                    | ServerFrame::Stderr { .. }
            ) {
                return Err(invalid("output must use binary frame"));
            }
            Ok(Some(frame))
        }
        _ => Err(invalid("unknown shell response frame")),
    }
}
pub async fn write_server(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &ServerFrame,
) -> io::Result<()> {
    match frame {
        ServerFrame::Output { data } => write_frame(writer, OUTPUT, data).await,
        ServerFrame::Scrollback { data } => write_frame(writer, SCROLLBACK, data).await,
        ServerFrame::Stdout { data } => write_frame(writer, STDOUT, data).await,
        ServerFrame::Stderr { data } => write_frame(writer, STDERR, data).await,
        _ => {
            write_frame(
                writer,
                JSON,
                &serde_json::to_vec(frame).map_err(|_| invalid("invalid shell response"))?,
            )
            .await
        }
    }
}
pub fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub fn valid_size(cols: u16, rows: u16) -> bool {
    (2..=1000).contains(&cols) && (2..=1000).contains(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn binary_is_lossless_bounded_and_directional() {
        let data = vec![0, 255, 27, 10, 13];
        let mut bytes = Vec::new();
        write_client(&mut bytes, &ClientFrame::Input { data: data.clone() })
            .await
            .unwrap();
        assert_eq!(bytes.len(), 5 + data.len());
        assert_eq!(
            read_client(&mut bytes.as_slice()).await.unwrap(),
            Some(ClientFrame::Input { data })
        );
        assert!(read_server(&mut bytes.as_slice()).await.is_err());
        assert!(
            write_client(
                &mut Vec::new(),
                &ClientFrame::Input {
                    data: vec![0; MAX_FRAME + 1]
                }
            )
            .await
            .is_err()
        );
        let hostile = [OUTPUT, 0xff, 0xff, 0xff, 0xff];
        assert!(read_server(&mut hostile.as_slice()).await.is_err());
        assert!(
            read_server(&mut [OUTPUT, 0, 0, 0, 4, 0].as_slice())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn command_request_round_trips_without_changing_attach_frames() {
        let frame = ClientFrame::Exec {
            command: "printf 'two words'\nexit 3".into(),
            cwd: Some("/tmp".into()),
            timeout_ms: DEFAULT_EXEC_TIMEOUT_MS,
            max_output_bytes: DEFAULT_EXEC_OUTPUT_BYTES,
        };
        let mut bytes = Vec::new();
        write_client(&mut bytes, &frame).await.unwrap();
        assert_eq!(
            read_client(&mut bytes.as_slice()).await.unwrap(),
            Some(frame)
        );
        for frame in [
            ServerFrame::Stdout { data: vec![0, 255] },
            ServerFrame::Stderr { data: vec![0, 255] },
        ] {
            let mut bytes = Vec::new();
            write_server(&mut bytes, &frame).await.unwrap();
            assert_eq!(bytes.len(), 7);
            assert!(read_client(&mut bytes.as_slice()).await.is_err());
        }
    }
    #[tokio::test]
    async fn control_and_binary_events_round_trip() {
        for frame in [
            ServerFrame::Attached {
                session: "main".into(),
                username: "owner".into(),
            },
            ServerFrame::TerminalState {
                echo: false,
                canonical: false,
            },
            ServerFrame::Scrollback { data: vec![1, 255] },
            ServerFrame::Exited { code: 7 },
            ServerFrame::Stdout { data: vec![0, 255] },
            ServerFrame::Stderr { data: vec![0, 255] },
            ServerFrame::ExecFinished {
                code: 9,
                timed_out: false,
                output_truncated: true,
            },
            ServerFrame::Pong,
        ] {
            let mut bytes = Vec::new();
            write_server(&mut bytes, &frame).await.unwrap();
            assert_eq!(
                read_server(&mut bytes.as_slice()).await.unwrap(),
                Some(frame)
            );
        }
        let mut bytes = Vec::new();
        write_client(
            &mut bytes,
            &ClientFrame::Attach {
                session: "main".into(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_client(&mut bytes.as_slice()).await.unwrap(),
            Some(ClientFrame::Attach { .. })
        ));
        assert_eq!(read_client(&mut [].as_slice()).await.unwrap(), None);
    }
}

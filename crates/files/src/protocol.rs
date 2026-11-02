use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
pub const MAX_METADATA: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub file_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Uploading,
    Committed,
    Aborted,
    Expired,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Unavailable,
    PermissionDenied,
    ServiceChanged,
    NotFound,
    PathDenied,
    Conflict,
    InvalidArgument,
    InvalidOffset,
    HashMismatch,
    StaleVersion,
    NoSpace,
    ResourceLimit,
    TransferExpired,
    AlreadyCommitted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Names of configured receiving folders; never a directory listing.
    Workspaces,
    UploadBegin {
        /// Empty selects the receiver's default export without discovery.
        workspace: String,
        path: String,
        size_bytes: u64,
        sha256: String,
        idempotency_key: String,
    },
    UploadWrite {
        transfer_id: String,
        offset: u64,
        chunk_sha256: String,
        #[serde(skip)]
        data: Vec<u8>,
    },
    UploadStatus {
        transfer_id: String,
    },
    UploadCommit {
        transfer_id: String,
    },
    UploadAbort {
        transfer_id: String,
    },
    ShareList {
        path: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    ShareStat {
        path: String,
    },
    /// Probe before requesting encoded ranges. Old peers may close the stream
    /// on this unknown operation; clients then reconnect using raw ranges.
    ShareCapabilities,
    ShareReadRange {
        path: String,
        expected_version: String,
        offset: u64,
        max_bytes: u64,
    },
    /// Explicit permission to return independently compressed zstd chunks.
    /// Offsets and lengths always refer to the original, uncompressed file.
    ShareReadRangeEncoded {
        path: String,
        expected_version: String,
        offset: u64,
        max_bytes: u64,
    },
    // Recognize legacy read requests only to reject them with PermissionDenied.
    // A Files grant permits sending to this host, never reading from it.
    FileStat {
        workspace: String,
        path: String,
    },
    FileReadRange {
        workspace: String,
        path: String,
        expected_version: String,
        offset: u64,
        max_bytes: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Workspaces {
        workspaces: Vec<String>,
    },
    Upload {
        transfer_id: String,
        state: TransferState,
        committed_offset: u64,
        size_bytes: u64,
        sha256: String,
        max_chunk_bytes: u64,
        expires_at: u64,
        file: Option<FileInfo>,
    },
    Written {
        committed_offset: u64,
    },
    File {
        file: FileInfo,
    },
    Entries {
        entries: Vec<ShareEntry>,
        next_cursor: Option<String>,
    },
    Data {
        offset: u64,
        #[serde(skip)]
        data: Vec<u8>,
    },
    ShareCapabilities {
        encodings: Vec<String>,
    },
    EncodedData {
        offset: u64,
        decoded_bytes: u64,
        encoding: String,
        #[serde(skip)]
        data: Vec<u8>,
    },
    Aborted,
    Error {
        code: ErrorCode,
        message: String,
        expected_offset: Option<u64>,
    },
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Files frame")
}

async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut first = [0];
    if reader.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let mut rest = [0; 3];
    reader.read_exact(&mut rest).await?;
    let metadata_len = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
    let data_len = reader.read_u32().await? as usize;
    if metadata_len == 0 || metadata_len > MAX_METADATA || data_len > MAX_CHUNK {
        return Err(invalid());
    }
    let mut metadata = vec![0; metadata_len];
    reader.read_exact(&mut metadata).await?;
    let mut data = vec![0; data_len];
    reader.read_exact(&mut data).await?;
    Ok(Some((metadata, data)))
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    metadata: Vec<u8>,
    data: &[u8],
) -> io::Result<()> {
    if metadata.is_empty() || metadata.len() > MAX_METADATA || data.len() > MAX_CHUNK {
        return Err(invalid());
    }
    writer.write_u32(metadata.len() as u32).await?;
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(&metadata).await?;
    writer.write_all(data).await?;
    writer.flush().await
}

pub async fn read_request(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Request>> {
    let Some((metadata, data)) = read_frame(reader).await? else {
        return Ok(None);
    };
    let mut request = serde_json::from_slice(&metadata).map_err(|_| invalid())?;
    if let Request::UploadWrite { data: target, .. } = &mut request {
        *target = data;
    } else if !data.is_empty() {
        return Err(invalid());
    }
    Ok(Some(request))
}

pub async fn write_request(
    writer: &mut (impl AsyncWrite + Unpin),
    request: &Request,
) -> io::Result<()> {
    let data = match request {
        Request::UploadWrite { data, .. } => data.as_slice(),
        _ => &[],
    };
    write_frame(
        writer,
        serde_json::to_vec(request).map_err(|_| invalid())?,
        data,
    )
    .await
}

pub async fn read_response(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Response>> {
    let Some((metadata, data)) = read_frame(reader).await? else {
        return Ok(None);
    };
    let mut response = serde_json::from_slice(&metadata).map_err(|_| invalid())?;
    if let Response::Data { data: target, .. } | Response::EncodedData { data: target, .. } =
        &mut response
    {
        *target = data;
    } else if !data.is_empty() {
        return Err(invalid());
    }
    Ok(Some(response))
}

/// Decode one bounded platform-source response, including its binary payload.
/// Platform adapters must check the received allocation size before decoding.
pub fn decode_response_frame(frame: &[u8]) -> io::Result<Response> {
    if frame.len() < 8 || frame.len() > MAX_CHUNK + MAX_METADATA + 8 {
        return Err(invalid());
    }
    let metadata_len = u32::from_be_bytes(frame[..4].try_into().map_err(|_| invalid())?) as usize;
    let data_len = u32::from_be_bytes(frame[4..8].try_into().map_err(|_| invalid())?) as usize;
    if metadata_len == 0
        || metadata_len > MAX_METADATA
        || data_len > MAX_CHUNK
        || frame.len() != 8 + metadata_len + data_len
    {
        return Err(invalid());
    }
    let mut response =
        serde_json::from_slice(&frame[8..8 + metadata_len]).map_err(|_| invalid())?;
    match &mut response {
        Response::Data { data, .. } | Response::EncodedData { data, .. } => {
            *data = frame[8 + metadata_len..].to_vec()
        }
        _ if data_len > 0 => return Err(invalid()),
        _ => {}
    }
    Ok(response)
}

pub async fn write_response(
    writer: &mut (impl AsyncWrite + Unpin),
    response: &Response,
) -> io::Result<()> {
    write_frame(
        writer,
        serde_json::to_vec(response).map_err(|_| invalid())?,
        match response {
            Response::Data { data, .. } | Response::EncodedData { data, .. } => data.as_slice(),
            _ => &[],
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn platform_frames_keep_binary_payloads_bounded_and_reject_malformed_sizes() {
        let response = Response::Data {
            offset: 9,
            data: vec![0, 255, 128],
        };
        let mut frame = vec![];
        write_response(&mut frame, &response).await.unwrap();
        assert_eq!(decode_response_frame(&frame).unwrap(), response);
        assert!(decode_response_frame(&frame[..frame.len() - 1]).is_err());
        let mut wrong = frame.clone();
        wrong[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_response_frame(&wrong).is_err());
        let metadata = br#"{"type":"aborted"}"#;
        let mut wrong = vec![];
        wrong.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        wrong.extend_from_slice(&1u32.to_be_bytes());
        wrong.extend_from_slice(metadata);
        wrong.push(1);
        assert!(decode_response_frame(&wrong).is_err());
        assert!(decode_response_frame(&[0; 7]).is_err());
    }
    #[tokio::test]
    async fn encoded_frames_keep_binary_payload_out_of_json() {
        let response = Response::EncodedData {
            offset: 99,
            decoded_bytes: 100_000,
            encoding: "zstd".into(),
            data: zstd::bulk::compress(&vec![42; 100_000], 1).unwrap(),
        };
        assert!(
            serde_json::to_value(&response)
                .unwrap()
                .get("data")
                .is_none()
        );
        let mut frame = Vec::new();
        write_response(&mut frame, &response).await.unwrap();
        assert_eq!(
            read_response(&mut frame.as_slice()).await.unwrap(),
            Some(response.clone())
        );
        assert_eq!(decode_response_frame(&frame).unwrap(), response);
    }
    #[tokio::test]
    async fn binary_payload_is_bounded_and_never_json_encoded() {
        let request = Request::UploadWrite {
            transfer_id: "a".repeat(64),
            offset: 0,
            chunk_sha256: "b".repeat(64),
            data: vec![0, 255, 1, 128],
        };
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).await.unwrap();
        assert_eq!(
            read_request(&mut bytes.as_slice()).await.unwrap(),
            Some(request)
        );
        assert!(read_response(&mut bytes.as_slice()).await.is_err());
        assert!(read_request(&mut [255; 8].as_slice()).await.is_err());
        assert!(
            read_request(&mut bytes[..bytes.len() - 1].as_ref())
                .await
                .is_err()
        );
    }
}

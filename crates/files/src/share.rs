//! Read-only shares use distinct services and grants from incoming Files.
use crate::{
    ErrorCode, FileInfo, MAX_CHUNK, MAX_METADATA, Request, Response, ShareEntry, encode_hash,
    write_response,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Semaphore, mpsc},
};

/// A platform-owned read-only folder capability. Implementations must resolve
/// every path within the selected folder, preserve stable versions across range
/// reads, and observe cancellation during storage work. File bytes remain binary.
pub trait ShareSource: Send + Sync {
    fn request(
        &self,
        root: &Path,
        request: &Request,
        cancel: Arc<AtomicBool>,
    ) -> std::result::Result<Response, ErrorCode>;
}

const PAGE_ENTRIES: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
type Result<T> = std::result::Result<T, ErrorCode>;
fn io_code(error: io::Error) -> ErrorCode {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ErrorCode::NotFound,
        Some(libc::ELOOP | libc::ENOTDIR | libc::EACCES | libc::EPERM) => ErrorCode::PathDenied,
        _ => ErrorCode::Unavailable,
    }
}
fn parts(path: &str) -> Result<Vec<&str>> {
    if path.len() > 4096 || path.contains(['\0', '\\']) {
        return Err(ErrorCode::PathDenied);
    }
    if path.is_empty() {
        return Ok(vec![]);
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.iter().any(|part| {
        part.is_empty()
            || matches!(*part, "." | "..")
            || part.len() > 255
            || part.starts_with(".wispnet-")
    }) {
        return Err(ErrorCode::PathDenied);
    }
    Ok(parts)
}
fn open_at(directory: &File, name: &std::ffi::OsStr, is_directory: bool) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(name.as_bytes()).map_err(|_| ErrorCode::PathDenied)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if is_directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        return Err(io_code(io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(io_code)?;
    if !meta.is_dir() && !meta.is_file() {
        return Err(ErrorCode::PathDenied);
    }
    Ok(file)
}
fn open(root: &Path, path: &str) -> Result<File> {
    let components = parts(path)?;
    let mut file = crate::server::open_directory(root).map_err(io_code)?;
    for (index, name) in components.iter().enumerate() {
        file = open_at(
            &file,
            std::ffi::OsStr::new(name),
            index + 1 < components.len(),
        )?;
    }
    Ok(file)
}
fn entry(path: &str, name: &str, file: &File) -> Result<ShareEntry> {
    let meta = file.metadata().map_err(io_code)?;
    Ok(ShareEntry {
        name: name.into(),
        path: path.into(),
        is_dir: meta.is_dir(),
        size_bytes: if meta.is_file() { meta.len() } else { 0 },
    })
}
struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn list(root: &Path, path: &str, cursor: Option<String>, cancel: &AtomicBool) -> Result<Response> {
    if cursor
        .as_ref()
        .is_some_and(|cursor| parts(cursor).is_err() || cursor.contains('/') || cursor.is_empty())
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let directory = open(root, path)?;
    if !directory.metadata().map_err(io_code)?.is_dir() {
        return Err(ErrorCode::PathDenied);
    }
    // fdopendir owns a separate descriptor. No path is re-resolved for listing.
    let duplicate = directory.try_clone().map_err(io_code)?.into_raw_fd();
    let raw = unsafe { libc::fdopendir(duplicate) };
    if raw.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(io_code(io::Error::last_os_error()));
    }
    let handle = Directory(raw);
    let mut candidates = BTreeMap::new();
    let mut count = 0;
    let mut more = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Unavailable);
        }
        let raw_entry = unsafe { libc::readdir(handle.0) };
        if raw_entry.is_null() {
            break;
        }
        count += 1;
        if count > MAX_DIRECTORY_ENTRIES {
            return Err(ErrorCode::ResourceLimit);
        }
        let name = unsafe { CStr::from_ptr((*raw_entry).d_name.as_ptr()) };
        let Ok(name) = name.to_str() else {
            continue;
        };
        if parts(name).is_err()
            || name.is_empty()
            || cursor.as_deref().is_some_and(|cursor| name <= cursor)
        {
            continue;
        }
        let child_path = if path.is_empty() {
            name.into()
        } else {
            format!("{path}/{name}")
        };
        if parts(&child_path).is_err() {
            continue;
        }
        let child = match open_at(&directory, std::ffi::OsStr::new(name), false) {
            Ok(file) => file,
            Err(ErrorCode::PathDenied | ErrorCode::NotFound) => continue,
            Err(error) => return Err(error),
        };
        candidates.insert(name.to_owned(), entry(&child_path, name, &child)?);
        if candidates.len() > PAGE_ENTRIES {
            candidates.pop_last();
            more = true;
        }
    }
    let mut entries: Vec<_> = candidates.into_values().collect();
    // Long relative paths still fit the fixed metadata frame ceiling.
    loop {
        let next_cursor = if more {
            entries.last().map(|entry| entry.name.clone())
        } else {
            None
        };
        let response = Response::Entries {
            entries: entries.clone(),
            next_cursor,
        };
        if serde_json::to_vec(&response)
            .map_err(|_| ErrorCode::Unavailable)?
            .len()
            <= MAX_METADATA
        {
            return Ok(response);
        }
        entries.pop();
        more = true;
        if entries.is_empty() {
            return Err(ErrorCode::ResourceLimit);
        }
    }
}
fn stat(root: &Path, path: &str, cancel: &AtomicBool) -> Result<Response> {
    let mut file = open(root, path)?;
    let meta = file.metadata().map_err(io_code)?;
    if !meta.is_file() {
        return Err(ErrorCode::PathDenied);
    }
    let before = crate::server::fingerprint(&meta);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut remaining = meta.len();
    while remaining > 0 {
        if cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Unavailable);
        }
        let length = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..length]).map_err(io_code)?;
        if count == 0 {
            return Err(ErrorCode::StaleVersion);
        }
        remaining -= count as u64;
        hasher.update(&buffer[..count]);
    }
    if crate::server::fingerprint(&file.metadata().map_err(io_code)?) != before {
        return Err(ErrorCode::StaleVersion);
    }
    let hash = encode_hash(hasher.finalize());
    Ok(Response::File {
        file: FileInfo {
            path: path.into(),
            size_bytes: meta.len(),
            file_version: format!("{hash}:{before}"),
            sha256: hash,
        },
    })
}
fn read(
    root: &Path,
    path: &str,
    expected_version: &str,
    offset: u64,
    max_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Response> {
    if max_bytes == 0 || max_bytes > MAX_CHUNK as u64 {
        return Err(ErrorCode::InvalidArgument);
    }
    let Some((hash, expected)) = expected_version.split_once(':') else {
        return Err(ErrorCode::StaleVersion);
    };
    if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(ErrorCode::StaleVersion);
    }
    let mut file = open(root, path)?;
    let meta = file.metadata().map_err(io_code)?;
    if !meta.is_file() {
        return Err(ErrorCode::PathDenied);
    }
    if crate::server::fingerprint(&meta) != expected {
        return Err(ErrorCode::StaleVersion);
    }
    if offset > meta.len() {
        return Err(ErrorCode::InvalidOffset);
    }
    if cancel.load(Ordering::Acquire) {
        return Err(ErrorCode::Unavailable);
    }
    file.seek(SeekFrom::Start(offset)).map_err(io_code)?;
    let mut data = vec![0; (meta.len() - offset).min(max_bytes) as usize];
    file.read_exact(&mut data).map_err(io_code)?;
    if crate::server::fingerprint(&file.metadata().map_err(io_code)?) != expected {
        return Err(ErrorCode::StaleVersion);
    }
    Ok(Response::Data { offset, data })
}
fn validate_request(request: &Request) -> Result<()> {
    match request {
        Request::ShareCapabilities => {}
        Request::ShareList { path, cursor } => {
            parts(path)?;
            if let Some(cursor) = cursor
                && (cursor.is_empty() || parts(cursor)?.len() != 1)
            {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        Request::ShareStat { path } => {
            if parts(path)?.is_empty() {
                return Err(ErrorCode::PathDenied);
            }
        }
        Request::ShareReadRange {
            path,
            expected_version,
            max_bytes,
            ..
        }
        | Request::ShareReadRangeEncoded {
            path,
            expected_version,
            max_bytes,
            ..
        } => {
            if parts(path)?.is_empty() {
                return Err(ErrorCode::PathDenied);
            }
            if *max_bytes == 0
                || *max_bytes > MAX_CHUNK as u64
                || expected_version.is_empty()
                || expected_version.len() > 4096
            {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        _ => return Err(ErrorCode::PermissionDenied),
    }
    Ok(())
}
fn validate_response(request: &Request, response: Response) -> Result<Response> {
    if serde_json::to_vec(&response)
        .map_err(|_| ErrorCode::Unavailable)?
        .len()
        > MAX_METADATA
    {
        return Err(ErrorCode::ResourceLimit);
    }
    match (request, &response) {
        (_, Response::Error { code, .. }) => return Err(*code),
        (
            Request::ShareList { path, cursor },
            Response::Entries {
                entries,
                next_cursor,
            },
        ) => {
            if entries.len() > PAGE_ENTRIES
                || !entries.windows(2).all(|pair| pair[0].name < pair[1].name)
            {
                return Err(ErrorCode::Unavailable);
            }
            for entry in entries {
                if parts(&entry.name)?.len() != 1 || parts(&entry.path)?.is_empty() {
                    return Err(ErrorCode::PathDenied);
                }
                let expected = if path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{path}/{}", entry.name)
                };
                if entry.path != expected
                    || cursor.as_ref().is_some_and(|cursor| &entry.name <= cursor)
                {
                    return Err(ErrorCode::PathDenied);
                }
            }
            if let Some(next) = next_cursor
                && entries.last().is_none_or(|entry| &entry.name != next)
            {
                return Err(ErrorCode::Unavailable);
            }
        }
        (Request::ShareStat { path }, Response::File { file }) => {
            if &file.path != path
                || file.size_bytes > (1u64 << 40)
                || file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                || file.file_version.is_empty()
                || file.file_version.len() > 4096
            {
                return Err(ErrorCode::Unavailable);
            }
        }
        (
            Request::ShareReadRange {
                offset, max_bytes, ..
            },
            Response::Data {
                offset: actual,
                data,
            },
        ) => {
            if offset != actual || data.len() > *max_bytes as usize {
                return Err(ErrorCode::Unavailable);
            }
        }
        _ => return Err(ErrorCode::PermissionDenied),
    }
    Ok(response)
}
fn dispatch(
    root: &Path,
    request: Request,
    cancel: Arc<AtomicBool>,
    publisher: Option<&dyn crate::Publisher>,
) -> Result<Response> {
    validate_request(&request)?;
    if cancel.load(Ordering::Acquire) {
        return Err(ErrorCode::Unavailable);
    }
    if matches!(request, Request::ShareCapabilities) {
        return Ok(Response::ShareCapabilities {
            encodings: vec!["zstd".into()],
        });
    }
    // Platform storage adapters need no codec knowledge. They see only the
    // already-supported bounded raw read operation; encoding happens after
    // validating their response, inside the native blocking worker.
    let request = match request {
        Request::ShareReadRangeEncoded {
            path,
            expected_version,
            offset,
            max_bytes,
        } => Request::ShareReadRange {
            path,
            expected_version,
            offset,
            max_bytes,
        },
        other => other,
    };
    if let Some(source) = publisher.and_then(|publisher| publisher.share_source()) {
        // The native path is an owner-selected private marker directory only.
        // Its contents are never used as a fallback for an unknown capability.
        crate::server::open_directory(root).map_err(io_code)?;
        return validate_response(&request, source.request(root, &request, cancel)?);
    }
    match request {
        Request::ShareList { path, cursor } => list(root, &path, cursor, &cancel),
        Request::ShareStat { path } => stat(root, &path, &cancel),
        Request::ShareReadRange {
            path,
            expected_version,
            offset,
            max_bytes,
        } => read(root, &path, &expected_version, offset, max_bytes, &cancel),
        _ => Err(ErrorCode::PermissionDenied),
    }
}

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
pub(super) async fn serve(
    root: PathBuf,
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    bulk: Arc<Semaphore>,
    publisher: Option<Arc<dyn crate::Publisher>>,
) -> io::Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = Cancel(cancel.clone());
    let (sender, mut receiver) = mpsc::channel(8);
    let outstanding = Arc::new(AtomicUsize::new(0));
    // Continue reading while the source hashes/reads. A reset or disconnect
    // cancels the blocking platform callback immediately, without waiting for
    // a large checkpoint hash to finish. Active work extends idle framing time.
    let read = async {
        crate::server::queue_requests(
            &mut reader,
            sender,
            outstanding.clone(),
            Duration::from_secs(300),
        )
        .await?;
        // Files clients keep their request direction open while awaiting work.
        // EOF with pending storage work is a disconnected/cancelled requester.
        if outstanding.load(Ordering::Acquire) > 0 {
            cancel.store(true, Ordering::Release);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "File share requester closed",
            ));
        }
        Ok(())
    };
    let process = async {
        let mut encoder = crate::AdaptiveEncoder::default();
        while let Some(request) = receiver.recv().await {
            let permit = bulk
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| io::Error::other("File share unavailable"))?;
            let root = root.clone();
            let cancel = cancel.clone();
            let publisher = publisher.clone();
            let (response, returned_encoder) = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let source = match &request {
                    Request::ShareReadRangeEncoded {
                        path,
                        expected_version,
                        ..
                    } => Some(format!("{path}\0{expected_version}")),
                    _ => None,
                };
                let response =
                    dispatch(&root, request, cancel, publisher.as_deref()).map(|response| {
                        match source {
                            Some(source) => encoder.encode(&source, response),
                            None => response,
                        }
                    });
                (response, encoder)
            })
            .await
            .map_err(|_| io::Error::other("File share unavailable"))?;
            encoder = returned_encoder;
            let response = response.unwrap_or_else(|code| Response::Error {
                code,
                message: "File share request failed".into(),
                expected_offset: None,
            });
            let payload_bytes = match &response {
                Response::Data { data, .. } | Response::EncodedData { data, .. } => data.len(),
                _ => 0,
            };
            let write_started = std::time::Instant::now();
            tokio::time::timeout(
                Duration::from_secs(300),
                write_response(&mut writer, &response),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "File share write timeout"))??;
            encoder.observe_write(payload_bytes, write_started.elapsed());
            outstanding.fetch_sub(1, Ordering::AcqRel);
        }
        Ok::<(), io::Error>(())
    };
    tokio::try_join!(read, process)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileService, read_response, sha256, write_request};
    use std::fs;

    fn info(root: &Path, path: &str) -> FileInfo {
        match stat(root, path, &AtomicBool::new(false)).unwrap() {
            Response::File { file } => file,
            _ => panic!("file response required"),
        }
    }
    #[test]
    fn share_ranges_pin_versions_and_reject_source_replacement() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("step-1.pt"), b"checkpoint bytes").unwrap();
        let file = info(root.path(), "step-1.pt");
        assert_eq!(file.sha256, sha256(b"checkpoint bytes"));
        assert!(
            matches!(read(root.path(), "step-1.pt", &file.file_version, 3, 4, &AtomicBool::new(false)).unwrap(), Response::Data { offset: 3, data } if data == b"ckpo")
        );
        fs::write(root.path().join("new"), b"checkpoint bytes").unwrap();
        fs::rename(root.path().join("new"), root.path().join("step-1.pt")).unwrap();
        assert_eq!(
            read(
                root.path(),
                "step-1.pt",
                &file.file_version,
                0,
                4,
                &AtomicBool::new(false)
            )
            .unwrap_err(),
            ErrorCode::StaleVersion
        );
        let file = info(root.path(), "step-1.pt");
        fs::write(root.path().join("step-1.pt"), b"changed in place").unwrap();
        assert_eq!(
            read(
                root.path(),
                "step-1.pt",
                &file.file_version,
                0,
                4,
                &AtomicBool::new(false)
            )
            .unwrap_err(),
            ErrorCode::StaleVersion
        );
    }
    #[test]
    fn share_paths_and_listings_cannot_escape_or_expose_partial_files() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"private").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        symlink(outside.path().join("secret"), root.path().join("linked")).unwrap();
        fs::write(root.path().join(".wispnet-partial"), b"partial").unwrap();
        fs::write(root.path().join("safe"), b"safe").unwrap();
        let fifo = CString::new(root.path().join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for path in [
            "/secret",
            "../secret",
            "escape/secret",
            "linked",
            "fifo",
            ".wispnet-partial",
            "a//b",
            "a\\b",
        ] {
            assert!(
                stat(root.path(), path, &AtomicBool::new(false)).is_err(),
                "{path}"
            );
        }
        assert!(
            matches!(list(root.path(), "", None, &AtomicBool::new(false)).unwrap(), Response::Entries { entries, next_cursor: None } if entries.len() == 1 && entries[0].name == "safe")
        );
        let alias = outside.path().join("alias");
        symlink(root.path(), &alias).unwrap();
        assert!(stat(&alias, "safe", &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn configured_folder_replaced_by_file_or_symlink_cannot_be_read() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("selected");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("model"), b"weights").unwrap();
        assert_eq!(info(&root, "model").size_bytes, 7);
        fs::remove_dir_all(&root).unwrap();
        fs::write(&root, b"unexpected file").unwrap();
        assert_eq!(
            list(&root, "", None, &AtomicBool::new(false)).unwrap_err(),
            ErrorCode::PathDenied
        );
        assert_eq!(
            stat(&root, "", &AtomicBool::new(false)).unwrap_err(),
            ErrorCode::PathDenied
        );
        fs::remove_file(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"private").unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        assert_eq!(
            list(&root, "", None, &AtomicBool::new(false)).unwrap_err(),
            ErrorCode::PathDenied
        );
        assert_eq!(
            stat(&root, "secret", &AtomicBool::new(false)).unwrap_err(),
            ErrorCode::PathDenied
        );
    }
    #[test]
    fn listings_are_bounded_paginated_and_complete() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..79 {
            fs::write(root.path().join(format!("step-{index:03}")), b"weights").unwrap();
        }
        let mut cursor = None;
        let mut names = vec![];
        loop {
            let response = list(root.path(), "", cursor, &AtomicBool::new(false)).unwrap();
            assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_METADATA);
            let Response::Entries {
                entries,
                next_cursor,
            } = response
            else {
                panic!("entries expected");
            };
            assert!(entries.len() <= PAGE_ENTRIES);
            names.extend(entries.into_iter().map(|entry| entry.name));
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(names.len(), 79);
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    }
    #[tokio::test]
    async fn share_compression_is_explicit_and_keeps_legacy_ranges_raw() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; 256 * 1024];
        fs::write(root.path().join("model"), &bytes).unwrap();
        let file = info(root.path(), "model");
        let service = FileService::new(state.path().into()).unwrap();
        let (mut client, server) = tokio::io::duplex(MAX_CHUNK * 2);
        let (reader, writer) = tokio::io::split(server);
        let path = root.path().to_owned();
        let task = tokio::spawn(async move {
            service
                .serve_share("peer", "inc", path, reader, writer)
                .await
                .unwrap();
        });
        write_request(&mut client, &Request::ShareCapabilities)
            .await
            .unwrap();
        assert!(
            matches!(read_response(&mut client).await.unwrap(), Some(Response::ShareCapabilities { encodings }) if encodings == ["zstd"])
        );
        for encoded in [true, false] {
            let request = if encoded {
                Request::ShareReadRangeEncoded {
                    path: "model".into(),
                    expected_version: file.file_version.clone(),
                    offset: 0,
                    max_bytes: bytes.len() as u64,
                }
            } else {
                Request::ShareReadRange {
                    path: "model".into(),
                    expected_version: file.file_version.clone(),
                    offset: 0,
                    max_bytes: bytes.len() as u64,
                }
            };
            write_request(&mut client, &request).await.unwrap();
            let response = read_response(&mut client).await.unwrap().unwrap();
            assert_eq!(matches!(response, Response::EncodedData { .. }), encoded);
            assert_eq!(
                crate::decode_data_response(response).unwrap(),
                Response::Data {
                    offset: 0,
                    data: bytes.clone()
                }
            );
        }
        drop(client);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn binary_share_stream_reads_but_cannot_upload() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::write(root.path().join("model"), b"\0\xffweights").unwrap();
        let service = FileService::new(state.path().into()).unwrap();
        let (mut client, server) = tokio::io::duplex(MAX_CHUNK * 2);
        let (reader, writer) = tokio::io::split(server);
        let path = root.path().to_owned();
        let task = tokio::spawn(async move {
            service
                .serve_share("peer", "incarnation", path, reader, writer)
                .await
                .unwrap();
        });
        write_request(
            &mut client,
            &Request::ShareStat {
                path: "model".into(),
            },
        )
        .await
        .unwrap();
        let Some(Response::File { file }) = read_response(&mut client).await.unwrap() else {
            panic!("file expected");
        };
        write_request(
            &mut client,
            &Request::ShareReadRange {
                path: "model".into(),
                expected_version: file.file_version,
                offset: 0,
                max_bytes: MAX_CHUNK as u64,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_response(&mut client).await.unwrap(), Some(Response::Data { offset: 0, data }) if data == b"\0\xffweights")
        );
        write_request(
            &mut client,
            &Request::UploadBegin {
                workspace: "".into(),
                path: "injected".into(),
                size_bytes: 3,
                sha256: sha256(b"bad"),
                idempotency_key: "upload".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_response(&mut client).await.unwrap(),
            Some(Response::Error {
                code: ErrorCode::PermissionDenied,
                ..
            })
        ));
        drop(client);
        task.await.unwrap();
        assert!(!root.path().join("injected").exists());
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use crate::{
        FileService, Limits, Publication, Publisher, read_response, sha256, write_request,
    };
    use std::sync::atomic::AtomicUsize;

    struct Provider {
        calls: AtomicUsize,
        wait: bool,
        entered: AtomicBool,
        stopped: AtomicBool,
    }
    impl Provider {
        fn new(wait: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                wait,
                entered: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
            }
        }
    }
    impl Publisher for Provider {
        fn share_source(&self) -> Option<&dyn ShareSource> {
            Some(self)
        }
        fn publish(&self, _: Publication<'_>, _: File, _: Arc<AtomicBool>) -> io::Result<()> {
            Err(io::Error::other("Not an inbox adapter"))
        }
    }
    impl ShareSource for Provider {
        fn request(
            &self,
            root: &Path,
            request: &Request,
            cancel: Arc<AtomicBool>,
        ) -> Result<Response> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if root.file_name().and_then(|name| name.to_str()) != Some("capability") {
                return Err(ErrorCode::PermissionDenied);
            }
            if self.wait {
                self.entered.store(true, Ordering::Release);
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.stopped.store(true, Ordering::Release);
                return Err(ErrorCode::Unavailable);
            }
            match request {
                Request::ShareList { .. } => Ok(Response::Entries {
                    entries: vec![ShareEntry {
                        name: "remote.pt".into(),
                        path: "remote.pt".into(),
                        is_dir: false,
                        size_bytes: 7,
                    }],
                    next_cursor: None,
                }),
                Request::ShareStat { path } => Ok(Response::File {
                    file: FileInfo {
                        path: path.clone(),
                        size_bytes: 7,
                        sha256: sha256(b"weights"),
                        file_version: "provider-version".into(),
                    },
                }),
                Request::ShareReadRange {
                    expected_version,
                    offset,
                    max_bytes,
                    ..
                } => {
                    if expected_version != "provider-version" {
                        return Err(ErrorCode::StaleVersion);
                    }
                    let bytes = b"weights";
                    if *offset > bytes.len() as u64 {
                        return Err(ErrorCode::InvalidOffset);
                    }
                    let end = (*offset + *max_bytes).min(bytes.len() as u64) as usize;
                    Ok(Response::Data {
                        offset: *offset,
                        data: bytes[*offset as usize..end].to_vec(),
                    })
                }
                _ => panic!("non-share operation reached source"),
            }
        }
    }
    #[test]
    fn provider_boundary_filters_paths_operations_versions_and_native_fallback() {
        let parent = tempfile::tempdir().unwrap();
        let marker = parent.path().join("capability");
        std::fs::create_dir(&marker).unwrap();
        let unknown = parent.path().join("unknown");
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("private"), b"must not fall back").unwrap();
        let provider = Provider::new(false);
        let cancel = Arc::new(AtomicBool::new(false));
        for request in [
            Request::Workspaces,
            Request::FileStat {
                workspace: "".into(),
                path: "private".into(),
            },
            Request::ShareStat {
                path: "../private".into(),
            },
            Request::ShareReadRange {
                path: "remote.pt".into(),
                expected_version: "version".into(),
                offset: 0,
                max_bytes: MAX_CHUNK as u64 + 1,
            },
        ] {
            assert!(dispatch(&marker, request, cancel.clone(), Some(&provider)).is_err());
        }
        assert_eq!(provider.calls.load(Ordering::Acquire), 0);
        assert_eq!(
            dispatch(
                &unknown,
                Request::ShareStat {
                    path: "private".into()
                },
                cancel.clone(),
                Some(&provider)
            )
            .unwrap_err(),
            ErrorCode::PermissionDenied
        );
        assert!(
            matches!(dispatch(&marker,Request::ShareStat { path:"remote.pt".into() },cancel.clone(),Some(&provider)).unwrap(),Response::File { file } if file.sha256==sha256(b"weights"))
        );
        assert!(
            matches!(dispatch(&marker,Request::ShareReadRange { path:"remote.pt".into(),expected_version:"provider-version".into(),offset:1,max_bytes:3 },cancel.clone(),Some(&provider)).unwrap(),Response::Data { offset:1,data } if data==b"eig")
        );
        assert!(
            matches!(dispatch(&marker,Request::ShareReadRangeEncoded { path:"remote.pt".into(),expected_version:"provider-version".into(),offset:1,max_bytes:3 },cancel.clone(),Some(&provider)).unwrap(),Response::Data { offset:1,data } if data==b"eig")
        );
        assert_eq!(
            dispatch(
                &marker,
                Request::ShareReadRange {
                    path: "remote.pt".into(),
                    expected_version: "changed".into(),
                    offset: 0,
                    max_bytes: 3
                },
                cancel,
                Some(&provider)
            )
            .unwrap_err(),
            ErrorCode::StaleVersion
        );
    }
    #[test]
    fn provider_responses_cannot_cross_paths_or_exceed_requested_binary_range() {
        let request = Request::ShareList {
            path: "nested".into(),
            cursor: None,
        };
        assert!(
            validate_response(
                &request,
                Response::Entries {
                    entries: vec![ShareEntry {
                        name: "private".into(),
                        path: "../private".into(),
                        is_dir: false,
                        size_bytes: 1
                    }],
                    next_cursor: None
                }
            )
            .is_err()
        );
        let request = Request::ShareReadRange {
            path: "model".into(),
            expected_version: "v".into(),
            offset: 2,
            max_bytes: 1,
        };
        assert!(
            validate_response(
                &request,
                Response::Data {
                    offset: 2,
                    data: vec![1, 2]
                }
            )
            .is_err()
        );
        assert!(
            validate_response(
                &request,
                Response::Data {
                    offset: 3,
                    data: vec![1]
                }
            )
            .is_err()
        );
        assert!(
            validate_response(
                &request,
                Response::Entries {
                    entries: vec![],
                    next_cursor: None
                }
            )
            .is_err()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_reads_use_binary_streams_and_disconnect_cancels_storage_work() {
        let parent = tempfile::tempdir().unwrap();
        let marker = parent.path().join("capability");
        std::fs::create_dir(&marker).unwrap();
        let provider = Arc::new(Provider::new(false));
        let service = FileService::with_publisher(
            parent.path().join("state"),
            Limits::default(),
            Some(provider),
        )
        .unwrap();
        let (mut client, server) = tokio::io::duplex(MAX_CHUNK * 2);
        let (reader, writer) = tokio::io::split(server);
        let root = marker.clone();
        let task = tokio::spawn(async move {
            service
                .serve_share("peer", "inc", root, reader, writer)
                .await
                .unwrap();
        });
        write_request(
            &mut client,
            &Request::ShareReadRange {
                path: "remote.pt".into(),
                expected_version: "provider-version".into(),
                offset: 0,
                max_bytes: 7,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_response(&mut client).await.unwrap(),Some(Response::Data { offset:0,data }) if data==b"weights")
        );
        drop(client);
        task.await.unwrap();
        let provider = Arc::new(Provider::new(true));
        let service = FileService::with_publisher(
            parent.path().join("state-2"),
            Limits::default(),
            Some(provider.clone()),
        )
        .unwrap();
        let (mut client, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(server);
        let task = tokio::spawn(async move {
            service
                .serve_share("peer", "inc", marker, reader, writer)
                .await
        });
        write_request(
            &mut client,
            &Request::ShareStat {
                path: "remote.pt".into(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !provider.entered.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        drop(client);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !provider.stopped.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn going_offline_cancels_active_provider_hash_even_when_reenabled_immediately() {
        let parent = tempfile::tempdir().unwrap();
        let marker = parent.path().join("capability");
        std::fs::create_dir(&marker).unwrap();
        let provider = Arc::new(Provider::new(true));
        let service = FileService::with_publisher(
            parent.path().join("state"),
            Limits::default(),
            Some(provider.clone()),
        )
        .unwrap();
        let active = service.clone();
        let (mut client, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(server);
        let task = tokio::spawn(async move {
            active
                .serve_share("peer", "inc", marker, reader, writer)
                .await
        });
        write_request(
            &mut client,
            &Request::ShareStat {
                path: "remote.pt".into(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !provider.entered.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        service.set_enabled(false);
        service.set_enabled(true);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !provider.stopped.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
}

use crate::{
    ErrorCode, FileInfo, MAX_CHUNK, Request, Response, TransferState, encode_hash, read_request,
    sha256, write_response,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::CString,
    fs::{self, File, Metadata},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Semaphore, mpsc, watch},
};

/// Resource ceilings apply to incomplete uploads; committed artifacts belong to
/// the export owner and are never removed by transfer cleanup.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_upload_bytes: u64,
    pub max_pending_bytes: u64,
    pub max_transfers: usize,
    pub retention_secs: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 1024 * 1024 * 1024 * 1024,
            max_pending_bytes: 2 * 1024 * 1024 * 1024 * 1024,
            max_transfers: 1024,
            retention_secs: 24 * 60 * 60,
        }
    }
}

/// A platform storage adapter publishes verified bytes before upload success.
/// Implementations must be idempotent by transfer_id and never replace unrelated
/// files. The private staging root identifies the owner-selected destination.
pub trait Publisher: Send + Sync {
    /// Optional platform folder capabilities, independent from inbox publication.
    /// Returning a source disables native-path fallback for every share request.
    fn share_source(&self) -> Option<&dyn crate::ShareSource> {
        None
    }
    fn publish(
        &self,
        publication: Publication<'_>,
        source: File,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<()>;
}
pub struct Publication<'a> {
    pub transfer_id: &'a str,
    pub root: &'a Path,
    pub path: &'a str,
    pub size_bytes: u64,
    pub sha256: &'a str,
}

#[derive(Clone)]
pub struct FileService(Arc<Inner>);
struct Inner {
    publisher: Option<Arc<dyn Publisher>>,
    enabled: AtomicBool,
    share_stops: watch::Sender<()>,
    directory: File,
    state: PathBuf,
    limits: Limits,
    begin_lock: Mutex<()>,
    locks: Mutex<HashMap<String, Weak<TransferLock>>>,
    bulk: Arc<Semaphore>,
}
#[derive(Default)]
struct TransferLock {
    mutex: Mutex<()>,
    abort: AtomicBool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    transfer_id: String,
    peer: String,
    incarnation: String,
    workspace: String,
    root: PathBuf,
    path: String,
    size_bytes: u64,
    sha256: String,
    idempotency_key: String,
    state: TransferState,
    committed_offset: u64,
    expires_at: u64,
    file: Option<FileInfo>,
}
impl Manifest {
    fn response(&self) -> Response {
        Response::Upload {
            transfer_id: self.transfer_id.clone(),
            // Cleanup can be deferred by an unavailable export. The wire
            // state still expires on schedule while private ownership/quota
            // metadata remains until its partial can safely be removed.
            state: if self.state == TransferState::Uploading && now() >= self.expires_at {
                TransferState::Expired
            } else {
                self.state
            },
            committed_offset: self.committed_offset,
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
            max_chunk_bytes: MAX_CHUNK as u64,
            expires_at: self.expires_at,
            file: self.file.clone(),
        }
    }
}

#[derive(Debug)]
struct Failure {
    code: ErrorCode,
    expected: Option<u64>,
}
type Result<T> = std::result::Result<T, Failure>;
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        let code = match error.raw_os_error() {
            Some(libc::ENOSPC | libc::EDQUOT) => ErrorCode::NoSpace,
            Some(libc::ELOOP | libc::ENOTDIR) => ErrorCode::PathDenied,
            Some(libc::EEXIST) => ErrorCode::Conflict,
            Some(libc::ENOENT) => ErrorCode::NotFound,
            Some(libc::EACCES | libc::EPERM) => ErrorCode::PathDenied,
            _ if error.kind() == io::ErrorKind::PermissionDenied => ErrorCode::PathDenied,
            _ if error.kind() == io::ErrorKind::AlreadyExists => ErrorCode::Conflict,
            _ => ErrorCode::Unavailable,
        };
        fail(code)
    }
}
fn fail(code: ErrorCode) -> Failure {
    Failure {
        code,
        expected: None,
    }
}
fn offset(expected: u64) -> Failure {
    Failure {
        code: ErrorCode::InvalidOffset,
        expected: Some(expected),
    }
}
fn response_error(error: Failure) -> Response {
    Response::Error {
        code: error.code,
        message: match error.code {
            ErrorCode::Unavailable => "File service unavailable",
            ErrorCode::PermissionDenied => "File access denied",
            ErrorCode::ServiceChanged => "File service changed",
            ErrorCode::NotFound => "File or transfer not found",
            ErrorCode::PathDenied => "Path denied",
            ErrorCode::Conflict => "Transfer or destination conflict",
            ErrorCode::InvalidArgument => "Invalid file request",
            ErrorCode::InvalidOffset => "Invalid file offset",
            ErrorCode::HashMismatch => "File checksum mismatch",
            ErrorCode::StaleVersion => "File version changed",
            ErrorCode::NoSpace => "Insufficient disk space",
            ErrorCode::ResourceLimit => "File transfer resource limit reached",
            ErrorCode::TransferExpired => "File transfer expired",
            ErrorCode::AlreadyCommitted => "File already committed",
        }
        .into(),
        expected_offset: error.expected,
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canceled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(fail(ErrorCode::Unavailable))
    } else {
        Ok(())
    }
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl FileService {
    pub fn new(state_dir: PathBuf) -> io::Result<Self> {
        Self::with_limits(state_dir, Limits::default())
    }
    pub fn with_limits(state_dir: PathBuf, limits: Limits) -> io::Result<Self> {
        Self::with_publisher(state_dir, limits, None)
    }
    pub fn with_publisher(
        state_dir: PathBuf,
        limits: Limits,
        publisher: Option<Arc<dyn Publisher>>,
    ) -> io::Result<Self> {
        fs::create_dir_all(&state_dir)?;
        if fs::symlink_metadata(&state_dir)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Files state directory must not be a symlink",
            ));
        }
        // The node owns this path; canonicalization also handles platform
        // temporary-directory aliases such as macOS /var -> /private/var.
        let state_dir = state_dir.canonicalize()?;
        let directory = open_directory(&state_dir)?;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        directory.sync_all()?;
        if let Some(parent) = state_dir.parent() {
            open_directory(parent)?.sync_all()?;
        }
        Ok(Self(Arc::new(Inner {
            publisher,
            enabled: AtomicBool::new(true),
            share_stops: watch::channel(()).0,
            directory,
            state: state_dir,
            limits,
            begin_lock: Mutex::new(()),
            locks: Mutex::new(HashMap::new()),
            bulk: Arc::new(Semaphore::new(4)),
        })))
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.0.enabled.store(enabled, Ordering::Release);
        if !enabled {
            self.0.share_stops.send_replace(());
        }
    }
    /// Read-only shares have independent service grants, enforced by the tunnel.
    pub async fn serve_share(
        &self,
        _peer: &str,
        _incarnation: &str,
        root: PathBuf,
        reader: impl AsyncRead + Unpin,
        writer: impl AsyncWrite + Unpin,
    ) -> io::Result<()> {
        let mut stops = self.0.share_stops.subscribe();
        if !self.0.enabled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Files disabled",
            ));
        }
        let canonical = root.canonicalize()?;
        if canonical.starts_with(&self.0.state) || self.0.state.starts_with(&canonical) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Private Files state cannot be shared",
            ));
        }
        tokio::select! {
            biased;
            _ = stops.changed() => Err(io::Error::new(io::ErrorKind::Interrupted, "File sharing stopped")),
            result = crate::share::serve(root, reader, writer, self.0.bulk.clone(), self.0.publisher.clone()) => result,
        }
    }
    /// Called by the node's maintenance timer, including while no peers connect.
    pub async fn cleanup_expired(&self) -> io::Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = this
                .0
                .begin_lock
                .lock()
                .map_err(|_| io::Error::other("Files cleanup unavailable"))?;
            let _disk = this
                .disk_lock("begin", false)
                .map_err(|_| io::Error::other("Files cleanup unavailable"))?;
            this.cleanup_and_quota()
                .map(|_| ())
                .map_err(|_| io::Error::other("Files cleanup unavailable"))
        })
        .await
        .map_err(|_| io::Error::other("Files cleanup unavailable"))?
    }

    /// Dropping this future (disconnect/revocation) cancels unfinished hashing
    /// and prevents a detached blocking commit from publishing its destination.
    pub async fn serve(
        &self,
        peer: &str,
        incarnation: &str,
        exports: BTreeMap<String, PathBuf>,
        mut reader: impl AsyncRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) -> io::Result<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(cancel.clone());
        let (sender, mut receiver) = mpsc::channel(8);
        let outstanding = Arc::new(AtomicUsize::new(0));
        // Keep framing in its own future. Batching only cancels channel receives,
        // never a partially consumed network frame. The queue and write batch
        // each hold at most eight chunks, bounding hostile-peer buffering too.
        let read = queue_requests(
            &mut reader,
            sender,
            outstanding.clone(),
            Duration::from_secs(300),
        );
        let process = async {
            let mut pending = None;
            loop {
                let request = match pending.take() {
                    Some(request) => request,
                    None => match receiver.recv().await {
                        Some(request) => request,
                        None => break,
                    },
                };
                let mut requests = vec![request];
                if matches!(requests[0], Request::UploadWrite { .. }) {
                    while requests.len() < 8 {
                        let next = match receiver.try_recv() {
                            Ok(request) => Some(request),
                            Err(mpsc::error::TryRecvError::Empty) => {
                                tokio::time::timeout(Duration::from_millis(2), receiver.recv())
                                    .await
                                    .ok()
                                    .flatten()
                            }
                            Err(mpsc::error::TryRecvError::Disconnected) => None,
                        };
                        let Some(next) = next else {
                            break;
                        };
                        if contiguous_writes(requests.last().unwrap(), &next) {
                            requests.push(next);
                        } else {
                            pending = Some(next);
                            break;
                        }
                    }
                }
                let responses = if requests.len() > 1 {
                    self.dispatch_batch_cancel(
                        peer,
                        incarnation,
                        &exports,
                        requests,
                        cancel.clone(),
                    )
                    .await
                } else {
                    vec![
                        self.dispatch_cancel(
                            peer,
                            incarnation,
                            &exports,
                            requests.pop().unwrap(),
                            cancel.clone(),
                        )
                        .await,
                    ]
                };
                for response in responses {
                    tokio::time::timeout(
                        Duration::from_secs(300),
                        write_response(&mut writer, &response),
                    )
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "Files response timed out")
                    })??;
                    outstanding.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Ok::<_, io::Error>(())
        };
        tokio::try_join!(read, process)?;
        Ok(())
    }

    pub async fn dispatch(
        &self,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
        request: Request,
    ) -> Response {
        self.dispatch_cancel(
            peer,
            incarnation,
            exports,
            request,
            Arc::new(AtomicBool::new(false)),
        )
        .await
    }
    async fn dispatch_cancel(
        &self,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
        request: Request,
        cancel: Arc<AtomicBool>,
    ) -> Response {
        let permit = if matches!(
            request,
            Request::UploadWrite { .. } | Request::UploadCommit { .. }
        ) {
            match self.0.bulk.clone().acquire_owned().await {
                Ok(p) => Some(p),
                Err(_) => return response_error(fail(ErrorCode::Unavailable)),
            }
        } else {
            None
        };
        let this = self.clone();
        let peer = peer.to_owned();
        let incarnation = incarnation.to_owned();
        let exports = exports.clone();
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            this.execute(&peer, &incarnation, &exports, request, &cancel)
                .unwrap_or_else(response_error)
        })
        .await
        {
            Ok(response) => response,
            Err(_) => response_error(fail(ErrorCode::Unavailable)),
        }
    }

    async fn dispatch_batch_cancel(
        &self,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
        requests: Vec<Request>,
        cancel: Arc<AtomicBool>,
    ) -> Vec<Response> {
        let count = requests.len();
        let permit = match self.0.bulk.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return (0..count)
                    .map(|_| response_error(fail(ErrorCode::Unavailable)))
                    .collect();
            }
        };
        let this = self.clone();
        let peer = peer.to_owned();
        let incarnation = incarnation.to_owned();
        let exports = exports.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match this.write_batch(&peer, &incarnation, &exports, &requests, &cancel) {
                Ok(Some(committed_offset)) => (0..count)
                    .map(|_| Response::Written { committed_offset })
                    .collect(),
                Ok(None) => requests
                    .into_iter()
                    .map(|request| {
                        this.execute(&peer, &incarnation, &exports, request, &cancel)
                            .unwrap_or_else(response_error)
                    })
                    .collect(),
                Err(error) => (0..count)
                    .map(|_| {
                        response_error(Failure {
                            code: error.code,
                            expected: error.expected,
                        })
                    })
                    .collect(),
            }
        })
        .await
        .unwrap_or_else(|_| {
            (0..count)
                .map(|_| response_error(fail(ErrorCode::Unavailable)))
                .collect()
        })
    }

    /// Fast path for an ordered, valid group. Publish one cumulative durable
    /// offset after syncing its bytes and manifest once. Every request still
    /// receives one response; duplicates/errors fall back to normal RPC rules.
    fn write_batch(
        &self,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
        requests: &[Request],
        cancel: &AtomicBool,
    ) -> Result<Option<u64>> {
        let Some(Request::UploadWrite {
            transfer_id,
            offset: first,
            ..
        }) = requests.first()
        else {
            return Ok(None);
        };
        for request in requests {
            let Request::UploadWrite {
                chunk_sha256, data, ..
            } = request
            else {
                return Ok(None);
            };
            if data.is_empty()
                || data.len() > MAX_CHUNK
                || !valid_hash(chunk_sha256)
                || sha256(data) != *chunk_sha256
            {
                return Ok(None);
            }
        }
        let existing = self.load(transfer_id)?;
        self.bound(&existing, peer, incarnation, exports)?;
        let lock = self.transfer_lock(transfer_id)?;
        let _guard = lock
            .mutex
            .lock()
            .map_err(|_| fail(ErrorCode::Unavailable))?;
        let _disk = self.disk_lock(transfer_id, false)?;
        let mut manifest = self.load(transfer_id)?;
        self.bound(&manifest, peer, incarnation, exports)?;
        if manifest.state != TransferState::Uploading || *first != manifest.committed_offset {
            return Ok(None);
        }
        self.require_uploading(&mut manifest)?;
        let mut end = manifest.committed_offset;
        for request in requests {
            let Request::UploadWrite {
                transfer_id: id,
                offset: position,
                data,
                ..
            } = request
            else {
                return Ok(None);
            };
            if id != transfer_id || *position != end {
                return Ok(None);
            }
            end = end
                .checked_add(data.len() as u64)
                .ok_or_else(|| offset(manifest.committed_offset))?;
        }
        if end > manifest.size_bytes {
            return Ok(None);
        }
        let (directory, _, partial) = self.partial_parent(&manifest)?;
        let mut file = open_file_at(&directory, &partial, libc::O_RDWR, 0)?;
        if file.metadata()?.len() < manifest.committed_offset {
            return Err(fail(ErrorCode::Unavailable));
        }
        file.set_len(manifest.committed_offset)?;
        file.seek(SeekFrom::Start(manifest.committed_offset))?;
        for request in requests {
            if !self.0.enabled.load(Ordering::Acquire) {
                return Err(fail(ErrorCode::Unavailable));
            }
            canceled(cancel)?;
            if lock.abort.load(Ordering::Acquire) {
                return Err(fail(ErrorCode::Conflict));
            }
            if let Request::UploadWrite { data, .. } = request {
                file.write_all(data)?;
            }
        }
        canceled(cancel)?;
        file.sync_data()?;
        canceled(cancel)?;
        if lock.abort.load(Ordering::Acquire) {
            return Err(fail(ErrorCode::Conflict));
        }
        manifest.committed_offset = end;
        self.save(&manifest)?;
        Ok(Some(end))
    }

    fn transfer_lock(&self, id: &str) -> Result<Arc<TransferLock>> {
        let mut locks = self
            .0
            .locks
            .lock()
            .map_err(|_| fail(ErrorCode::Unavailable))?;
        locks.retain(|_, weak| weak.strong_count() > 0);
        if !valid_hash(id) {
            return Err(fail(ErrorCode::InvalidArgument));
        }
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(TransferLock::default());
        locks.insert(id.into(), Arc::downgrade(&lock));
        Ok(lock)
    }
    fn disk_lock(&self, id: &str, nonblocking: bool) -> Result<File> {
        // A fixed set of 256 advisory lock shards prevents cross-instance
        // writers (including a restarted node) racing, without accumulating
        // an unbounded lock file for every historical transfer ID.
        let name = if id == "begin" {
            "begin"
        } else {
            id.get(..2)
                .ok_or_else(|| fail(ErrorCode::InvalidArgument))?
        };
        let file = open_file_at(
            &self.0.directory,
            &format!("{name}.lock"),
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?;
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 },
            )
        } != 0
        {
            return Err(fail(ErrorCode::ResourceLimit));
        }
        Ok(file)
    }

    fn load(&self, id: &str) -> Result<Manifest> {
        if !valid_hash(id) {
            return Err(fail(ErrorCode::InvalidArgument));
        }
        let file = open_file_at(&self.0.directory, &format!("{id}.json"), libc::O_RDONLY, 0)?;
        if file.metadata()?.len() > 16 * 1024 {
            return Err(fail(ErrorCode::Unavailable));
        }
        serde_json::from_reader(file).map_err(|_| fail(ErrorCode::Unavailable))
    }
    fn save(&self, manifest: &Manifest) -> Result<()> {
        let bytes = serde_json::to_vec(manifest).map_err(|_| fail(ErrorCode::Unavailable))?;
        let temp = format!("{}.tmp", manifest.transfer_id);
        // Only the service's private directory contains these names. A stale
        // temp from a process crash is harmless and is replaced under the lock.
        unlink_at(&self.0.directory, &temp)?;
        let mut file = open_file_at(
            &self.0.directory,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        rename_at(
            &self.0.directory,
            &temp,
            &format!("{}.json", manifest.transfer_id),
        )?;
        self.0.directory.sync_all()?;
        Ok(())
    }
    fn bound(
        &self,
        manifest: &Manifest,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
    ) -> Result<()> {
        if manifest.peer != peer {
            return Err(fail(ErrorCode::PermissionDenied));
        }
        if manifest.incarnation != incarnation
            || exports.get(&manifest.workspace) != Some(&manifest.root)
        {
            return Err(fail(ErrorCode::ServiceChanged));
        }
        Ok(())
    }
    fn export<'a>(
        &self,
        exports: &'a BTreeMap<String, PathBuf>,
        workspace: &str,
    ) -> Result<&'a Path> {
        if workspace.is_empty()
            || workspace.len() > 64
            || !workspace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            || !workspace.as_bytes()[0].is_ascii_alphanumeric()
        {
            return Err(fail(ErrorCode::InvalidArgument));
        }
        let root = exports
            .get(workspace)
            .ok_or_else(|| fail(ErrorCode::PathDenied))?;
        // Exporting the service's own metadata would disclose transfer state.
        if self.0.state.starts_with(root) || root.starts_with(&self.0.state) {
            return Err(fail(ErrorCode::PathDenied));
        }
        Ok(root)
    }
    fn partial_parent(&self, manifest: &Manifest) -> Result<(File, String, String)> {
        let (parent, name) = parent(&manifest.root, &manifest.path, false)?;
        Ok((
            parent,
            name,
            format!(".wispnet-upload-{}", manifest.transfer_id),
        ))
    }
    fn expire(&self, manifest: &mut Manifest) -> Result<()> {
        if now() >= manifest.expires_at && manifest.state == TransferState::Uploading {
            match self.partial_parent(manifest) {
                Ok((directory, _, partial)) => {
                    if let Err(error) =
                        unlink_at(&directory, &partial).and_then(|()| directory.sync_all())
                    {
                        let error = Failure::from(error);
                        // Keep ownership metadata and retry later if a mount or
                        // owner permission change makes cleanup unavailable.
                        if matches!(error.code, ErrorCode::PathDenied | ErrorCode::Unavailable) {
                            return Ok(());
                        }
                        return Err(error);
                    }
                }
                // The owner already removed the export or job directory.
                Err(Failure {
                    code: ErrorCode::NotFound,
                    ..
                }) => {}
                // Never follow a substituted symlink or discard metadata that
                // is still needed to clean up an unavailable owned partial.
                Err(Failure {
                    code: ErrorCode::PathDenied | ErrorCode::Unavailable,
                    ..
                }) => return Ok(()),
                Err(error) => return Err(error),
            }
            manifest.state = TransferState::Expired;
            self.save(manifest)?;
        }
        Ok(())
    }
    fn require_uploading(&self, manifest: &mut Manifest) -> Result<()> {
        self.expire(manifest)?;
        if now() >= manifest.expires_at {
            return Err(fail(ErrorCode::TransferExpired));
        }
        match manifest.state {
            TransferState::Uploading => Ok(()),
            TransferState::Expired => Err(fail(ErrorCode::TransferExpired)),
            TransferState::Committed => Err(fail(ErrorCode::AlreadyCommitted)),
            _ => Err(fail(ErrorCode::Conflict)),
        }
    }
    // Recover the crash window after the no-replace destination link was made
    // but before committed metadata was durable. Never unlink the destination.
    fn recover_commit(&self, manifest: &mut Manifest, cancel: &AtomicBool) -> Result<bool> {
        if self.0.publisher.is_some() {
            return Ok(false);
        }
        if manifest.state != TransferState::Uploading
            || manifest.committed_offset != manifest.size_bytes
        {
            return Ok(false);
        }
        let (directory, name, partial) = self.partial_parent(manifest)?;
        let mut destination = match open_file_at(&directory, &name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let destination_metadata = destination.metadata()?;
        match open_file_at(&directory, &partial, libc::O_RDONLY, 0) {
            Ok(file) => {
                let metadata = file.metadata()?;
                if metadata.dev() != destination_metadata.dev()
                    || metadata.ino() != destination_metadata.ino()
                {
                    return Ok(false);
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error) => return Err(error.into()),
        }
        if destination_metadata.len() != manifest.size_bytes
            || hash_cancel(&mut destination, cancel, &AtomicBool::new(false))? != manifest.sha256
        {
            return Err(fail(ErrorCode::HashMismatch));
        }
        if fingerprint(&destination.metadata()?) != fingerprint(&destination_metadata) {
            return Err(fail(ErrorCode::StaleVersion));
        }
        canceled(cancel)?;
        unlink_at(&directory, &partial)?;
        directory.sync_all()?;
        manifest.file = Some(info(
            &manifest.path,
            manifest.sha256.clone(),
            &destination.metadata()?,
        ));
        manifest.state = TransferState::Committed;
        self.save(manifest)?;
        Ok(true)
    }
    fn cleanup_and_quota(&self) -> Result<(usize, u64)> {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in fs::read_dir(&self.0.state)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            if !valid_hash(id) {
                continue;
            }
            let lock = self.transfer_lock(id)?;
            let Ok(_guard) = lock.mutex.try_lock() else {
                // An active transfer is counted from its atomically published
                // manifest without waiting for its disk work.
                let manifest = self.load(id)?;
                count += 1;
                if manifest.state == TransferState::Uploading {
                    bytes = bytes.saturating_add(manifest.size_bytes);
                }
                continue;
            };
            let Ok(_disk) = self.disk_lock(id, true) else {
                let manifest = self.load(id)?;
                count += 1;
                if manifest.state == TransferState::Uploading {
                    bytes = bytes.saturating_add(manifest.size_bytes);
                }
                continue;
            };
            let mut manifest = self.load(id)?;
            self.expire(&mut manifest)?;
            if now() >= manifest.expires_at && manifest.state != TransferState::Uploading {
                unlink_at(&self.0.directory, &format!("{id}.json"))?;
            } else {
                count += 1;
                if manifest.state == TransferState::Uploading {
                    bytes = bytes.saturating_add(manifest.size_bytes);
                }
            }
        }
        self.0.directory.sync_all()?;
        Ok((count, bytes))
    }

    fn execute(
        &self,
        peer: &str,
        incarnation: &str,
        exports: &BTreeMap<String, PathBuf>,
        request: Request,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Response> {
        if !self.0.enabled.load(Ordering::Acquire) {
            return Err(fail(ErrorCode::Unavailable));
        }
        canceled(cancel)?;
        match request {
            Request::Workspaces => Ok(Response::Workspaces {
                workspaces: exports.keys().cloned().collect(),
            }),
            Request::UploadBegin {
                workspace,
                path,
                size_bytes,
                sha256: hash,
                idempotency_key,
            } => {
                if !valid_hash(&hash) || idempotency_key.is_empty() || idempotency_key.len() > 128 {
                    return Err(fail(ErrorCode::InvalidArgument));
                }
                components(&path)?;
                // Desktop/mobile senders leave destination selection to the
                // receiver. Bind the resolved export privately so resume and
                // policy checks retain the same guarantees as named uploads.
                let workspace = if workspace.is_empty() {
                    exports
                        .keys()
                        .next()
                        .cloned()
                        .ok_or_else(|| fail(ErrorCode::Unavailable))?
                } else {
                    workspace
                };
                let root = self.export(exports, &workspace)?.to_path_buf();
                let id = sha256(
                    &serde_json::to_vec(&(peer, incarnation, &idempotency_key))
                        .map_err(|_| fail(ErrorCode::InvalidArgument))?,
                );
                let _begin = self
                    .0
                    .begin_lock
                    .lock()
                    .map_err(|_| fail(ErrorCode::Unavailable))?;
                let _begin_disk = self.disk_lock("begin", false)?;
                if size_bytes > self.0.limits.max_upload_bytes {
                    return Err(fail(ErrorCode::ResourceLimit));
                }
                let (count, bytes) = self.cleanup_and_quota()?;
                let already_exists = self.load(&id).is_ok();
                if !already_exists
                    && (count >= self.0.limits.max_transfers
                        || bytes.saturating_add(size_bytes) > self.0.limits.max_pending_bytes)
                {
                    return Err(fail(ErrorCode::ResourceLimit));
                }
                let lock = self.transfer_lock(&id)?;
                let _guard = lock
                    .mutex
                    .lock()
                    .map_err(|_| fail(ErrorCode::Unavailable))?;
                let _disk = self.disk_lock(&id, false)?;
                match self.load(&id) {
                    Ok(mut manifest) => {
                        self.bound(&manifest, peer, incarnation, exports)?;
                        if manifest.workspace != workspace
                            || manifest.path != path
                            || manifest.size_bytes != size_bytes
                            || manifest.sha256 != hash
                            || manifest.idempotency_key != idempotency_key
                        {
                            return Err(fail(ErrorCode::Conflict));
                        }
                        self.expire(&mut manifest)?;
                        if manifest.state == TransferState::Uploading
                            && manifest.committed_offset == 0
                            && now() < manifest.expires_at
                        {
                            let (directory, _, partial) = self.partial_parent(&manifest)?;
                            let partial_file = open_file_at(
                                &directory,
                                &partial,
                                libc::O_RDWR | libc::O_CREAT,
                                0o600,
                            )?;
                            partial_file.sync_all()?;
                            directory.sync_all()?;
                        }
                        return Ok(manifest.response());
                    }
                    Err(Failure {
                        code: ErrorCode::NotFound,
                        ..
                    }) => {}
                    Err(error) => return Err(error),
                }
                let (directory, name) = parent(&root, &path, true)?;
                ensure_absent(&directory, &name)?;
                let partial = format!(".wispnet-upload-{id}");
                let manifest = Manifest {
                    transfer_id: id,
                    peer: peer.into(),
                    incarnation: incarnation.into(),
                    workspace,
                    root,
                    path,
                    size_bytes,
                    sha256: hash,
                    idempotency_key,
                    state: TransferState::Uploading,
                    committed_offset: 0,
                    expires_at: now().saturating_add(self.0.limits.retention_secs),
                    file: None,
                };
                // Persist ownership/path before creating partial data, so even
                // a crash during initialization leaves cleanup enough state.
                self.save(&manifest)?;
                let file = open_file_at(
                    &directory,
                    &partial,
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )?;
                file.sync_all()?;
                directory.sync_all()?;
                Ok(manifest.response())
            }
            Request::UploadStatus { transfer_id } => {
                let manifest = self.load(&transfer_id)?;
                self.bound(&manifest, peer, incarnation, exports)?;
                if manifest.state == TransferState::Uploading
                    && manifest.committed_offset == manifest.size_bytes
                {
                    let lock = self.transfer_lock(&transfer_id)?;
                    if let Ok(_guard) = lock.mutex.try_lock()
                        && let Ok(_disk) = self.disk_lock(&transfer_id, true)
                    {
                        let mut latest = self.load(&transfer_id)?;
                        if self.recover_commit(&mut latest, cancel)? {
                            return Ok(latest.response());
                        }
                    }
                }
                if now() >= manifest.expires_at && manifest.state == TransferState::Uploading {
                    // Expiry cannot block status behind an hours-long checksum.
                    let lock = self.transfer_lock(&transfer_id)?;
                    if let Ok(_guard) = lock.mutex.try_lock()
                        && let Ok(_disk) = self.disk_lock(&transfer_id, true)
                    {
                        let mut latest = self.load(&transfer_id)?;
                        self.expire(&mut latest)?;
                        return Ok(latest.response());
                    }
                }
                Ok(manifest.response())
            }
            Request::UploadWrite {
                transfer_id,
                offset: position,
                chunk_sha256,
                data,
            } => {
                if data.is_empty() || data.len() > MAX_CHUNK || !valid_hash(&chunk_sha256) {
                    return Err(fail(ErrorCode::InvalidArgument));
                }
                if sha256(&data) != chunk_sha256 {
                    return Err(fail(ErrorCode::HashMismatch));
                }
                let existing = self.load(&transfer_id)?;
                self.bound(&existing, peer, incarnation, exports)?;
                let lock = self.transfer_lock(&transfer_id)?;
                let _guard = lock
                    .mutex
                    .lock()
                    .map_err(|_| fail(ErrorCode::Unavailable))?;
                let _disk = self.disk_lock(&transfer_id, false)?;
                let mut manifest = self.load(&transfer_id)?;
                self.bound(&manifest, peer, incarnation, exports)?;
                if manifest.state == TransferState::Committed {
                    let end = position
                        .checked_add(data.len() as u64)
                        .ok_or_else(|| offset(manifest.committed_offset))?;
                    if end > manifest.committed_offset {
                        return Err(offset(manifest.committed_offset));
                    }
                    let (directory, name) = parent(&manifest.root, &manifest.path, false)?;
                    let mut file = open_file_at(&directory, &name, libc::O_RDONLY, 0)?;
                    let before = fingerprint(&file.metadata()?);
                    if manifest
                        .file
                        .as_ref()
                        .and_then(|file| file.file_version.split_once(':'))
                        .map(|(_, fingerprint)| fingerprint)
                        != Some(before.as_str())
                    {
                        return Err(fail(ErrorCode::StaleVersion));
                    }
                    let mut previous = vec![0; data.len()];
                    file.seek(SeekFrom::Start(position))?;
                    file.read_exact(&mut previous)?;
                    if fingerprint(&file.metadata()?) != before {
                        return Err(fail(ErrorCode::StaleVersion));
                    }
                    if previous != data {
                        return Err(fail(ErrorCode::Conflict));
                    }
                    return Ok(Response::Written {
                        committed_offset: manifest.committed_offset,
                    });
                }
                self.require_uploading(&mut manifest)?;
                let end = position
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| offset(manifest.committed_offset))?;
                if end > manifest.size_bytes || position > manifest.committed_offset {
                    return Err(offset(manifest.committed_offset));
                }
                let (directory, _, partial) = self.partial_parent(&manifest)?;
                let mut file = open_file_at(&directory, &partial, libc::O_RDWR, 0)?;
                if position < manifest.committed_offset {
                    if end > manifest.committed_offset {
                        return Err(offset(manifest.committed_offset));
                    }
                    let mut previous = vec![0; data.len()];
                    file.seek(SeekFrom::Start(position))?;
                    file.read_exact(&mut previous)?;
                    if previous != data {
                        return Err(fail(ErrorCode::Conflict));
                    }
                    return Ok(Response::Written {
                        committed_offset: manifest.committed_offset,
                    });
                }
                if lock.abort.load(Ordering::Acquire) {
                    return Err(fail(ErrorCode::Conflict));
                }
                canceled(cancel)?;
                if file.metadata()?.len() < manifest.committed_offset {
                    return Err(fail(ErrorCode::Unavailable));
                }
                file.set_len(manifest.committed_offset)?;
                file.seek(SeekFrom::Start(position))?;
                file.write_all(&data)?;
                file.sync_data()?;
                manifest.committed_offset = end;
                self.save(&manifest)?;
                Ok(Response::Written {
                    committed_offset: end,
                })
            }
            Request::UploadCommit { transfer_id } => {
                let existing = self.load(&transfer_id)?;
                self.bound(&existing, peer, incarnation, exports)?;
                let lock = self.transfer_lock(&transfer_id)?;
                let _guard = lock
                    .mutex
                    .lock()
                    .map_err(|_| fail(ErrorCode::Unavailable))?;
                let _disk = self.disk_lock(&transfer_id, false)?;
                let mut manifest = self.load(&transfer_id)?;
                self.bound(&manifest, peer, incarnation, exports)?;
                if manifest.state == TransferState::Committed {
                    return Ok(Response::File {
                        file: manifest.file.ok_or_else(|| fail(ErrorCode::Unavailable))?,
                    });
                }
                if self.recover_commit(&mut manifest, cancel)? {
                    return Ok(Response::File {
                        file: manifest.file.ok_or_else(|| fail(ErrorCode::Unavailable))?,
                    });
                }
                self.require_uploading(&mut manifest)?;
                if manifest.committed_offset != manifest.size_bytes {
                    return Err(offset(manifest.committed_offset));
                }
                let (directory, name, partial) = self.partial_parent(&manifest)?;
                let mut file = open_file_at(&directory, &partial, libc::O_RDONLY, 0)?;
                if file.metadata()?.len() != manifest.size_bytes {
                    return Err(fail(ErrorCode::HashMismatch));
                }
                let before = fingerprint(&file.metadata()?);
                let actual = hash_cancel(&mut file, cancel, &lock.abort)?;
                if actual != manifest.sha256 {
                    return Err(fail(ErrorCode::HashMismatch));
                }
                if fingerprint(&file.metadata()?) != before {
                    return Err(fail(ErrorCode::StaleVersion));
                }
                canceled(cancel)?;
                if lock.abort.load(Ordering::Acquire) {
                    return Err(fail(ErrorCode::Conflict));
                }
                if let Some(publisher) = &self.0.publisher {
                    file.seek(SeekFrom::Start(0))?;
                    publisher.publish(
                        Publication {
                            transfer_id: &manifest.transfer_id,
                            root: &manifest.root,
                            path: &manifest.path,
                            size_bytes: manifest.size_bytes,
                            sha256: &manifest.sha256,
                        },
                        file.try_clone()?,
                        cancel.clone(),
                    )?;
                    canceled(cancel)?;
                    if !self.0.enabled.load(Ordering::Acquire) {
                        return Err(fail(ErrorCode::Unavailable));
                    }
                    let file_info =
                        info(&manifest.path, manifest.sha256.clone(), &file.metadata()?);
                    manifest.state = TransferState::Committed;
                    manifest.file = Some(file_info.clone());
                    // Persist success before dropping the only private staged copy.
                    // A crash before this save retries the idempotent publisher.
                    self.save(&manifest)?;
                    unlink_at(&directory, &partial)?;
                    directory.sync_all()?;
                    return Ok(Response::File { file: file_info });
                }
                // Hard-link creation is atomic and refuses to replace any
                // destination. Keep the partial link until committed metadata
                // is durable so a lost response/crash can be recovered.
                match link_at(&directory, &partial, &name) {
                    Ok(()) => directory.sync_all()?,
                    Err(error) if error.code == ErrorCode::Conflict => {
                        let destination = open_file_at(&directory, &name, libc::O_RDONLY, 0)?;
                        let left = file.metadata()?;
                        let right = destination.metadata()?;
                        if left.dev() != right.dev() || left.ino() != right.ino() {
                            return Err(error);
                        }
                    }
                    Err(error) => return Err(error),
                }
                unlink_at(&directory, &partial)?;
                directory.sync_all()?;
                let file_info = info(&manifest.path, manifest.sha256.clone(), &file.metadata()?);
                manifest.state = TransferState::Committed;
                manifest.file = Some(file_info.clone());
                self.save(&manifest)?;
                Ok(Response::File { file: file_info })
            }
            Request::UploadAbort { transfer_id } => {
                let existing = self.load(&transfer_id)?;
                self.bound(&existing, peer, incarnation, exports)?;
                let lock = self.transfer_lock(&transfer_id)?;
                lock.abort.store(true, Ordering::Release);
                let _guard = lock
                    .mutex
                    .lock()
                    .map_err(|_| fail(ErrorCode::Unavailable))?;
                let _disk = self.disk_lock(&transfer_id, false)?;
                let mut manifest = self.load(&transfer_id)?;
                self.bound(&manifest, peer, incarnation, exports)?;
                if manifest.state == TransferState::Committed {
                    return Err(fail(ErrorCode::AlreadyCommitted));
                }
                if manifest.state == TransferState::Aborted {
                    return Ok(Response::Aborted);
                }
                if self.recover_commit(&mut manifest, cancel)? {
                    return Err(fail(ErrorCode::AlreadyCommitted));
                }
                let (directory, name, partial) = self.partial_parent(&manifest)?;
                // A crash between publishing and persisting committed state
                // must be recognized before abort removes its partial link.
                if let (Ok(left), Ok(right)) = (
                    open_file_at(&directory, &partial, libc::O_RDONLY, 0),
                    open_file_at(&directory, &name, libc::O_RDONLY, 0),
                ) {
                    let left = left.metadata()?;
                    let right = right.metadata()?;
                    if left.dev() == right.dev() && left.ino() == right.ino() {
                        manifest.file = Some(info(&manifest.path, manifest.sha256.clone(), &right));
                        manifest.state = TransferState::Committed;
                        self.save(&manifest)?;
                        return Err(fail(ErrorCode::AlreadyCommitted));
                    }
                }
                unlink_at(&directory, &partial)?;
                directory.sync_all()?;
                manifest.state = TransferState::Aborted;
                self.save(&manifest)?;
                Ok(Response::Aborted)
            }
            // Every Files caller, including legacy clients, has an incoming-only
            // grant. Reject reads before touching any export or revealing metadata.
            Request::FileStat { .. }
            | Request::FileReadRange { .. }
            | Request::ShareList { .. }
            | Request::ShareStat { .. }
            | Request::ShareReadRange { .. }
            | Request::ShareReadRangeEncoded { .. }
            | Request::ShareCapabilities => Err(fail(ErrorCode::PermissionDenied)),
        }
    }
}

pub(super) async fn queue_requests(
    mut reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<Request>,
    outstanding: Arc<AtomicUsize>,
    idle_timeout: Duration,
) -> io::Result<()> {
    loop {
        // Retain the very same framing future across timer extensions: a
        // timeout check must never discard a partially consumed frame. A peer
        // waiting for a long checksum/commit response is not an idle peer.
        let request = {
            let read = read_request(&mut reader);
            tokio::pin!(read);
            loop {
                match tokio::time::timeout(idle_timeout, &mut read).await {
                    Ok(request) => break request?,
                    Err(_) if outstanding.load(Ordering::Acquire) > 0 => continue,
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Files frame timed out",
                        ));
                    }
                }
            }
        };
        let Some(request) = request else {
            return Ok(());
        };
        outstanding.fetch_add(1, Ordering::AcqRel);
        sender
            .send(request)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Files stream closed"))?;
    }
}

pub(super) fn fingerprint(metadata: &Metadata) -> String {
    sha256(
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
        .as_bytes(),
    )
}
fn contiguous_writes(previous: &Request, next: &Request) -> bool {
    matches!((previous, next),
        (Request::UploadWrite { transfer_id: left, offset, data, .. }, Request::UploadWrite { transfer_id: right, offset: next, .. })
        if left == right && offset.checked_add(data.len() as u64) == Some(*next))
}
fn info(path: &str, hash: String, metadata: &Metadata) -> FileInfo {
    FileInfo {
        path: path.into(),
        size_bytes: metadata.len(),
        file_version: format!("{hash}:{}", fingerprint(metadata)),
        sha256: hash,
    }
}
fn hash_cancel(mut file: impl Read, cancel: &AtomicBool, abort: &AtomicBool) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        canceled(cancel)?;
        if abort.load(Ordering::Acquire) {
            return Err(fail(ErrorCode::Conflict));
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(encode_hash(hasher.finalize()))
}
fn cstring(value: &str) -> io::Result<CString> {
    CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Files path"))
}
pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute directory required",
        ));
    }
    let components: Vec<_> = path.components().collect();
    // Android permits searching an app's /data ancestors, not listing them.
    // O_PATH retains a no-follow directory capability without requiring read
    // access. The final directory remains readable for fsync and enumeration.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let ancestor_mode = libc::O_PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let ancestor_mode = libc::O_RDONLY;
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if components.len() > 1 {
                    ancestor_mode
                } else {
                    libc::O_RDONLY
                },
        )
        .open("/")?;
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt;
                let name = CString::new(name.as_bytes()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid Files directory")
                })?;
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_DIRECTORY
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC
                            | if index + 1 == components.len() {
                                libc::O_RDONLY
                            } else {
                                ancestor_mode
                            },
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                directory = unsafe { File::from_raw_fd(fd) };
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid Files directory",
                ));
            }
        }
    }
    Ok(directory)
}
fn open_file_at(directory: &File, name: &str, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = cstring(name)?;
    // NONBLOCK prevents a substituted FIFO from hanging before type checking.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            // Darwin mode_t is u16; variadic C arguments require promotion.
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "regular file required",
        ));
    }
    Ok(file)
}
fn components(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() || path.len() > 4096 || path.contains(['\0', '\\']) {
        return Err(fail(ErrorCode::PathDenied));
    }
    let components: Vec<_> = path.split('/').collect();
    if components.iter().any(|p| {
        p.is_empty() || *p == "." || *p == ".." || p.len() > 255 || p.starts_with(".wispnet-")
    }) {
        return Err(fail(ErrorCode::PathDenied));
    }
    Ok(components)
}
fn parent(root: &Path, path: &str, create: bool) -> Result<(File, String)> {
    let components = components(path)?;
    let mut directory = open_directory(root)?;
    for component in &components[..components.len() - 1] {
        let name = cstring(component)?;
        if create {
            let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::last_os_error().into());
            }
            if result == 0 {
                directory.sync_all()?;
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok((directory, components.last().unwrap().to_string()))
}
fn ensure_absent(directory: &File, name: &str) -> Result<()> {
    let name = cstring(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Err(fail(ErrorCode::Conflict));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error.into())
    }
}
fn unlink_at(directory: &File, name: &str) -> io::Result<()> {
    let name = cstring(name)?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error)
    }
}
fn rename_at(directory: &File, old: &str, new: &str) -> io::Result<()> {
    let old = cstring(old)?;
    let new = cstring(new)?;
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            old.as_ptr(),
            directory.as_raw_fd(),
            new.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn link_at(directory: &File, old: &str, new: &str) -> Result<()> {
    let old = cstring(old)?;
    let new = cstring(new)?;
    if unsafe {
        libc::linkat(
            directory.as_raw_fd(),
            old.as_ptr(),
            directory.as_raw_fd(),
            new.as_ptr(),
            0,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn state_can_be_opened_through_search_only_ancestors_without_following_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let ancestor = temporary.path().join("search-only");
        let parent = ancestor.join("app-private");
        let state = parent.join("files");
        fs::create_dir_all(&state).unwrap();
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            }
        }
        let _restore = Restore(ancestor.clone());
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o111)).unwrap();
        let service = FileService::new(state).unwrap();
        service.0.directory.sync_all().unwrap();
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&ancestor, &alias).unwrap();
        assert!(open_directory(&alias.join("app-private/files")).is_err());
    }

    #[derive(Default)]
    struct TestPublisher {
        fail: AtomicBool,
        publications: Mutex<BTreeMap<String, Vec<u8>>>,
        calls: AtomicUsize,
    }
    impl Publisher for TestPublisher {
        fn publish(
            &self,
            publication: Publication<'_>,
            mut file: File,
            cancel: Arc<AtomicBool>,
        ) -> io::Result<()> {
            assert!(!cancel.load(Ordering::Acquire));
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Acquire) {
                return Err(io::Error::other("provider unavailable"));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            assert_eq!(sha256(&bytes), publication.sha256);
            assert_eq!(bytes.len() as u64, publication.size_bytes);
            self.publications
                .lock()
                .unwrap()
                .entry(publication.transfer_id.into())
                .or_insert(bytes);
            Ok(())
        }
    }
    #[tokio::test]
    async fn platform_publication_is_verified_retryable_and_acknowledged_only_after_save() {
        let mut f = Fixture::new();
        let publisher = Arc::new(TestPublisher::default());
        f.service = FileService::with_publisher(
            f.state.clone(),
            Limits::default(),
            Some(publisher.clone()),
        )
        .unwrap();
        let id = f
            .begin("phone.bin", b"verified bytes", "platform-storage")
            .await;
        f.write(&id, 0, b"verified bytes").await;
        publisher.fail.store(true, Ordering::Release);
        error(
            f.call(Request::UploadCommit {
                transfer_id: id.clone(),
            })
            .await,
            ErrorCode::Unavailable,
        );
        assert_eq!(f.service.load(&id).unwrap().state, TransferState::Uploading);
        assert!(f.root.join(format!(".wispnet-upload-{id}")).exists());
        publisher.fail.store(false, Ordering::Release);
        // Also cover the crash after the provider saved a file but before native
        // committed metadata. The adapter reuses that same transfer's document.
        publisher
            .publications
            .lock()
            .unwrap()
            .insert(id.clone(), b"verified bytes".to_vec());
        f.service = FileService::with_publisher(
            f.state.clone(),
            Limits::default(),
            Some(publisher.clone()),
        )
        .unwrap();
        assert!(matches!(
            f.call(Request::UploadCommit {
                transfer_id: id.clone()
            })
            .await,
            Response::File { .. }
        ));
        assert!(!f.root.join(format!(".wispnet-upload-{id}")).exists());
        assert_eq!(publisher.publications.lock().unwrap().len(), 1);
        assert_eq!(
            publisher.publications.lock().unwrap()[&id],
            b"verified bytes"
        );
        let calls = publisher.calls.load(Ordering::Relaxed);
        assert!(matches!(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            Response::File { .. }
        ));
        assert_eq!(publisher.calls.load(Ordering::Relaxed), calls);
    }
    #[tokio::test]
    async fn platform_receiver_offline_preserves_offsets_and_reenables_resume() {
        let f = Fixture::new();
        let id = f.begin("resume.bin", b"1234", "offline").await;
        f.write(&id, 0, b"12").await;
        f.service.set_enabled(false);
        error(f.write(&id, 2, b"34").await, ErrorCode::Unavailable);
        assert_eq!(f.service.load(&id).unwrap().committed_offset, 2);
        f.service.set_enabled(true);
        assert!(matches!(
            f.write(&id, 2, b"34").await,
            Response::Written {
                committed_offset: 4
            }
        ));
        assert!(matches!(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            Response::File { .. }
        ));
    }

    struct Fixture {
        _temp: TempDir,
        state: PathBuf,
        root: PathBuf,
        exports: BTreeMap<String, PathBuf>,
        service: FileService,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let root = base.join("export");
            fs::create_dir(&root).unwrap();
            let state = base.join("state");
            let service = FileService::new(state.clone()).unwrap();
            Self {
                _temp: temp,
                state,
                root: root.clone(),
                exports: BTreeMap::from([("training.v1".into(), root)]),
                service,
            }
        }
        async fn call(&self, request: Request) -> Response {
            self.service
                .dispatch("peer-a", "inc-a", &self.exports, request)
                .await
        }
        async fn begin(&self, path: &str, bytes: &[u8], key: &str) -> String {
            match self
                .call(Request::UploadBegin {
                    workspace: "training.v1".into(),
                    path: path.into(),
                    size_bytes: bytes.len() as u64,
                    sha256: sha256(bytes),
                    idempotency_key: key.into(),
                })
                .await
            {
                Response::Upload {
                    transfer_id,
                    state: TransferState::Uploading,
                    ..
                } => transfer_id,
                response => panic!("unexpected begin: {response:?}"),
            }
        }
        async fn write(&self, id: &str, position: u64, bytes: &[u8]) -> Response {
            self.call(Request::UploadWrite {
                transfer_id: id.into(),
                offset: position,
                chunk_sha256: sha256(bytes),
                data: bytes.to_vec(),
            })
            .await
        }
    }
    fn error(response: Response, expected: ErrorCode) {
        match response {
            Response::Error { code, message, .. } => {
                assert_eq!(code, expected, "{message}");
                assert!(!message.contains("/"));
            }
            response => panic!("expected {expected:?}, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn receiver_default_upload_is_private_resumable_and_keeps_named_targets() {
        let mut f = Fixture::new();
        let other = f.root.parent().unwrap().join("other-private-folder");
        fs::create_dir(&other).unwrap();
        f.exports.insert("z-other".into(), other.clone());
        let bytes = b"sender never needs the receiving folder";
        let begin = Request::UploadBegin {
            workspace: String::new(),
            path: "sample.bin".into(),
            size_bytes: bytes.len() as u64,
            sha256: sha256(bytes),
            idempotency_key: "receiver-default".into(),
        };
        let started = f.call(begin.clone()).await;
        let Response::Upload {
            transfer_id: id, ..
        } = &started
        else {
            panic!("unexpected default upload: {started:?}");
        };
        let id = id.clone();
        let wire = serde_json::to_string(&started).unwrap();
        assert!(!wire.contains("training.v1"));
        assert!(!wire.contains(f.root.to_str().unwrap()));
        assert_eq!(
            f.write(&id, 0, &bytes[..12]).await,
            Response::Written {
                committed_offset: 12
            }
        );
        f.service = FileService::new(f.state.clone()).unwrap();
        assert!(matches!(
            f.call(begin).await,
            Response::Upload { transfer_id, committed_offset: 12, .. } if transfer_id == id
        ));
        assert_eq!(
            f.write(&id, 12, &bytes[12..]).await,
            Response::Written {
                committed_offset: bytes.len() as u64
            }
        );
        let committed = f
            .call(Request::UploadCommit {
                transfer_id: id.clone(),
            })
            .await;
        assert!(matches!(&committed, Response::File { file } if file.path == "sample.bin"));
        assert_eq!(fs::read(f.root.join("sample.bin")).unwrap(), bytes);
        assert!(!other.join("sample.bin").exists());
        for response in [
            committed,
            f.call(Request::UploadStatus { transfer_id: id }).await,
        ] {
            let wire = serde_json::to_string(&response).unwrap();
            assert!(!wire.contains("training.v1"));
            assert!(!wire.contains(f.root.to_str().unwrap()));
        }
        // Explicit CLI/SDK workspaces continue to select their named export.
        let named = f
            .call(Request::UploadBegin {
                workspace: "z-other".into(),
                path: "named-empty.bin".into(),
                size_bytes: 0,
                sha256: sha256(b""),
                idempotency_key: "named-target".into(),
            })
            .await;
        let Response::Upload { transfer_id, .. } = named else {
            panic!("unexpected named upload: {named:?}");
        };
        assert!(matches!(
            f.call(Request::UploadCommit { transfer_id }).await,
            Response::File { .. }
        ));
        assert!(other.join("named-empty.bin").is_file());
        assert!(!f.root.join("named-empty.bin").exists());
    }

    #[tokio::test]
    async fn receiver_default_requires_an_export_and_retains_private_binding_checks() {
        let f = Fixture::new();
        let request = Request::UploadBegin {
            workspace: String::new(),
            path: "sample.bin".into(),
            size_bytes: 1,
            sha256: sha256(b"a"),
            idempotency_key: "default-binding".into(),
        };
        error(
            f.service
                .dispatch("peer-a", "inc-a", &BTreeMap::new(), request.clone())
                .await,
            ErrorCode::Unavailable,
        );
        let started = f.call(request.clone()).await;
        let Response::Upload { transfer_id, .. } = started else {
            panic!("unexpected default upload: {started:?}");
        };
        let replacement = f.root.parent().unwrap().join("replacement");
        fs::create_dir(&replacement).unwrap();
        let exports = BTreeMap::from([("training.v1".into(), replacement)]);
        error(
            f.service
                .dispatch("peer-a", "inc-a", &exports, request)
                .await,
            ErrorCode::ServiceChanged,
        );
        error(
            f.service
                .dispatch(
                    "peer-a",
                    "inc-a",
                    &exports,
                    Request::UploadStatus { transfer_id },
                )
                .await,
            ErrorCode::ServiceChanged,
        );
    }

    #[tokio::test]
    async fn durable_resume_duplicate_ack_lost_commit_and_empty_files() {
        let mut f = Fixture::new();
        let bytes = vec![37; 32 * 1024];
        let id = f.begin("jobs/a/train.bin", &bytes, "key-a").await;
        assert_eq!(
            f.write(&id, 0, &bytes[..16384]).await,
            Response::Written {
                committed_offset: 16384
            }
        );
        assert!(!f.root.join("jobs/a/train.bin").exists());
        f.service = FileService::new(f.state.clone()).unwrap();
        assert_eq!(f.begin("jobs/a/train.bin", &bytes, "key-a").await, id);
        assert_eq!(
            f.write(&id, 0, &bytes[..16384]).await,
            Response::Written {
                committed_offset: 16384
            }
        );
        error(f.write(&id, 0, &[38; 16384]).await, ErrorCode::Conflict);
        let invalid = f.write(&id, 20000, &[1]).await;
        assert!(matches!(
            invalid,
            Response::Error {
                code: ErrorCode::InvalidOffset,
                expected_offset: Some(16384),
                ..
            }
        ));
        assert_eq!(
            f.write(&id, 16384, &bytes[16384..]).await,
            Response::Written {
                committed_offset: 32768
            }
        );
        let committed = f
            .call(Request::UploadCommit {
                transfer_id: id.clone(),
            })
            .await;
        let Response::File { .. } = committed.clone() else {
            panic!("{committed:?}");
        };
        assert_eq!(fs::read(f.root.join("jobs/a/train.bin")).unwrap(), bytes);
        assert_eq!(
            f.call(Request::UploadCommit {
                transfer_id: id.clone()
            })
            .await,
            committed
        );
        assert!(matches!(
            f.call(Request::UploadStatus {
                transfer_id: id.clone()
            })
            .await,
            Response::Upload {
                state: TransferState::Committed,
                file: Some(_),
                ..
            }
        ));
        error(
            f.call(Request::UploadAbort { transfer_id: id }).await,
            ErrorCode::AlreadyCommitted,
        );
        let id = f.begin("empty", &[], "empty").await;
        assert!(matches!(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            Response::File {
                file: FileInfo { size_bytes: 0, .. }
            }
        ));
    }

    #[tokio::test]
    async fn binding_checks_and_idempotency_conflicts() {
        let f = Fixture::new();
        let id = f.begin("input", b"hello", "stable-key").await;
        error(
            f.call(Request::UploadBegin {
                workspace: "training.v1".into(),
                path: "other".into(),
                size_bytes: 5,
                sha256: sha256(b"hello"),
                idempotency_key: "stable-key".into(),
            })
            .await,
            ErrorCode::Conflict,
        );
        error(
            f.service
                .dispatch(
                    "peer-b",
                    "inc-a",
                    &f.exports,
                    Request::UploadStatus {
                        transfer_id: id.clone(),
                    },
                )
                .await,
            ErrorCode::PermissionDenied,
        );
        error(
            f.service
                .dispatch(
                    "peer-a",
                    "inc-b",
                    &f.exports,
                    Request::UploadStatus {
                        transfer_id: id.clone(),
                    },
                )
                .await,
            ErrorCode::ServiceChanged,
        );
        error(
            f.service
                .dispatch(
                    "peer-a",
                    "inc-a",
                    &BTreeMap::new(),
                    Request::UploadStatus { transfer_id: id },
                )
                .await,
            ErrorCode::ServiceChanged,
        );
        error(
            f.call(Request::UploadStatus {
                transfer_id: "../../secret".into(),
            })
            .await,
            ErrorCode::InvalidArgument,
        );
    }

    #[tokio::test]
    async fn rejects_paths_symlinks_special_files_and_private_partial_names() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.state.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        symlink(&outside, f.root.join("escape")).unwrap();
        symlink(outside.join("secret"), f.root.join("linked-file")).unwrap();
        for (path, expected) in [
            ("/absolute", ErrorCode::PathDenied),
            ("../outside/secret", ErrorCode::PathDenied),
            ("escape/secret", ErrorCode::PathDenied),
            ("linked-file", ErrorCode::Conflict),
            (".wispnet-upload-private", ErrorCode::PathDenied),
            ("a//b", ErrorCode::PathDenied),
            ("./secret", ErrorCode::PathDenied),
        ] {
            error(
                f.call(Request::UploadBegin {
                    workspace: "training.v1".into(),
                    path: path.into(),
                    size_bytes: 0,
                    sha256: sha256(b""),
                    idempotency_key: path.into(),
                })
                .await,
                expected,
            );
        }
        let fifo = CString::new(f.root.join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        error(
            f.call(Request::UploadBegin {
                workspace: "training.v1".into(),
                path: "fifo".into(),
                size_bytes: 0,
                sha256: sha256(b""),
                idempotency_key: "fifo".into(),
            })
            .await,
            ErrorCode::Conflict,
        );
        symlink(&f.root, outside.join("alias")).unwrap();
        let exports = BTreeMap::from([("alias".into(), outside.join("alias"))]);
        error(
            f.service
                .dispatch(
                    "peer-a",
                    "inc-a",
                    &exports,
                    Request::UploadBegin {
                        workspace: "alias".into(),
                        path: "linked-file".into(),
                        size_bytes: 0,
                        sha256: sha256(b""),
                        idempotency_key: "alias".into(),
                    },
                )
                .await,
            ErrorCode::PathDenied,
        );
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");
    }

    #[tokio::test]
    async fn validates_chunks_total_hash_size_and_no_replace_publication() {
        let f = Fixture::new();
        let id = f.begin("input", b"hello", "chunk-errors").await;
        error(
            f.call(Request::UploadWrite {
                transfer_id: id.clone(),
                offset: 0,
                chunk_sha256: sha256(b"wrong"),
                data: b"hello".to_vec(),
            })
            .await,
            ErrorCode::HashMismatch,
        );
        error(f.write(&id, 0, &[]).await, ErrorCode::InvalidArgument);
        error(f.write(&id, 0, b"too long").await, ErrorCode::InvalidOffset);
        error(
            f.call(Request::UploadCommit {
                transfer_id: id.clone(),
            })
            .await,
            ErrorCode::InvalidOffset,
        );
        assert!(matches!(
            f.write(&id, 0, b"bad!!").await,
            Response::Written { .. }
        ));
        error(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            ErrorCode::HashMismatch,
        );
        assert!(!f.root.join("input").exists());
        let id = f.begin("output", b"right", "no-replace").await;
        f.write(&id, 0, b"right").await;
        fs::write(f.root.join("output"), b"previous checkpoint").unwrap();
        error(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            ErrorCode::Conflict,
        );
        assert_eq!(
            fs::read(f.root.join("output")).unwrap(),
            b"previous checkpoint"
        );
        error(
            f.call(Request::UploadBegin {
                workspace: "training.v1".into(),
                path: "output".into(),
                size_bytes: 5,
                sha256: sha256(b"right"),
                idempotency_key: "already-exists".into(),
            })
            .await,
            ErrorCode::Conflict,
        );
    }

    #[tokio::test]
    async fn incoming_grants_never_read_existing_or_own_uploaded_files() {
        let f = Fixture::new();
        fs::write(f.root.join("private"), b"receiver's private bytes").unwrap();
        let id = f
            .begin("uploaded", b"sender's bytes", "incoming-only")
            .await;
        f.write(&id, 0, b"sender's bytes").await;
        let Response::File { file } = f.call(Request::UploadCommit { transfer_id: id }).await
        else {
            panic!("upload commit failed");
        };
        error(
            f.call(Request::ShareCapabilities).await,
            ErrorCode::PermissionDenied,
        );
        for path in ["private", "uploaded", "missing", "../outside/secret"] {
            for request in [
                Request::ShareList {
                    path: path.into(),
                    cursor: None,
                },
                Request::ShareStat { path: path.into() },
                Request::ShareReadRange {
                    path: path.into(),
                    expected_version: file.file_version.clone(),
                    offset: 0,
                    max_bytes: 1,
                },
                Request::ShareReadRangeEncoded {
                    path: path.into(),
                    expected_version: file.file_version.clone(),
                    offset: 0,
                    max_bytes: 1,
                },
            ] {
                error(f.call(request).await, ErrorCode::PermissionDenied);
            }
            error(
                f.call(Request::FileStat {
                    workspace: "training.v1".into(),
                    path: path.into(),
                })
                .await,
                ErrorCode::PermissionDenied,
            );
            // Even a known version from the sender's own commit is no read grant.
            error(
                f.call(Request::FileReadRange {
                    workspace: "training.v1".into(),
                    path: path.into(),
                    expected_version: file.file_version.clone(),
                    offset: 0,
                    max_bytes: MAX_CHUNK as u64,
                })
                .await,
                ErrorCode::PermissionDenied,
            );
        }
        assert_eq!(
            fs::read(f.root.join("private")).unwrap(),
            b"receiver's private bytes"
        );
        assert_eq!(
            fs::read(f.root.join("uploaded")).unwrap(),
            b"sender's bytes"
        );
    }

    #[tokio::test]
    async fn abort_expiry_quota_and_cleanup_preserve_completed_outputs() {
        let mut f = Fixture::new();
        f.service = FileService::with_limits(
            f.state.clone(),
            Limits {
                max_upload_bytes: 8,
                max_pending_bytes: 8,
                max_transfers: 4,
                retention_secs: 3600,
            },
        )
        .unwrap();
        let id = f.begin("abandoned", b"12345678", "abandoned").await;
        f.write(&id, 0, b"1234").await;
        error(
            f.call(Request::UploadBegin {
                workspace: "training.v1".into(),
                path: "quota".into(),
                size_bytes: 1,
                sha256: sha256(b"a"),
                idempotency_key: "quota".into(),
            })
            .await,
            ErrorCode::ResourceLimit,
        );
        assert_eq!(
            f.call(Request::UploadAbort {
                transfer_id: id.clone()
            })
            .await,
            Response::Aborted
        );
        assert_eq!(
            f.call(Request::UploadAbort {
                transfer_id: id.clone()
            })
            .await,
            Response::Aborted
        );
        assert!(!f.root.join(format!(".wispnet-upload-{id}")).exists());
        let id = f.begin("expired", b"1234", "expired").await;
        f.write(&id, 0, b"12").await;
        let mut manifest = f.service.load(&id).unwrap();
        manifest.expires_at = now().saturating_sub(1);
        f.service.save(&manifest).unwrap();
        assert!(matches!(
            f.call(Request::UploadStatus {
                transfer_id: id.clone()
            })
            .await,
            Response::Upload {
                state: TransferState::Expired,
                ..
            }
        ));
        assert!(!f.root.join(format!(".wispnet-upload-{id}")).exists());
        fs::write(f.root.join("completed"), b"safe checkpoint").unwrap();
        f.service.cleanup_expired().await.unwrap();
        error(
            f.call(Request::UploadStatus { transfer_id: id }).await,
            ErrorCode::NotFound,
        );
        assert_eq!(
            fs::read(f.root.join("completed")).unwrap(),
            b"safe checkpoint"
        );
        assert_eq!(
            Failure::from(io::Error::from_raw_os_error(libc::ENOSPC)).code,
            ErrorCode::NoSpace
        );
    }

    #[tokio::test]
    async fn recovers_both_publish_crash_windows_and_initialization() {
        let f = Fixture::new();
        for (key, unlink) in [("link-made", false), ("link-removed", true)] {
            let id = f.begin(key, b"checkpoint", key).await;
            f.write(&id, 0, b"checkpoint").await;
            let manifest = f.service.load(&id).unwrap();
            let (directory, name, partial) = f.service.partial_parent(&manifest).unwrap();
            link_at(&directory, &partial, &name).unwrap();
            directory.sync_all().unwrap();
            if unlink {
                unlink_at(&directory, &partial).unwrap();
                directory.sync_all().unwrap();
            }
            let restarted = FileService::new(f.state.clone()).unwrap();
            let response = restarted
                .dispatch(
                    "peer-a",
                    "inc-a",
                    &f.exports,
                    Request::UploadCommit {
                        transfer_id: id.clone(),
                    },
                )
                .await;
            let Response::File { file } = response else {
                panic!("{response:?}");
            };
            assert!(!f.root.join(partial).exists());
            assert_eq!(file.path, key);
            assert_eq!(file.sha256, sha256(b"checkpoint"));
            assert_eq!(fs::read(f.root.join(key)).unwrap(), b"checkpoint");
            error(
                f.call(Request::UploadAbort { transfer_id: id }).await,
                ErrorCode::AlreadyCommitted,
            );
        }
        let id = f.begin("initializing", b"first", "initializing").await;
        fs::remove_file(f.root.join(format!(".wispnet-upload-{id}"))).unwrap();
        assert_eq!(f.begin("initializing", b"first", "initializing").await, id);
        assert!(matches!(
            f.write(&id, 0, b"first").await,
            Response::Written { .. }
        ));
    }

    #[tokio::test]
    async fn restart_discards_unacknowledged_tail_and_cross_instance_writes_are_serialized() {
        let f = Fixture::new();
        let id = f.begin("resumed", b"12345678", "resumed").await;
        f.write(&id, 0, b"1234").await;
        let manifest = f.service.load(&id).unwrap();
        let (directory, _, partial) = f.service.partial_parent(&manifest).unwrap();
        let mut file = open_file_at(&directory, &partial, libc::O_RDWR, 0).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"unacknowledged").unwrap();
        file.sync_all().unwrap();
        let restarted = FileService::new(f.state.clone()).unwrap();
        let request = Request::UploadWrite {
            transfer_id: id.clone(),
            offset: 4,
            chunk_sha256: sha256(b"5678"),
            data: b"5678".to_vec(),
        };
        let (left, right) = tokio::join!(
            f.service
                .dispatch("peer-a", "inc-a", &f.exports, request.clone()),
            restarted.dispatch("peer-a", "inc-a", &f.exports, request)
        );
        assert_eq!(
            left,
            Response::Written {
                committed_offset: 8
            }
        );
        assert_eq!(right, left);
        assert!(matches!(
            f.call(Request::UploadCommit { transfer_id: id }).await,
            Response::File { .. }
        ));
        assert_eq!(fs::read(f.root.join("resumed")).unwrap(), b"12345678");
    }

    #[tokio::test]
    async fn expired_missing_or_replaced_parent_cannot_block_unrelated_uploads() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let id = f.begin("deleted/input", b"1234", "deleted-parent").await;
        f.write(&id, 0, b"12").await;
        let mut manifest = f.service.load(&id).unwrap();
        manifest.expires_at = now().saturating_sub(1);
        f.service.save(&manifest).unwrap();
        fs::remove_dir_all(f.root.join("deleted")).unwrap();
        f.service.cleanup_expired().await.unwrap();
        assert!(matches!(
            f.service.load(&id),
            Err(Failure {
                code: ErrorCode::NotFound,
                ..
            })
        ));
        f.begin("new-after-deletion", b"safe", "new-after-deletion")
            .await;

        let id = f.begin("replaced/input", b"1234", "replaced-parent").await;
        f.write(&id, 0, b"12").await;
        let mut manifest = f.service.load(&id).unwrap();
        manifest.expires_at = now().saturating_sub(1);
        f.service.save(&manifest).unwrap();
        let moved = f.root.parent().unwrap().join("moved-job");
        let outside = f.root.parent().unwrap().join("outside-job");
        fs::rename(f.root.join("replaced"), &moved).unwrap();
        fs::create_dir(&outside).unwrap();
        let partial_name = format!(".wispnet-upload-{id}");
        fs::write(outside.join(&partial_name), b"unrelated checkpoint").unwrap();
        symlink(&outside, f.root.join("replaced")).unwrap();
        f.service.cleanup_expired().await.unwrap();
        assert!(
            f.service.load(&id).is_ok(),
            "unavailable cleanup must retain its ownership metadata"
        );
        assert_eq!(
            fs::read(outside.join(&partial_name)).unwrap(),
            b"unrelated checkpoint"
        );
        assert!(moved.join(&partial_name).exists());
        f.begin("new-after-symlink", b"safe", "new-after-symlink")
            .await;
        error(f.write(&id, 2, b"34").await, ErrorCode::TransferExpired);
        fs::remove_file(f.root.join("replaced")).unwrap();
        fs::rename(&moved, f.root.join("replaced")).unwrap();
        f.service.cleanup_expired().await.unwrap();
        assert!(matches!(
            f.service.load(&id),
            Err(Failure {
                code: ErrorCode::NotFound,
                ..
            })
        ));
        assert!(!f.root.join("replaced").join(&partial_name).exists());
        assert_eq!(
            fs::read(outside.join(&partial_name)).unwrap(),
            b"unrelated checkpoint"
        );
    }

    #[tokio::test]
    async fn native_serve_groups_durable_offsets_without_reordering_control_requests() {
        use crate::{read_response, write_request};
        let f = Fixture::new();
        let content = vec![59; 4 * 4096];
        let id = f.begin("batched", &content, "batched").await;
        let mut requests = Vec::new();
        for n in 0..4 {
            write_request(
                &mut requests,
                &Request::UploadWrite {
                    transfer_id: id.clone(),
                    offset: n * 4096,
                    chunk_sha256: sha256(&content[..4096]),
                    data: content[..4096].to_vec(),
                },
            )
            .await
            .unwrap();
        }
        write_request(
            &mut requests,
            &Request::UploadStatus {
                transfer_id: id.clone(),
            },
        )
        .await
        .unwrap();
        write_request(
            &mut requests,
            &Request::UploadCommit {
                transfer_id: id.clone(),
            },
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            f.service.serve(
                "peer-a",
                "inc-a",
                f.exports.clone(),
                requests.as_slice(),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let mut output = output.as_slice();
        for _ in 0..4 {
            assert_eq!(
                read_response(&mut output).await.unwrap(),
                Some(Response::Written {
                    committed_offset: content.len() as u64
                })
            );
        }
        assert!(matches!(
            read_response(&mut output).await.unwrap(),
            Some(Response::Upload {
                state: TransferState::Uploading,
                committed_offset: 16384,
                ..
            })
        ));
        assert!(matches!(
            read_response(&mut output).await.unwrap(),
            Some(Response::File { .. })
        ));
        assert!(read_response(&mut output).await.unwrap().is_none());
        let restarted = FileService::new(f.state.clone()).unwrap();
        assert!(matches!(
            restarted
                .dispatch(
                    "peer-a",
                    "inc-a",
                    &f.exports,
                    Request::UploadStatus { transfer_id: id }
                )
                .await,
            Response::Upload {
                state: TransferState::Committed,
                committed_offset: 16384,
                ..
            }
        ));
        assert_eq!(fs::read(f.root.join("batched")).unwrap(), content);
    }

    #[tokio::test]
    async fn grouping_timeout_never_discards_a_partially_received_frame() {
        use crate::{read_response, write_request};
        use tokio::io::AsyncWriteExt;
        let f = Fixture::new();
        let content = vec![71; 8192];
        let id = f.begin("fragmented", &content, "fragmented").await;
        let mut first = Vec::new();
        let mut second = Vec::new();
        write_request(
            &mut first,
            &Request::UploadWrite {
                transfer_id: id.clone(),
                offset: 0,
                chunk_sha256: sha256(&content[..4096]),
                data: content[..4096].to_vec(),
            },
        )
        .await
        .unwrap();
        write_request(
            &mut second,
            &Request::UploadWrite {
                transfer_id: id,
                offset: 4096,
                chunk_sha256: sha256(&content[..4096]),
                data: content[..4096].to_vec(),
            },
        )
        .await
        .unwrap();
        let (client, server) = tokio::io::duplex(32 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server);
        let (mut reader, mut writer) = tokio::io::split(client);
        let sending = async {
            writer.write_all(&first).await.unwrap();
            writer.write_all(&second[..3]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(15)).await;
            writer.write_all(&second[3..]).await.unwrap();
            writer.shutdown().await.unwrap();
        };
        let receiving = async {
            for expected in [4096, 8192] {
                assert_eq!(
                    read_response(&mut reader).await.unwrap(),
                    Some(Response::Written {
                        committed_offset: expected
                    })
                );
            }
        };
        let serving = f.service.serve(
            "peer-a",
            "inc-a",
            f.exports.clone(),
            server_reader,
            server_writer,
        );
        let (_, _, result) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(sending, receiving, serving)
        })
        .await
        .unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn pending_response_extends_idle_deadline_without_restarting_frame_parser() {
        use crate::write_request;
        use tokio::io::AsyncWriteExt;
        let request = Request::UploadStatus {
            transfer_id: "a".repeat(64),
        };
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).await.unwrap();
        let (reader, mut writer) = tokio::io::duplex(1024);
        let (sender, mut receiver) = mpsc::channel(8);
        let outstanding = Arc::new(AtomicUsize::new(1));
        let queuing = queue_requests(
            reader,
            sender,
            outstanding.clone(),
            Duration::from_millis(5),
        );
        let sending = async {
            writer.write_all(&bytes[..3]).await.unwrap();
            // Simulate waiting for a long-running earlier operation while the
            // next request header has already been partially received.
            tokio::time::sleep(Duration::from_millis(35)).await;
            writer.write_all(&bytes[3..]).await.unwrap();
            writer.shutdown().await.unwrap();
        };
        let receiving = async {
            assert_eq!(receiver.recv().await, Some(request));
        };
        let (result, _, _) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(queuing, sending, receiving)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(outstanding.load(Ordering::Acquire), 2);

        let (reader, _writer) = tokio::io::duplex(1024);
        let (sender, _receiver) = mpsc::channel(8);
        let idle = queue_requests(
            reader,
            sender,
            Arc::new(AtomicUsize::new(0)),
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();
        assert_eq!(idle.kind(), io::ErrorKind::TimedOut);
    }
}

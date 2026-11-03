//! Read-only shares use independent service grants; inbox access is never read access.
use super::*;
use wispnet_files::ShareEntry;
#[path = "folders.rs"]
mod folders;
pub use folders::FolderInfo;

#[derive(Clone, Copy)]
enum DownloadExpectation<'a> {
    Any,
    Version(&'a str),
    Pinned(&'a FileInfo),
}

struct ShareSession {
    stream: BoxStream,
    _resources: Resources,
    encoded: bool,
    // One download attempt can span folder children and reconnect its stream.
    // Only newly acknowledged decoded payload contributes to this counter.
    downloaded_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharePage {
    pub entries: Vec<ShareEntry>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DownloadJournal {
    binding: Binding,
    file: FileInfo,
    offset: u64,
}

impl Client {
    /// List one page inside an explicitly granted read-only share.
    pub async fn list_shared_files(
        self: &Arc<Self>,
        target: FileTarget,
        path: &str,
        cursor: Option<String>,
    ) -> Result<SharePage> {
        valid_share_path(path, true)?;
        if cursor.as_ref().is_some_and(|c| c.len() > 4096) {
            return Err(protocol_error());
        }
        let (mut stream, _resources) = self.share_stream(&target).await?;
        Self::list_shared_files_on(&mut stream, path, cursor).await
    }

    async fn list_shared_files_on(
        stream: &mut BoxStream,
        path: &str,
        cursor: Option<String>,
    ) -> Result<SharePage> {
        tokio::time::timeout(Duration::from_secs(30), async {
            write_request(
                &mut *stream,
                &Request::ShareList {
                    path: path.into(),
                    cursor: cursor.clone(),
                },
            )
            .await
            .map_err(TransferError::io)?;
            match response(&mut *stream).await? {
                Response::Entries {
                    entries,
                    next_cursor,
                } => {
                    if entries.len() > 256 || next_cursor.as_ref().is_some_and(|c| c.len() > 4096) {
                        return Err(protocol_error());
                    }
                    let mut previous = cursor.as_deref();
                    for entry in &entries {
                        if previous.is_some_and(|previous| entry.name.as_str() <= previous) {
                            return Err(protocol_error());
                        }
                        previous = Some(&entry.name);
                    }
                    if let Some(next) = &next_cursor
                        && (entries.last().is_none_or(|last| &last.name != next)
                            || cursor.as_ref().is_some_and(|cursor| next <= cursor))
                    {
                        return Err(protocol_error());
                    }
                    let mut names = BTreeSet::new();
                    for entry in &entries {
                        valid_share_path(&entry.path, false)?;
                        let expected = if path.is_empty() {
                            entry.name.clone()
                        } else {
                            format!("{path}/{}", entry.name)
                        };
                        if entry.name.is_empty()
                            || entry.name.contains('/')
                            || entry.name == "."
                            || entry.name == ".."
                            || entry.path != expected
                            || !names.insert(&entry.name)
                        {
                            return Err(protocol_error());
                        }
                    }
                    Ok(SharePage {
                        entries,
                        next_cursor,
                    })
                }
                _ => Err(protocol_error()),
            }
        })
        .await
        .map_err(|_| TransferError::new("UNAVAILABLE", "Share listing timed out"))?
    }

    /// Download a selected shared file. Durable resume is bound to its source
    /// version and destination; a changed source must be explicitly restarted.
    pub async fn get_file(
        self: &Arc<Self>,
        target: FileTarget,
        remote_path: &str,
        local_path: &Path,
        options: TransferOptions,
    ) -> Result<FileInfo> {
        valid_share_path(remote_path, false)?;
        let mut guard = CancelOnDrop {
            cancellation: options.cancellation.clone(),
            armed: true,
        };
        let result = tokio::select! {
            biased;
            _ = options.cancellation.cancelled() => Err(TransferError::new("CANCELLED", "Download paused; resumable data retained")),
            result = self.get_file_inner(target, remote_path, local_path, &options, None) => result,
        };
        guard.armed = false;
        result
    }

    pub(crate) async fn shares_command(
        self: &Arc<Self>,
        value: &Value,
    ) -> std::result::Result<Value, String> {
        let target = FileTarget {
            host: crate::peer(value)?,
            service: crate::argument(value, "service")?.into(),
            incarnation: crate::argument(value, "incarnation")?.into(),
        };
        valid_share_target(&target).map_err(|e| e.to_string())?;
        let path = crate::argument(value, "path")?.to_owned();
        let recursive = value
            .get("recursive")
            .map(|v| v.as_bool().ok_or("recursive must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        valid_share_path(&path, value["op"] == "file_list" || recursive)
            .map_err(|e| e.to_string())?;
        let inspecting = value["op"] == "file_stat";
        let expected_version = value
            .get("expected_version")
            .map(|v| {
                v.as_str()
                    .filter(|v| !v.is_empty() && v.len() <= 4096)
                    .map(str::to_owned)
                    .ok_or("expected_version must be a nonempty version string")
            })
            .transpose()?;
        if value["op"] == "file_list" {
            let cursor = value
                .get("cursor")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("cursor must be a string")
                })
                .transpose()?;
            return self
                .list_shared_files(target, &path, cursor)
                .await
                .and_then(|p| serde_json::to_value(p).map_err(|_| protocol_error()))
                .map_err(|e| e.to_string());
        }
        let local_path = if inspecting {
            PathBuf::new()
        } else {
            PathBuf::from(crate::argument(value, "local_path")?)
        };
        let resume = value
            .get("resume")
            .map(|v| v.as_bool().ok_or("resume must be a boolean"))
            .transpose()?
            .unwrap_or(true);
        let permit = self
            .files
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "RESOURCE_LIMIT: two file transfers are already active")?;
        let cancellation = FileCancellation::default();
        let id = crate::token();
        let job = Arc::new(Job {
            host: target.host,
            cancellation: cancellation.clone(),
            status: Mutex::new(
                json!({"transfer":id,"direction":if inspecting { "inspection" } else { "download" },"recursive":recursive,"state":"running","phase":"preparing","verified_bytes":0,"size_bytes":0}),
            ),
            finished: Mutex::new(None),
        });
        if !inspecting {
            job.status.lock().unwrap_or_else(|e| e.into_inner())["downloaded_bytes"] = json!(0);
        }
        {
            let mut jobs = self.files.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.retain(|_, job| {
                job.finished
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none_or(|at| at.elapsed() < Duration::from_secs(3600))
            });
            if jobs.len() >= 64 {
                let oldest = jobs
                    .iter()
                    .filter_map(|(id, job)| {
                        job.finished
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .map(|at| (id.clone(), at))
                    })
                    .min_by_key(|(_, at)| *at)
                    .map(|(id, _)| id);
                if let Some(id) = oldest {
                    jobs.remove(&id);
                } else {
                    return Err("RESOURCE_LIMIT: file transfer history is full".into());
                }
            }
            jobs.insert(id, job.clone());
        }
        let progress_job = job.clone();
        let options = TransferOptions {
            resume,
            cancellation,
            progress: Some(Arc::new(move |p| {
                let mut s = progress_job
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                s["phase"] = json!(p.phase);
                s["verified_bytes"] = json!(p.verified_bytes);
                s["size_bytes"] = json!(p.size_bytes);
                if let Some(n) = p.downloaded_bytes {
                    s["downloaded_bytes"] = json!(n);
                }
                if let Some(n) = p.files_complete {
                    s["files_complete"] = json!(n);
                }
                if let Some(n) = p.files_total {
                    s["files_total"] = json!(n);
                }
            })),
        };
        let client = self.clone();
        let operation_job = job.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let _permit = permit;
            let result = if inspecting {
                let mut guard = CancelOnDrop {
                    cancellation: options.cancellation.clone(),
                    armed: true,
                };
                let result = tokio::select! {
                    biased;
                    _ = options.cancellation.cancelled() => Err(TransferError::new("CANCELLED", "File inspection cancelled")),
                    result = async {
                        let (mut stream, _resources) = client.share_stream(&target).await?;
                        Self::shared_file_info(&mut stream, &path).await
                    } => result,
                };
                guard.armed = false;
                result.and_then(|file| serde_json::to_value(file).map_err(|_| protocol_error()))
            } else if recursive {
                client
                    .get_folder(target, &path, &local_path, options)
                    .await
                    .and_then(|folder| serde_json::to_value(folder).map_err(|_| protocol_error()))
            } else {
                let mut guard = CancelOnDrop {
                    cancellation: options.cancellation.clone(),
                    armed: true,
                };
                let result = tokio::select! {
                    biased;
                    _ = options.cancellation.cancelled() => Err(TransferError::new("CANCELLED", "Download paused; resumable data retained")),
                    result = client.get_file_inner(target, &path, &local_path, &options, expected_version.as_deref()) => result,
                };
                guard.armed = false;
                result.and_then(|file| serde_json::to_value(file).map_err(|_| protocol_error()))
            };
            let mut status = operation_job
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status["duration_ms"] = json!(started.elapsed().as_millis() as u64);
            match result {
                Ok(file) => {
                    status["state"] = json!("complete");
                    status["phase"] = json!("complete");
                    status["verified_bytes"] = file["size_bytes"].clone();
                    status["size_bytes"] = file["size_bytes"].clone();
                    if recursive {
                        status["files_complete"] = file["files_total"].clone();
                        status["files_total"] = file["files_total"].clone();
                        status["folder"] = file;
                    } else {
                        status["file"] = file;
                    }
                    if !inspecting {
                        status["local_path"] = json!(local_path);
                    }
                }
                Err(e) => {
                    status["state"] = json!(if e.code == "CANCELLED" {
                        "cancelled"
                    } else {
                        "failed"
                    });
                    status["error"] = json!(e);
                }
            }
            *operation_job
                .finished
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        });
        let status = job.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(status)
    }

    async fn share_session(self: &Arc<Self>, target: &FileTarget) -> Result<ShareSession> {
        let (mut stream, resources) = self.share_stream(target).await?;
        let negotiated = tokio::time::timeout(Duration::from_secs(3), async {
            write_request(&mut stream, &Request::ShareCapabilities)
                .await
                .map_err(TransferError::io)?;
            match response(&mut stream).await? {
                Response::ShareCapabilities { encodings } if encodings.len() <= 16 => {
                    Ok(encodings.iter().any(|e| e == "zstd"))
                }
                _ => Err(protocol_error()),
            }
        })
        .await;
        if let Ok(Ok(encoded)) = negotiated {
            return Ok(ShareSession {
                stream,
                _resources: resources,
                encoded,
                downloaded_bytes: 0,
            });
        }
        // Legacy peers may close the stream on the unknown capability request.
        // Reopen with the original range operation, preserving interoperability.
        drop(stream);
        drop(resources);
        let (stream, resources) = self.share_stream(target).await?;
        Ok(ShareSession {
            stream,
            _resources: resources,
            encoded: false,
            downloaded_bytes: 0,
        })
    }

    async fn shared_file_info(stream: &mut BoxStream, remote: &str) -> Result<FileInfo> {
        write_request(
            stream,
            &Request::ShareStat {
                path: remote.into(),
            },
        )
        .await
        .map_err(TransferError::io)?;
        let info = match timed_response(stream).await? {
            Response::File { file } => file,
            _ => return Err(protocol_error()),
        };
        if info.path != remote
            || info.size_bytes > 1u64 << 40
            || !wispnet_core::enrollment::valid_code(&info.sha256)
            || info.file_version.is_empty()
            || info.file_version.len() > 4096
        {
            return Err(protocol_error());
        }
        Ok(info)
    }

    async fn share_stream(self: &Arc<Self>, target: &FileTarget) -> Result<(BoxStream, Resources)> {
        valid_share_target(target)?;
        let mut resources = Resources::default();
        let stream = tokio::time::timeout(
            Duration::from_secs(15),
            self.open_share_stream(target, &mut resources),
        )
        .await
        .map_err(|_| TransferError::new("UNAVAILABLE", "Share connection timed out"))??;
        Ok((stream, resources))
    }

    async fn open_share_stream(
        self: &Arc<Self>,
        target: &FileTarget,
        resources: &mut Resources,
    ) -> Result<BoxStream> {
        if target.host == self.endpoint.id() {
            let authority = self
                .local_authority
                .as_ref()
                .ok_or_else(|| TransferError::new("UNAVAILABLE", "Local host unavailable"))?;
            let snapshot = authority
                .snapshot()
                .await
                .map_err(|_| TransferError::new("UNAVAILABLE", "Share unavailable"))?;
            let service = snapshot
                .services
                .iter()
                .find(|service| {
                    service.id == target.service
                        && service.kind == wispnet_core::host::ServiceKind::FileShare
                        && service.enabled
                })
                .ok_or_else(|| {
                    TransferError::new("PERMISSION_DENIED", "Read-only share unavailable")
                })?;
            if service.incarnation != target.incarnation {
                return Err(TransferError::new(
                    "SERVICE_CHANGED",
                    "Share changed; refresh its catalog",
                ));
            }
            let root = snapshot
                .file_shares
                .get(&target.service)
                .cloned()
                .ok_or_else(protocol_error)?;
            if self.files.local.get().is_none() {
                let manager = FileService::new(authority.state_dir().join("files"))
                    .map_err(TransferError::io)?;
                let _ = self.files.local.set(manager);
            }
            let manager = self.files.local.get().unwrap().clone();
            let authority = authority.clone();
            let share = target.service.clone();
            let incarnation = target.incarnation.clone();
            let own = self.endpoint.id().to_string();
            let (stream, server) = tokio::io::duplex(MAX_CHUNK * 2);
            resources.local = Some(tokio::spawn(async move {
                let (reader, writer) = tokio::io::split(server);
                let guard = async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let still_enabled = authority.snapshot().await.is_ok_and(|snapshot| {
                            snapshot.services.iter().any(|service| {
                                service.id == share
                                    && service.kind == wispnet_core::host::ServiceKind::FileShare
                                    && service.incarnation == incarnation
                                    && service.enabled
                            })
                        });
                        if !still_enabled {
                            break;
                        }
                    }
                };
                tokio::select! {
                    _ = guard => {},
                    _ = manager.serve_share(&own, &incarnation, root, reader, writer) => {},
                }
            }));
            return Ok(Box::new(stream));
        }
        if self
            .saved_host(target.host)
            .await
            .map_err(|_| TransferError::new("UNAVAILABLE", "Cannot inspect pairing"))?
            .is_none()
        {
            return Err(TransferError::new(
                "PERMISSION_DENIED",
                "Pair with this host first",
            ));
        }
        let catalog = self
            .catalog(target.host)
            .await
            .map_err(|_| TransferError::new("UNAVAILABLE", "Cannot read host catalog"))?;
        let service = catalog["services"]
            .as_array()
            .and_then(|services| {
                services.iter().find(|service| {
                    service["id"] == target.service && service["kind"] == "file_share"
                })
            })
            .ok_or_else(|| {
                TransferError::new(
                    "PERMISSION_DENIED",
                    "Read-only share unavailable or not granted",
                )
            })?;
        if service["incarnation"] != target.incarnation {
            return Err(TransferError::new(
                "SERVICE_CHANGED",
                "Share changed; refresh its catalog",
            ));
        }
        let host_target = self
            .target(target.host)
            .await
            .map_err(|_| TransferError::new("UNAVAILABLE", "Host unavailable"))?;
        let connection = wispnet_agent::tunnel::connect_target(&self.endpoint, host_target)
            .await
            .map_err(|_| TransferError::new("UNAVAILABLE", "Cannot connect to share"))?;
        resources.connection = Some(connection.clone());
        let stream = wispnet_agent::tunnel::open_service_bound(
            &connection,
            &target.service,
            &target.incarnation,
        )
        .await
        .map_err(|error| match error {
            wispnet_agent::tunnel::TunnelError::Status(_) => {
                TransferError::new("PERMISSION_DENIED", "Share access denied")
            }
            _ => TransferError::new("UNAVAILABLE", "Share connection interrupted"),
        })?;
        if self
            .saved_host(target.host)
            .await
            .map_err(|_| TransferError::new("UNAVAILABLE", "Cannot inspect pairing"))?
            .is_none()
        {
            return Err(TransferError::new(
                "PERMISSION_DENIED",
                "Pairing was removed",
            ));
        }
        Ok(Box::new(stream))
    }

    async fn get_file_inner(
        self: &Arc<Self>,
        target: FileTarget,
        remote: &str,
        local: &Path,
        options: &TransferOptions,
        expected_version: Option<&str>,
    ) -> Result<FileInfo> {
        let mut session = self.share_session(&target).await?;
        self.get_file_on_session(
            target,
            remote,
            local,
            options,
            &mut session,
            expected_version
                .map(DownloadExpectation::Version)
                .unwrap_or(DownloadExpectation::Any),
        )
        .await
    }

    async fn get_file_on_session(
        self: &Arc<Self>,
        target: FileTarget,
        remote: &str,
        local: &Path,
        options: &TransferOptions,
        session: &mut ShareSession,
        expected: DownloadExpectation<'_>,
    ) -> Result<FileInfo> {
        let local = absolute_file_path(local)?;
        let binding = Binding {
            version: 1,
            own: self.endpoint.id().to_string(),
            target: target.clone(),
            workspace: String::new(),
            remote_path: remote.into(),
            local_path: local.clone(),
            direction: "download".into(),
        };
        let resume = blocking({
            let state = self.state.join("file-downloads");
            let binding = binding.clone();
            move || resume_files(&state, &binding)
        })
        .await?;
        let key =
            wispnet_files::sha256(&serde_json::to_vec(&binding).map_err(|_| protocol_error())?);
        let partial = local
            .parent()
            .ok_or_else(protocol_error)?
            .join(format!(".wispnet-download-{key}.part"));
        let info = if let DownloadExpectation::Pinned(file) = expected {
            file.clone()
        } else {
            Self::shared_file_info(&mut session.stream, remote).await?
        };
        if matches!(expected, DownloadExpectation::Version(version) if version != info.file_version)
        {
            return Err(TransferError::new(
                "STALE_VERSION",
                "Shared file changed; restart the download",
            ));
        }
        let existing = blocking({
            let local = local.clone();
            let info = info.clone();
            let cancellation = options.cancellation.clone();
            let allow_existing = options.resume;
            move || {
                if !local.try_exists().map_err(TransferError::io)? {
                    return Ok(false);
                }
                let mut f = safe_open(&local, false, false)?;
                if !allow_existing {
                    return Ok(false);
                }
                Ok(
                    f.metadata().map_err(TransferError::io)?.len() == info.size_bytes
                        && hash_reader_cancelled(&mut f, Some(&cancellation))? == info.sha256,
                )
            }
        })
        .await?;
        if existing && options.resume {
            options.report_download(
                "complete",
                info.size_bytes,
                info.size_bytes,
                session.downloaded_bytes,
            );
            return Ok(info);
        }
        if std::fs::symlink_metadata(&local).is_ok() {
            return Err(TransferError::new(
                "CONFLICT",
                "Download destination already exists",
            ));
        }
        let saved = if options.resume {
            blocking({
                let journal = resume.journal.clone();
                let lock = resume._lock.clone();
                move || {
                    let _lock = lock;
                    load_download_journal(&journal)
                }
            })
            .await?
        } else {
            None
        };
        let mut journal = DownloadJournal {
            binding: binding.clone(),
            file: info.clone(),
            offset: 0,
        };
        if options.resume
            && std::fs::symlink_metadata(&partial).is_ok()
            && let Some(saved) = saved
        {
            if saved.binding != binding
                || saved.file.file_version != info.file_version
                || saved.file.sha256 != info.sha256
                || saved.file.size_bytes != info.size_bytes
            {
                return Err(TransferError::new(
                    "STALE_VERSION",
                    "Shared file changed; restart with resume disabled",
                ));
            }
            journal = saved;
        }
        let output = Arc::new(
            blocking({
                let partial = partial.clone();
                let offset = journal.offset;
                let lock = resume._lock.clone();
                move || {
                    let _lock = lock;
                    let file = safe_open(&partial, true, true)?;
                    let meta = file.metadata().map_err(TransferError::io)?;
                    if meta.nlink() != 1
                        || meta.uid() != unsafe { libc::geteuid() }
                        || meta.permissions().mode() & 0o077 != 0
                    {
                        return Err(TransferError::new(
                            "PATH_DENIED",
                            "Download partial must be private to its owner",
                        ));
                    }
                    if offset > meta.len() {
                        return Err(TransferError::new(
                            "UNAVAILABLE",
                            "Saved download prefix is incomplete",
                        ));
                    }
                    file.set_len(offset).map_err(TransferError::io)?;
                    Ok(file)
                }
            })
            .await?,
        );
        if journal.offset > info.size_bytes {
            return Err(protocol_error());
        }
        save_download_journal(&resume.journal, &journal, &resume._lock).await?;
        options.report_download(
            "transferring",
            journal.offset,
            info.size_bytes,
            session.downloaded_bytes,
        );
        let mut retries = 0;
        'transfer: while journal.offset < info.size_bytes {
            // Queue a bounded window before reading payloads so WAN RTT does
            // not impose one round trip per chunk. Only durable bytes advance
            // the journal, including when a later response is interrupted.
            let mut requested = journal.offset;
            let mut pending = 0;
            for _ in 0..PIPELINE {
                if requested >= info.size_bytes {
                    break;
                }
                let count = (info.size_bytes - requested).min(MAX_CHUNK as u64);
                let request = if session.encoded {
                    Request::ShareReadRangeEncoded {
                        path: remote.into(),
                        expected_version: info.file_version.clone(),
                        offset: requested,
                        max_bytes: count,
                    }
                } else {
                    Request::ShareReadRange {
                        path: remote.into(),
                        expected_version: info.file_version.clone(),
                        offset: requested,
                        max_bytes: count,
                    }
                };
                if let Err(error) = write_request(&mut session.stream, &request).await {
                    let error = TransferError::io(error);
                    if retryable(&error) && retries < MAX_RETRIES {
                        retries += 1;
                        let downloaded_bytes = session.downloaded_bytes;
                        *session = self.share_session(&target).await?;
                        session.downloaded_bytes = downloaded_bytes;
                        continue 'transfer;
                    }
                    return Err(error);
                }
                requested += count;
                pending += 1;
            }
            for _ in 0..pending {
                let expected = (info.size_bytes - journal.offset).min(MAX_CHUNK as u64);
                let response = match timed_response(&mut session.stream).await {
                    Ok(response @ Response::EncodedData { .. }) => {
                        blocking(move || {
                            wispnet_files::decode_data_response(response)
                                .map_err(|_| protocol_error())
                        })
                        .await
                    }
                    Ok(response) => Ok(response),
                    Err(error) => Err(error),
                };
                let (offset, data) = match response {
                    Ok(Response::Data { offset, data })
                        if offset == journal.offset && data.len() as u64 == expected =>
                    {
                        (offset, data)
                    }
                    Ok(_) => return Err(protocol_error()),
                    Err(error) if retryable(&error) && retries < MAX_RETRIES => {
                        retries += 1;
                        let downloaded_bytes = session.downloaded_bytes;
                        *session = self.share_session(&target).await?;
                        session.downloaded_bytes = downloaded_bytes;
                        continue 'transfer;
                    }
                    Err(error) => return Err(error),
                };
                let count = data.len() as u64;
                blocking({
                    let output = output.clone();
                    let lock = resume._lock.clone();
                    move || {
                        let _lock = lock;
                        output
                            .write_all_at(&data, offset)
                            .map_err(TransferError::io)?;
                        output.sync_data().map_err(TransferError::io)
                    }
                })
                .await?;
                journal.offset += count;
                save_download_journal(&resume.journal, &journal, &resume._lock).await?;
                session.downloaded_bytes += count;
                options.report_download(
                    "transferring",
                    journal.offset,
                    info.size_bytes,
                    session.downloaded_bytes,
                );
                if options.cancellation.is_cancelled() {
                    return Err(TransferError::new(
                        "CANCELLED",
                        "Download paused; resumable data retained",
                    ));
                }
            }
        }
        options.report_download(
            "verifying",
            info.size_bytes,
            info.size_bytes,
            session.downloaded_bytes,
        );
        let verified_fingerprint = blocking({
            let output = output.clone();
            let expected = info.sha256.clone();
            let cancellation = options.cancellation.clone();
            let lock = resume._lock.clone();
            move || {
                let _lock = lock;
                let before = download_fingerprint(&output.metadata().map_err(TransferError::io)?);
                let mut reader = output.try_clone().map_err(TransferError::io)?;
                std::io::Seek::seek(&mut reader, SeekFrom::Start(0)).map_err(TransferError::io)?;
                if hash_reader_cancelled(&mut reader, Some(&cancellation))? != expected {
                    return Err(TransferError::new(
                        "HASH_MISMATCH",
                        "Downloaded file checksum does not match",
                    ));
                }
                if download_fingerprint(&output.metadata().map_err(TransferError::io)?) != before {
                    return Err(TransferError::new(
                        "STALE_VERSION",
                        "Download partial changed during verification",
                    ));
                }
                Ok(before)
            }
        })
        .await?;
        // Folder files remain in a private staging tree; the enclosing batch
        // rechecks every pinned version immediately before directory publication.
        // Standalone downloads must verify again before publishing this file.
        if !matches!(expected, DownloadExpectation::Pinned(_)) {
            write_request(
                &mut session.stream,
                &Request::ShareStat {
                    path: remote.into(),
                },
            )
            .await
            .map_err(TransferError::io)?;
            match timed_response(&mut session.stream).await? {
                Response::File { file } if file == info => {}
                Response::File { .. } => {
                    return Err(TransferError::new(
                        "STALE_VERSION",
                        "Shared file changed during download",
                    ));
                }
                _ => return Err(protocol_error()),
            }
        }
        blocking({
            let local = local.clone();
            let partial = partial.clone();
            let cancellation = options.cancellation.clone();
            let lock = resume._lock.clone();
            let journal_path = resume.journal.clone();
            let output = output.clone();
            move || {
                let _lock = lock;
                if cancellation.is_cancelled() {
                    return Err(TransferError::new(
                        "CANCELLED",
                        "Download paused; resumable data retained",
                    ));
                }
                publish_download(&output, verified_fingerprint, &partial, &local)?;
                std::fs::remove_file(&journal_path).map_err(TransferError::io)?;
                sync_parent(&journal_path)
            }
        })
        .await?;
        options.report_download(
            "complete",
            info.size_bytes,
            info.size_bytes,
            session.downloaded_bytes,
        );
        Ok(info)
    }
}

fn valid_share_path(path: &str, root: bool) -> Result<()> {
    if (path.is_empty() && root)
        || (!path.is_empty()
            && path.len() <= 4096
            && !path.contains('\0')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".."))
    {
        return Ok(());
    }
    Err(TransferError::new(
        "PATH_DENIED",
        "Shared paths must stay within the selected share",
    ))
}
async fn timed_response(stream: &mut (impl AsyncRead + Unpin)) -> Result<Response> {
    tokio::time::timeout(Duration::from_secs(300), response(stream))
        .await
        .map_err(|_| TransferError::new("UNAVAILABLE", "Share response timed out"))?
}
fn load_download_journal(path: &Path) -> Result<Option<DownloadJournal>> {
    let f = match safe_open(path, false, false) {
        Ok(f) => f,
        Err(e) if e.code == "NOT_FOUND" => return Ok(None),
        Err(e) => return Err(e),
    };
    if f.metadata().map_err(TransferError::io)?.len() > 65536 {
        return Err(protocol_error());
    }
    serde_json::from_reader(f)
        .map(Some)
        .map_err(|_| protocol_error())
}
async fn save_download_journal(
    path: &Path,
    journal: &DownloadJournal,
    lock: &Arc<File>,
) -> Result<()> {
    let path = path.to_owned();
    let journal = journal.clone();
    let lock = lock.clone();
    blocking(move || {
        let _lock = lock;
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or_else(protocol_error)?)
            .map_err(TransferError::io)?;
        serde_json::to_writer(temp.as_file_mut(), &journal).map_err(|_| protocol_error())?;
        temp.as_file().sync_all().map_err(TransferError::io)?;
        temp.persist(&path)
            .map_err(|e| TransferError::io(e.error))?;
        sync_parent(&path)
    })
    .await
}

/// Verify that publication still names the exact regular inode whose content
/// was hashed. Destination creation never replaces an existing entry.
type DownloadFingerprint = (u64, u64, u64, i64, i64, i64, i64);
fn download_fingerprint(meta: &std::fs::Metadata) -> DownloadFingerprint {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}
fn publish_download(
    verified: &File,
    expected: DownloadFingerprint,
    partial: &Path,
    destination: &Path,
) -> Result<()> {
    let pinned = verified.metadata().map_err(TransferError::io)?;
    let named = std::fs::symlink_metadata(partial).map_err(TransferError::io)?;
    if download_fingerprint(&pinned) != expected
        || download_fingerprint(&named) != expected
        || !named.is_file()
        || named.dev() != pinned.dev()
        || named.ino() != pinned.ino()
        || named.len() != pinned.len()
    {
        return Err(TransferError::new(
            "PATH_DENIED",
            "Download partial was replaced",
        ));
    }
    std::fs::hard_link(partial, destination).map_err(TransferError::io)?;
    let published = std::fs::symlink_metadata(destination).map_err(TransferError::io)?;
    if !published.is_file() || published.dev() != pinned.dev() || published.ino() != pinned.ino() {
        std::fs::remove_file(destination).map_err(TransferError::io)?;
        return Err(TransferError::new(
            "PATH_DENIED",
            "Download partial was replaced",
        ));
    }
    sync_parent(destination)?;
    std::fs::remove_file(partial).map_err(TransferError::io)?;
    sync_parent(destination)
}

fn valid_share_target(target: &FileTarget) -> Result<()> {
    if target.service == "files" {
        return Err(TransferError::new(
            "PERMISSION_DENIED",
            "Inbox access does not permit reading files",
        ));
    }
    if !wispnet_core::protocol::valid_service_id(&target.service)
        || !wispnet_core::enrollment::valid_code(&target.incarnation)
    {
        return Err(TransferError::new(
            "INVALID_ARGUMENT",
            "Select a read-only share and its current incarnation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct PlatformSource;
    impl wispnet_files::Publisher for PlatformSource {
        fn share_source(&self) -> Option<&dyn wispnet_files::ShareSource> {
            Some(self)
        }
        fn publish(
            &self,
            _publication: wispnet_files::Publication<'_>,
            _source: File,
            _cancel: Arc<std::sync::atomic::AtomicBool>,
        ) -> io::Result<()> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    impl wispnet_files::ShareSource for PlatformSource {
        fn request(
            &self,
            _root: &Path,
            request: &Request,
            cancel: Arc<std::sync::atomic::AtomicBool>,
        ) -> std::result::Result<Response, wispnet_files::ErrorCode> {
            use wispnet_files::{ErrorCode, ShareEntry};
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(ErrorCode::Unavailable);
            }
            let content = b"platform-provided bytes";
            match request {
                Request::ShareList { path, .. } if path.is_empty() => Ok(Response::Entries {
                    entries: vec![ShareEntry {
                        name: "source.bin".into(),
                        path: "source.bin".into(),
                        is_dir: false,
                        size_bytes: content.len() as u64,
                    }],
                    next_cursor: None,
                }),
                Request::ShareStat { path } if path == "source.bin" => Ok(Response::File {
                    file: FileInfo {
                        path: path.clone(),
                        size_bytes: content.len() as u64,
                        sha256: wispnet_files::sha256(content),
                        file_version: "platform-version".into(),
                    },
                }),
                Request::ShareReadRange {
                    path,
                    expected_version,
                    offset,
                    max_bytes,
                } if path == "source.bin" && expected_version == "platform-version" => {
                    let start = (*offset as usize).min(content.len());
                    let end = (start + *max_bytes as usize).min(content.len());
                    Ok(Response::Data {
                        offset: *offset,
                        data: content[start..end].to_vec(),
                    })
                }
                _ => Err(ErrorCode::PermissionDenied),
            }
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_platform_shares_use_the_provider_for_listing_and_downloads() {
        let state = tempfile::tempdir().unwrap();
        let marker = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = wispnet_core::identity::SecretKey::generate();
        let own = key.public();
        let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
        let client = Client::new_peer_with_publisher(
            key,
            state.path().to_owned(),
            config,
            Some(Arc::new(PlatformSource)),
        )
        .await
        .unwrap();
        client._host.as_ref().unwrap().set_files_enabled(true);
        let authority = client.local_authority.as_ref().unwrap();
        let snapshot = authority.snapshot().await.unwrap();
        let (mut store, owner) = wispnet_registry::host::owner_store(state.path(), own).unwrap();
        store
            .host_mutate(
                own,
                &owner,
                wispnet_registry::host::Mutation::FileShare {
                    expected_revision: snapshot.revision,
                    id: "source".into(),
                    root: marker.path().canonicalize().unwrap(),
                    allow: vec![],
                },
            )
            .unwrap();
        let service = authority
            .snapshot()
            .await
            .unwrap()
            .services
            .into_iter()
            .find(|s| s.id == "source")
            .unwrap();
        let target = FileTarget {
            host: own,
            service: service.id,
            incarnation: service.incarnation,
        };
        assert!(std::fs::read_dir(marker.path()).unwrap().next().is_none());
        assert_eq!(
            client
                .list_shared_files(target.clone(), "", None)
                .await
                .unwrap()
                .entries[0]
                .name,
            "source.bin"
        );
        let output = destination.path().join("chosen.bin");
        client
            .get_file(target, "source.bin", &output, TransferOptions::default())
            .await
            .unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"platform-provided bytes");
        let status = client
            .command(&json!({"op":"local_shares_status"}))
            .await
            .unwrap();
        assert_eq!(status["shares"][0]["id"], "source");
        assert!(client.command(&json!({"op":"local_share_remove","id":"files","expected_revision":status["revision"]})).await.unwrap_err().contains("Read-only share"));
        client.endpoint.close().await;
    }
    #[test]
    fn publication_refuses_a_replaced_or_changed_verified_partial() {
        let root = tempfile::tempdir().unwrap();
        let partial = root.path().join("part");
        let dest = root.path().join("dest");
        std::fs::write(&partial, b"verified").unwrap();
        let file = safe_open(&partial, false, false).unwrap();
        let fingerprint = download_fingerprint(&file.metadata().unwrap());
        std::fs::rename(&partial, root.path().join("old")).unwrap();
        std::fs::write(&partial, b"replaced").unwrap();
        assert_eq!(
            publish_download(&file, fingerprint, &partial, &dest)
                .unwrap_err()
                .code,
            "PATH_DENIED"
        );
        assert!(!dest.exists());
        std::fs::remove_file(&partial).unwrap();
        std::fs::rename(root.path().join("old"), &partial).unwrap();
        std::fs::write(&partial, b"changed after hash").unwrap();
        assert_eq!(
            publish_download(&file, fingerprint, &partial, &dest)
                .unwrap_err()
                .code,
            "PATH_DENIED"
        );
        assert!(!dest.exists());
    }
}

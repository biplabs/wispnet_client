//! Folder jobs pin a bounded source manifest and publish one verified directory.
//! The private staging tree and its file journals survive cancellation/restarts.
use super::*;
use std::os::unix::{ffi::OsStrExt, fs::DirBuilderExt};

const MAX_FOLDER_ENTRIES: usize = 10_000;
const MAX_FOLDER_DEPTH: usize = 128;
const MAX_FOLDER_MANIFEST: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderInfo {
    pub path: String,
    pub size_bytes: u64,
    pub files_total: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FolderManifest {
    binding: Binding,
    directories: Vec<String>,
    files: Vec<FileInfo>,
    stage: PathBuf,
    stage_device: u64,
    stage_inode: u64,
    complete: bool,
}

impl Client {
    /// Download a whole shared directory as one resumable job. Original file
    /// bytes are verified individually and the destination is never replaced.
    pub async fn get_folder(
        self: &Arc<Self>,
        target: FileTarget,
        remote: &str,
        local: &Path,
        options: TransferOptions,
    ) -> Result<FolderInfo> {
        valid_share_target(&target)?;
        valid_share_path(remote, true)?;
        let mut guard = CancelOnDrop {
            cancellation: options.cancellation.clone(),
            armed: true,
        };
        let result = tokio::select! {
            biased;
            _ = options.cancellation.cancelled() => Err(TransferError::new("CANCELLED", "Folder download paused; resumable data retained")),
            result = self.get_folder_inner(target, remote, local, &options) => result,
        };
        guard.armed = false;
        result
    }

    async fn get_folder_inner(
        self: &Arc<Self>,
        target: FileTarget,
        remote: &str,
        local: &Path,
        options: &TransferOptions,
    ) -> Result<FolderInfo> {
        let local = absolute_file_path(local)?;
        let binding = Binding {
            version: 1,
            own: self.endpoint.id().to_string(),
            target: target.clone(),
            workspace: String::new(),
            remote_path: remote.into(),
            local_path: local.clone(),
            direction: "folder_download".into(),
        };
        let resume = blocking({
            let state = self.state.join("folder-downloads");
            let binding = binding.clone();
            move || resume_files(&state, &binding)
        })
        .await?;
        let saved = blocking({
            let path = resume.journal.clone();
            move || load_folder_manifest(&path)
        })
        .await?;
        let mut session = self.share_session(&target).await?;
        folder_progress(options, "preparing", 0, 0, 0, 0, 0);
        let (directories, entries) = Self::folder_listing(&mut session, remote, options).await?;
        let mut manifest = if options.resume
            && let Some(saved) = saved.as_ref()
        {
            if saved.binding != binding || !same_listing(saved, &directories, &entries) {
                return Err(stale_folder());
            }
            saved.clone()
        } else {
            if std::fs::symlink_metadata(&local).is_ok() {
                return Err(TransferError::new(
                    "CONFLICT",
                    "Folder destination already exists",
                ));
            }
            if let Some(saved) = saved {
                if saved.binding != binding {
                    return Err(stale_folder());
                }
                if std::fs::symlink_metadata(&saved.stage).is_ok() {
                    validate_stage(&saved, &saved.stage)?;
                    std::fs::remove_dir_all(&saved.stage).map_err(TransferError::io)?;
                }
            }
            let mut files = Vec::with_capacity(entries.len());
            for entry in &entries {
                if options.cancellation.is_cancelled() {
                    return Err(paused_folder());
                }
                let file = Self::shared_file_info(&mut session.stream, &entry.path).await?;
                if file.size_bytes != entry.size_bytes {
                    return Err(stale_folder());
                }
                files.push(file);
            }
            let stage = tempfile::Builder::new()
                .prefix(".wispnet-folder-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in(local.parent().unwrap())
                .map_err(TransferError::io)?
                .keep();
            let meta = std::fs::symlink_metadata(&stage).map_err(TransferError::io)?;
            let manifest = FolderManifest {
                binding,
                directories,
                files,
                stage,
                stage_device: meta.dev(),
                stage_inode: meta.ino(),
                complete: false,
            };
            if let Err(error) = save_folder_manifest(&resume, &manifest).await {
                let _ = std::fs::remove_dir(&manifest.stage);
                return Err(error);
            }
            manifest
        };
        let total = manifest
            .files
            .iter()
            .try_fold(0u64, |sum, f| sum.checked_add(f.size_bytes))
            .filter(|n| *n <= 1u64 << 40)
            .ok_or_else(|| {
                TransferError::new("RESOURCE_LIMIT", "Folder exceeds the transfer size limit")
            })?;
        let count = manifest.files.len() as u64;
        // Recover the publication receipt if the process stopped after rename
        // but before the private manifest's complete bit was synced.
        let published = std::fs::symlink_metadata(&local).is_ok();
        if !published && manifest.complete {
            let stage = tempfile::Builder::new()
                .prefix(".wispnet-folder-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in(local.parent().unwrap())
                .map_err(TransferError::io)?
                .keep();
            let meta = std::fs::symlink_metadata(&stage).map_err(TransferError::io)?;
            manifest.stage = stage;
            manifest.stage_device = meta.dev();
            manifest.stage_inode = meta.ino();
            manifest.complete = false;
            save_folder_manifest(&resume, &manifest).await?;
        }
        let root = if published {
            validate_stage(&manifest, &local)?;
            local.clone()
        } else {
            if manifest.complete {
                return Err(TransferError::new(
                    "NOT_FOUND",
                    "Completed folder destination was removed",
                ));
            }
            validate_stage(&manifest, &manifest.stage)?;
            manifest.stage.clone()
        };
        for directory in &manifest.directories {
            safe_folder_directory(&root, directory)?;
        }
        let mut completed_bytes = 0;
        for (index, file) in manifest.files.iter().enumerate() {
            if options.cancellation.is_cancelled() {
                return Err(paused_folder());
            }
            let relative = folder_relative(remote, &file.path)?;
            let destination = root.join(relative);
            let callback = options.progress.clone();
            let base = completed_bytes;
            let file_index = index as u64;
            let file_options = TransferOptions {
                resume: true,
                cancellation: options.cancellation.clone(),
                progress: callback.map(|callback| {
                    Arc::new(move |p: TransferProgress| {
                        callback(TransferProgress {
                            phase: if p.phase == "complete" {
                                "transferring".into()
                            } else {
                                p.phase
                            },
                            verified_bytes: base + p.verified_bytes,
                            size_bytes: total,
                            downloaded_bytes: p.downloaded_bytes,
                            files_complete: Some(file_index),
                            files_total: Some(count),
                        });
                    }) as ProgressCallback
                }),
            };
            self.get_file_on_session(
                target.clone(),
                &file.path,
                &destination,
                &file_options,
                &mut session,
                DownloadExpectation::Pinned(file),
            )
            .await?;
            completed_bytes += file.size_bytes;
            folder_progress(
                options,
                "transferring",
                completed_bytes,
                total,
                index as u64 + 1,
                count,
                session.downloaded_bytes,
            );
        }
        // Directory listings are live, not a filesystem snapshot. Refuse a
        // changed membership or changed file instead of silently mixing runs.
        folder_progress(
            options,
            "verifying",
            total,
            total,
            count,
            count,
            session.downloaded_bytes,
        );
        let (directories, entries) = Self::folder_listing(&mut session, remote, options).await?;
        if !same_listing(&manifest, &directories, &entries) {
            return Err(stale_folder());
        }
        for file in &manifest.files {
            if Self::shared_file_info(&mut session.stream, &file.path).await? != *file {
                return Err(stale_folder());
            }
        }
        blocking({
            let root = root.clone();
            let local = local.clone();
            let manifest = manifest.clone();
            let cancellation = options.cancellation.clone();
            let lock = resume._lock.clone();
            move || {
                let _lock = lock;
                validate_folder_contents(&root, &manifest, &cancellation)?;
                if !published {
                    for directory in manifest.directories.iter().rev() {
                        if cancellation.is_cancelled() {
                            return Err(paused_folder());
                        }
                        File::open(root.join(directory))
                            .and_then(|f| f.sync_all())
                            .map_err(TransferError::io)?;
                    }
                    File::open(&root)
                        .and_then(|f| f.sync_all())
                        .map_err(TransferError::io)?;
                    validate_stage(&manifest, &root)?;
                    if cancellation.is_cancelled() {
                        return Err(paused_folder());
                    }
                    publish_folder(&root, &local)?;
                    sync_parent(&local)?;
                }
                Ok(())
            }
        })
        .await?;
        manifest.complete = true;
        save_folder_manifest(&resume, &manifest).await?;
        folder_progress(
            options,
            "complete",
            total,
            total,
            count,
            count,
            session.downloaded_bytes,
        );
        Ok(FolderInfo {
            path: remote.into(),
            size_bytes: total,
            files_total: count,
        })
    }

    async fn folder_listing(
        session: &mut ShareSession,
        root: &str,
        options: &TransferOptions,
    ) -> Result<(Vec<String>, Vec<ShareEntry>)> {
        let mut pending = vec![root.to_owned()];
        let mut directories = Vec::new();
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        let mut metadata_bytes = 0usize;
        while let Some(path) = pending.pop() {
            let mut cursor = None;
            loop {
                if options.cancellation.is_cancelled() {
                    return Err(paused_folder());
                }
                let page = Self::list_shared_files_on(&mut session.stream, &path, cursor).await?;
                for entry in page.entries {
                    let relative = folder_relative(root, &entry.path)?;
                    if relative.split('/').count() > MAX_FOLDER_DEPTH
                        || !seen.insert(entry.path.clone())
                        || seen.len() > MAX_FOLDER_ENTRIES
                    {
                        return Err(TransferError::new(
                            "RESOURCE_LIMIT",
                            "Folder has too many entries or nesting levels",
                        ));
                    }
                    metadata_bytes = metadata_bytes
                        .checked_add(entry.path.len() + 512)
                        .ok_or_else(protocol_error)?;
                    if metadata_bytes as u64 > MAX_FOLDER_MANIFEST / 2 {
                        return Err(TransferError::new(
                            "RESOURCE_LIMIT",
                            "Folder manifest is too large",
                        ));
                    }
                    if entry.is_dir {
                        directories.push(relative.to_owned());
                        pending.push(entry.path);
                    } else {
                        files.push(entry);
                    }
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        directories.sort();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((directories, files))
    }
}

fn folder_progress(
    options: &TransferOptions,
    phase: &str,
    bytes: u64,
    total: u64,
    files: u64,
    count: u64,
    downloaded_bytes: u64,
) {
    if let Some(progress) = &options.progress {
        progress(TransferProgress {
            phase: phase.into(),
            verified_bytes: bytes,
            size_bytes: total,
            downloaded_bytes: Some(downloaded_bytes),
            files_complete: Some(files),
            files_total: Some(count),
        });
    }
}
fn stale_folder() -> TransferError {
    TransferError::new(
        "STALE_VERSION",
        "Shared folder changed; restart the folder download",
    )
}
fn paused_folder() -> TransferError {
    TransferError::new(
        "CANCELLED",
        "Folder download paused; resumable data retained",
    )
}
fn folder_relative<'a>(root: &str, path: &'a str) -> Result<&'a str> {
    valid_share_path(path, false)?;
    if root.is_empty() {
        return Ok(path);
    }
    path.strip_prefix(root)
        .and_then(|p| p.strip_prefix('/'))
        .filter(|p| !p.is_empty())
        .ok_or_else(protocol_error)
}
fn same_listing(manifest: &FolderManifest, directories: &[String], entries: &[ShareEntry]) -> bool {
    manifest.directories == directories
        && manifest.files.len() == entries.len()
        && manifest
            .files
            .iter()
            .zip(entries)
            .all(|(file, entry)| file.path == entry.path && file.size_bytes == entry.size_bytes)
}
fn validate_stage(manifest: &FolderManifest, path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(TransferError::io)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o077 != 0
        || meta.dev() != manifest.stage_device
        || meta.ino() != manifest.stage_inode
        || manifest.stage.parent() != manifest.binding.local_path.parent()
        || !manifest
            .stage
            .file_name()
            .is_some_and(|n| n.as_bytes().starts_with(b".wispnet-folder-"))
    {
        return Err(TransferError::new(
            "PATH_DENIED",
            "Folder download staging directory was replaced",
        ));
    }
    Ok(())
}
fn safe_folder_directory(root: &Path, relative: &str) -> Result<()> {
    valid_share_path(relative, false)?;
    let mut path = root.to_owned();
    for part in relative.split('/') {
        path.push(part);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(TransferError::io(error)),
        }
        let meta = std::fs::symlink_metadata(&path).map_err(TransferError::io)?;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.permissions().mode() & 0o077 != 0
        {
            return Err(TransferError::new(
                "PATH_DENIED",
                "Unsafe folder download directory",
            ));
        }
    }
    Ok(())
}
fn validate_folder_contents(
    root: &Path,
    manifest: &FolderManifest,
    cancellation: &FileCancellation,
) -> Result<()> {
    let mut expected = BTreeMap::new();
    for directory in &manifest.directories {
        expected.insert(directory.clone(), None);
    }
    for file in &manifest.files {
        expected.insert(
            folder_relative(&manifest.binding.remote_path, &file.path)?.to_owned(),
            Some(file.size_bytes),
        );
    }
    let mut pending = vec![PathBuf::new()];
    let mut found = 0;
    while let Some(relative) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err(paused_folder());
        }
        for entry in std::fs::read_dir(root.join(&relative)).map_err(TransferError::io)? {
            if cancellation.is_cancelled() {
                return Err(paused_folder());
            }
            let entry = entry.map_err(TransferError::io)?;
            let child = relative.join(entry.file_name());
            let name = child.to_str().ok_or_else(protocol_error)?;
            let expected_size = expected.get(name).ok_or_else(|| {
                TransferError::new(
                    "PATH_DENIED",
                    "Unexpected file in folder download staging directory",
                )
            })?;
            let meta = std::fs::symlink_metadata(entry.path()).map_err(TransferError::io)?;
            if meta.file_type().is_symlink()
                || (expected_size.is_none() && !meta.is_dir())
                || expected_size.is_some_and(|size| !meta.is_file() || meta.len() != size)
            {
                return Err(TransferError::new(
                    "PATH_DENIED",
                    "Folder download staging contents were replaced",
                ));
            }
            found += 1;
            if meta.is_dir() {
                pending.push(child);
            }
        }
    }
    if found != expected.len() {
        return Err(TransferError::new(
            "PATH_DENIED",
            "Folder download staging entries are missing",
        ));
    }
    Ok(())
}

fn load_folder_manifest(path: &Path) -> Result<Option<FolderManifest>> {
    let file = match safe_open(path, false, false) {
        Ok(file) => file,
        Err(e) if e.code == "NOT_FOUND" => return Ok(None),
        Err(e) => return Err(e),
    };
    if file.metadata().map_err(TransferError::io)?.len() > MAX_FOLDER_MANIFEST {
        return Err(protocol_error());
    }
    let manifest: FolderManifest = serde_json::from_reader(file).map_err(|_| protocol_error())?;
    if manifest.files.len() + manifest.directories.len() > MAX_FOLDER_ENTRIES {
        return Err(protocol_error());
    }
    for directory in &manifest.directories {
        valid_share_path(directory, false)?;
    }
    for file in &manifest.files {
        folder_relative(&manifest.binding.remote_path, &file.path)?;
    }
    Ok(Some(manifest))
}
async fn save_folder_manifest(resume: &ResumeFiles, manifest: &FolderManifest) -> Result<()> {
    let path = resume.journal.clone();
    let bytes = serde_json::to_vec(manifest).map_err(|_| protocol_error())?;
    if bytes.len() as u64 > MAX_FOLDER_MANIFEST {
        return Err(protocol_error());
    }
    let lock = resume._lock.clone();
    let stage = (!manifest.complete).then(|| manifest.stage.clone());
    blocking(move || {
        let _lock = lock;
        // The private receipt must never reference a directory entry that has
        // not reached durable storage yet.
        if let Some(stage) = stage {
            File::open(&stage)
                .and_then(|f| f.sync_all())
                .map_err(TransferError::io)?;
            sync_parent(&stage)?;
        }
        let mut file =
            tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(TransferError::io)?;
        file.write_all(&bytes).map_err(TransferError::io)?;
        file.as_file().sync_all().map_err(TransferError::io)?;
        file.persist(&path)
            .map_err(|e| TransferError::io(e.error))?;
        sync_parent(&path)
    })
    .await
}
fn publish_folder(stage: &Path, destination: &Path) -> Result<()> {
    let stage =
        std::ffi::CString::new(stage.as_os_str().as_bytes()).map_err(|_| protocol_error())?;
    let destination =
        std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(|_| protocol_error())?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    let result = -1;
    if result != 0 {
        return Err(TransferError::io(io::Error::last_os_error()));
    }
    Ok(())
}

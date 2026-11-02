//! Protected client bootstrap records. No invitation capabilities or host permissions.
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use wispnet_core::{
    config::TransportConfig,
    identity::{EndpointId, IdentityError},
    pairing::Bootstrap,
};
fn error() -> IdentityError {
    IdentityError("saved peer bootstrap unavailable or corrupt")
}
fn directory(state: &Path, create: bool) -> Result<Option<PathBuf>, IdentityError> {
    if !state.is_absolute()
        || state
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(error());
    }
    let root = fs::symlink_metadata(state).map_err(|_| error())?;
    if !root.is_dir() || root.uid() != unsafe { libc::geteuid() } || root.mode() & 0o777 != 0o700 {
        return Err(error());
    }
    let path = state.join("peer-pins");
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                fs::File::open(state)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| error())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(error()),
        }
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error()),
    };
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(error());
    }
    Ok(Some(path))
}
fn protected(file: &fs::File) -> Result<(), IdentityError> {
    let m = file.metadata().map_err(|_| error())?;
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
    {
        return Err(error());
    }
    Ok(())
}
pub fn load(state: &Path, expected: EndpointId) -> Result<Option<Bootstrap>, IdentityError> {
    let Some(directory) = directory(state, false)? else {
        return Ok(None);
    };
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(format!("{expected}.json")))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error()),
    };
    protected(&file)?;
    if file.metadata().map_err(|_| error())?.len() > 4096 {
        return Err(error());
    }
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| error())?;
    if bytes.len() > 4096 {
        return Err(error());
    }
    let record: Bootstrap = serde_json::from_slice(&bytes).map_err(|_| error())?;
    if !record.valid(expected) {
        return Err(error());
    }
    Ok(Some(record))
}
/// Forget a validated saved host without changing any remote permissions.
pub fn remove(state: &Path, host: EndpointId) -> Result<(), IdentityError> {
    let Some(directory) = directory(state, false)? else {
        return Ok(());
    };
    if load(state, host)?.is_some() {
        fs::remove_file(directory.join(format!("{host}.json"))).map_err(|_| error())?;
        fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| error())?;
    }
    Ok(())
}
/// Save only after authenticated pairing acknowledgement and explicit expected pin.
/// Existing corrupt records are never overwritten as an implicit trust repair.
pub fn save(state: &Path, record: &Bootstrap) -> Result<(), IdentityError> {
    if !record.valid(record.host) {
        return Err(error());
    }
    let directory = directory(state, true)?.ok_or_else(error)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".lock"))
        .map_err(|_| error())?;
    protected(&lock)?;
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(error());
    }
    let _previous = load(state, record.host)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(|_| error())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| error())?;
    let bytes = serde_json::to_vec(record).map_err(|_| error())?;
    if bytes.len() > 4096 {
        return Err(error());
    }
    temporary.write_all(&bytes).map_err(|_| error())?;
    temporary.as_file().sync_all().map_err(|_| error())?;
    temporary
        .persist(directory.join(format!("{}.json", record.host)))
        .map_err(|_| error())?;
    fs::File::open(directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| error())?;
    Ok(())
}
/// Explicit local transport configuration wins, including disabling relay use.
/// Otherwise an authenticated saved bootstrap supplies the sole explicit relay.
pub fn transport(
    configured: Option<&TransportConfig>,
    saved: Option<&Bootstrap>,
) -> Option<TransportConfig> {
    configured.cloned().or_else(|| {
        saved.map(|saved| TransportConfig {
            backup_relay_urls: Vec::new(),
            prefer_primary_relay: true,
            keep_backup_relays_warm: true,
            discovery_relay_url: None,
            relay_url: Some(saved.relay_url.clone()),
            relay_only: false,
            relay_ca_file: None,
            bind_addr: "127.0.0.1:0".parse().expect("constant address"),
            max_connections: 16,
            max_streams: 64,
            max_streams_per_connection: 8,
        })
    })
}

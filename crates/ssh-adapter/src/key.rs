//! SSH gateway keys are independent of Wispnet's identity and scoped to one peer.
use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

pub(crate) fn load(state: &Path, host: &str) -> Result<PrivateKey, String> {
    if host.len() != 64 || !host.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("SSH host key requires a pinned peer ID".into());
    }
    let directory = state.join("ssh-adapter");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    let meta = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err("SSH adapter key directory must be owner-only and not a symlink".into());
    }
    let path = directory.join(format!("{host}.key"));
    match read(&path) {
        Ok(key) => return Ok(key),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.to_string()),
        Err(_) => {}
    }
    let key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(|e| e.to_string())?;
    let text = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(&directory).map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    match file.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(&directory)
                .and_then(|d| d.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error.to_string()),
    }
    // Concurrent starts converge on the winning file, including its permissions.
    read(&path).map_err(|e| e.to_string())
}

fn read(path: &Path) -> std::io::Result<PrivateKey> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.len() > 16 * 1024
    {
        return Err(std::io::Error::other(
            "SSH host key must be a bounded owner-only regular file",
        ));
    }
    let mut text = String::new();
    file.take(16 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 16 * 1024 {
        return Err(std::io::Error::other("SSH host key too large"));
    }
    let key = PrivateKey::from_openssh(&text).map_err(std::io::Error::other)?;
    if key.algorithm() != Algorithm::Ed25519 || key.is_encrypted() {
        return Err(std::io::Error::other(
            "SSH adapter requires an unencrypted Ed25519 gateway key",
        ));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn concurrent_starts_keep_one_stable_key_and_peers_have_distinct_keys() {
        let root = tempfile::tempdir().unwrap();
        let host = "a".repeat(64);
        let keys: Vec<_> = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| load(root.path(), &host).unwrap().public_key().clone()))
                .collect();
            tasks.into_iter().map(|task| task.join().unwrap()).collect()
        });
        assert!(keys.iter().all(|key| *key == keys[0]));
        assert_ne!(
            load(root.path(), &"b".repeat(64)).unwrap().public_key(),
            &keys[0]
        );
    }

    #[test]
    fn untrusted_key_permissions_and_symlinks_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let host = "a".repeat(64);
        load(root.path(), &host).unwrap();
        let directory = root.path().join("ssh-adapter");
        let path = directory.join(format!("{host}.key"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(root.path(), &host).is_err());
        fs::remove_file(&path).unwrap();
        symlink(root.path().join("absent"), &path).unwrap();
        assert!(load(root.path(), &host).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
        symlink(root.path(), &directory).unwrap();
        assert!(load(root.path(), &host).is_err());
    }
}

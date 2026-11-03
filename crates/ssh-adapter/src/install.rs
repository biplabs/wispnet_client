use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub(crate) fn save(host: &str, stanza: &str) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("HOME is unset")?;
    save_in(Path::new(&home), host, stanza).map_err(|e| e.to_string())
}

fn save_in(home: &Path, host: &str, stanza: &str) -> std::io::Result<()> {
    let dir = home.join(".ssh");
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err(std::io::Error::other(
            "SSH directory must belong to you and not be writable by others",
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(".wispnet.lock"))?;
    let meta = lock.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other("Unsafe SSH configuration lock"));
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let path = dir.join("config");
    let mut old = String::new();
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => {
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o022 != 0
                || meta.len() > 1024 * 1024
            {
                return Err(std::io::Error::other(
                    "Unsafe or oversized SSH configuration",
                ));
            }
            file.take(1024 * 1024 + 1).read_to_string(&mut old)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let begin = format!("# Wispnet bridge {host} begin\n");
    let end = format!("# Wispnet bridge {host} end\n");
    if let Some(start) = old.find(&begin) {
        let finish = old[start..]
            .find(&end)
            .ok_or_else(|| std::io::Error::other("Incomplete Wispnet SSH entry"))?
            + start
            + end.len();
        old.replace_range(start..finish, "");
    }
    if old.len() > 1024 * 1024 {
        return Err(std::io::Error::other(
            "SSH configuration grew while reading",
        ));
    }
    let alias = stanza
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Host "))
        .unwrap_or("");
    if old.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("Host"))
            && fields.any(|value| value == alias)
    }) {
        return Err(std::io::Error::other(
            "This SSH alias already has a custom entry; edit ~/.ssh/config to resolve it",
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    write!(file, "{begin}{stanza}Host *\n{end}{old}")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_preserves_custom_aliases_and_rejects_symlinked_config() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".ssh");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("config");
        let custom = "Host shell-desk.localhost\n  HostName my-existing-host\n";
        fs::write(&path, custom).unwrap();
        assert!(save_in(root.path(), "peer", "Host shell-desk.localhost\n").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), custom);
        fs::remove_file(&path).unwrap();
        let target = root.path().join("other-config");
        fs::write(&target, custom).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(save_in(root.path(), "peer", "Host shell-desk.localhost\n").is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), custom);
    }
    #[test]
    fn install_is_repeatable_and_preserves_other_entries() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".ssh")).unwrap();
        let path = root.path().join(".ssh/config");
        fs::write(&path, "Host personal\n  HostName example.org\n").unwrap();
        save_in(root.path(), "peer", "Host shell-desk.localhost\n").unwrap();
        let once = fs::read_to_string(&path).unwrap();
        save_in(root.path(), "peer", "Host shell-desk.localhost\n").unwrap();
        assert_eq!(once, fs::read_to_string(path).unwrap());
        assert!(once.ends_with("Host personal\n  HostName example.org\n"));
    }
}

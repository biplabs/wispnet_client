//! Shared source identity for Cargo, Flutter and release tooling.
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{fs, io, path::Path, process::Command};

pub fn git(root: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return Err(io::Error::other("Cannot read source revision"));
    }
    Ok(output.stdout)
}
pub fn identity(root: &Path, version: &str) -> io::Result<String> {
    let revision = String::from_utf8(git(root, &["rev-parse", "--short=12", "HEAD"])?)
        .map_err(io::Error::other)?;
    let revision = revision.trim();
    if git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty() {
        return Ok(format!("{version}+g{revision}"));
    }
    let paths = git(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let mut paths: Vec<_> = paths.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    paths.sort_unstable();
    paths.dedup();
    let mut digest = Sha256::new();
    for name in paths {
        use std::os::unix::ffi::OsStrExt;
        let path = root.join(std::ffi::OsStr::from_bytes(name));
        digest.update(name);
        digest.update([0]);
        if path.is_file() {
            digest.update((fs::metadata(&path)?.permissions().mode() & 0o111).to_string());
            digest.update([0]);
            digest.update(fs::read(path)?);
        } else {
            digest.update(b"<deleted>");
        }
        digest.update([0]);
    }
    Ok(format!(
        "{version}+g{revision}.dirty.{}",
        &format!("{:x}", digest.finalize())[..12]
    ))
}

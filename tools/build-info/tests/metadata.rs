//! Contract tests for build labels using private Git repositories.
#[path = "../../../scripts/build_metadata.rs"]
mod metadata;

use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};

struct Repository {
    directory: tempfile::TempDir,
}

impl Repository {
    fn new() -> Self {
        let repository = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        repository.git(&["init", "--quiet", "--initial-branch=main"]);
        repository.git(&["config", "core.filemode", "true"]);
        repository.write("source.txt", b"version one\n");
        repository.commit();
        repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .args(arguments)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Git fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, name: &str, contents: &[u8]) {
        let path = self.path().join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "--all"]);
        self.git(&[
            "-c",
            "user.name=Build metadata fixture",
            "-c",
            "user.email=build-metadata@example.invalid",
            "commit",
            "--quiet",
            "--message=Fixture source snapshot",
        ]);
    }

    fn revision(&self) -> String {
        self.git(&["rev-parse", "--short=12", "HEAD"])
    }

    fn identity(&self, version: &str) -> String {
        metadata::identity(self.path(), version).unwrap()
    }

    fn clean_label(&self, version: &str) -> String {
        format!("{version}+g{}", self.revision())
    }

    // These byte streams are fixed fixture expectations, independent of Git
    // discovery, file traversal, metadata lookup, and source-reading code.
    fn dirty_label(&self, version: &str, expected_bytes: &[u8]) -> String {
        let digest = format!("{:x}", Sha256::digest(expected_bytes));
        format!("{}.dirty.{}", self.clean_label(version), &digest[..12])
    }
}

#[test]
fn clean_snapshot_and_release_version_have_stable_labels() {
    let repository = Repository::new();
    assert_eq!(
        repository.identity("1.2.3"),
        repository.clean_label("1.2.3")
    );
    assert_eq!(repository.identity("1.2.3"), repository.identity("1.2.3"));
    assert_eq!(
        repository.identity("1.2.4"),
        repository.clean_label("1.2.4")
    );
    assert_ne!(repository.identity("1.2.3"), repository.identity("1.2.4"));
}

#[test]
fn edited_source_has_the_same_identity_before_staging_and_after_unstaging() {
    let repository = Repository::new();
    repository.write("source.txt", b"version two\n");
    let expected = repository.dirty_label("1.2.3", b"source.txt\x000\x00version two\n\x00");
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["add", "source.txt"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["reset", "--quiet", "HEAD", "--", "source.txt"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    let bumped = repository.identity("1.2.4");
    assert_eq!(bumped, expected.replacen("1.2.3+", "1.2.4+", 1));
    repository.write("source.txt", b"version one\n");
    assert_eq!(
        repository.identity("1.2.3"),
        repository.clean_label("1.2.3")
    );
}

#[test]
fn untracked_source_affects_the_label_and_ignored_outputs_do_not() {
    let repository = Repository::new();
    repository.write(".gitignore", b"ignored/\n*.scratch\n");
    repository.commit();
    let clean = repository.identity("1.2.3");
    fs::create_dir(repository.path().join("ignored")).unwrap();
    repository.write("ignored/build-output", b"machine-local output");
    repository.write("local.scratch", b"temporary output");
    assert_eq!(repository.identity("1.2.3"), clean);
    repository.write("fresh file.rs", b"new source\n");
    let expected = repository.dirty_label(
        "1.2.3",
        b".gitignore\x000\x00ignored/\n*.scratch\n\x00fresh file.rs\x000\x00new source\n\x00source.txt\x000\x00version one\n\x00",
    );
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.write("ignored/build-output", b"different local output");
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["add", "fresh file.rs"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["reset", "--quiet", "HEAD", "--", "fresh file.rs"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    fs::remove_file(repository.path().join("fresh file.rs")).unwrap();
    assert_eq!(repository.identity("1.2.3"), clean);
}

#[test]
fn a_deleted_tracked_source_is_distinct_from_an_empty_file() {
    let repository = Repository::new();
    repository.write("source.txt", b"");
    let empty = repository.identity("1.2.3");
    assert_eq!(
        empty,
        repository.dirty_label("1.2.3", b"source.txt\x000\x00\x00")
    );
    fs::remove_file(repository.path().join("source.txt")).unwrap();
    let deleted = repository.identity("1.2.3");
    assert_eq!(
        deleted,
        repository.dirty_label("1.2.3", b"source.txt\x00<deleted>\x00")
    );
    assert_ne!(empty, deleted);
    repository.git(&["checkout", "--", "source.txt"]);
    assert_eq!(
        repository.identity("1.2.3"),
        repository.clean_label("1.2.3")
    );
}

#[test]
fn executable_changes_are_hashed_and_private_read_permissions_are_not() {
    let repository = Repository::new();
    let path = repository.path().join("source.txt");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    // The existing identity contract uses decimal 73 for the executable mask
    // 0o111; changing that encoding would rename every dirty executable tree.
    let expected = repository.dirty_label("1.2.3", b"source.txt\x0073\x00version one\n\x00");
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["add", "source.txt"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    repository.git(&["reset", "--quiet", "HEAD", "--", "source.txt"]);
    assert_eq!(repository.identity("1.2.3"), expected);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        repository.identity("1.2.3"),
        repository.clean_label("1.2.3")
    );
}

#[test]
fn symlinks_follow_file_contents_and_dangling_links_use_the_deleted_marker() {
    let repository = Repository::new();
    let pointer = repository.path().join("pointer");
    symlink("source.txt", &pointer).unwrap();
    repository.commit();
    let clean = repository.identity("1.2.3");
    assert_eq!(clean, repository.clean_label("1.2.3"));
    repository.write("source.txt", b"version two\n");
    assert_eq!(
        repository.identity("1.2.3"),
        repository.dirty_label(
            "1.2.3",
            b"pointer\x000\x00version two\n\x00source.txt\x000\x00version two\n\x00"
        )
    );
    repository.write("source.txt", b"version one\n");
    assert_eq!(repository.identity("1.2.3"), clean);
    fs::remove_file(&pointer).unwrap();
    symlink("missing-target", &pointer).unwrap();
    assert_eq!(
        repository.identity("1.2.3"),
        repository.dirty_label(
            "1.2.3",
            b"pointer\x00<deleted>\x00source.txt\x000\x00version one\n\x00"
        )
    );
    fs::remove_file(&pointer).unwrap();
    symlink("source.txt", pointer).unwrap();
    assert_eq!(repository.identity("1.2.3"), clean);
}

#[test]
fn version_and_android_code_commands_need_no_runtime_interpreter() {
    let executable = env!("CARGO_BIN_EXE_wispnet-build-info");
    let version = Command::new(executable)
        .arg("--version")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        env!("CARGO_PKG_VERSION")
    );
    let code = Command::new(executable)
        .arg("--android-version-code")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(code.status.success());
    let expected = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse::<u32>().unwrap())
        .fold(0_u32, |value, part| value * 1000 + part);
    let actual: u32 = String::from_utf8(code.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(actual, expected);
    assert!((1..=2_100_000_000).contains(&actual));
    let invalid = Command::new(executable).arg("--unknown").output().unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Usage:"));
}

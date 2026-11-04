#[path = "../../../scripts/build_metadata.rs"]
mod metadata;
use std::{env, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../..");
    let version = metadata::identity(&root, &env::var("CARGO_PKG_VERSION").unwrap())
        .expect("shared build identity");
    println!("cargo:rustc-env=WISPNET_VERSION={version}");
    // Recheck Git on every Cargo invocation: mtime watches cannot reliably see
    // new untracked files, executable-mode changes, or every packed-ref update.
    // This path is deliberately absent and never written.
    let check = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("check-source-identity");
    println!("cargo:rerun-if-changed={}", check.display());
}

//! Binary, resumable incoming file transfers. A grant permits sending files to
//! the host's configured folders, never reading files from them.
//! Peer authentication, grants and active
//! revocation are enforced by Wispnet before and during `FileService::serve`.
mod compression;
mod protocol;
mod server;
mod share;
pub use compression::{AdaptiveEncoder, decode_data_response};
pub use protocol::*;
pub use server::{FileService, Limits, Publication, Publisher};
pub use share::ShareSource;

use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

pub fn sha256(data: &[u8]) -> String {
    encode_hash(Sha256::digest(data))
}

fn encode_hash(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Blocking, streaming checksum; callers on an async runtime must offload it.
pub fn hash_file(path: &Path) -> io::Result<String> {
    hash_reader(File::open(path)?)
}

fn hash_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(encode_hash(hasher.finalize()))
}

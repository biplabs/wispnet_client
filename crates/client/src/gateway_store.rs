//! Owner-only address reservations. No browser credentials or authority are persisted.
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub peer: EndpointId,
    pub service: String,
    pub incarnation: String,
    pub hostname: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}
impl Binding {
    pub fn authority(&self) -> String {
        #[cfg(target_os = "ios")]
        let hostname = "127.0.0.1";
        #[cfg(not(target_os = "ios"))]
        let hostname = self.hostname.as_str();
        format!("{}:{}", hostname, self.port)
    }
    pub fn url(&self) -> String {
        format!("http://{}/", self.authority())
    }
    pub fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.hostname).chain(self.aliases.iter())
    }
    fn valid(&self) -> bool {
        self.port >= 1024
            && wispnet_core::protocol::valid_service_id(&self.service)
            && wispnet_core::enrollment::valid_code(&self.incarnation)
            && valid_hostname(&self.hostname)
            && self.aliases.len() <= 1
            && self
                .aliases
                .iter()
                .all(|a| legacy_hostname(a) && a != &self.hostname)
    }
}
fn valid_hostname(hostname: &str) -> bool {
    hostname.strip_suffix(".localhost").is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 63
            && !s.starts_with('-')
            && !s.ends_with('-')
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}
pub(super) fn legacy_hostname(hostname: &str) -> bool {
    hostname
        .strip_prefix('w')
        .and_then(|s| s.strip_suffix(".localhost"))
        .is_some_and(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}
fn slug(name: &str, fallback: &str, limit: usize) -> String {
    let mut result = String::new();
    for b in name.bytes() {
        if result.len() >= limit {
            break;
        }
        if b.is_ascii_alphanumeric() {
            result.push((b as char).to_ascii_lowercase());
        } else if !result.is_empty() && !result.ends_with('-') {
            result.push('-');
        }
    }
    let trimmed = result.trim_end_matches('-');
    if trimmed.is_empty() {
        fallback.into()
    } else {
        trimmed.into()
    }
}
pub(super) fn readable_hostname(
    system: Option<&str>,
    peer: EndpointId,
    service: &str,
    _saved: &[Binding],
) -> String {
    use sha1::{Digest, Sha1};
    let label = slug(service, "service", 24);
    let service_label = if label == service {
        label
    } else {
        // Sanitization and truncation must not allocate order-dependent suffixes:
        // every device derives the same distinct label from the full service ID.
        let digest: String = Sha1::digest(service.as_bytes())[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{}--{}", slug(service, "service", 15), &digest[..8])
    };
    format!(
        "{}-{}-{}.localhost",
        slug(system.unwrap_or("system"), "system", 20),
        &peer.to_string()[..12],
        service_label
    )
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    owner: EndpointId,
    routes: Vec<Binding>,
}
fn error() -> String {
    "Saved browser addresses are unavailable or invalid".into()
}
fn root(state: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(state).map_err(|_| error())?;
    if !m.is_dir() || m.mode() & 0o777 != 0o700 || m.uid() != unsafe { libc::geteuid() } {
        return Err(error());
    }
    Ok(())
}
pub(super) fn load(state: &Path, owner: EndpointId) -> Result<Vec<Binding>, String> {
    root(state)?;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("browser-routes.json"))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(error()),
    };
    let m = file.metadata().map_err(|_| error())?;
    if !m.is_file()
        || m.nlink() != 1
        || m.mode() & 0o777 != 0o600
        || m.uid() != unsafe { libc::geteuid() }
        || m.len() > 32768
    {
        return Err(error());
    }
    let mut bytes = Vec::new();
    file.take(32769)
        .read_to_end(&mut bytes)
        .map_err(|_| error())?;
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| error())?;
    if ![1, 2].contains(&saved.version)
        || saved.owner != owner
        || saved.routes.len() > 16
        || saved.routes.iter().any(|r| !r.valid())
    {
        return Err(error());
    }
    for (i, r) in saved.routes.iter().enumerate() {
        if saved.routes[..i].iter().any(|b| {
            b.names().any(|name| r.names().any(|other| other == name))
                || (b.peer == r.peer && b.service == r.service)
        }) {
            return Err(error());
        }
    }
    Ok(saved.routes)
}
pub(super) fn save(state: &Path, owner: EndpointId, routes: &[Binding]) -> Result<(), String> {
    root(state)?;
    let path = state.join("browser-routes.json");
    let mut file = wispnet_registry::control::credential_temp(&path).map_err(|_| error())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| error())?;
    serde_json::to_writer(
        &mut file,
        &Saved {
            version: 2,
            owner,
            routes: routes.to_vec(),
        },
    )
    .map_err(|_| error())?;
    file.flush().map_err(|_| error())?;
    file.as_file().sync_all().map_err(|_| error())?;
    file.persist(&path).map_err(|_| error())?;
    fs::File::open(state)
        .and_then(|f| f.sync_all())
        .map_err(|_| error())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readable_names_are_deterministic_and_distinguish_transformed_service_ids() {
        let owner = wispnet_core::identity::SecretKey::generate().public();
        let name = readable_hostname(Some("J Strix"), owner, "llama", &[]);
        assert_eq!(
            name,
            format!("j-strix-{}-llama.localhost", &owner.to_string()[..12])
        );
        let saved = vec![Binding {
            peer: owner,
            service: "llama".into(),
            incarnation: "a".repeat(64),
            hostname: name.clone(),
            port: 7842,
            aliases: vec![],
        }];
        assert_eq!(
            readable_hostname(Some("J Strix"), owner, "llama", &saved),
            name
        );
        let transformed = readable_hostname(Some("J Strix"), owner, "model.v1", &[]);
        let mimic = transformed
            .strip_prefix(&format!("j-strix-{}-", &owner.to_string()[..12]))
            .unwrap()
            .strip_suffix(".localhost")
            .unwrap();
        assert_ne!(
            transformed,
            readable_hostname(Some("J Strix"), owner, mimic, &[])
        );
        assert_ne!(
            readable_hostname(Some("J Strix"), owner, "model.v1", &[]),
            readable_hostname(Some("J Strix"), owner, "model_v1", &[])
        );
        assert_ne!(
            readable_hostname(None, owner, &format!("{}a", "x".repeat(40)), &[]),
            readable_hostname(None, owner, &format!("{}b", "x".repeat(40)), &[])
        );
        for label in ["", "中文", "../../example.com", &"A".repeat(256)] {
            assert!(valid_hostname(&readable_hostname(
                Some(label),
                owner,
                &"s".repeat(64),
                &[]
            )));
        }
    }
    #[test]
    fn address_records_reject_corruption_duplicates_unsafe_permissions_and_symlinks() {
        let state = tempfile::tempdir().unwrap();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let owner = wispnet_core::identity::SecretKey::generate().public();
        let binding = Binding {
            peer: owner,
            service: "site".into(),
            incarnation: "a".repeat(64),
            hostname: format!("w{}.localhost", "b".repeat(32)),
            port: 49152,
            aliases: vec![],
        };
        save(state.path(), owner, std::slice::from_ref(&binding)).unwrap();
        assert_eq!(load(state.path(), owner).unwrap()[0].url(), binding.url());
        let path = state.path().join("browser-routes.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(state.path(), owner).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"{truncated").unwrap();
        assert!(load(state.path(), owner).is_err());
        save(state.path(), owner, &[binding.clone(), binding]).unwrap();
        assert!(load(state.path(), owner).is_err());
        fs::remove_file(&path).unwrap();
        let outside = state.path().join("outside");
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(load(state.path(), owner).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    }
}

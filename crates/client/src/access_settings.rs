//! Device-local listener preferences, independent of owning-host policy.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct ServicePort {
    pub host: iroh::EndpointId,
    pub service: String,
    pub port: u16,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// One loopback HTTP port for registry and all services. The field name is
    /// retained for owner-API and saved-settings compatibility.
    pub registry_port: u16,
    /// Legacy preference retained for saved-settings compatibility; all websites are allowed.
    pub allowed_origins: Vec<String>,
    /// Legacy per-service preferences, retained only to read older settings.
    pub service_ports: Vec<ServicePort>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            registry_port: 7842,
            allowed_origins: vec![],
            service_ports: vec![],
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.registry_port < 1024
            || self.allowed_origins.len() > 32
            || self.service_ports.len() > 16
        {
            return Err("Invalid access settings".into());
        }
        for origin in &self.allowed_origins {
            let u = url::Url::parse(origin).map_err(|_| "Invalid website origin")?;
            if origin.len() > 512
                || !["http", "https"].contains(&u.scheme())
                || u.origin().ascii_serialization() != *origin
            {
                return Err(
                    "Use exact website origins, such as https://example.com (no path)".into(),
                );
            }
        }
        for (i, route) in self.service_ports.iter().enumerate() {
            if route.port < 1024
                || !wispnet_core::protocol::valid_service_id(&route.service)
                || self.service_ports[..i]
                    .iter()
                    .any(|r| r.host == route.host && r.service == route.service)
            {
                return Err("Invalid legacy service port preference".into());
            }
        }
        Ok(())
    }
    pub fn load(state: &Path) -> Result<Self, String> {
        let settings: Self = load(state, "access-settings.json")?.unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self, state: &Path) -> Result<(), String> {
        self.validate()?;
        save(state, "access-settings.json", self)
    }
}

/// Missing preference preserves startup behavior for existing installations.
pub fn saved_enabled(state: &Path) -> Result<Option<bool>, String> {
    load(state, "access-enabled.json")
}

pub fn save_enabled(state: &Path, enabled: bool) -> Result<(), String> {
    if matches!(saved_enabled(state), Ok(Some(current)) if current == enabled) {
        return Ok(());
    }
    save(state, "access-enabled.json", &enabled)
}

fn load<T: serde::de::DeserializeOwned>(state: &Path, name: &str) -> Result<Option<T>, String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join(name))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Cannot read access settings".into()),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect access settings")?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 32768
    {
        return Err("Unsafe access settings file".into());
    }
    let mut bytes = Vec::new();
    file.take(32769)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read access settings")?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Invalid access settings".into())
}

fn save(state: &Path, name: &str, value: &impl Serialize) -> Result<(), String> {
    let path = state.join(name);
    let mut file = wispnet_registry::control::credential_temp(&path)
        .map_err(|_| "Cannot save access settings")?;
    serde_json::to_writer(&mut file, value).map_err(|_| "Cannot save access settings")?;
    file.flush()
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| "Cannot save access settings")?;
    file.persist(path)
        .map_err(|_| "Cannot save access settings")?;
    std::fs::File::open(state)
        .and_then(|f| f.sync_all())
        .map_err(|_| "Cannot sync access settings")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn shared_ports_accept_legacy_preferences_but_reject_unsafe_settings() {
        let host = wispnet_core::identity::SecretKey::generate().public();
        let mut settings = Settings::default();
        settings.service_ports.push(ServicePort {
            host,
            service: "llama".into(),
            port: settings.registry_port,
        });
        assert!(settings.validate().is_ok());
        settings.service_ports[0].port = 18401;
        settings.service_ports.push(ServicePort {
            host,
            service: "other".into(),
            port: 18401,
        });
        assert!(settings.validate().is_ok());
        settings.service_ports[1].service = "llama".into();
        assert!(settings.validate().is_err());
        settings.service_ports.pop();
        for origin in [
            "*",
            "null",
            "https://example.com/path",
            "https://example.com/",
            "https://example.com\r\nX-Test: value",
        ] {
            settings.allowed_origins = vec![origin.into()];
            assert!(settings.validate().is_err(), "{origin}");
        }
        settings.allowed_origins = vec![
            "https://example.com".into(),
            "http://localhost:18765".into(),
        ];
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        settings.save(state.path()).unwrap();
        assert_eq!(Settings::load(state.path()).unwrap(), settings);
        let path = state.path().join("access-settings.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Settings::load(state.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        let target = state.path().join("target.json");
        std::fs::write(&target, "{}").unwrap();
        symlink(target, path).unwrap();
        assert!(Settings::load(state.path()).is_err());
    }
}

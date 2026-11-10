use serde::Deserialize;
use std::io::Read;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

/// Configuration is intentionally not Debug: future credentials must not leak.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u16,
    // device_name is the legacy config spelling of the one readable device ID.
    #[serde(default = "machine_name", alias = "device_id")]
    pub device_name: String,
    pub state_dir: PathBuf,
    pub log_level: LogLevel,
    #[serde(default)]
    pub transport: Option<TransportConfig>,
    #[serde(default)]
    pub registry: Option<RegistryConfig>,
    #[serde(default)]
    pub controller: Option<ControllerConfig>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

/// Resolve an omitted device name from the OS, never from a display placeholder.
fn machine_name() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(name) = fs::read_to_string("/proc/sys/kernel/hostname") {
        return name.trim().to_owned();
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

/// Upgrade old device labels into bounded, URL-friendly readable IDs. Identity
/// keys and policy records are deliberately independent of this value.
pub fn readable_device_id(value: &str) -> String {
    if crate::protocol::valid_service_id(value) {
        return value.to_owned();
    }
    let mut id = String::new();
    for c in value.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || (!id.is_empty() && matches!(c, '.' | '_' | '-')) {
            id.push(c);
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
        if id.len() == 64 {
            break;
        }
    }
    let id = id.trim_end_matches(['.', '_', '-']);
    if id.is_empty() {
        "device".into()
    } else {
        id.into()
    }
}

/// Only a trusted local HTTPS reverse proxy may reach this listener.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    pub bind_addr: std::net::SocketAddr,
    #[serde(default = "default_mesh")]
    pub mesh: String,
}

fn default_mesh() -> String {
    "default".into()
}
/// Out-of-band controller origin and device credential, never a service route.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub origin: String,
    pub credential_file: PathBuf,
    /// Optional independently provisioned trust root for a private HTTPS controller.
    #[serde(default)]
    pub ca_certificate_file: Option<PathBuf>,
}

/// Parse an out-of-band controller origin without accepting embedded authority.
pub fn controller_origin(origin: &str) -> Result<url::Url, ConfigError> {
    let url =
        url::Url::parse(origin).map_err(|_| ConfigError("invalid controller HTTPS origin"))?;
    if origin.len() > 2048
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(ConfigError(
            "controller must be an HTTPS origin without credentials, path, query or fragment",
        ));
    }
    Ok(url)
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    /// Primary packet relay and invitation origin; no implicit public relay defaults.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// Additional packet relays and peer bootstrap candidates. No implicit public defaults.
    #[serde(default)]
    pub backup_relay_urls: Vec<String>,
    /// Prefer the primary packet relay, independently of standby-session retention.
    #[serde(default = "default_relay_policy")]
    pub prefer_primary_relay: bool,
    /// Retain packet and discovery sessions for every configured relay.
    #[serde(default = "default_relay_policy")]
    pub keep_backup_relays_warm: bool,
    /// Outside-NAT QUIC discovery fallback after failed/non-public primary discovery.
    #[serde(default)]
    pub discovery_relay_url: Option<String>,
    #[serde(default)]
    pub relay_only: bool,
    /// PEM trust root for a privately operated HTTPS relay.
    #[serde(default)]
    pub relay_ca_file: Option<PathBuf>,
    pub bind_addr: std::net::SocketAddr,
    #[serde(default = "default_connections")]
    pub max_connections: usize,
    #[serde(default = "default_streams")]
    pub max_streams: usize,
    #[serde(default = "default_per_connection")]
    pub max_streams_per_connection: usize,
}
fn default_relay_policy() -> bool {
    true
}
fn default_connections() -> usize {
    16
}
fn default_streams() -> usize {
    64
}
fn default_per_connection() -> usize {
    8
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub id: String,
    pub backend: std::net::SocketAddr,
    pub allow: Vec<crate::identity::EndpointId>,
}
impl TransportConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(relay) = &self.relay_url {
            controller_origin(relay).map_err(|_| {
                ConfigError(
                    "relay must be an HTTPS origin without credentials, path, query or fragment",
                )
            })?;
        }
        if self.backup_relay_urls.len() > 4 {
            return Err(ConfigError("at most four backup relays are supported"));
        }
        if !self.backup_relay_urls.is_empty() && self.relay_url.is_none() {
            return Err(ConfigError("backup relays require a primary relay URL"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for relay in self.relay_url.iter().chain(self.backup_relay_urls.iter()) {
            let origin =
                controller_origin(relay).map_err(|_| ConfigError("invalid relay HTTPS origin"))?;
            if !seen.insert(origin.to_string()) {
                return Err(ConfigError("relay URLs must be distinct"));
            }
        }
        if let Some(discovery) = &self.discovery_relay_url {
            let discovery = controller_origin(discovery)
                .map_err(|_| ConfigError("discovery relay must be an HTTPS origin without credentials, path, query or fragment"))?;
            let primary = self
                .relay_url
                .as_ref()
                .ok_or(ConfigError("discovery relay requires a primary relay URL"))?;
            if controller_origin(primary)? == discovery {
                return Err(ConfigError(
                    "discovery relay must differ from the primary relay",
                ));
            }
        }
        if (self.relay_only || self.relay_ca_file.is_some()) && self.relay_url.is_none() {
            return Err(ConfigError(
                "relay-only mode and relay CA require an explicit relay URL",
            ));
        }
        if self
            .relay_ca_file
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
        {
            return Err(ConfigError("relay CA file must be absolute"));
        }
        if !(1..=128).contains(&self.max_connections)
            || !(1..=256).contains(&self.max_streams)
            || !(1..=128).contains(&self.max_streams_per_connection)
        {
            return Err(ConfigError(
                "transport limits must be connections 1-128, streams 1-256, streams per connection 1-128",
            ));
        }
        Ok(())
    }
}
pub fn validate_services(services: &[ServiceConfig]) -> Result<(), ConfigError> {
    if services.len() > 128 {
        return Err(ConfigError("at most 128 services may be configured"));
    }
    let mut ids = std::collections::HashSet::new();
    for service in services {
        if matches!(service.id.as_str(), "shell" | "files")
            || !crate::protocol::valid_service_id(&service.id)
            || !ids.insert(&service.id)
        {
            return Err(ConfigError(
                "service IDs must be unique lowercase identifiers of 1-64 bytes; shell and files are reserved",
            ));
        }
        if !service.backend.ip().is_loopback() || service.backend.port() == 0 {
            return Err(ConfigError(
                "service backend must be an explicit loopback IP with a nonzero port",
            ));
        }
        if service.allow.len() > 128 {
            return Err(ConfigError(
                "at most 128 peer grants may be configured per service",
            ));
        }
        let unique: std::collections::HashSet<_> = service.allow.iter().collect();
        if unique.len() != service.allow.len() {
            return Err(ConfigError("duplicate peer grant"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// Never retain parser messages: they may include raw configuration or secrets.
#[derive(Debug)]
pub struct ConfigError(pub &'static str);
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.is_absolute() {
            return Err(ConfigError("--config must be an absolute path"));
        }
        let file = fs::File::open(path).map_err(|_| {
            ConfigError("cannot read configuration; check --config and file permissions")
        })?;
        let metadata = file
            .metadata()
            .map_err(|_| ConfigError("cannot inspect configuration"))?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(ConfigError(
                "configuration must be a regular file of at most 64 KiB",
            ));
        }
        let mut text = String::new();
        file.take(65537).read_to_string(&mut text).map_err(|_| {
            ConfigError("cannot read UTF-8 configuration; check file permissions and encoding")
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.len() > 65536 {
            return Err(ConfigError("configuration exceeds 64 KiB"));
        }
        let mut config: Self = toml::from_str(text).map_err(|_| ConfigError("invalid TOML or schema; expected base fields and optional transport/services (see config/node/example.toml)"))?;
        if !matches!(config.schema_version, 1 | 2) {
            return Err(ConfigError(
                "schema_version must be 1 (legacy) or 2 (independent host)",
            ));
        }
        if config.device_name.trim().is_empty()
            || config.device_name.len() > 128
            || config.device_name.chars().any(char::is_control)
        {
            return Err(ConfigError(
                "device_name must contain 1-128 bytes without control characters",
            ));
        }
        config.device_name = readable_device_id(&config.device_name);
        if !config.state_dir.is_absolute()
            || config
                .state_dir
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
            || config.state_dir.parent().is_none()
        {
            return Err(ConfigError(
                "state_dir must be an absolute non-root path without .. components",
            ));
        }
        if config.schema_version == 2
            && (config.transport.is_none()
                || config.controller.is_some()
                || config.registry.is_some()
                || !config.services.is_empty())
        {
            return Err(ConfigError(
                "independent hosts require transport and durable local authority; controller/registry/static services are not allowed; use config/node/host.toml with fresh host state",
            ));
        }
        if let Some(transport) = &config.transport {
            transport.validate()?;
        }
        if config.transport.is_none() && !config.services.is_empty() {
            return Err(ConfigError(
                "services require an explicit transport section",
            ));
        }
        if let Some(registry) = &config.registry
            && (!registry.bind_addr.ip().is_loopback()
                || registry.bind_addr.port() == 0
                || !crate::protocol::valid_service_id(&registry.mesh))
        {
            return Err(ConfigError(
                "registry must bind an explicit loopback IP and nonzero port",
            ));
        }
        if let Some(controller) = &config.controller
            && (controller_origin(&controller.origin).is_err()
                || controller.ca_certificate_file.as_ref().is_some_and(|p| {
                    !p.is_absolute()
                        || p.components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                })
                || !controller.credential_file.is_absolute()
                || controller
                    .credential_file
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir)))
        {
            return Err(ConfigError(
                "controller requires an HTTPS origin and absolute credential_file",
            ));
        }
        validate_services(&config.services)?;
        Ok(config)
    }

    /// Called only after all configuration has been validated.
    pub fn prepare_state(&self) -> Result<(), ConfigError> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.state_dir).map_err(|_| {
            ConfigError("cannot create state_dir; check its parent and permissions")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::symlink_metadata(&self.state_dir)
                .map_err(|_| ConfigError("cannot inspect state_dir"))?;
            if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
                return Err(ConfigError(
                    "state_dir must be a real private directory (chmod 700); do not use a shared directory",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readable_device_ids_upgrade_legacy_names_without_changing_state() {
        for (old, id) in [
            ("HiBreak", "hibreak"),
            ("Johnny’s phone", "johnny-s-phone"),
            ("j-strix", "j-strix"),
            ("device-1._-", "device-1._-"),
            ("🔥", "device"),
        ] {
            let config = Config::parse(&valid().replace("'laptop'", &format!("'{old}'"))).unwrap();
            assert_eq!(config.device_name, id);
            assert_eq!(config.state_dir, PathBuf::from("/tmp/wispnet-test"));
        }
        let config = Config::parse(&valid().replace("device_name", "device_id")).unwrap();
        assert_eq!(config.device_name, "laptop");
    }
    fn valid() -> String {
        "schema_version = 1
device_name = 'laptop'
state_dir = '/tmp/wispnet-test'
log_level = 'info'"
            .into()
    }
    #[test]
    fn omitted_device_name_uses_os_name_and_explicit_names_are_preserved() {
        let text = valid().replace("device_name = 'laptop'\n", "");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.device_name, readable_device_id(&machine_name()));
        assert!(!config.device_name.is_empty());
        assert_eq!(Config::parse(&valid()).unwrap().device_name, "laptop");
        assert!(Config::parse(&valid().replace("'laptop'", "''")).is_err());
    }

    #[test]
    fn outside_discovery_is_explicit_and_requires_a_distinct_primary() {
        let base = format!("{}\n[transport]\nbind_addr='0.0.0.0:0'\n", valid());
        let primary = "relay_url='https://relay.example'\n";
        let external = "discovery_relay_url='https://observer.example'\n";
        let legacy = Config::parse(&format!("{base}{primary}")).unwrap();
        assert!(legacy.transport.unwrap().discovery_relay_url.is_none());
        assert!(Config::parse(&format!("{base}{primary}{external}")).is_ok());
        assert!(Config::parse(&format!("{base}{external}")).is_err());
        for url in [
            "https://relay.example/",
            "http://observer.example",
            "https://user@observer.example",
            "https://observer.example/path",
            "https://observer.example/?token=secret",
        ] {
            assert!(
                Config::parse(&format!("{base}{primary}discovery_relay_url='{url}'\n")).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn rejects_bad_configuration() {
        assert!(Config::parse(&valid()).is_ok());
        for text in [
            valid().replace("= 1", "= 2"),
            valid().replace("/tmp/wispnet-test", "relative"),
            valid().replace("/tmp/wispnet-test", "/"),
            valid().replace("/tmp/wispnet-test", "/tmp/../secret"),
            valid().replace("laptop", ""),
            valid().replace("'info'", "'trace'"),
            format!(
                "{}
unknown = 'value'",
                valid()
            ),
        ] {
            assert!(Config::parse(&text).is_err());
        }
    }
    #[test]
    fn accepts_explicit_high_stream_capacity_without_changing_defaults() {
        let baseline = Config::parse(&format!(
            "{}\n[transport]\nbind_addr='127.0.0.1:0'\n",
            valid()
        ))
        .unwrap();
        assert_eq!(baseline.transport.unwrap().max_streams_per_connection, 8);
        for streams in [32, 33, 64, 128] {
            let text = format!(
                "{}\n[transport]\nbind_addr='127.0.0.1:0'\nmax_streams=256\nmax_streams_per_connection={streams}\n",
                valid()
            );
            let config = Config::parse(&text).unwrap();
            assert_eq!(
                config.transport.unwrap().max_streams_per_connection,
                streams
            );
        }
    }

    #[test]
    fn invalid_network_configuration_fails_before_startup() {
        let peer = crate::identity::SecretKey::generate().public().to_string();
        let transport = "\n[transport]\nbind_addr='127.0.0.1:0'\n";
        let service =
            format!("\n[[services]]\nid='website'\nbackend='127.0.0.1:8080'\nallow=['{peer}']\n");
        assert!(Config::parse(&format!("{}{transport}{service}", valid())).is_ok());
        for text in [
            format!("{}{service}", valid()),
            format!("{}{transport}max_connections=0\n", valid()),
            format!("{}{transport}max_streams=257\n", valid()),
            format!("{}{transport}max_streams_per_connection=0\n", valid()),
            format!("{}{transport}max_streams_per_connection=129\n", valid()),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace("127.0.0.1:8080", "192.168.1.1:8080")
            ),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace("127.0.0.1:8080", "127.0.0.1:0")
            ),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace("'website'", "'http://upstream'")
            ),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace(&peer, "SECRET-SENTINEL")
            ),
            format!("{}{transport}{service}{service}", valid()),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace("'website'", "'shell'")
            ),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace("'website'", "'files'")
            ),
            format!(
                "{}{transport}{}",
                valid(),
                service.replace(&format!("['{peer}']"), &format!("['{peer}','{peer}']"))
            ),
        ] {
            let error = Config::parse(&text)
                .err()
                .expect("invalid network configuration accepted");
            assert!(!error.to_string().contains("SECRET-SENTINEL"));
        }
    }

    #[test]
    fn registry_binds_and_controller_origins_fail_closed() {
        for address in ["0.0.0.0:7780", "[::]:7780", "192.0.2.1:7780", "127.0.0.1:0"] {
            assert!(
                Config::parse(&format!("{}\n[registry]\nbind_addr='{address}'", valid())).is_err()
            );
        }
        assert!(
            Config::parse(&format!(
                "{}\n[registry]\nbind_addr='127.0.0.1:7780'",
                valid()
            ))
            .is_ok()
        );
        for origin in [
            "http://controller.example",
            "https://user:SECRET-SENTINEL@controller.example",
            "https://controller.example/path",
            "https://controller.example/?token=SECRET-SENTINEL",
            "https://controller.example/#x",
            "https://controller.example:0",
        ] {
            let text = format!(
                "{}\n[controller]\norigin='{origin}'\ncredential_file='/private/device.token'",
                valid()
            );
            let error = Config::parse(&text).err().unwrap();
            assert!(!error.to_string().contains("SECRET-SENTINEL"));
        }
    }
    #[test]
    fn independent_host_configuration_requires_explicit_local_authority() {
        let base = valid().replace("schema_version = 1", "schema_version = 2");
        let transport = "\n[transport]\nbind_addr='127.0.0.1:0'\n";
        assert!(Config::parse(&format!("{base}{transport}")).is_ok());
        assert!(Config::parse(&base).is_err());
        for legacy in [
            "\n[controller]\norigin='https://controller.example'\ncredential_file='/private/token'",
            "\n[registry]\nbind_addr='127.0.0.1:7780'",
            "\n[[services]]\nid='legacy'\nbackend='127.0.0.1:8080'\nallow=[]",
        ] {
            assert!(Config::parse(&format!("{base}{transport}{legacy}")).is_err());
        }
    }
    #[test]
    fn relay_configuration_is_explicit_https_and_redacted() {
        let base = format!("{}\n[transport]\nbind_addr='127.0.0.1:0'\n", valid());
        for extra in [
            "relay_only=true",
            "relay_ca_file='/private/ca.pem'",
            "relay_url='http://relay.example'",
            "relay_url='https://user:SECRET-SENTINEL@relay.example'",
            "relay_url='https://relay.example/path'",
            "relay_url='https://relay.example/?SECRET-SENTINEL'",
            "relay_url='https://relay.example:0'",
            "relay_url='https://relay.example'\nrelay_ca_file='relative.pem'",
        ] {
            let error = Config::parse(&format!("{base}{extra}"))
                .err()
                .expect("invalid relay accepted");
            assert!(!error.to_string().contains("SECRET-SENTINEL"));
        }
        assert!(
            Config::parse(&format!(
                "{base}relay_url='https://relay.example'\nrelay_only=true"
            ))
            .is_ok()
        );
    }
    #[test]
    fn errors_never_echo_sensitive_input() {
        for field in [
            "private_key",
            "enrollment_token",
            "browser_token",
            "http_payload",
        ] {
            let text = format!(
                "{}
{field} = 'SECRET-SENTINEL'",
                valid()
            );
            let error = Config::parse(&text).err().unwrap();
            assert!(!format!("{error:?} {error}").contains("SECRET-SENTINEL"));
        }
        let error = Config::parse("device_name = 'SECRET-SENTINEL")
            .err()
            .unwrap();
        assert!(!error.to_string().contains("SECRET-SENTINEL"));
    }
}

#[cfg(test)]
mod backup_relay_tests {
    use super::*;

    #[test]
    fn relay_policy_defaults_preserve_existing_configs_and_accept_independent_choices() {
        let base = "schema_version=2\nstate_dir='/tmp/wispnet'\nlog_level='info'\n[transport]\nbind_addr='0.0.0.0:0'\nrelay_url='https://home.example'\nbackup_relay_urls=['https://outside.example']\n";
        let legacy = Config::parse(base).unwrap().transport.unwrap();
        assert!(legacy.prefer_primary_relay);
        assert!(legacy.keep_backup_relays_warm);
        for prefer in [false, true] {
            for warm in [false, true] {
                let config = Config::parse(&format!(
                    "{base}prefer_primary_relay={prefer}\nkeep_backup_relays_warm={warm}\n"
                ))
                .unwrap()
                .transport
                .unwrap();
                assert_eq!(config.prefer_primary_relay, prefer);
                assert_eq!(config.keep_backup_relays_warm, warm);
            }
        }
        for invalid in ["prefer_primary_relay='false'", "keep_backup_relays_warm=0"] {
            assert!(Config::parse(&format!("{base}{invalid}\n")).is_err());
        }
    }

    #[test]
    fn backup_relays_are_explicit_bounded_distinct_https_origins() {
        let base = "schema_version=2\nstate_dir='/tmp/wispnet'\nlog_level='info'\n[transport]\nbind_addr='0.0.0.0:0'\n";
        let primary = "relay_url='https://home.example'\n";
        let parse = |extras: &str| Config::parse(&format!("{base}{primary}{extras}"));
        assert!(
            parse("")
                .unwrap()
                .transport
                .unwrap()
                .backup_relay_urls
                .is_empty()
        );
        assert_eq!(
            parse("backup_relay_urls=['https://outside.example']")
                .unwrap()
                .transport
                .unwrap()
                .backup_relay_urls
                .len(),
            1
        );
        for extras in [
            "backup_relay_urls=['http://outside.example']",
            "backup_relay_urls=['https://home.example/']",
            "backup_relay_urls=['https://outside.example','https://outside.example/']",
            "backup_relay_urls=['https://user:password@outside.example']",
            "backup_relay_urls=['https://outside.example/path']",
            "backup_relay_urls=['https://a.example','https://b.example','https://c.example','https://d.example','https://e.example']",
        ] {
            assert!(parse(extras).is_err(), "{extras}");
        }
        assert!(
            Config::parse(&format!(
                "{base}backup_relay_urls=['https://outside.example']"
            ))
            .is_err()
        );
    }
}

//! Standalone packet relay: no application/node identity or persistent registry.
use iroh_relay::server::{
    Access, AccessControl, AcmeConfig, CertConfig, ClientRateLimit, ClientRequest, ConnectionId,
    QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig,
};
use rustls_pki_types::pem::PemObject;
use serde::Deserialize;
use std::{
    fs::OpenOptions,
    io::Read,
    net::SocketAddr,
    num::NonZeroU32,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    #[serde(default)]
    http_only: bool,
    certificate: Option<PathBuf>,
    private_key: Option<PathBuf>,
    acme_hostname: Option<String>,
    acme_cache: Option<PathBuf>,
    max_connections: usize,
    #[serde(default)]
    client_rx: ClientRxConfig,
    quic_listen: Option<SocketAddr>,
    quic_certificate: Option<PathBuf>,
    quic_private_key: Option<PathBuf>,
}

/// Ingress allowance for each relay client connection, in bytes rather than bits.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ClientRxConfig {
    enabled: bool,
    bytes_per_second: NonZeroU32,
    burst_bytes: NonZeroU32,
}

impl Default for ClientRxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bytes_per_second: NonZeroU32::new(1024 * 1024).expect("nonzero default rate"),
            burst_bytes: NonZeroU32::new(2 * 1024 * 1024).expect("nonzero default burst"),
        }
    }
}

impl ClientRxConfig {
    fn rate_limit(&self) -> Option<ClientRateLimit> {
        self.enabled.then(|| {
            let mut limit = ClientRateLimit::new(self.bytes_per_second);
            limit.max_burst_bytes = Some(self.burst_bytes);
            limit
        })
    }
}

fn read_protected(path: &Path, secret: bool) -> Result<Vec<u8>, &'static str> {
    if !path.is_absolute() {
        return Err("relay file paths must be absolute");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "relay file unavailable")?;
    let m = file
        .metadata()
        .map_err(|_| "relay file metadata unavailable")?;
    // SAFETY: geteuid takes no pointers and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !m.is_file()
        || m.nlink() != 1
        || (m.uid() != uid && m.uid() != 0)
        || m.mode() & if secret { 0o077 } else { 0o022 } != 0
        || m.len() > 65536
    {
        return Err("relay file must be protected, regular, singly linked and bounded");
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "relay file read failed")?;
    if bytes.len() > 65536 {
        return Err("relay file too large");
    }
    Ok(bytes)
}

#[derive(Debug)]
struct Admission {
    current: AtomicUsize,
    maximum: usize,
}
impl Admission {
    fn admit(&self) -> bool {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.maximum).then_some(n + 1)
            })
            .is_ok()
    }
}
impl AccessControl for Admission {
    async fn on_connect(&self, _request: &ClientRequest) -> Access {
        if self.admit() {
            Access::Allow
        } else {
            Access::Deny {
                reason: Some("relay capacity".into()),
            }
        }
    }
    fn on_disconnect(&self, _endpoint: iroh_base::EndpointId, _connection: ConnectionId) {
        self.current.fetch_sub(1, Ordering::AcqRel);
    }
}

fn manual_tls_config(
    certificate: &Path,
    private_key: &Path,
) -> Result<rustls::ServerConfig, &'static str> {
    let certificates = read_protected(certificate, false)?;
    let certificates = rustls_pki_types::CertificateDer::pem_slice_iter(&certificates)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid relay certificate")?;
    let key = Zeroizing::new(read_protected(private_key, true)?);
    let key = rustls_pki_types::PrivateKeyDer::from_pem_slice(&key)
        .map_err(|_| "invalid relay private key")?;
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| "relay TLS configuration failed")?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|_| "relay certificate/key mismatch")
}

fn quic_config(config: &Config) -> Result<Option<QuicConfig>, &'static str> {
    let Some(address) = config.quic_listen else {
        if config.quic_certificate.is_some() || config.quic_private_key.is_some() {
            return Err("QUIC certificate requires quic_listen");
        }
        return Ok(None);
    };
    if address.port() == 0 {
        return Err("QUIC discovery requires a fixed nonzero UDP port");
    }
    let mut quic = QuicConfig::new(address);
    match (&config.quic_certificate, &config.quic_private_key) {
        (Some(certificate), Some(key)) => {
            quic.server_config = Some(manual_tls_config(certificate, key)?);
        }
        (None, None) if !config.http_only => {}
        _ => return Err("HTTP-only relay requires a complete QUIC certificate/key pair"),
    }
    Ok(Some(quic))
}

fn certificate_config(config: &Config) -> Result<CertConfig, &'static str> {
    let tls_builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| "relay TLS configuration failed")?
    .with_no_client_auth();
    let result = match (
        &config.certificate,
        &config.private_key,
        &config.acme_hostname,
        &config.acme_cache,
    ) {
        (Some(certificate), Some(private_key), None, None) => CertConfig::Manual {
            server_config: manual_tls_config(certificate, private_key)?,
        },

        (None, None, Some(hostname), Some(cache)) => {
            if !hostname.contains('.')
                || hostname.len() > 253
                || !hostname
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
                || !cache.is_absolute()
            {
                return Err("invalid ACME hostname/cache");
            }
            let meta = std::fs::symlink_metadata(cache).map_err(|_| "ACME cache unavailable")?;
            // SAFETY: geteuid has no preconditions.
            if !meta.is_dir()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o077 != 0
            {
                return Err("unsafe ACME cache");
            }
            CertConfig::LetsEncrypt {
                acme_config: AcmeConfig::letsencrypt(true)
                    .domains(vec![hostname.clone()])
                    .cache_path(cache.clone()),
                server_config_builder: tls_builder,
            }
        }
        _ => return Err("choose exactly one complete manual or ACME TLS configuration"),
    };
    Ok(result)
}

fn server_config(config: &Config) -> Result<ServerConfig, &'static str> {
    if config.listen.port() == 0 || !(1..=1024).contains(&config.max_connections) {
        return Err("invalid relay listener or connection limit");
    }
    if config.http_only
        && (!config.listen.ip().is_loopback()
            || config.certificate.is_some()
            || config.private_key.is_some()
            || config.acme_hostname.is_some()
            || config.acme_cache.is_some())
    {
        return Err(
            "HTTP-only packet relay must be loopback-only with TLS configured at its front end",
        );
    }
    let certificate_config = if config.http_only {
        None
    } else {
        Some(certificate_config(config)?)
    };
    // Auxiliary HTTP stays loopback-only. UDP discovery is explicitly opt-in;
    // metrics remain disabled.
    let mut relay = RelayConfig::new(if config.http_only {
        config.listen
    } else {
        "127.0.0.1:0"
            .parse::<SocketAddr>()
            .expect("constant address")
    });
    if let Some(certificate_config) = certificate_config {
        relay.tls = Some(TlsConfig::new(config.listen, certificate_config));
    }
    relay.access = Arc::new(Admission {
        current: AtomicUsize::new(0),
        maximum: config.max_connections,
    });
    relay.limits.client_rx = config.client_rx.rate_limit();
    let mut server_config = ServerConfig::default();
    server_config.relay = Some(relay);
    server_config.quic = quic_config(config)?;
    Ok(server_config)
}

async fn serve(path: &Path) -> Result<(), &'static str> {
    let bytes = Zeroizing::new(read_protected(path, true)?);
    let text = std::str::from_utf8(&bytes).map_err(|_| "invalid relay configuration")?;
    let config: Config = toml::from_str(text).map_err(|_| "invalid relay configuration")?;
    let server = Server::spawn(server_config(&config)?)
        .await
        .map_err(|_| "relay listener startup failed")?;
    println!(
        "{{\"event\":\"started\",\"role\":\"packet_relay\",\"listen\":\"{}\"}}",
        config.listen
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "relay signal setup failed")?;
    tokio::select! { _=term.recv()=>{}, _=tokio::signal::ctrl_c()=>{} }
    server
        .shutdown()
        .await
        .map_err(|_| "relay shutdown failed")?;
    println!("{{\"event\":\"stopped\",\"role\":\"packet_relay\"}}");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = if args.len() == 2 && args[0] == "--config" {
        serve(Path::new(&args[1])).await
    } else {
        Err("usage: wispnet-relay --config /absolute/relay.toml")
    };
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_bounded_and_capacity_can_be_reused() {
        let gate = Admission {
            current: AtomicUsize::new(0),
            maximum: 2,
        };
        assert!(gate.admit());
        assert!(gate.admit());
        assert!(!gate.admit());
        gate.current.fetch_sub(1, Ordering::AcqRel);
        assert!(gate.admit());
        assert!(!gate.admit());
    }
    #[test]
    fn private_key_file_rejects_shared_permissions_links_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key.pem");
        std::fs::write(&key, b"not logged key material").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_protected(&key, true).is_ok());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_protected(&key, true).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = root.path().join("alias");
        symlink(&key, &alias).unwrap();
        assert!(read_protected(&alias, true).is_err());
        let hard = root.path().join("hard");
        std::fs::hard_link(&key, &hard).unwrap();
        assert!(read_protected(&key, true).is_err());
    }
    #[test]
    fn quic_discovery_is_opt_in_and_http_mode_requires_its_own_tls() {
        let mut config: Config =
            toml::from_str("listen='127.0.0.1:18443'\nhttp_only=true\nmax_connections=2").unwrap();
        assert!(quic_config(&config).unwrap().is_none());
        config.quic_listen = Some("0.0.0.0:7842".parse().unwrap());
        assert!(quic_config(&config).is_err());
        config.http_only = false;
        assert!(quic_config(&config).unwrap().is_some());
        config.quic_listen = Some("0.0.0.0:0".parse().unwrap());
        assert!(quic_config(&config).is_err());
        config.quic_listen = None;
        config.quic_certificate = Some("/not-read.pem".into());
        assert!(quic_config(&config).is_err());
    }
    #[test]
    fn tls_mode_never_silently_falls_back_or_accepts_mixed_settings() {
        let mut config = Config {
            http_only: false,
            listen: "127.0.0.1:8443".parse().unwrap(),
            max_connections: 2,
            client_rx: ClientRxConfig::default(),
            quic_listen: None,
            quic_certificate: None,
            quic_private_key: None,
            certificate: None,
            private_key: None,
            acme_hostname: None,
            acme_cache: None,
        };
        assert_eq!(
            certificate_config(&config).unwrap_err(),
            "choose exactly one complete manual or ACME TLS configuration"
        );
        config.certificate = Some("/not-read.pem".into());
        assert_eq!(
            certificate_config(&config).unwrap_err(),
            "choose exactly one complete manual or ACME TLS configuration"
        );
        config.private_key = Some("/not-read.key".into());
        config.acme_hostname = Some("relay.example.test".into());
        config.acme_cache = Some("/not-read".into());
        assert_eq!(
            certificate_config(&config).unwrap_err(),
            "choose exactly one complete manual or ACME TLS configuration"
        );
        config.certificate = None;
        config.private_key = None;
        config.acme_cache = Some("relative/cache".into());
        assert_eq!(
            certificate_config(&config).unwrap_err(),
            "invalid ACME hostname/cache"
        );
    }

    #[test]
    fn client_ingress_defaults_and_overrides_reach_the_relay_server() {
        let base = "listen='127.0.0.1:18443'\nhttp_only=true\nmax_connections=2\n";
        let config: Config = toml::from_str(base).unwrap();
        let relay = server_config(&config).unwrap().relay.unwrap();
        let rate = relay.limits.client_rx.unwrap();
        assert_eq!(rate.bytes_per_second.get(), 1024 * 1024);
        assert_eq!(rate.max_burst_bytes.unwrap().get(), 2 * 1024 * 1024);

        let config: Config = toml::from_str(&format!(
            "{base}[client_rx]\nbytes_per_second=33554432\nburst_bytes=67108864\n"
        ))
        .unwrap();
        let rate = server_config(&config)
            .unwrap()
            .relay
            .unwrap()
            .limits
            .client_rx
            .unwrap();
        assert_eq!(rate.bytes_per_second.get(), 32 * 1024 * 1024);
        assert_eq!(rate.max_burst_bytes.unwrap().get(), 64 * 1024 * 1024);

        let config: Config =
            toml::from_str(&format!("{base}[client_rx]\nenabled=false\n")).unwrap();
        assert!(
            server_config(&config)
                .unwrap()
                .relay
                .unwrap()
                .limits
                .client_rx
                .is_none()
        );
    }

    #[test]
    fn client_ingress_rejects_invalid_values_and_misspelled_options() {
        let base = "listen='127.0.0.1:18443'\nhttp_only=true\nmax_connections=2\n[client_rx]\n";
        for option in [
            "bytes_per_second=0",
            "burst_bytes=0",
            "bytes_per_second=-1",
            "burst_bytes=-1",
            "bytes_per_second=4294967296",
            "burst_bytes=4294967296",
            "bytes_per_second=1.5",
            "enabled='false'",
            "bytes_per_secon=1048576",
            "enabled=false\nburst_bytes=0",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("{base}{option}\n")).is_err(),
                "accepted invalid ingress option: {option}"
            );
        }
    }
}

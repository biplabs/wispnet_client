//! Disposable loopback endpoint/relay fixture, built separately against each version.
use std::{error::Error, io::Write, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use iroh::{Endpoint, EndpointAddr, RelayMode, endpoint::presets};
use iroh_relay::{
    server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig},
    tls::CaTlsConfig,
};
use rustls_pki_types::{CertificateDer, pem::PemObject};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const ALPN: &[u8] = b"wispnet-upgrade-compat/1";

fn report(value: serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}

async fn relay(ca_file: &Path) -> Result<()> {
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])?;
    std::fs::write(ca_file, certificate.cert.pem())?;
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.cert.der().clone()],
        rustls_pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
    )?;
    let mut relay = RelayConfig::new("127.0.0.1:0".parse::<SocketAddr>()?);
    relay.tls = Some(TlsConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        CertConfig::Manual { server_config: tls },
    ));
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    let server = Server::spawn(config).await?;
    report(serde_json::json!({"relay_url": format!("https://{}", server.https_addr().unwrap())}));
    std::future::pending::<()>().await;
    Ok(())
}

async fn endpoint(mode: &str, relay_url: &str, ca_file: &str) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .alpns(vec![ALPN.to_vec()]);
    if mode == "direct" {
        builder = builder
            .relay_mode(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")?;
    } else {
        let cert = CertificateDer::from_pem_file(ca_file)?;
        builder = builder
            .relay_mode(RelayMode::custom([relay_url.parse()?]))
            .ca_tls_config(CaTlsConfig::custom_roots([cert]));
    }
    let endpoint = builder.bind().await?;
    if mode == "relay" {
        endpoint.online().await;
    }
    Ok(endpoint)
}

async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("relay") => relay(Path::new(&args[1])).await,
        Some("serve") => {
            let endpoint = endpoint(&args[1], &args[2], &args[3]).await?;
            report(serde_json::json!({
                "id": endpoint.id().to_string(),
                "address": endpoint.bound_sockets().first().map(ToString::to_string),
            }));
            let connection = endpoint.accept().await.ok_or("missing incoming")?.await?;
            for marker in 1..=2u8 {
                let (mut send, mut recv) = connection.accept_bi().await?;
                let bytes = recv.read_to_end(131_072).await?;
                assert_eq!(bytes, vec![marker; 65_537]);
                send.write_all(&bytes).await?;
                send.finish()?;
                let _ = send.stopped().await;
            }
            report(serde_json::json!({"remote_id": connection.remote_id().to_string()}));
            connection.closed().await;
            endpoint.close().await;
            Ok(())
        }
        Some("connect") => {
            let endpoint = endpoint(&args[1], &args[4], &args[5]).await?;
            let mut target = EndpointAddr::new(args[2].parse()?);
            target = if args[1] == "direct" {
                target.with_ip_addr(args[3].parse()?)
            } else {
                target.with_relay_url(args[4].parse()?)
            };
            let connection = endpoint.connect(target, ALPN).await?;
            assert_eq!(connection.remote_id().to_string(), args[2]);
            for marker in 1..=2u8 {
                let (mut send, mut recv) = connection.open_bi().await?;
                send.write_all(&vec![marker; 65_537]).await?;
                send.finish()?;
                assert_eq!(recv.read_to_end(131_072).await?, vec![marker; 65_537]);
            }
            let relay_path = connection
                .paths()
                .iter()
                .any(|p| p.is_selected() && p.is_relay());
            assert_eq!(relay_path, args[1] == "relay");
            report(serde_json::json!({
                "client_id": endpoint.id().to_string(),
                "server_id": connection.remote_id().to_string(),
                "relay_path": relay_path,
                "streams": 2,
            }));
            connection.close(0u32.into(), b"compat complete");
            endpoint.close().await;
            Ok(())
        }
        _ => Err("expected relay, serve, or connect".into()),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    tokio::time::timeout(Duration::from_secs(40), run(&args)).await??;
    Ok(())
}

//! Agent identity and authenticated service-host lifecycle.
use std::future::Future;
use wispnet_core::config::Config;
pub mod catalogs;
pub mod control;
pub mod diagnostics;
pub mod handshake;
pub mod identity;
pub mod pairing;
pub mod peers;
pub mod tunnel;

/// Own and drain component tasks here as features are added. No detached tasks.
pub async fn run(
    config: &Config,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    use wispnet_core::identity::IdentityStore;
    let store = identity::FileIdentityStore::new(&config.state_dir)?;
    let _lease = store.lease()?;
    let identity = store.load()?;
    if config.schema_version == 2 {
        let authority =
            wispnet_registry::host::Authority::open(&config.state_dir, identity.endpoint_id())?
                .with_device_id(&config.device_name);
        let host = tunnel::TunnelHost::bind_host(
            identity.secret_key(),
            config.transport.as_ref().ok_or("host transport required")?,
            authority,
        )
        .await?;
        tracing::info!(component="agent",event="started",networking=true,mode="independent_host",endpoint_id=%host.endpoint().id(),bind_addr=%host.endpoint().bound_sockets().first().map(ToString::to_string).unwrap_or_else(|| "relay-only".into()));
        shutdown.await;
        host.shutdown().await;
        tracing::info!(component = "agent", event = "stopped");
        return Ok(());
    }
    if let Some(controller) = &config.controller {
        let secret = wispnet_registry::control::read_token(&controller.credential_file)?;
        let client = wispnet_registry::client::Client::configured(controller, secret)?;
        let directory = client.directory(Some(identity.endpoint_id())).await?;
        tracing::info!(
            component = "agent",
            event = "registry_bootstrap",
            revision = directory.revision
        );
    }
    let host = if let Some(transport) = &config.transport {
        Some(tunnel::TunnelHost::bind(identity.secret_key(), transport, &config.services).await?)
    } else {
        None
    };
    if let Some(host) = &host {
        tracing::info!(component = "agent", event = "started", networking = true,
            endpoint_id = %host.endpoint().id(), bind_addr = %host.endpoint().bound_sockets().first().map(ToString::to_string).unwrap_or_else(|| "relay-only".into()));
    } else {
        tracing::info!(component = "agent", event = "started", networking = false);
    }
    shutdown.await;
    if let Some(host) = host {
        host.shutdown().await;
    }
    tracing::info!(component = "agent", event = "stopped");
    Ok(())
}

#[cfg(any(target_os = "android", test))]
mod os_dns;

// Exercise the exact vendored implementation in the workspace's required tests.
#[cfg(all(test, target_os = "linux"))]
#[path = "../../../vendor/iroh/src/runtime/linux.rs"]
mod quic_timer_tests;

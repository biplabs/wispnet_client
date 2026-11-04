use std::{env, path::PathBuf, process::ExitCode};
mod desktop;
mod files;
mod firewall;
mod install;
mod manage;
mod owner;
mod retry;
mod shell;
mod tui;
use tracing_subscriber::prelude::*;
use wispnet_agent::{diagnostics, identity::FileIdentityStore, tunnel};
use wispnet_core::{
    config::Config,
    identity::{EndpointId, IdentityStore},
};

const USAGE: &str = "Wispnet CLI
Interactive management: wispnet [--config /absolute/path/host.toml]
Pairing code: wispnet --pairing-code [--config /absolute/path/host.toml]
QR pairing: wispnet --qrcode [--config /absolute/path/host.toml]
Usage: wispnet <check|identity-init|status|agent|registry|registry-init|registry-backup|registry-directory> --config /absolute/path/config.toml
Owner automation: wispnet node-request [--config CONFIG] [--response] [--request FILE | --batch PLAN | --retry ID | --operations]
Desktop adapter: wispnet desktop-bridge [--config CONFIG]
Install: wispnet app-install --root BUNDLE [--headless] [--no-start] [--init]
Uninstall: wispnet app-uninstall
Firewall: wispnet firewall-install --uid UID | firewall-uninstall --uid UID
Backup: wispnet registry-backup --config /absolute/path/config.toml --output /absolute/path/new-backup.sqlite3
Host: wispnet <host-init|host-status> --config /absolute/path/host.toml
Host invitations: wispnet host-invitations --config /absolute/path/host.toml
Host change: wispnet host-mutate --config /absolute/path/host.toml --request /absolute/path/request.json
Catalog: wispnet host-catalog --config /absolute/path/client.toml --peer PUBLIC_ID [--address IP:UDP_PORT]
Pairing bootstrap: wispnet peer-hello --config /absolute/path/client.toml --peer PINNED_HOST_ID [--address IP:UDP_PORT]
Probe: wispnet <peer-probe|peer-watch> --config /absolute/path/client.toml --peer PUBLIC_ID [--address IP:UDP_PORT]
Reset: wispnet identity-reset --config /absolute/path/config.toml --expect-id PUBLIC_ID
Tunnel: wispnet tunnel --config /absolute/path/config.toml --peer PUBLIC_ID [--address IP:UDP_PORT] --service SERVICE_ID
Tunnel mode relays raw stdin/stdout. It is not a browser gateway. Independent hosts use local invitation approval. See docs/pairing-bootstrap.md and docs/per-host-control.md.";

fn main() -> ExitCode {
    let shell_args: Vec<_> = env::args_os().skip(1).collect();
    let native = match shell_args.first().and_then(|arg| arg.to_str()) {
        Some("desktop-bridge") => Some(desktop::run_args(&shell_args[1..])),
        Some("node-request") => Some(owner::run_args(&shell_args[1..])),
        Some("firewall-helper") => Some(firewall::run_args(&shell_args[1..])),
        Some("firewall-install") => Some(firewall::install_args(&shell_args[1..])),
        Some("firewall-uninstall") => Some(firewall::uninstall_args(&shell_args[1..])),
        Some("app-install") => Some(install::run_args(&shell_args[1..])),
        Some("app-uninstall") => Some(install::uninstall_args(&shell_args[1..])),
        Some("package-setup") => Some(install::package_args(&shell_args[1..])),
        _ => None,
    };
    if let Some(outcome) = native {
        return match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("wispnet: {error}");
                ExitCode::FAILURE
            }
        };
    }
    #[cfg(feature = "ssh-adapter")]
    if shell_args.first().is_some_and(|a| a == "ssh-bridge") {
        return match wispnet_ssh_adapter::run_args(&shell_args[1..]) {
            Ok(code) => ExitCode::from(code.min(255) as u8),
            Err(error) => {
                eprintln!("wispnet: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if shell_args.first().is_some_and(|a| a == "files") {
        return match files::run_args(&shell_args[1..]) {
            Ok(code) => ExitCode::from(code.min(255) as u8),
            Err(error) => {
                println!("{}", files::error_result(&error));
                eprintln!("wispnet: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if shell_args.first().is_some_and(|a| a == "shell") {
        return match shell::run_args(&shell_args[1..]) {
            Ok(code) => ExitCode::from(code.min(255) as u8),
            Err(error) => {
                eprintln!("wispnet: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("wispnet: cannot start runtime");
            return ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(execute());
    // Tokio stdio uses blocking workers; interrupted stdin must not keep this CLI
    // alive indefinitely. Network tasks are explicitly closed before this point.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wispnet: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("wispnet {}", env!("WISPNET_VERSION"));
        return Ok(());
    }
    if args.is_empty() || (args.len() == 2 && args[0] == "--config") {
        let path = if args.is_empty() {
            PathBuf::from(env::var_os("HOME").ok_or("HOME is required; pass --config")?)
                .join(".config/wispnet-node/host.toml")
        } else {
            PathBuf::from(&args[1])
        };
        return tui::run(path);
    }
    if args
        .first()
        .is_some_and(|a| a == "--qrcode" || a == "--pairing-code")
        && args.len() == 1
    {
        let home = env::var_os("HOME").ok_or("HOME is required; pass --config explicitly")?;
        args.extend([
            "--config".into(),
            PathBuf::from(home)
                .join(".config/wispnet-node/host.toml")
                .into_os_string(),
        ]);
    }
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "{USAGE}\n\n{}\n\n{}\n\n{}",
            manage::HELP,
            shell::HELP,
            files::HELP
        );
        #[cfg(feature = "ssh-adapter")]
        println!("\n{}", wispnet_ssh_adapter::HELP);
        return Ok(());
    }
    if args
        .first()
        .and_then(|a| a.to_str())
        .is_some_and(manage::handles)
    {
        let value = manage::run(&args)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let host_invite = args.first().is_some_and(|a| a == "host-invite");
    let host_mutate = args.first().is_some_and(|a| a == "host-mutate");
    let peer_read = args.first().is_some_and(|a| {
        matches!(
            a.to_str(),
            Some("host-catalog" | "peer-hello" | "peer-probe" | "peer-watch")
        )
    });
    let pairing_check = args
        .first()
        .is_some_and(|a| a == "pairing-check" || a == "pairing-request");
    let enroll = args.first().is_some_and(|a| a == "enroll");
    let backup = args.first().is_some_and(|a| a == "registry-backup");
    let reset = args.first().is_some_and(|a| a == "identity-reset");
    let tunnel_mode = args.first().is_some_and(|a| a == "tunnel");
    let invalid_arguments = if host_invite {
        !matches!(args.len(), 5 | 7)
            || args[1] != "--config"
            || args[3] != "--output"
            || (args.len() == 7 && (args[5] != "--format" || args[6] != "code"))
    } else if host_mutate {
        args.len() != 5 || args[1] != "--config" || args[3] != "--request"
    } else if peer_read {
        !matches!(args.len(), 5 | 7)
            || args[1] != "--config"
            || args[3] != "--peer"
            || (args.len() == 7 && args[5] != "--address")
    } else if tunnel_mode {
        !matches!(args.len(), 7 | 9)
            || args[1] != "--config"
            || args[3] != "--peer"
            || (args.len() == 9 && args[5] != "--address")
            || args[args.len() - 2] != "--service"
    } else if pairing_check {
        args.len() != 7 || args[1] != "--config" || args[3] != "--peer" || args[5] != "--invitation"
    } else if enroll {
        args.len() != 5 || args[1] != "--config" || args[3] != "--invitation"
    } else if backup {
        args.len() != 5 || args[1] != "--config" || args[3] != "--output"
    } else if reset {
        args.len() != 5 || args[1] != "--config" || args[3] != "--expect-id"
    } else {
        args.len() != 3 || args[1] != "--config"
    };
    if invalid_arguments {
        return Err(USAGE.into());
    }
    let mode = args[0].to_str().ok_or(USAGE)?;
    if !matches!(
        mode,
        "check"
            | "identity-init"
            | "identity-reset"
            | "status"
            | "agent"
            | "registry"
            | "registry-init"
            | "registry-backup"
            | "registry-directory"
            | "enroll"
            | "host-init"
            | "--qrcode"
            | "--pairing-code"
            | "host-invite"
            | "host-invitations"
            | "host-status"
            | "host-mutate"
            | "host-catalog"
            | "pairing-request"
            | "pairing-check"
            | "peer-hello"
            | "peer-probe"
            | "peer-watch"
            | "tunnel"
    ) {
        return Err(USAGE.into());
    }
    let target = if tunnel_mode {
        let peer: EndpointId = args[4]
            .to_str()
            .ok_or("invalid public endpoint ID")?
            .parse()
            .map_err(|_| "invalid public endpoint ID")?;
        let address = if args.len() == 9 {
            Some(
                args[6]
                    .to_str()
                    .ok_or("invalid peer socket address")?
                    .parse::<std::net::SocketAddr>()
                    .map_err(|_| "invalid peer socket address")?,
            )
        } else {
            None
        };
        let service = args[args.len() - 1]
            .to_str()
            .ok_or("invalid service ID")?
            .to_owned();
        wispnet_core::protocol::request(&service).map_err(tunnel::TunnelError::Status)?;
        Some((peer, address, service))
    } else {
        None
    };
    let backup_path = if backup {
        let path = PathBuf::from(&args[4]);
        if !path.is_absolute()
            || path.parent().is_none()
            || path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Err("backup path must be absolute without .. components".into());
        }
        Some(path)
    } else {
        None
    };
    let config = Config::load(&PathBuf::from(&args[2]))?;
    if mode == "--pairing-code" {
        let value = wispnet_node::invitation::issue(&config)?;
        println!("{}", value["code"].as_str().ok_or("Missing pairing code")?);
        return Ok(());
    }
    if mode == "--qrcode" {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return Err("QR pairing requires a terminal; use ssh -t for remote pairing".into());
        }
        let value = wispnet_node::invitation::issue(&config)?;
        let matrix: Vec<Vec<bool>> = serde_json::from_value(value["matrix"].clone())?;
        if wispnet_node::invitation::terminal_columns().is_some_and(|cols| cols < matrix.len()) {
            return Err(format!("Widen your terminal to at least {} columns (or reduce its font size), then run wispnet --qrcode again", matrix.len()).into());
        }
        println!(
            "Scan in Wispnet → Scan pairing QR. Expires in five minutes.\nHost: {}",
            value["host"].as_str().unwrap_or("")
        );
        print!("{}", wispnet_node::invitation::terminal(&matrix));
        println!(
            "Approve the incoming request on this host, then grant services. Keep this QR private."
        );
        return Ok(());
    }
    if config.schema_version == 2
        && matches!(
            mode,
            "registry" | "registry-init" | "registry-directory" | "enroll"
        )
    {
        return Err("legacy controller commands are not available in independent host mode; use host-invite and pairing-request".into());
    }
    if matches!(
        mode,
        "host-init" | "host-status" | "host-mutate" | "host-invite" | "host-invitations"
    ) {
        if config.schema_version != 2 {
            return Err("host commands require schema_version=2; use config/node/host.toml".into());
        }
        if mode == "host-init" {
            config.prepare_state()?;
        }
        let identity_store = FileIdentityStore::new(&config.state_dir)?;
        let _lease = if mode == "host-init" {
            Some(identity_store.lease()?)
        } else {
            None
        };
        let initialized = config
            .state_dir
            .join("host-owner.token")
            .try_exists()
            .map_err(|_| "cannot inspect host credential")?
            || config
                .state_dir
                .join("host.sqlite3")
                .try_exists()
                .map_err(|_| "cannot inspect host authority")?;
        let identity = if mode == "host-init" && !initialized {
            identity_store.initialize()?
        } else {
            identity_store.load()?
        };
        if mode == "host-init" {
            wispnet_registry::host::initialize(&config.state_dir, identity.endpoint_id())?;
            println!(
                "Independent host initialized\nEndpoint ID: {}",
                identity.endpoint_id()
            );
            return Ok(());
        }
        let (mut store, secret) =
            wispnet_registry::host::owner_store(&config.state_dir, identity.endpoint_id())?;
        if mode == "host-invite" {
            use std::io::Write;
            let path = PathBuf::from(&args[4]);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
                || path
                    .try_exists()
                    .map_err(|_| "cannot inspect invitation output")?
            {
                return Err("invitation output must be a new absolute private file".into());
            }
            let mut file = wispnet_registry::control::credential_temp(&path)?;
            let relay = config
                .transport
                .as_ref()
                .and_then(|t| t.relay_url.as_deref())
                .ok_or("invitation requires configured relay")?;
            let mut invitation = store.pairing_invite(
                identity.endpoint_id(),
                &secret,
                relay,
                wispnet_registry::enrollment::now(),
                300,
            )?;
            invitation.host_name = Some(config.device_name.clone());
            let bytes = zeroize::Zeroizing::new(if args.len() == 7 {
                wispnet_agent::pairing::encode_invitation(&invitation)?
                    .as_bytes()
                    .to_vec()
            } else {
                serde_json::to_vec(&invitation)?
            });
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            file.persist_noclobber(&path)
                .map_err(|_| "cannot save private invitation")?;
            std::fs::File::open(path.parent().ok_or("invalid invitation output")?)?.sync_all()?;
            println!(
                "{}",
                serde_json::json!({"host":invitation.host,"invitation":invitation.invitation,
                "state":"issued","expires_at":invitation.expires_at})
            );
        }
        if mode == "host-invitations" {
            println!(
                "{}",
                serde_json::to_string_pretty(&store.pairing_list(
                    identity.endpoint_id(),
                    &secret,
                    wispnet_registry::enrollment::now()
                )?)?
            );
        }
        if mode == "host-status" {
            println!(
                "{}",
                serde_json::to_string_pretty(&store.host_snapshot(identity.endpoint_id())?)?
            );
        }
        if mode == "host-mutate" {
            use std::io::Read;
            let file = std::fs::File::open(PathBuf::from(&args[4]))
                .map_err(|_| "cannot read owner request")?;
            if !file.metadata()?.is_file() {
                return Err("owner request must be a regular file".into());
            }
            let mut bytes = Vec::new();
            file.take(32769)
                .read_to_end(&mut bytes)
                .map_err(|_| "cannot read owner request")?;
            if bytes.len() > 32768 {
                return Err("owner request exceeds 32 KiB".into());
            }
            let operation = serde_json::from_slice(&bytes).map_err(|_| "invalid owner request")?;
            let revision = store.host_mutate(identity.endpoint_id(), &secret, operation)?;
            println!("Host revision: {revision}");
        }
        return Ok(());
    }
    if matches!(mode, "pairing-check" | "pairing-request") {
        let expected: EndpointId = args[4]
            .to_str()
            .ok_or("invalid peer")?
            .parse()
            .map_err(|_| "invalid peer")?;
        let invitation = wispnet_agent::pairing::read_invitation(&PathBuf::from(&args[6]))?;
        let now = wispnet_registry::enrollment::now();
        let transport = config
            .transport
            .as_ref()
            .ok_or("invitation requires transport configuration")?;
        let target =
            wispnet_agent::pairing::invitation_target(&invitation, expected, transport, now)?;
        let store = FileIdentityStore::new(&config.state_dir)?;
        let _lease = store.lease()?;
        let identity = store.load()?;
        let _saved = wispnet_agent::peers::load(&config.state_dir, expected)?;
        let bootstrap = invitation
            .bootstrap(expected, now)
            .ok_or("invalid host bootstrap")?;
        let endpoint =
            tunnel::bind_configured_client(identity.secret_key(), Some(transport)).await?;
        if mode == "pairing-request" {
            let result = wispnet_agent::pairing::request_enrollment(
                &endpoint,
                expected,
                &invitation,
                transport,
                identity.secret_key(),
                &config.device_name,
            )
            .await;
            endpoint.close().await;
            let pending = result?;
            wispnet_agent::peers::save(&config.state_dir, &bootstrap)?;
            println!(
                "{}",
                serde_json::json!({"host":pending.host,"peer":pending.peer,
                "invitation":pending.invitation,"state":"pending_owner_approval"})
            );
            return Ok(());
        }
        let result = wispnet_agent::pairing::hello(&endpoint, target).await;
        endpoint.close().await;
        let hello = result?;
        println!(
            "{}",
            serde_json::json!({"host": hello.host, "version": hello.version,
            "state": "bootstrap_verified", "approval_checked": false, "capability_checked": false})
        );
        return Ok(());
    }
    if matches!(
        mode,
        "host-catalog" | "peer-hello" | "peer-probe" | "peer-watch"
    ) {
        let peer: EndpointId = args[4]
            .to_str()
            .ok_or("invalid peer")?
            .parse()
            .map_err(|_| "invalid peer")?;
        let address = if args.len() == 7 {
            Some(
                args[6]
                    .to_str()
                    .ok_or("invalid address")?
                    .parse::<std::net::SocketAddr>()
                    .map_err(|_| "invalid address")?,
            )
        } else {
            None
        };
        let store = FileIdentityStore::new(&config.state_dir)?;
        let _lease = store.lease()?;
        let identity = store.load()?;
        let saved = wispnet_agent::peers::load(&config.state_dir, peer)?;
        let transport = wispnet_agent::peers::transport(config.transport.as_ref(), saved.as_ref());
        let address = address.or_else(|| saved.as_ref().and_then(|p| p.direct_hint));
        let target = tunnel::peer_target(peer, transport.as_ref(), address)?;
        let endpoint =
            tunnel::bind_configured_client(identity.secret_key(), transport.as_ref()).await?;
        let relay_configured = transport.as_ref().is_some_and(|c| c.relay_url.is_some());
        if mode == "peer-watch" {
            // One identity lease and endpoint for the whole watch. Only fresh,
            // bounded read-only catalog probes are repeated; never service ops.
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let mut network =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
            let mut retry = retry::Retry::default();
            loop {
                let mut report = tokio::select! {
                    _ = term.recv() => break,
                    _ = interrupt.recv() => break,
                    _ = network.recv() => {
                        // Android's native route monitor cannot deliver system
                        // callbacks. This bridge represents the app notification.
                        endpoint.network_change().await;
                        retry.reset();
                        continue;
                    }
                    result = diagnostics::probe(&endpoint, peer, target.clone(), relay_configured) => {
                        match result {
                            Ok(report) => report,
                            Err(error) => serde_json::json!({"event":"probe", "ok":false, "host":peer,
                                "client":endpoint.id(), "relay_status":diagnostics::relay_state(&endpoint, relay_configured),
                                "error":error.to_string()})
                        }
                    }
                };
                let delay = retry.completed(report["ok"] == true);
                report["retry_after_ms"] = serde_json::json!(delay.as_millis());
                println!("{report}");
                tokio::select! {
                    _ = term.recv() => break,
                    _ = interrupt.recv() => break,
                    _ = network.recv() => {
                        endpoint.network_change().await;
                        retry.reset();
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            endpoint.close().await;
            return Ok(());
        }
        let result = async {
            if mode == "peer-hello" {
                let hello = wispnet_agent::pairing::hello(&endpoint, target).await?;
                return serde_json::to_value(hello)
                    .map_err(|_| tunnel::TunnelError::Transport("invalid pairing response"));
            }
            if mode == "peer-probe" {
                return diagnostics::probe(&endpoint, peer, target, relay_configured).await;
            }
            let connection = wispnet_agent::control::connect_target(&endpoint, target).await?;
            let catalog = wispnet_agent::control::catalog_on(&connection, peer).await?;
            serde_json::to_value(catalog)
                .map_err(|_| tunnel::TunnelError::Transport("invalid catalog"))
        }
        .await;
        endpoint.close().await;
        println!("{}", serde_json::to_string_pretty(&result?)?);
        return Ok(());
    }
    if mode == "check" {
        println!("Configuration valid");
        return Ok(());
    }
    if matches!(mode, "registry-init" | "registry-backup") {
        if mode == "registry-init" && config.registry.is_none() {
            return Err("registry-init requires a registry section".into());
        }
        config.prepare_state()?;
        let mut store =
            wispnet_registry::store::Store::open(&config.state_dir.join("registry.sqlite3"))?;
        if mode == "registry-init" {
            let mesh = &config
                .registry
                .as_ref()
                .ok_or("registry-init requires a registry section")?
                .mesh;
            wispnet_registry::control::initialize(
                &mut store,
                &config.state_dir.join("owner.token"),
                mesh,
            )?;
            println!(
                "Registry initialized; owner credential is in state_dir/owner.token (keep private)"
            );
        } else {
            store.backup(backup_path.as_ref().ok_or("backup path required")?)?;
            println!("Registry backup complete");
        }
        return Ok(());
    }
    if mode == "enroll" {
        use std::io::Write;
        let controller = config
            .controller
            .as_ref()
            .ok_or("controller section required")?;
        let invitation = wispnet_registry::control::read_invitation(&PathBuf::from(&args[4]))?;
        if wispnet_core::config::controller_origin(&controller.origin)?.as_str()
            != invitation.controller
        {
            return Err("invitation controller differs from configured HTTPS origin".into());
        }
        config.prepare_state()?;
        let store = FileIdentityStore::new(&config.state_dir)?;
        let _lease = store.lease()?;
        let identity = store.initialize()?;
        if controller
            .credential_file
            .try_exists()
            .map_err(|_| "cannot inspect device credential")?
        {
            let secret = wispnet_registry::control::read_token(&controller.credential_file)?;
            let client = wispnet_registry::client::Client::configured(controller, secret)?;
            client.directory(Some(identity.endpoint_id())).await?;
            println!("Already enrolled; existing identity and credential retained");
            return Ok(());
        }
        let mut temporary =
            wispnet_registry::control::credential_temp(&controller.credential_file)?;
        let client = wispnet_registry::client::Client::configured(
            controller,
            zeroize::Zeroizing::new(invitation.code.clone()),
        )?;
        let mut outcome = client
            .enroll(&invitation, &config.device_name, identity.secret_key())
            .await?;
        if outcome.state == "enrolled" {
            let secret = zeroize::Zeroizing::new(
                outcome
                    .token
                    .take()
                    .ok_or("missing enrollment credential")?,
            );
            temporary
                .write_all(secret.as_bytes())
                .map_err(|_| "cannot write device credential")?;
            temporary
                .as_file()
                .sync_all()
                .map_err(|_| "cannot sync device credential")?;
            temporary
                .persist_noclobber(&controller.credential_file)
                .map_err(
                    |_| "cannot commit device credential; ask owner to revoke before retrying",
                )?;
            std::fs::File::open(
                controller
                    .credential_file
                    .parent()
                    .ok_or("credential parent required")?,
            )
            .and_then(|f| f.sync_all())
            .map_err(|_| "cannot sync credential directory")?;
            println!(
                "Enrolled device: {}\nEndpoint ID: {}",
                config.device_name,
                identity.endpoint_id()
            );
        } else {
            println!(
                "Pending owner approval\nDevice: {}\nEndpoint ID: {}\nRerun enroll with the same invitation after approval",
                config.device_name,
                identity.endpoint_id()
            );
        }
        return Ok(());
    }
    if mode == "registry-directory" {
        let controller = config
            .controller
            .as_ref()
            .ok_or("controller section required")?;
        let identity = FileIdentityStore::new(&config.state_dir)?.load()?;
        let secret = wispnet_registry::control::read_token(&controller.credential_file)?;
        let client = wispnet_registry::client::Client::configured(controller, secret)?;
        let directory = client.directory(Some(identity.endpoint_id())).await?;
        println!(
            "Mesh: {}\nRevision: {}\nAuthorized services: {}",
            directory.mesh,
            directory.revision,
            directory.services.len()
        );
        return Ok(());
    }
    if matches!(mode, "identity-init" | "identity-reset" | "status") {
        if mode == "identity-init" {
            config.prepare_state()?;
        }
        let store = FileIdentityStore::new(&config.state_dir)?;
        if mode == "identity-reset" {
            let expected: EndpointId = args[4]
                .to_str()
                .ok_or("invalid public endpoint ID")?
                .parse()
                .map_err(|_| "invalid public endpoint ID")?;
            store.reset(expected)?;
            println!(
                "Identity removed. Revoke the old device before explicitly initializing and enrolling a replacement."
            );
        } else {
            let identity = if mode == "identity-init" {
                store.initialize()?
            } else {
                store.load()?
            };
            println!(
                "Device: {}\nEndpoint ID: {}",
                config.device_name,
                identity.endpoint_id()
            );
        }
        return Ok(());
    }
    // Only explicit, fixed lifecycle fields are logged; never config, args or payloads.
    let level = match config.log_level {
        wispnet_core::config::LogLevel::Error => tracing::Level::ERROR,
        wispnet_core::config::LogLevel::Warn => tracing::Level::WARN,
        wispnet_core::config::LogLevel::Info => tracing::Level::INFO,
        wispnet_core::config::LogLevel::Debug => tracing::Level::DEBUG,
    };
    // Keep third-party protocol traces out of normal diagnostics.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("wispnet_agent", level)
                        .with_target("wispnet_node", level)
                        .with_target("wispnet_cli", level)
                        .with_target("wispnet_registry", level),
                ),
        )
        .init();
    config.prepare_state()?;
    // Register signal handlers before reporting started.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let shutdown = async move {
        tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
    };
    match mode {
        "agent" => wispnet_node::run(&config, shutdown).await?,
        "registry" => wispnet_registry::run(&config, shutdown).await?,
        "tunnel" => {
            let (peer, address, service) = target.ok_or("missing tunnel target")?;
            let store = FileIdentityStore::new(&config.state_dir)?;
            let _lease = store.lease()?;
            let identity = store.load()?;
            let saved = wispnet_agent::peers::load(&config.state_dir, peer)?;
            let transport =
                wispnet_agent::peers::transport(config.transport.as_ref(), saved.as_ref());
            let address = address.or_else(|| saved.as_ref().and_then(|p| p.direct_hint));
            let endpoint =
                tunnel::bind_configured_client(identity.secret_key(), transport.as_ref()).await?;
            let operation = async {
                let connection = tunnel::connect_target(
                    &endpoint,
                    tunnel::peer_target(peer, transport.as_ref(), address)?,
                )
                .await?;
                // An explicit CLI open resolves a fresh catalog. Browser/listing opens
                // instead carry their already selected incarnation end to end.
                let catalog = match wispnet_agent::control::connect_target(
                    &endpoint,
                    tunnel::peer_target(peer, transport.as_ref(), address)?,
                )
                .await
                {
                    Ok(control) => {
                        let result = wispnet_agent::control::catalog_on(&control, peer).await;
                        control.close(0u32.into(), b"catalog complete");
                        result.ok()
                    }
                    Err(_) => None,
                };
                let stream = if let Some(catalog) = catalog {
                    let selected = catalog.services.iter().find(|s| s.id == service).ok_or(
                        tunnel::TunnelError::Status(wispnet_core::protocol::Status::UnknownService),
                    )?;
                    tunnel::open_service_bound(&connection, &service, &selected.incarnation).await?
                } else {
                    // Legacy static fixtures have no control protocol. Dynamic hosts
                    // categorically deny these unbound requests, including on failure.
                    tunnel::open_service(&connection, &service).await?
                };
                let (mut reader, mut writer) = tokio::io::split(stream);
                let mut input = tokio::io::stdin();
                let mut output = tokio::io::stdout();
                tokio::try_join!(
                    async {
                        tokio::io::copy(&mut input, &mut writer).await?;
                        tokio::io::AsyncWriteExt::shutdown(&mut writer).await
                    },
                    async {
                        tokio::io::copy(&mut reader, &mut output).await?;
                        tokio::io::AsyncWriteExt::flush(&mut output).await
                    }
                )
                .map_err(|_| tunnel::TunnelError::Transport("tunnel I/O interrupted"))?;
                Ok::<(), tunnel::TunnelError>(())
            };
            let outcome = tokio::select! {
                outcome = operation => outcome,
                _ = shutdown => Err(tunnel::TunnelError::Transport("tunnel cancelled")),
            };
            endpoint.close().await;
            outcome?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

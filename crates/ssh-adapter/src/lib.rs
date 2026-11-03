//! Optional, owner-local SSH compatibility frontend for native Wispnet Shell.
//! SSH ends at this process; the node authenticates and authorizes remote access.
mod install;
mod key;
mod local;
mod server;

use local::Local;
use std::{ffi::OsString, path::PathBuf};

pub const HELP: &str = "SSH adapter: wispnet ssh-bridge --host DEVICE_ID --stdio [--config PATH]\nSSH config:  wispnet ssh-bridge --host DEVICE_ID --print-config [--config PATH]\nOne-time setup: wispnet ssh-bridge --host DEVICE_ID --install-config [--config PATH]\nThe adapter runs over stdio only. Shell access must already be granted on the target.\nInteractive shells and streaming commands are supported; SFTP and forwarding are not.";

struct Options {
    host: String,
    config: PathBuf,
    print_config: bool,
    install_config: bool,
}

/// Run one local SSH connection, or print its explicitly pinned OpenSSH stanza.
pub fn run_args(args: &[OsString]) -> Result<u32, String> {
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        println!("{HELP}");
        return Ok(0);
    }
    let options = options(args)?;
    let local = Local::load(&options.config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let outcome = runtime.block_on(async {
        let (host, name) = local.resolve(&options.host).await?;
        if options.print_config || options.install_config {
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            let stanza = config_stanza(&host, &name, &executable, &local.config)?;
            if options.install_config {
                install::save(&host, &stanza)?;
                println!(
                    "ssh shell-{}.localhost",
                    wispnet_core::config::readable_device_id(&name)
                );
            } else {
                print!("{stanza}");
            }
            return Ok(0);
        }
        let key = key::load(&local.state, &host)?;
        server::run(local, host, key).await?;
        Ok(0)
    });
    // Tokio's stdin worker can be blocked after the peer closes the SSH session.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    outcome
}

fn options(args: &[OsString]) -> Result<Options, String> {
    let mut host = None;
    let mut config = None;
    let mut stdio = false;
    let mut print_config = false;
    let mut install_config = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str().ok_or(HELP)? {
            "--stdio" if !stdio => stdio = true,
            "--print-config" if !print_config => print_config = true,
            "--install-config" if !install_config => install_config = true,
            "--host" if host.is_none() => {
                host = Some(args.next().and_then(|s| s.to_str()).ok_or(HELP)?.to_owned())
            }
            "--config" if config.is_none() => {
                config = Some(PathBuf::from(args.next().ok_or(HELP)?))
            }
            _ => return Err(HELP.into()),
        }
    }
    if [stdio, print_config, install_config]
        .into_iter()
        .filter(|v| *v)
        .count()
        != 1
    {
        return Err(HELP.into());
    }
    let host = host
        .filter(|s| !s.is_empty() && s.len() <= 64 && !s.contains('\0'))
        .ok_or(HELP)?;
    let config = config.map(Ok).unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config/wispnet-node/host.toml"))
            .ok_or("HOME is unset; pass --config")
    })?;
    Ok(Options {
        host,
        config,
        print_config,
        install_config,
    })
}

fn config_stanza(
    host: &str,
    name: &str,
    executable: &std::path::Path,
    config: &std::path::Path,
) -> Result<String, String> {
    let alias = wispnet_core::config::readable_device_id(name);
    let executable = proxy_argument(executable.to_str().ok_or("Executable path must be UTF-8")?)?;
    let config = proxy_argument(config.to_str().ok_or("Config path must be UTF-8")?)?;
    Ok(format!(
        "Host shell-{alias}.localhost\n    HostName shell-{alias}.localhost\n    User wispnet\n    HostKeyAlias wispnet-{host}\n    ProxyCommand {executable} ssh-bridge --host {host} --config {config} --stdio\n"
    ))
}

fn proxy_argument(value: &str) -> Result<String, String> {
    if value.chars().any(|c| c.is_control()) {
        return Err("SSH config paths cannot contain control characters".into());
    }
    // OpenSSH expands percent tokens before invoking the local user's shell.
    Ok(format!(
        "'{}'",
        value.replace('%', "%%").replace('\'', "'\\''")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_pins_identity_and_quotes_paths_without_shell_interpolation() {
        let host = "a".repeat(64);
        let config = config_stanza(
            &host,
            "My desktop",
            std::path::Path::new("/tmp/A ' $name%/wispnet"),
            std::path::Path::new("/tmp/config path.toml"),
        )
        .unwrap();
        assert!(config.starts_with("Host shell-my-desktop.localhost\n"));
        assert!(config.contains(&format!("--host {host}")));
        assert!(config.contains("'\\''"));
        assert!(config.contains("$name%%"));
        assert!(
            config_stanza(
                &host,
                "desk",
                std::path::Path::new("/tmp/new\nline"),
                std::path::Path::new("/tmp/config")
            )
            .is_err()
        );
    }
}

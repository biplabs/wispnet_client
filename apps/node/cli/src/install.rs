//! Native distribution installation. User authority is never recreated on upgrade.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    error::Error,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!(
            "{} failed ({status})",
            command.get_program().to_string_lossy()
        )
        .into());
    }
    Ok(())
}
fn text(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn home() -> Result<PathBuf> {
    Ok(std::env::var_os("HOME").ok_or("HOME is required")?.into())
}
fn hostname() -> Result<String> {
    let mut bytes = [0_u8; 256];
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("Hostname is too long")?;
    Ok(String::from_utf8(bytes[..end].to_vec())?)
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn unlink(path: &Path) -> Result<()> {
    if exists(path) {
        fs::remove_file(path)?;
    }
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("Destination has no parent")?)?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn copy(source: &Path, target: &Path) -> Result<()> {
    atomic(
        target,
        &fs::read(source)?,
        fs::metadata(source)?.mode() & 0o777,
    )
}
fn link(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target.parent().ok_or("Destination has no parent")?)?;
    let temporary = target.with_extension("installing");
    unlink(&temporary)?;
    symlink(source, &temporary)?;
    fs::rename(temporary, target)?;
    Ok(())
}
fn tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
        } else if metadata.is_dir() {
            tree(&from, &to)?;
        } else if metadata.is_file() {
            copy(&from, &to)?;
        } else {
            return Err("Unsupported bundle payload".into());
        }
    }
    Ok(())
}
fn relative(name: &str) -> Result<&Path> {
    let path = Path::new(name);
    if name.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("Invalid distribution path: {name}").into());
    }
    Ok(path)
}
fn inventory(root: &Path, at: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(at)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            inventory(root, &path, files)?;
        } else if metadata.is_file() || metadata.is_symlink() {
            files.insert(
                path.strip_prefix(root)?
                    .to_str()
                    .ok_or("Non-UTF8 distribution path")?
                    .to_owned(),
            );
        } else {
            return Err("Unsupported distribution payload".into());
        }
    }
    Ok(())
}
fn verify(root: &Path) -> Result<Option<Value>> {
    if !root.join("manifest.json").exists() {
        if root.join("bin/wispnet").exists() {
            return Err("Distribution manifest is missing".into());
        }
        return Ok(None);
    }
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    if manifest["format"] != 1 || manifest["license"] != "Apache-2.0" {
        return Err("Unsupported distribution format or license".into());
    }
    let platform = manifest["platform"].as_str().ok_or("Missing platform")?;
    if !matches!(platform, "linux-x86_64" | "macos-x86_64" | "macos-aarch64") {
        return Err("Unsupported distribution platform".into());
    }
    if platform.starts_with("macos") {
        if manifest["desktop"] != "flutter" {
            return Err("Unsupported distribution desktop".into());
        }
        mac_build_metadata(&manifest)?;
    }
    let hashes = manifest["sha256"]
        .as_object()
        .ok_or("Missing bundle checksums")?;
    let empty = serde_json::Map::new();
    let links = manifest
        .get("symlinks")
        .map(|v| v.as_object().ok_or("Invalid bundle symlinks"))
        .transpose()?
        .unwrap_or(&empty);
    let required = if platform.starts_with("macos") {
        vec![
            "bin/wispnet",
            "Wispnet.app/Contents/MacOS/Wispnet",
            "Wispnet.app/Contents/MacOS/wispnet-node",
            "scripts/install_macos.sh",
            "apps/node/desktop/icon.png",
        ]
    } else {
        vec!["bin/wispnet", "scripts/install_node_desktop.sh"]
    };
    if required.iter().any(|n| !hashes.contains_key(*n)) {
        return Err("Distribution is missing required payloads".into());
    }
    let canonical = root.canonicalize()?;
    let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
    for (name, digest) in hashes {
        let path = root.join(relative(name)?);
        if links.contains_key(name)
            || fs::symlink_metadata(&path)?.is_symlink()
            || path.canonicalize()?.strip_prefix(&canonical).is_err()
            || !path.is_file()
            || format!("{:x}", Sha256::digest(fs::read(&path)?))
                != digest.as_str().ok_or("Invalid checksum")?
        {
            return Err(format!("Bundle checksum mismatch: {name}").into());
        }
        expected.insert(name.clone());
    }
    for (name, target) in links {
        let path = root.join(relative(name)?);
        let target = target.as_str().ok_or("Invalid symlink")?;
        if Path::new(target).is_absolute()
            || !fs::symlink_metadata(&path)?.is_symlink()
            || fs::read_link(&path)? != Path::new(target)
            || path.canonicalize()?.strip_prefix(&canonical).is_err()
        {
            return Err(format!("Bundle checksum mismatch: {name}").into());
        }
        expected.insert(name.clone());
    }
    let mut actual = BTreeSet::new();
    inventory(root, root, &mut actual)?;
    if actual != expected {
        return Err("Distribution payload does not match its manifest".into());
    }
    Ok(Some(manifest))
}
fn mac_build_metadata(manifest: &Value) -> Result<(String, String, String)> {
    let count = match &manifest["source_count"] {
        Value::String(count) => count.clone(),
        Value::Number(count) if count.is_u64() => count.to_string(),
        _ => return Err("Invalid distribution build metadata".into()),
    };
    let version = manifest["version"]
        .as_str()
        .ok_or("Invalid distribution build metadata")?;
    let parts: Vec<_> = version.split('.').collect();
    let identity = manifest["build_identity"]
        .as_str()
        .ok_or("Invalid distribution build metadata")?;
    if count.is_empty()
        || !count.bytes().all(|b| b.is_ascii_digit())
        || !(2..=4).contains(&parts.len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        || identity.is_empty()
    {
        return Err("Invalid distribution build metadata".into());
    }
    Ok((count, version.to_owned(), identity.to_owned()))
}
#[derive(Default)]
struct Options {
    root: PathBuf,
    no_start: bool,
    headless: bool,
    firewall: bool,
    init: bool,
    skip_build: bool,
    verify_only: bool,
}
fn options(args: &[OsString]) -> Result<Options> {
    let mut result = Options {
        root: std::env::current_dir()?,
        ..Default::default()
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str().ok_or("Invalid argument")? {
        "--root"=>result.root=PathBuf::from(args.next().ok_or("--root requires a path")?),
        "--no-start"=>result.no_start=true,"--headless"=>result.headless=true,
        "--configure-firewall"=>result.firewall=true,"--init"=>result.init=true,
        "--skip-build"=>result.skip_build=true,"--verify-only"=>result.verify_only=true,
        _=>return Err("Usage: wispnet app-install --root BUNDLE [--no-start] [--headless] [--configure-firewall] [--init] [--skip-build] [--verify-only]".into()),
    }
    }
    if result.firewall && result.no_start {
        return Err("--configure-firewall cannot be combined with --no-start".into());
    }
    result.root = result.root.canonicalize()?;
    Ok(result)
}
pub fn run_args(args: &[OsString]) -> std::result::Result<(), String> {
    install(args).map_err(|e| e.to_string())
}
fn install(args: &[OsString]) -> Result<()> {
    let options = options(args)?;
    let manifest = verify(&options.root)?;
    if options.verify_only {
        println!("Bundle verified");
        return Ok(());
    }
    if let Some(m) = &manifest {
        let expected = format!(
            "{}-{}",
            if cfg!(target_os = "macos") {
                "macos"
            } else {
                "linux"
            },
            std::env::consts::ARCH
        );
        if m["platform"] != expected {
            return Err("Bundle platform does not match this computer".into());
        }
    }
    if cfg!(target_os = "macos") {
        install_macos(&options, manifest.as_ref())
    } else {
        install_linux(&options)
    }
}
fn owner_launcher(node: &Path, home: &Path) -> Result<()> {
    link(node, &home.join(".local/bin/wispnet"))?;
    // A shell wrapper forwards arguments without evaluating them.
    let quoted = node.to_string_lossy().replace('\'', "'\\''");
    atomic(
        &home.join(".local/bin/wispnet-node"),
        format!("#!/bin/sh\nexec '{quoted}' node-request \"$@\"\n").as_bytes(),
        0o755,
    )?;
    // Retire only the obsolete code alongside the newly installed executable.
    for name in ["node_request.py", "mcp_headers.py"] {
        unlink(&node.parent().unwrap().join(name))?;
    }
    let old = node.parent().unwrap().join("wispnet_desktop");
    if old.is_dir() {
        fs::remove_dir_all(old)?;
    }
    Ok(())
}
fn systemctl(args: &[&str]) -> Command {
    let mut c = Command::new("systemctl");
    c.arg("--user").args(args);
    c
}
fn wait_node(node: &Path, config: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if crate::owner::NodeApi::new(config)
            .and_then(|api| api.request(json!({"op":"status"})))
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("Installed node did not become ready: {}", node.display()).into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn install_linux(options: &Options) -> Result<()> {
    let root = &options.root;
    let home = home()?;
    let node_source = if root.join("bin/wispnet").exists() {
        root.join("bin/wispnet")
    } else {
        root.join("target/debug/wispnet")
    };
    run(Command::new(&node_source)
        .arg("--help")
        .stdout(Stdio::null()))?;
    let graphical = !options.headless
        && systemctl(&["is-active", "--quiet", "graphical-session.target"])
            .status()?
            .success();
    let mut desktop_source = root.join("bin/Wispnet");
    if graphical && !desktop_source.exists() {
        if !root.join("scripts/build_flutter_desktop.sh").exists() {
            return Err(
                "This is a headless bundle. Install a desktop bundle to run the GUI.".into(),
            );
        }
        run(&mut Command::new(
            root.join("scripts/build_flutter_desktop.sh"),
        ))?;
        desktop_source = root.join("apps/flutter/build/linux/x64/release/bundle/Wispnet");
    }
    if options.firewall {
        let mut command = Command::new(
            if graphical
                && Command::new("which")
                    .arg("pkexec")
                    .stdout(Stdio::null())
                    .status()?
                    .success()
            {
                "pkexec"
            } else {
                "sudo"
            },
        );
        command
            .arg(&node_source)
            .arg("firewall-install")
            .arg("--uid")
            .arg(unsafe { libc::getuid() }.to_string());
        run(&mut command)?;
    }
    let config = home.join(".config/wispnet-node/host.toml");
    let legacy = home.join(".config/wipsnet-node/host.toml");
    if legacy.exists() && !config.exists() {
        private_dir(config.parent().unwrap())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config)?;
        output.write_all(&fs::read(&legacy)?)?;
        let old = legacy.parent().unwrap().join("admin-targets.json");
        let new = config.parent().unwrap().join("admin-targets.json");
        if old.exists() && !new.exists() {
            atomic(&new, &fs::read(old)?, 0o600)?;
        }
    }
    let units = home.join(".config/systemd/user");
    fs::create_dir_all(&units)?;
    let unit = units.join("wispnet-host.service");
    let source = root.join("deploy/node/wispnet-host.service");
    let unit_changed = fs::read(&unit).ok() != Some(fs::read(&source)?);
    copy(&source, &unit)?;
    let node = home.join(".local/libexec/wispnet-node/wispnet");
    let node_changed = fs::read(&node).ok() != Some(fs::read(&node_source)?);
    if node_changed {
        copy(&node_source, &node)?;
        fs::set_permissions(&node, fs::Permissions::from_mode(0o755))?;
    }
    owner_launcher(&node, &home)?;
    if !options.no_start {
        let legacy_unit = units.join("wipsnet-host.service");
        if exists(&legacy_unit) {
            run(&mut systemctl(&[
                "disable",
                "--now",
                "wipsnet-host.service",
            ]))?;
            unlink(&legacy_unit)?;
        }
        run(&mut systemctl(&["daemon-reload"]))?;
        run(&mut systemctl(&["enable", "wispnet-host.service"]))?;
        if node_changed
            || unit_changed
            || !systemctl(&["is-active", "--quiet", "wispnet-host.service"])
                .status()?
                .success()
        {
            run(&mut systemctl(&["restart", "wispnet-host.service"]))?;
        }
        wait_node(&node, &config)?;
        unlink(&home.join(".local/libexec/wipsnet-node/wipsnet"))?;
    }
    if !graphical {
        println!("Installed headless node; identity, configuration and grants preserved.");
        return Ok(());
    }
    let destination = home.join(".local/libexec/wispnet-desktop");
    fs::create_dir_all(&destination)?;
    copy(
        &root.join("apps/node/desktop/icon.png"),
        &destination.join("icon.png"),
    )?;
    if desktop_source
        .parent()
        .unwrap()
        .join("lib/libflutter_linux_gtk.so")
        .exists()
    {
        let stage = tempfile::Builder::new()
            .prefix("flutter-")
            .tempdir_in(&destination)?;
        tree(desktop_source.parent().unwrap(), stage.path())?;
        // Use this install's current native helper, including when an older build tree is reused.
        copy(&node_source, &stage.path().join("wispnet"))?;
        let runtime = stage.keep();
        link(&runtime.join("Wispnet"), &destination.join("Wispnet"))?;
    } else {
        copy(&desktop_source, &destination.join("Wispnet"))?;
    }
    copy(&node_source, &destination.join("wispnet"))?;
    let launcher = home.join(".local/bin/wispnet-desktop");
    link(&destination.join("Wispnet"), &launcher)?;
    let applications = home.join(".local/share/applications");
    atomic(&applications.join("wispnet.desktop"),format!("[Desktop Entry]\nType=Application\nName=Wispnet\nComment=Your services and connected devices\nExec=\"{}\" %u\nIcon={}\nCategories=Network;\nTerminal=false\nMimeType=x-scheme-handler/wispnet;\n",launcher.display(),destination.join("icon.png").display()).as_bytes(),0o644)?;
    copy(
        &root.join("deploy/node/wispnet-desktop.service"),
        &units.join("wispnet-desktop.service"),
    )?;
    if Command::new("which")
        .arg("update-desktop-database")
        .stdout(Stdio::null())
        .status()?
        .success()
    {
        run(Command::new("update-desktop-database").arg(&applications))?;
    }
    if options.no_start {
        println!("Installed desktop files; services were not started.");
        return Ok(());
    }
    if Command::new("which")
        .arg("xdg-mime")
        .stdout(Stdio::null())
        .status()?
        .success()
    {
        run(Command::new("xdg-mime").args([
            "default",
            "wispnet.desktop",
            "x-scheme-handler/wispnet",
        ]))?;
    }
    run(&mut systemctl(&["daemon-reload"]))?;
    run(&mut systemctl(&["enable", "wispnet-desktop.service"]))?;
    run(&mut systemctl(&["restart", "wispnet-desktop.service"]))?;
    std::thread::sleep(Duration::from_secs(2));
    run(&mut systemctl(&[
        "is-active",
        "--quiet",
        "wispnet-desktop.service",
    ]))?;
    println!("Installed Flutter desktop and node; identity, configuration and grants preserved.");
    Ok(())
}
fn write_plist(path: &Path, value: &plist::Value) -> Result<()> {
    let mut data = Vec::new();
    value.to_writer_xml(&mut data)?;
    atomic(path, &data, 0o600)
}
/// Set up a drag-installed app without modifying its signed bundle. Explicit
/// --config bridge sessions (tests and owner automation) never enter this path.
#[cfg(target_os = "macos")]
pub fn prepare_macos_desktop() -> std::result::Result<(), String> {
    prepare_macos_desktop_inner().map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn prepare_macos_desktop_inner() -> Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let home = home()?;
    let Some(app) = installed_app(&executable, &home)? else {
        // Source-tree executables continue using the explicit source installer.
        return Ok(());
    };
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&app)
        .stdout(Stdio::null()))?;
    let config = home.join(".config/wispnet-node/host.toml");
    initialize_macos_host(&home, &config, &executable)?;
    run(Command::new(&executable)
        .arg("host-status")
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null()))?;
    let node = home.join(".local/libexec/wispnet-node/wispnet");
    let changed = !node.exists()
        || Sha256::digest(fs::read(&node)?) != Sha256::digest(fs::read(&executable)?);
    if changed {
        copy(&executable, &node)?;
    }
    owner_launcher(&node, &home)?;
    let agents = home.join("Library/LaunchAgents");
    let logs = home.join("Library/Logs/Wispnet");
    private_dir(&logs)?;
    for (label, args, log) in [
        (
            "app.wispnet.node",
            vec![
                node.to_string_lossy().into_owned(),
                "agent".into(),
                "--config".into(),
                config.to_string_lossy().into_owned(),
            ],
            "node.log",
        ),
        (
            "app.wispnet.desktop",
            vec![
                app.join("Contents/MacOS/Wispnet")
                    .to_string_lossy()
                    .into_owned(),
                "--background".into(),
            ],
            "desktop.log",
        ),
    ] {
        let mut value = plist::Dictionary::new();
        value.insert("Label".into(), label.into());
        value.insert(
            "ProgramArguments".into(),
            plist::Value::Array(args.into_iter().map(plist::Value::String).collect()),
        );
        value.insert("RunAtLoad".into(), true.into());
        value.insert("ThrottleInterval".into(), 3.into());
        value.insert("Umask".into(), 63.into());
        let mut keep = plist::Dictionary::new();
        keep.insert("SuccessfulExit".into(), false.into());
        value.insert("KeepAlive".into(), keep.into());
        if label.ends_with("desktop") {
            value.insert("LimitLoadToSessionType".into(), "Aqua".into());
        }
        for field in ["StandardOutPath", "StandardErrorPath"] {
            value.insert(
                field.into(),
                logs.join(log).to_string_lossy().into_owned().into(),
            );
        }
        let log_path = logs.join(log);
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)?;
        fs::set_permissions(log_path, fs::Permissions::from_mode(0o600))?;
        write_plist(&agents.join(format!("{label}.plist")), &value.into())?;
    }
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let service = format!("{domain}/app.wispnet.node");
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &service])
        .output()?
        .status
        .success();
    if changed && loaded {
        run(Command::new("/bin/launchctl").args(["bootout", &service]))?;
    }
    if changed || !loaded {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let result = Command::new("/bin/launchctl")
                .args(["bootstrap", &domain])
                .arg(agents.join("app.wispnet.node.plist"))
                .output()?;
            if result.status.success() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // The desktop is already running. Its LaunchAgent takes effect next login;
    // bootstrapping it here would launch a second copy during first-run setup.
    wait_node(&node, &config)
}

#[cfg(any(target_os = "macos", test))]
fn installed_app(executable: &Path, home: &Path) -> Result<Option<PathBuf>> {
    if executable
        .file_name()
        .is_none_or(|name| name != "wispnet-node")
        || executable
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|name| name != "MacOS")
        || executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_none_or(|name| name != "Contents")
    {
        return Ok(None);
    }
    let app = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("Missing app bundle")?;
    if app.file_name().is_none_or(|name| name != "Wispnet.app") {
        return Ok(None);
    }
    // Flutter development builds use the source installer and must not register
    // a login agent pointing into a disposable build directory.
    if app.ancestors().any(|root| {
        root.join("Cargo.toml").is_file()
            && app == root.join("apps/flutter/build/macos/Build/Products/Release/Wispnet.app")
    }) {
        return Ok(None);
    }
    let parent = app.parent().ok_or("Missing app parent")?;
    let user_apps = home.join("Applications");
    if parent != Path::new("/Applications") && parent != user_apps {
        return Err("Move Wispnet to Applications, eject the disk image, then open Wispnet from Applications.".into());
    }
    Ok(Some(app.to_owned()))
}

fn initialize_macos_host(home: &Path, config: &Path, node: &Path) -> Result<()> {
    if config.exists() {
        return Ok(());
    }
    let state = home.join(".local/state/wispnet-node");
    if state.exists() {
        return Err(
            "State exists without configuration; recover its configuration before installing"
                .into(),
        );
    }
    private_dir(config.parent().ok_or("Missing configuration parent")?)?;
    let name = hostname()?.split('.').next().unwrap_or("device").to_owned();
    let content = format!(
        "schema_version=2\ndevice_name={}\nstate_dir={}\nlog_level=\"info\"\n[transport]\nbind_addr=\"0.0.0.0:0\"\nrelay_url=\"https://relay.wispnet.app\"\nrelay_only=false\n",
        serde_json::to_string(&name)?,
        serde_json::to_string(&state)?
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(config)?;
    output.write_all(content.as_bytes())?;
    output.sync_all()?;
    run(Command::new(node)
        .arg("host-init")
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null()))
}

fn install_macos(options: &Options, manifest: Option<&Value>) -> Result<()> {
    let root = &options.root;
    let home = home()?;
    let config = home.join(".config/wispnet-node/host.toml");
    if !config.exists() && !options.init {
        return Err("No existing host configuration. Use --init to explicitly create this Mac's independent identity.".into());
    }
    if manifest.is_none() && !options.skip_build {
        run(Command::new("cargo")
            .args(["build", "--workspace", "--locked"])
            .current_dir(root))?;
        run(&mut Command::new(
            root.join("scripts/build_flutter_desktop.sh"),
        ))?;
    }
    let node_source = root.join(if manifest.is_some() {
        "bin/wispnet"
    } else {
        "target/debug/wispnet"
    });
    let app_source = root.join(if manifest.is_some() {
        "Wispnet.app"
    } else {
        "apps/flutter/build/macos/Build/Products/Release/Wispnet.app"
    });
    run(Command::new(&node_source)
        .arg("--help")
        .stdout(Stdio::null()))?;
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&app_source))?;
    let (build_number, app_version, build_identity) = if let Some(manifest) = manifest {
        mac_build_metadata(manifest)?
    } else {
        let version = text(Command::new(&node_source).arg("--version"))?;
        let identity = version
            .strip_prefix("wispnet ")
            .ok_or("Invalid node build identity")?
            .to_owned();
        let count = text(
            Command::new("git")
                .args(["rev-list", "--count", "HEAD"])
                .current_dir(root),
        )?;
        let metadata = json!({"source_count": count, "version": identity.split('+').next().unwrap_or(""), "build_identity": identity});
        mac_build_metadata(&metadata)?
    };
    initialize_macos_host(&home, &config, &node_source)?;
    let api = crate::owner::NodeApi::new(&config)?;
    run(Command::new(&node_source)
        .arg("host-status")
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null()))?;
    let node = home.join(".local/libexec/wispnet-node/wispnet");
    copy(&node_source, &node)?;
    owner_launcher(&node, &home)?;
    let app = home.join("Applications/Wispnet.app");
    let staged = home.join("Applications/Wispnet.installing.app");
    if staged.exists() {
        fs::remove_dir_all(&staged)?;
    }
    tree(&app_source, &staged)?;
    copy(&node_source, &staged.join("Contents/MacOS/wispnet-node"))?;
    // Prepare and sign the complete replacement before moving the installed app.
    let resources = staged.join("Contents/Resources");
    fs::create_dir_all(&resources)?;
    let temporary_icons = tempfile::Builder::new()
        .prefix("wispnet-icons-")
        .tempdir()?;
    let iconset = temporary_icons.path().join("Wispnet.iconset");
    fs::create_dir(&iconset)?;
    for size in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let suffix = if scale == 2 { "@2x" } else { "" };
            let pixels = (size * scale).to_string();
            run(Command::new("sips")
                .args(["-z", &pixels, &pixels])
                .arg(root.join("apps/node/desktop/icon.png"))
                .arg("--out")
                .arg(iconset.join(format!("icon_{size}x{size}{suffix}.png")))
                .stdout(Stdio::null()))?;
        }
    }
    run(Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .arg("-o")
        .arg(resources.join("Wispnet.icns")))?;
    let info_path = staged.join("Contents/Info.plist");
    let mut info = plist::Value::from_file(&info_path)?;
    let dict = info.as_dictionary_mut().ok_or("Invalid app Info.plist")?;
    for (field, value) in [
        ("CFBundleIdentifier", "app.wispnet.desktop"),
        ("CFBundleName", "Wispnet"),
        ("CFBundleDisplayName", "Wispnet"),
        ("CFBundleExecutable", "Wispnet"),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleIconFile", "Wispnet.icns"),
        ("LSMinimumSystemVersion", "13.0"),
    ] {
        dict.insert(field.into(), value.into());
    }
    dict.insert("NSHighResolutionCapable".into(), true.into());
    dict.insert("CFBundleVersion".into(), build_number.into());
    dict.insert("CFBundleShortVersionString".into(), app_version.into());
    dict.insert("WispnetBuildIdentity".into(), build_identity.into());
    dict.insert(
        "WispnetStateDirectory".into(),
        api.state.to_string_lossy().into_owned().into(),
    );
    write_plist(&info_path, &info)?;
    run(Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&staged))?;
    if app.exists() {
        let old = home.join("Library/Application Support/Wispnet/Backups/Wispnet.previous.app");
        fs::create_dir_all(old.parent().unwrap())?;
        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        fs::rename(&app, old)?;
    }
    fs::rename(staged, &app)?;
    let register = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = Command::new(register).arg("-u").arg(&app_source).output();
    run(Command::new(register).arg("-f").arg(&app))?;
    let agents = home.join("Library/LaunchAgents");
    let logs = home.join("Library/Logs/Wispnet");
    private_dir(&logs)?;
    for log in ["node.log", "desktop.log"] {
        let path = logs.join(log);
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    for (label, arguments, log) in [
        (
            "app.wispnet.node",
            vec![
                node.to_string_lossy().into_owned(),
                "agent".into(),
                "--config".into(),
                config.to_string_lossy().into_owned(),
            ],
            "node.log",
        ),
        (
            "app.wispnet.desktop",
            vec![
                app.join("Contents/MacOS/Wispnet")
                    .to_string_lossy()
                    .into_owned(),
                "--background".into(),
            ],
            "desktop.log",
        ),
    ] {
        let mut value = plist::Dictionary::new();
        value.insert("Label".into(), label.into());
        value.insert(
            "ProgramArguments".into(),
            plist::Value::Array(arguments.into_iter().map(plist::Value::String).collect()),
        );
        value.insert("RunAtLoad".into(), true.into());
        value.insert("ThrottleInterval".into(), 3.into());
        value.insert("Umask".into(), 63.into());
        let mut keep = plist::Dictionary::new();
        keep.insert("SuccessfulExit".into(), false.into());
        value.insert("KeepAlive".into(), keep.into());
        if label.ends_with("desktop") {
            value.insert("LimitLoadToSessionType".into(), "Aqua".into());
        }
        for field in ["StandardOutPath", "StandardErrorPath"] {
            value.insert(
                field.into(),
                logs.join(log).to_string_lossy().into_owned().into(),
            );
        }
        write_plist(&agents.join(format!("{label}.plist")), &value.into())?;
    }
    if options.no_start {
        println!(
            "Installed {}; launch agents were not started.",
            app.display()
        );
        return Ok(());
    }
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    for label in ["app.wispnet.desktop", "app.wispnet.node"] {
        let _ = Command::new("launchctl")
            .arg("bootout")
            .arg(format!("{domain}/{label}"))
            .output();
    }
    for label in ["app.wispnet.node", "app.wispnet.desktop"] {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let result = Command::new("launchctl")
                .arg("bootstrap")
                .arg(&domain)
                .arg(agents.join(format!("{label}.plist")))
                .output()?;
            if result.status.success() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if label.ends_with("node") {
            wait_node(&node, &config)?;
        }
    }
    run(Command::new("open").arg(&app))?;
    println!(
        "Installed and opened {}; identity and grants preserved.",
        app.display()
    );
    Ok(())
}
pub fn package_args(args: &[OsString]) -> std::result::Result<(), String> {
    package(args).map_err(|e| e.to_string())
}
fn package(args: &[OsString]) -> Result<()> {
    if args.first().is_some_and(|a| a == "--remove") && args.len() == 1 {
        return crate::firewall::package_remove().map_err(Into::into);
    }
    if args.first().is_some_and(|a| a == "--firewall-uid") && args.len() == 2 {
        return crate::firewall::package_configure(args[1].to_str().ok_or("Invalid UID")?.parse()?)
            .map_err(Into::into);
    }
    if !args.is_empty() && args != [OsString::from("--headless")] {
        return Err("Usage: wispnet-setup [--headless | --firewall-uid UID | --remove]".into());
    }
    if unsafe { libc::geteuid() } == 0 {
        return Err("Run wispnet-setup as your regular user".into());
    }
    let home = home()?;
    for name in ["wispnet-host.service", "wispnet-desktop.service"] {
        if exists(&home.join(".config/systemd/user").join(name)) {
            return Err("A standalone user service overrides the package. Run its uninstaller first; identity is preserved.".into());
        }
    }
    let config = home.join(".config/wispnet-node/host.toml");
    if !config.exists() {
        private_dir(config.parent().unwrap())?;
        let legacy = home.join(".config/wipsnet-node/host.toml");
        let content = if legacy.exists() {
            fs::read(&legacy)?
        } else {
            format!("schema_version=2\ndevice_name={}\nstate_dir={}\nlog_level=\"info\"\n[transport]\nbind_addr=\"0.0.0.0:0\"\nrelay_url=\"https://relay.wispnet.app\"\nbackup_relay_urls=[\"https://usw1-1.relay.n0.iroh.link\"]\n",serde_json::to_string(&hostname()?)?,serde_json::to_string(&home.join(".local/state/wispnet-node"))?).into_bytes()
        };
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config)?;
        output.write_all(&content)?;
        if !legacy.exists() {
            run(Command::new("/usr/bin/wispnet")
                .arg("host-init")
                .arg("--config")
                .arg(&config))?;
        }
    }
    run(Command::new("/usr/bin/wispnet")
        .arg("host-status")
        .arg("--config")
        .arg(config))?;
    run(&mut systemctl(&["daemon-reload"]))?;
    run(&mut systemctl(&["enable", "--now", "wispnet-host.service"]))?;
    if args.is_empty() {
        run(&mut systemctl(&[
            "enable",
            "--now",
            "wispnet-desktop.service",
        ]))?;
    }
    println!(
        "Wispnet configured. Optional firewall setup: sudo wispnet-setup --firewall-uid {}",
        unsafe { libc::getuid() }
    );
    Ok(())
}
fn remove_ssh(home: &Path) -> Result<()> {
    let directory = home.join(".ssh");
    if !exists(&directory) {
        return Ok(());
    }
    let meta = fs::symlink_metadata(&directory)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o022 != 0 {
        return Err("Unsafe SSH directory".into());
    }
    let path = directory.join("config");
    if !exists(&path) {
        return Ok(());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".wispnet.lock"))?;
    let meta = lock.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err("Unsafe SSH configuration lock".into());
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let meta = input.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::getuid() }
        || meta.mode() & 0o022 != 0
        || meta.len() > 1024 * 1024
    {
        return Err("Unsafe or oversized SSH configuration".into());
    }
    let mut original = String::new();
    input.take(1024 * 1024 + 1).read_to_string(&mut original)?;
    if original.len() > 1024 * 1024 {
        return Err("SSH configuration grew while reading".into());
    }
    let mut updated = String::new();
    let mut peer: Option<&str> = None;
    for line in original.split_inclusive('\n') {
        if let Some(marker) = line
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("# Wispnet bridge ")
        {
            let (identity, kind) = marker
                .split_once(' ')
                .ok_or("Malformed Wispnet SSH marker")?;
            if identity.len() != 64
                || !identity
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("Malformed Wispnet SSH identity".into());
            }
            match (peer, kind) {
                (None, "begin") => peer = Some(identity),
                (Some(current), "end") if current == identity => peer = None,
                _ => return Err("Malformed Wispnet SSH block".into()),
            }
        } else if peer.is_none() {
            updated.push_str(line);
        }
    }
    if peer.is_some() {
        return Err("Incomplete Wispnet SSH block".into());
    }
    if updated != original {
        atomic(&path, updated.as_bytes(), 0o600)?;
    }
    Ok(())
}
pub fn uninstall_args(args: &[OsString]) -> std::result::Result<(), String> {
    uninstall(args).map_err(|e| e.to_string())
}
fn uninstall(args: &[OsString]) -> Result<()> {
    if !args.is_empty() {
        return Err("Usage: wispnet app-uninstall".into());
    }
    if unsafe { libc::geteuid() } == 0 {
        return Err("Run as your regular user; firewall cleanup requests sudo separately".into());
    }
    let home = home()?;
    let units = home.join(".config/systemd/user");
    if cfg!(target_os = "macos") {
        for label in ["app.wispnet.desktop", "app.wispnet.node"] {
            let plist = home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            if exists(&plist) {
                let _ = Command::new("launchctl")
                    .arg("bootout")
                    .arg(format!("gui/{}/{label}", unsafe { libc::getuid() }))
                    .output();
                unlink(&plist)?;
            }
        }
    } else {
        for name in [
            "wispnet-desktop.service",
            "wispnet-host.service",
            "wipsnet-host.service",
        ] {
            if exists(&units.join(name)) {
                run(&mut systemctl(&["disable", "--now", name]))?;
            }
        }
        if exists(Path::new("/etc/wispnet-firewall.json"))
            || exists(Path::new("/etc/systemd/system/wispnet-firewall.service"))
        {
            run(Command::new("sudo")
                .arg(std::env::current_exe()?)
                .arg("firewall-uninstall")
                .arg("--uid")
                .arg(unsafe { libc::getuid() }.to_string()))?;
        }
    }
    remove_ssh(&home)?;
    for name in [
        "wispnet-desktop.service",
        "wispnet-host.service",
        "wipsnet-host.service",
    ] {
        unlink(&units.join(name))?;
    }
    for name in ["wispnet", "wispnet-node", "wispnet-desktop"] {
        unlink(&home.join(".local/bin").join(name))?;
    }
    unlink(&home.join(".local/share/applications/wispnet.desktop"))?;
    for path in [
        home.join(".local/libexec/wispnet-node"),
        home.join(".local/libexec/wispnet-desktop"),
        home.join("Applications/Wispnet.app"),
    ] {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
            unlink(&path)?;
        } else if path.exists() {
            fs::remove_dir_all(path)?;
        }
    }
    if !cfg!(target_os = "macos") {
        run(&mut systemctl(&["daemon-reload"]))?;
    }
    println!(
        "Wispnet and managed SSH aliases removed; identity, pairing and private state preserved."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drag_install_requires_applications_and_never_uses_disk_image_paths() {
        let home = Path::new("/Users/example");
        for root in ["/Applications", "/Users/example/Applications"] {
            let executable = Path::new(root).join("Wispnet.app/Contents/MacOS/wispnet-node");
            assert_eq!(
                installed_app(&executable, home).unwrap(),
                Some(Path::new(root).join("Wispnet.app"))
            );
        }
        for root in ["/Volumes/Wispnet", "/Users/example/Downloads", "/tmp/build"] {
            let executable = Path::new(root).join("Wispnet.app/Contents/MacOS/wispnet-node");
            assert!(
                installed_app(&executable, home)
                    .unwrap_err()
                    .to_string()
                    .contains("Move Wispnet")
            );
        }
        assert!(
            installed_app(Path::new("/tmp/target/debug/wispnet"), home)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn first_launch_preserves_existing_configuration_and_refuses_orphaned_state() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let config = home.join(".config/wispnet-node/host.toml");
        let node = Path::new("/does-not-exist");
        atomic(
            &config,
            b"existing configuration is preserved byte for byte",
            0o600,
        )
        .unwrap();
        initialize_macos_host(home, &config, node).unwrap();
        assert_eq!(
            fs::read(&config).unwrap(),
            b"existing configuration is preserved byte for byte"
        );
        fs::remove_file(&config).unwrap();
        private_dir(&home.join(".local/state/wispnet-node")).unwrap();
        assert!(
            initialize_macos_host(home, &config, node)
                .unwrap_err()
                .to_string()
                .contains("State exists without configuration")
        );
        assert!(!config.exists());
    }

    #[test]
    fn mac_metadata_rejects_invalid_versions_before_installation() {
        let valid =
            json!({"source_count":"42","version":"0.1.0","build_identity":"0.1.0+g123456789abc"});
        assert_eq!(
            mac_build_metadata(&valid).unwrap(),
            ("42".into(), "0.1.0".into(), "0.1.0+g123456789abc".into())
        );
        for (field, value) in [
            ("source_count", json!(true)),
            ("source_count", json!("-1")),
            ("version", json!("0.1.0; injected")),
            ("version", json!("0..1")),
            ("build_identity", Value::Null),
            ("build_identity", json!("")),
        ] {
            let mut manifest = valid.clone();
            manifest[field] = value;
            assert!(
                mac_build_metadata(&manifest).is_err(),
                "accepted invalid {field}"
            );
        }
        let temporary = tempfile::tempdir().unwrap();
        let mut manifest = valid;
        manifest["format"] = json!(1);
        manifest["license"] = json!("Apache-2.0");
        manifest["platform"] = json!("macos-aarch64");
        manifest["desktop"] = json!("flutter");
        manifest["version"] = json!("invalid");
        fs::write(
            temporary.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(
            verify(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("build metadata")
        );
    }
    #[test]
    fn manifest_rejects_corruption_and_unlisted_payload() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        atomic(&root.join("bin/wispnet"), b"native", 0o755).unwrap();
        atomic(
            &root.join("scripts/install_node_desktop.sh"),
            b"shell",
            0o755,
        )
        .unwrap();
        let m = json!({"format":1,"license":"Apache-2.0","platform":"linux-x86_64","sha256":{"bin/wispnet":format!("{:x}",Sha256::digest(b"native")),"scripts/install_node_desktop.sh":format!("{:x}",Sha256::digest(b"shell"))}});
        atomic(
            &root.join("manifest.json"),
            &serde_json::to_vec(&m).unwrap(),
            0o644,
        )
        .unwrap();
        assert!(verify(root).is_ok());
        fs::write(root.join("extra.py"), "unexpected").unwrap();
        assert!(verify(root).unwrap_err().to_string().contains("manifest"));
        fs::remove_file(root.join("extra.py")).unwrap();
        fs::write(root.join("bin/wispnet"), "corrupt").unwrap();
        assert!(
            verify(root)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }
    #[test]
    fn ssh_removal_preserves_other_entries_and_rejects_incomplete_blocks() {
        let temp = tempfile::tempdir().unwrap();
        private_dir(&temp.path().join(".ssh")).unwrap();
        let config = temp.path().join(".ssh/config");
        let peer = "a".repeat(64);
        let original = format!(
            "Host personal\r\n  HostName example.com\r\n# Wispnet bridge {peer} begin\nHost managed\n# Wispnet bridge {peer} end\n"
        );
        atomic(&config, original.as_bytes(), 0o600).unwrap();
        remove_ssh(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "Host personal\r\n  HostName example.com\r\n"
        );
        let broken = format!("# Wispnet bridge {peer} begin\nHost managed\n");
        atomic(&config, broken.as_bytes(), 0o600).unwrap();
        assert!(remove_ssh(temp.path()).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), broken);
    }
}

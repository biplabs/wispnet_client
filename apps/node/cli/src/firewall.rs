//! Root-only, socket-verified firewall reconciliation for the installed host.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CHAIN: &str = "WISPNET-UDP";
const CONFIG: &str = "/etc/wispnet-firewall.json";
const STATUS: &str = "/run/wispnet-firewall/status.json";
const MAX_REPORT: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Family {
    Ipv4,
    Ipv6,
}
impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
        }
    }
}
type Port = (u32, Family, u16);

fn read_json(path: &Path, owner: u32, private: bool) -> Result<Value, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        return Err("Unsafe report/config permissions".into());
    }
    let limit = if private { MAX_REPORT } else { 16384 };
    let mut data = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > limit {
        return Err("Oversized report/config".into());
    }
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}

fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
    })
}

fn configured_uids(path: &Path) -> Result<BTreeSet<u32>, String> {
    let value = read_json(path, 0, false)?;
    if !exact_keys(&value, &["version", "uids"]) || value["version"].as_u64() != Some(1) {
        return Err("Invalid helper configuration".into());
    }
    let uids = value["uids"]
        .as_array()
        .filter(|uids| uids.len() <= 32)
        .ok_or("Invalid allowed users")?;
    uids.iter()
        .map(|uid| {
            uid.as_u64()
                .filter(|uid| *uid > 0 && *uid <= u32::MAX as u64)
                .map(|uid| uid as u32)
                .ok_or_else(|| "Invalid allowed users".into())
        })
        .collect()
}

fn start_time(path: &Path) -> Result<String, String> {
    let stat = fs::read_to_string(path).map_err(|e| e.to_string())?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or_else(|| "Invalid process generation".into())
}

fn verified_ports(uid: u32, proc: &Path, runtime: &Path) -> Result<BTreeSet<Port>, String> {
    let directory = runtime.join(uid.to_string());
    let metadata = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("Unsafe runtime directory".into());
    }
    let report = read_json(&directory.join("wispnet-firewall.json"), uid, true)?;
    if !exact_keys(&report, &["version", "pid", "start_time", "ports"])
        || report["version"].as_u64() != Some(1)
    {
        return Err("Invalid socket report".into());
    }
    let pid = report["pid"]
        .as_u64()
        .filter(|pid| *pid > 1 && *pid < 1 << 31)
        .ok_or("Invalid process")?;
    let process = proc.join(pid.to_string());
    let status = fs::read_to_string(process.join("status")).map_err(|e| e.to_string())?;
    let uids: Vec<u32> = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or("Process user mismatch")?
        .split_whitespace()
        .map(|v| v.parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|_| "Process user mismatch")?;
    if uids != [uid; 4] {
        return Err("Process user mismatch".into());
    }
    let expected = format!(
        "0::/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/wispnet-host.service"
    );
    if fs::read_to_string(process.join("cgroup"))
        .map_err(|e| e.to_string())?
        .trim()
        != expected
    {
        return Err("Not the Wispnet host unit".into());
    }
    let generation = report["start_time"]
        .as_str()
        .ok_or("Invalid process generation")?;
    if start_time(&process.join("stat"))? != generation {
        return Err("Reused process ID".into());
    }
    let namespace = fs::metadata(process.join("ns/net")).map_err(|e| e.to_string())?;
    let own_namespace = fs::metadata(proc.join("self/ns/net")).map_err(|e| e.to_string())?;
    if namespace.dev() != own_namespace.dev() || namespace.ino() != own_namespace.ino() {
        return Err("Different network namespace".into());
    }
    let entries = report["ports"]
        .as_array()
        .filter(|entries| entries.len() <= 2)
        .ok_or("Too many sockets")?;
    let mut requested = BTreeSet::new();
    for item in entries {
        if !exact_keys(item, &["family", "port"]) {
            return Err("Invalid UDP socket".into());
        }
        let family = match item["family"].as_str() {
            Some("ipv4") => Family::Ipv4,
            Some("ipv6") => Family::Ipv6,
            _ => return Err("Invalid UDP socket".into()),
        };
        let port = item["port"]
            .as_u64()
            .filter(|port| (1024..=65535).contains(port))
            .ok_or("Invalid UDP socket")?;
        requested.insert((uid, family, port as u16));
    }
    let mut inodes = BTreeSet::new();
    for descriptor in fs::read_dir(process.join("fd")).map_err(|e| e.to_string())? {
        let descriptor = descriptor.map_err(|e| e.to_string())?;
        let target = match fs::read_link(descriptor.path()) {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|s| s.strip_prefix("socket:["))
            .and_then(|s| s.strip_suffix(']'))
            && !inode.is_empty()
            && inode.bytes().all(|b| b.is_ascii_digit())
        {
            inodes.insert(inode.to_string());
        }
    }
    let mut owned = BTreeSet::new();
    for (family, table) in [(Family::Ipv4, "udp"), (Family::Ipv6, "udp6")] {
        let source =
            fs::read_to_string(process.join("net").join(table)).map_err(|e| e.to_string())?;
        for line in source.lines().skip(1) {
            let columns: Vec<_> = line.split_whitespace().collect();
            if columns.len() < 10 {
                return Err("Invalid UDP socket table".into());
            }
            if !inodes.contains(columns[9])
                || columns[7]
                    .parse::<u32>()
                    .map_err(|_| "Invalid socket owner")?
                    != uid
            {
                continue;
            }
            let (address, port) = columns[1]
                .split_once(':')
                .ok_or("Invalid UDP socket table")?;
            if !loopback(family, address)? {
                owned.insert((
                    uid,
                    family,
                    u16::from_str_radix(port, 16).map_err(|_| "Invalid socket port")?,
                ));
            }
        }
    }
    if !requested.is_subset(&owned) {
        return Err("Reported port is not an owned UDP socket".into());
    }
    // Recheck generation after walking descriptors to close process-ID reuse races.
    if start_time(&process.join("stat"))? != generation {
        return Err("Process changed during verification".into());
    }
    Ok(requested)
}

fn loopback(family: Family, address: &str) -> Result<bool, String> {
    let size = match family {
        Family::Ipv4 => 4,
        Family::Ipv6 => 16,
    };
    if address.len() != size * 2 || !address.is_ascii() {
        return Err("Invalid UDP socket address".into());
    }
    let mut bytes = Vec::with_capacity(size);
    for offset in (0..address.len()).step_by(2) {
        bytes.push(
            u8::from_str_radix(&address[offset..offset + 2], 16)
                .map_err(|_| "Invalid UDP socket address")?,
        );
    }
    for word in bytes.chunks_exact_mut(4) {
        word.reverse();
    }
    Ok(match family {
        Family::Ipv4 => {
            Ipv4Addr::from(<[u8; 4]>::try_from(bytes.as_slice()).unwrap()).is_loopback()
        }
        Family::Ipv6 => {
            Ipv6Addr::from(<[u8; 16]>::try_from(bytes.as_slice()).unwrap()).is_loopback()
        }
    })
}

struct Reply {
    success: bool,
    stdout: String,
}
trait FirewallCommands {
    fn available(&self, family: Family) -> bool;
    fn call(&mut self, family: Family, args: &[String], check: bool) -> Result<Reply, String>;
}
struct Firewall {
    backends: BTreeMap<Family, PathBuf>,
}
impl Firewall {
    fn new() -> Result<Self, String> {
        let mut backends = BTreeMap::new();
        for (family, name) in [(Family::Ipv4, "iptables"), (Family::Ipv6, "ip6tables")] {
            for directory in ["/usr/sbin", "/usr/bin", "/sbin"] {
                let path = Path::new(directory).join(name);
                let metadata = match fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.to_string()),
                };
                if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                    return Err("Unsafe firewall executable".into());
                }
                backends.insert(family, path);
                break;
            }
        }
        if !backends.contains_key(&Family::Ipv4) {
            return Err("iptables is required".into());
        }
        Ok(Self { backends })
    }
}
impl FirewallCommands for Firewall {
    fn available(&self, family: Family) -> bool {
        self.backends.contains_key(&family)
    }
    fn call(&mut self, family: Family, args: &[String], check: bool) -> Result<Reply, String> {
        // Files avoid pipe-buffer deadlocks while preserving a hard process timeout.
        let mut stdout = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut stderr = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut child = Command::new(
            self.backends
                .get(&family)
                .ok_or("Missing firewall backend")?,
        )
        .args(["-w", "5"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(|e| e.to_string())?)
        .stderr(stderr.try_clone().map_err(|e| e.to_string())?)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C")
        .spawn()
        .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Firewall command timed out".into());
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
        };
        use std::io::{Seek, SeekFrom};
        stdout.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        stderr.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut output = String::new();
        (&mut stdout)
            .take(1_048_577)
            .read_to_string(&mut output)
            .map_err(|e| e.to_string())?;
        if output.len() > 1_048_576 {
            return Err("Oversized firewall command response".into());
        }
        if check && !status.success() {
            let mut diagnostic = String::new();
            (&mut stderr)
                .take(4096)
                .read_to_string(&mut diagnostic)
                .map_err(|e| e.to_string())?;
            return Err(format!("Firewall command failed: {}", diagnostic.trim()));
        }
        Ok(Reply {
            success: status.success(),
            stdout: output,
        })
    }
}

fn words(values: &[&str]) -> Vec<String> {
    values.iter().map(|word| (*word).to_owned()).collect()
}
fn rule(uid: u32, port: u16) -> Vec<String> {
    [
        words(&["-p", "udp", "-m", "udp", "--dport"]),
        vec![port.to_string()],
        words(&["-m", "comment", "--comment"]),
        vec![format!("wispnet-uid-{uid}")],
        words(&["-j", "ACCEPT"]),
    ]
    .concat()
}
// iptables -S uses shell quoting for comments; parse it without running a shell.
fn shell_words(source: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in source.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else {
                word.push(ch);
            }
            continue;
        }
        if ch == '\\' {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(mark) = quote {
            if ch == mark {
                quote = None;
            } else {
                word.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                started = true;
            }
            ch if ch.is_whitespace() => {
                if started {
                    result.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }
    if escaped || quote.is_some() {
        return Err("Invalid firewall rule quoting".into());
    }
    if started {
        result.push(word);
    }
    Ok(result)
}

fn reconcile(
    firewall: &mut impl FirewallCommands,
    desired: &BTreeSet<Port>,
    cleanup: bool,
) -> Result<(), String> {
    for family in [Family::Ipv4, Family::Ipv6] {
        if !firewall.available(family) {
            if desired.iter().any(|(_, f, _)| *f == family) {
                return Err("Missing IPv6 firewall backend".into());
            }
            continue;
        }
        let current = firewall.call(family, &words(&["-S", CHAIN]), false)?;
        let mut current_rules = BTreeSet::new();
        if !current.success {
            if cleanup {
                continue;
            }
            firewall.call(family, &words(&["-N", CHAIN]), true)?;
        } else {
            for line in current.stdout.lines() {
                let tokens = shell_words(line)?;
                if tokens == words(&["-N", CHAIN]) {
                    continue;
                }
                if tokens.len() != 14 || tokens[..2] != words(&["-A", CHAIN]) {
                    return Err("Foreign rule in Wispnet chain".into());
                }
                let uid_text = tokens[11]
                    .strip_prefix("wispnet-uid-")
                    .ok_or("Foreign rule in Wispnet chain")?;
                if uid_text.starts_with('0') || !uid_text.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("Foreign rule in Wispnet chain".into());
                }
                let uid = uid_text
                    .parse::<u32>()
                    .map_err(|_| "Foreign rule in Wispnet chain")?;
                let port = tokens[7]
                    .parse::<u16>()
                    .map_err(|_| "Foreign rule in Wispnet chain")?;
                if tokens[2..] != rule(uid, port) || port < 1024 {
                    return Err("Foreign rule in Wispnet chain".into());
                }
                current_rules.insert((uid, family, port));
            }
        }
        let wanted: BTreeSet<_> = if cleanup {
            BTreeSet::new()
        } else {
            desired
                .iter()
                .filter(|(_, f, _)| *f == family)
                .copied()
                .collect()
        };
        for (uid, _, port) in wanted.difference(&current_rules) {
            firewall.call(
                family,
                &[words(&["-A", CHAIN]), rule(*uid, *port)].concat(),
                true,
            )?;
        }
        for (uid, _, port) in current_rules.difference(&wanted) {
            firewall.call(
                family,
                &[words(&["-D", CHAIN]), rule(*uid, *port)].concat(),
                true,
            )?;
        }
        let input = firewall.call(family, &words(&["-S", "INPUT"]), true)?;
        let first = input
            .stdout
            .lines()
            .find(|line| line.starts_with("-A INPUT "))
            .map(shell_words)
            .transpose()?;
        if cleanup || first.as_deref() != Some(&words(&["-A", "INPUT", "-j", CHAIN])) {
            let mut removals = 0;
            while firewall
                .call(family, &words(&["-C", "INPUT", "-j", CHAIN]), false)?
                .success
            {
                firewall.call(family, &words(&["-D", "INPUT", "-j", CHAIN]), true)?;
                removals += 1;
                if removals > 1024 {
                    return Err("Too many Wispnet firewall jumps".into());
                }
            }
            if !cleanup {
                firewall.call(family, &words(&["-I", "INPUT", "1", "-j", CHAIN]), true)?;
            }
        }
        if cleanup {
            firewall.call(family, &words(&["-X", CHAIN]), true)?;
        }
    }
    Ok(())
}

fn publish(
    path: &Path,
    healthy: bool,
    ports: &BTreeSet<Port>,
    uids: &BTreeSet<u32>,
    error: Option<&str>,
) -> Result<(), String> {
    let value = json!({"version": 1, "healthy": healthy, "uids": uids, "ports": ports.iter().map(|(uid, family, port)| json!({"uid": uid, "family": family.name(), "port": port})).collect::<Vec<_>>(), "updated": SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs_f64(), "error": error});
    let directory = path.parent().ok_or("Missing firewall status directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|e| e.to_string())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, &value).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn run_args(args: &[OsString]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("Usage: wispnet firewall-helper".into());
    }
    if !cfg!(target_os = "linux") {
        return Err("The firewall helper requires Linux".into());
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("The firewall helper must run as root".into());
    }
    let status = Path::new(STATUS);
    let directory = status.parent().unwrap();
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let metadata = fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err("Unsafe firewall status directory".into());
    }
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("lock"))
        .map_err(|e| e.to_string())?;
    let lock_metadata = lock.metadata().map_err(|e| e.to_string())?;
    if !lock_metadata.is_file() || lock_metadata.uid() != 0 || lock_metadata.mode() & 0o022 != 0 {
        return Err("Unsafe firewall lock".into());
    }
    // SAFETY: lock is a live descriptor and flock does not retain a pointer.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Firewall helper is already running".into());
    }
    let mut firewall = Firewall::new()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let term = signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())
        .map_err(|e| e.to_string())?;
    let interrupt = signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        while !stopping.load(Ordering::Relaxed) {
            let mut uids = BTreeSet::new();
            let iteration = (|| {
                uids = configured_uids(Path::new(CONFIG))?;
                let mut desired = BTreeSet::new();
                for uid in &uids {
                    // Missing, stopped or invalid nodes lose their allow rules.
                    if let Ok(ports) =
                        verified_ports(*uid, Path::new("/proc"), Path::new("/run/user"))
                    {
                        desired.extend(ports);
                    }
                }
                reconcile(&mut firewall, &desired, false)?;
                publish(status, true, &desired, &uids, None)
            })();
            if let Err(error) = iteration {
                let _ = reconcile(&mut firewall, &BTreeSet::new(), false);
                publish(status, false, &BTreeSet::new(), &uids, Some(&error))?;
                eprintln!("{error}");
            }
            for _ in 0..10 {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    })();
    let cleanup = reconcile(&mut firewall, &BTreeSet::new(), true);
    let published = publish(
        status,
        false,
        &BTreeSet::new(),
        &BTreeSet::new(),
        Some("helper stopped"),
    );
    signal_hook::low_level::unregister(term);
    signal_hook::low_level::unregister(interrupt);
    outcome.and(cleanup).and(published)
}

const BINARY: &str = "/usr/local/libexec/wispnet-firewall";
const LEGACY_BINARY: &str = "/usr/local/libexec/wispnet-firewall.py";
const UNIT: &str = "/etc/systemd/system/wispnet-firewall.service";
const SERVICE: &str = "wispnet-firewall.service";

fn require_administrator(uid: Option<u32>) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("Firewall administration requires Linux".into());
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 || uid == Some(0) {
        return Err("Run as administrator and specify a non-root user ID".into());
    }
    Ok(())
}

fn username(uid: u32) -> Result<String, String> {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 65536];
    // SAFETY: record, result and buffer are valid writable allocations for this call.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() {
        return Err(format!("No local user for UID {uid}"));
    }
    // SAFETY: a successful getpwuid_r initializes record and points pw_name into buffer.
    let record = unsafe { record.assume_init() };
    if record.pw_name.is_null() {
        return Err("Invalid local user record".into());
    }
    // SAFETY: pw_name is a NUL-terminated string owned by the live buffer.
    unsafe { std::ffi::CStr::from_ptr(record.pw_name) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "Invalid local user name".into())
}

fn parse_admin_args(args: &[OsString], legacy: bool) -> Result<(u32, Option<u16>), String> {
    let mut uid = None;
    let mut port = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].to_str().ok_or("Invalid firewall arguments")?;
        index += 1;
        let value = args
            .get(index)
            .and_then(|s| s.to_str())
            .ok_or("Missing firewall argument value")?;
        index += 1;
        match flag {
            "--uid" if uid.is_none() => {
                uid = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|uid| *uid > 0)
                        .ok_or("Specify a non-root user ID")?,
                )
            }
            "--remove-legacy-port" if legacy && port.is_none() => {
                port = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port >= 1024)
                        .ok_or("Legacy UDP port must be unprivileged")?,
                )
            }
            _ => return Err(format!("Unknown or repeated firewall argument: {flag}")),
        }
    }
    Ok((uid.ok_or("--uid is required")?, port))
}

fn exists_or_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn write_root(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("Administrator destination must be absolute".into());
    }
    let parent = path
        .parent()
        .ok_or("Missing administrator destination directory")?;
    // Walk from / downwards, validating before creating descendants or replacing files.
    let mut parents: Vec<_> = parent.ancestors().collect();
    parents.reverse();
    for directory in parents {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(directory).map_err(|e| e.to_string())?;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                    .map_err(|e| e.to_string())?;
                fs::symlink_metadata(directory).map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "Unsafe destination directory: {}",
                directory.display()
            ));
        }
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!("Refusing symlink destination: {}", path.display()));
    }
    let mut file = tempfile::Builder::new()
        .prefix(".wispnet-")
        .tempfile_in(parent)
        .map_err(|e| e.to_string())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    file.write_all(content).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

fn save_uids(path: &Path, uids: &BTreeSet<u32>) -> Result<(), String> {
    let mut data =
        serde_json::to_vec(&json!({"version": 1, "uids": uids})).map_err(|e| e.to_string())?;
    data.push(b'\n');
    write_root(path, &data, 0o600)
}

fn systemctl(args: &[&str], check: bool) -> Result<std::process::Output, String> {
    let output = Command::new("/usr/bin/systemctl")
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C")
        .output()
        .map_err(|e| e.to_string())?;
    if check && !output.status.success() {
        return Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn wait_ready(uid: u32, require_ports: bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(status) = read_json(Path::new(STATUS), 0, false) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_secs_f64();
            let ready = status["healthy"].as_bool() == Some(true)
                && status["uids"]
                    .as_array()
                    .is_some_and(|uids| uids.iter().any(|u| u.as_u64() == Some(uid as u64)))
                && status["updated"]
                    .as_f64()
                    .is_some_and(|updated| (0.0..5.0).contains(&(now - updated)))
                && (!require_ports
                    || status["ports"].as_array().is_some_and(|ports| {
                        ports.iter().any(|p| p["uid"].as_u64() == Some(uid as u64))
                    }));
            if ready {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("Helper did not become ready; inspect journalctl -u wispnet-firewall.service. Manual UFW rules were preserved.".into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub fn install_args(args: &[OsString]) -> Result<(), String> {
    let (uid, legacy_port) = parse_admin_args(args, true)?;
    require_administrator(Some(uid))?;
    username(uid)?;
    Firewall::new()?;
    let config = Path::new(CONFIG);
    let mut uids = if exists_or_symlink(config) {
        configured_uids(config)?
    } else {
        BTreeSet::new()
    };
    uids.insert(uid);
    if uids.len() > 32 {
        return Err("Too many configured users".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    write_root(
        Path::new(BINARY),
        &fs::read(executable).map_err(|e| e.to_string())?,
        0o755,
    )?;
    save_uids(config, &uids)?;
    write_root(
        Path::new(UNIT),
        include_bytes!("../../../../deploy/node/wispnet-firewall.service"),
        0o644,
    )?;
    systemctl(&["daemon-reload"], true)?;
    systemctl(&["enable", SERVICE], true)?;
    systemctl(&["restart", SERVICE], true)?;
    wait_ready(uid, legacy_port.is_some())?;
    if let Some(port) = legacy_port {
        let ufw = ["/usr/sbin/ufw", "/usr/bin/ufw", "/sbin/ufw"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .ok_or("UFW not found; helper installed, legacy rule unchanged")?;
        let metadata = fs::metadata(ufw).map_err(|e| e.to_string())?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("Unsafe UFW executable; legacy rule unchanged".into());
        }
        let status = Command::new(ufw)
            .args(["--force", "delete", "allow", &format!("{port}/udp")])
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LANG", "C")
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("UFW legacy rule removal failed".into());
        }
    }
    // Remove the obsolete implementation only after the native helper is healthy.
    match fs::remove_file(LEGACY_BINARY) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.to_string()),
    }
    println!("Firewall helper installed. Dynamic node UDP ports are managed automatically.");
    Ok(())
}

pub fn uninstall_args(args: &[OsString]) -> Result<(), String> {
    let (uid, _) = parse_admin_args(args, false)?;
    require_administrator(Some(uid))?;
    let config = Path::new(CONFIG);
    let mut remaining = if exists_or_symlink(config) {
        configured_uids(config)?
    } else {
        BTreeSet::new()
    };
    remaining.remove(&uid);
    if !remaining.is_empty() {
        save_uids(config, &remaining)?;
        systemctl(&["restart", SERVICE], true)?;
    } else {
        let active = systemctl(&["is-active", "--quiet", SERVICE], false)?
            .status
            .success();
        if exists_or_symlink(Path::new(UNIT)) || active {
            systemctl(&["disable", "--now", SERVICE], true)?;
        }
        if systemctl(&["is-active", "--quiet", SERVICE], false)?
            .status
            .success()
        {
            return Err("Firewall helper is still running; files were preserved".into());
        }
        reconcile(&mut Firewall::new()?, &BTreeSet::new(), true)?;
        for path in [UNIT, BINARY, LEGACY_BINARY, CONFIG] {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.to_string()),
            }
        }
        systemctl(&["daemon-reload"], true)?;
    }
    println!("Firewall access removed for this user; shared helper retained only if needed.");
    Ok(())
}

pub fn package_configure(uid: u32) -> Result<(), String> {
    require_administrator(Some(uid))?;
    username(uid)?;
    if exists_or_symlink(Path::new(UNIT)) {
        return Err("A standalone firewall helper overrides the package. Remove that helper with its uninstaller first.".into());
    }
    let config = Path::new(CONFIG);
    let mut uids = if exists_or_symlink(config) {
        configured_uids(config)?
    } else {
        BTreeSet::new()
    };
    uids.insert(uid);
    if uids.len() > 32 {
        return Err("Too many configured users".into());
    }
    Firewall::new()?;
    save_uids(config, &uids)?;
    systemctl(&["enable", "--now", SERVICE], true)?;
    systemctl(&["restart", SERVICE], true)?;
    wait_ready(uid, false)
}

pub fn package_remove() -> Result<(), String> {
    require_administrator(None)?;
    let runtime = Path::new("/run/user");
    if runtime.exists() {
        for entry in fs::read_dir(runtime).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let Some(uid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if !entry.path().join("systemd/private").exists() {
                continue;
            }
            let user = username(uid)?;
            let machine = format!("{user}@.host");
            for unit in ["wispnet-desktop.service", "wispnet-host.service"] {
                let output = systemctl(
                    &[
                        "--user",
                        "--machine",
                        &machine,
                        "show",
                        "--property=FragmentPath",
                        "--value",
                        unit,
                    ],
                    true,
                )?;
                if String::from_utf8_lossy(&output.stdout).trim()
                    == format!("/usr/lib/systemd/user/{unit}")
                {
                    systemctl(&["--user", "--machine", &machine, "stop", unit], true)?;
                }
            }
        }
    }
    systemctl(&["disable", "--now", SERVICE], true)?;
    if systemctl(&["is-active", "--quiet", SERVICE], false)?
        .status
        .success()
    {
        return Err("Firewall helper is still running".into());
    }
    reconcile(&mut Firewall::new()?, &BTreeSet::new(), true)?;
    match fs::remove_file(CONFIG) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture {
        _temporary: tempfile::TempDir,
        proc: PathBuf,
        runtime: PathBuf,
        process: PathBuf,
        report: PathBuf,
        value: Value,
        uid: u32,
    }
    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let proc = temporary.path().join("proc");
            let runtime = temporary.path().join("run");
            let process = proc.join("4242");
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            for path in [
                process.join("fd"),
                process.join("net"),
                process.join("ns"),
                proc.join("self/ns"),
                runtime.join(uid.to_string()),
            ] {
                fs::create_dir_all(path).unwrap();
            }
            fs::set_permissions(
                runtime.join(uid.to_string()),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::write(proc.join("self/ns/net"), "").unwrap();
            fs::hard_link(proc.join("self/ns/net"), process.join("ns/net")).unwrap();
            fs::write(
                process.join("status"),
                format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .unwrap();
            let stat = format!(
                "4242 (wispnet host) {} 42",
                [&["S"][..], &["0"; 18][..]].concat().join(" ")
            );
            fs::write(process.join("stat"), stat).unwrap();
            fs::write(process.join("cgroup"), format!("0::/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/wispnet-host.service\n")).unwrap();
            symlink("socket:[1234]", process.join("fd/10")).unwrap();
            fs::write(
                process.join("net/udp"),
                format!("header\n0: 00000000:A080 00000000:0000 07 0:0 0:0 0 {uid} 0 1234\n"),
            )
            .unwrap();
            fs::write(process.join("net/udp6"), "header\n").unwrap();
            let report = runtime.join(uid.to_string()).join("wispnet-firewall.json");
            let value = json!({"version":1,"pid":4242,"start_time":"42","ports":[{"family":"ipv4","port":41088}]});
            let fixture = Self {
                _temporary: temporary,
                proc,
                runtime,
                process,
                report,
                value,
                uid,
            };
            fixture.save();
            fixture
        }
        fn save(&self) {
            fs::write(&self.report, serde_json::to_vec(&self.value).unwrap()).unwrap();
            fs::set_permissions(&self.report, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fn verify(&self) -> Result<BTreeSet<Port>, String> {
            verified_ports(self.uid, &self.proc, &self.runtime)
        }
    }

    #[test]
    fn verifies_live_owned_socket_and_accepts_stopped_relay_report() {
        let mut fixture = Fixture::new();
        assert_eq!(
            fixture.verify().unwrap(),
            BTreeSet::from([(fixture.uid, Family::Ipv4, 41088)])
        );
        fixture.value["ports"] = json!([]);
        fixture.save();
        assert!(fixture.verify().unwrap().is_empty());
    }

    #[test]
    fn rejects_wrong_service_stale_generation_and_foreign_user() {
        let fixture = Fixture::new();
        for (file, forged) in [
            ("cgroup", "0::/unrelated.service"),
            ("stat", "4242 (other) S 0 0"),
            ("status", "Uid: 4294967295 4294967295 4294967295 4294967295"),
        ] {
            let path = fixture.process.join(file);
            let old = fs::read(&path).unwrap();
            fs::write(&path, forged).unwrap();
            assert!(fixture.verify().is_err(), "forged {file}");
            fs::write(path, old).unwrap();
        }
    }

    #[test]
    fn rejects_unowned_ports_privileged_ports_and_foreign_descriptors() {
        let mut fixture = Fixture::new();
        for port in [443, 41089] {
            fixture.value["ports"][0]["port"] = json!(port);
            fixture.save();
            assert!(fixture.verify().is_err());
        }
        fixture.value["ports"][0]["port"] = json!(41088);
        fixture.save();
        fs::remove_file(fixture.process.join("fd/10")).unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_public_symlink_reports_and_other_network_namespaces() {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.report, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fixture.verify().is_err());
        fs::set_permissions(&fixture.report, fs::Permissions::from_mode(0o600)).unwrap();
        let original = fixture.report.with_extension("old");
        fs::rename(&fixture.report, &original).unwrap();
        symlink(&original, &fixture.report).unwrap();
        assert!(fixture.verify().is_err());
        fs::remove_file(&fixture.report).unwrap();
        fixture.save();
        fs::remove_file(fixture.process.join("ns/net")).unwrap();
        fs::write(fixture.process.join("ns/net"), "").unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_loopback_bindings_and_accepts_ipv6_wildcard() {
        let mut fixture = Fixture::new();
        let path = fixture.process.join("net/udp");
        fs::write(
            &path,
            format!(
                "header\n0: 0100007F:A080 0:0 07 0:0 0:0 0 {} 0 1234\n",
                fixture.uid
            ),
        )
        .unwrap();
        assert!(fixture.verify().is_err());
        fixture.value["ports"][0]["family"] = json!("ipv6");
        fixture.save();
        fs::write(
            fixture.process.join("net/udp6"),
            format!(
                "header\n0: 00000000000000000000000000000000:A080 0:0 07 0:0 0:0 0 {} 0 1234\n",
                fixture.uid
            ),
        )
        .unwrap();
        assert_eq!(
            fixture.verify().unwrap(),
            BTreeSet::from([(fixture.uid, Family::Ipv6, 41088)])
        );
        assert!(loopback(Family::Ipv6, "00000000000000000000000001000000").unwrap());
    }

    #[test]
    fn rejects_unknown_report_fields_booleans_and_oversized_reports() {
        let mut fixture = Fixture::new();
        fixture.value["ports"][0]["port"] = json!(true);
        fixture.save();
        assert!(fixture.verify().is_err());
        fixture.value["ports"][0]["port"] = json!(41088);
        fixture.value["untrusted"] = json!("field");
        fixture.save();
        assert!(fixture.verify().is_err());
        fs::write(&fixture.report, vec![b' '; MAX_REPORT as usize + 1]).unwrap();
        assert!(fixture.verify().unwrap_err().contains("Oversized"));
    }

    #[derive(Default)]
    struct FakeFirewall {
        chains: BTreeMap<Family, BTreeMap<String, Vec<Vec<String>>>>,
    }
    impl FakeFirewall {
        fn new() -> Self {
            Self {
                chains: [Family::Ipv4, Family::Ipv6]
                    .into_iter()
                    .map(|f| {
                        (
                            f,
                            BTreeMap::from([(
                                "INPUT".to_owned(),
                                vec![words(&["-j", "ufw-before-input"])],
                            )]),
                        )
                    })
                    .collect(),
            }
        }
    }
    impl FirewallCommands for FakeFirewall {
        fn available(&self, family: Family) -> bool {
            self.chains.contains_key(&family)
        }
        fn call(&mut self, family: Family, args: &[String], check: bool) -> Result<Reply, String> {
            let chains = self.chains.get_mut(&family).unwrap();
            let op = args[0].as_str();
            let chain = &args[1];
            let rest = args[2..].to_vec();
            let mut success = true;
            let mut stdout = String::new();
            match op {
                "-S" => match chains.get(chain) {
                    None => success = false,
                    Some(rules) => {
                        if chain != "INPUT" {
                            stdout.push_str(&format!("-N {chain}\n"));
                        }
                        for rule in rules {
                            stdout.push_str(&format!("-A {chain} {}\n", rule.join(" ")));
                        }
                    }
                },
                "-N" => {
                    chains.insert(chain.clone(), vec![]);
                }
                "-C" => success = chains.get(chain).unwrap().contains(&rest),
                "-A" => chains.get_mut(chain).unwrap().push(rest),
                "-I" => {
                    chains
                        .get_mut(chain)
                        .unwrap()
                        .insert(rest[0].parse::<usize>().unwrap() - 1, rest[1..].to_vec());
                }
                "-D" => {
                    let rules = chains.get_mut(chain).unwrap();
                    if let Some(index) = rules.iter().position(|rule| *rule == rest) {
                        rules.remove(index);
                    } else {
                        success = false;
                    }
                }
                "-X" => {
                    assert!(chains.get(chain).unwrap().is_empty());
                    chains.remove(chain);
                }
                _ => panic!("unexpected command {args:?}"),
            }
            if check && !success {
                return Err("fake backend failure".into());
            }
            Ok(Reply { success, stdout })
        }
    }

    #[test]
    fn reconciles_port_changes_user_removal_reload_and_full_cleanup() {
        let mut firewall = FakeFirewall::new();
        let first = BTreeSet::from([
            (1000, Family::Ipv4, 41088),
            (1001, Family::Ipv4, 43000),
            (1000, Family::Ipv6, 41088),
        ]);
        reconcile(&mut firewall, &first, false).unwrap();
        reconcile(&mut firewall, &first, false).unwrap();
        assert_eq!(firewall.chains[&Family::Ipv4][CHAIN].len(), 2);
        firewall
            .chains
            .get_mut(&Family::Ipv4)
            .unwrap()
            .get_mut("INPUT")
            .unwrap()
            .reverse();
        reconcile(
            &mut firewall,
            &BTreeSet::from([(1000, Family::Ipv4, 42000), (1001, Family::Ipv4, 43000)]),
            false,
        )
        .unwrap();
        assert_eq!(
            firewall.chains[&Family::Ipv4]["INPUT"],
            vec![words(&["-j", CHAIN]), words(&["-j", "ufw-before-input"])]
        );
        assert_eq!(
            firewall.chains[&Family::Ipv4][CHAIN],
            vec![rule(1001, 43000), rule(1000, 42000)]
        );
        assert!(firewall.chains[&Family::Ipv6][CHAIN].is_empty());
        reconcile(&mut firewall, &BTreeSet::new(), false).unwrap();
        assert!(firewall.chains[&Family::Ipv4][CHAIN].is_empty());
        reconcile(&mut firewall, &BTreeSet::new(), true).unwrap();
        reconcile(&mut firewall, &BTreeSet::new(), true).unwrap();
        assert!(!firewall.chains[&Family::Ipv4].contains_key(CHAIN));
        assert_eq!(
            firewall.chains[&Family::Ipv4]["INPUT"],
            vec![words(&["-j", "ufw-before-input"])]
        );
    }

    #[test]
    fn foreign_rules_prevent_any_chain_changes() {
        for foreign in [words(&["-j", "DROP"]), rule(1000, 443), {
            let mut rule = rule(1000, 41088);
            *rule.last_mut().unwrap() = "DROP".into();
            rule
        }] {
            let mut firewall = FakeFirewall::new();
            firewall
                .chains
                .get_mut(&Family::Ipv4)
                .unwrap()
                .insert(CHAIN.into(), vec![foreign.clone()]);
            assert!(reconcile(&mut firewall, &BTreeSet::new(), true).is_err());
            assert_eq!(firewall.chains[&Family::Ipv4][CHAIN], vec![foreign]);
        }
    }

    #[test]
    fn missing_ipv6_backend_fails_when_ipv6_rule_is_required() {
        let mut firewall = FakeFirewall::new();
        firewall.chains.remove(&Family::Ipv6);
        assert!(
            reconcile(
                &mut firewall,
                &BTreeSet::from([(1000, Family::Ipv6, 41088)]),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn status_publication_is_atomic_readable_and_preserves_per_user_ports() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("status.json");
        publish(
            &path,
            true,
            &BTreeSet::from([(1001, Family::Ipv6, 42000), (1000, Family::Ipv4, 41088)]),
            &BTreeSet::from([1000, 1001]),
            None,
        )
        .unwrap();
        let status: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(status["uids"], json!([1000, 1001]));
        assert_eq!(
            status["ports"][0],
            json!({"uid":1000,"family":"ipv4","port":41088})
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        publish(
            &path,
            false,
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some("helper stopped"),
        )
        .unwrap();
        let status: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(status["healthy"], false);
        assert_eq!(status["error"], "helper stopped");
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn administration_rejects_missing_duplicate_root_and_privileged_inputs() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        for values in [
            vec![],
            vec!["--uid", "0"],
            vec!["--uid", "1000", "--uid", "1001"],
            vec!["--uid", "1000", "--remove-legacy-port", "443"],
            vec!["--uid", "true"],
        ] {
            assert!(parse_admin_args(&args(&values), true).is_err());
        }
        assert_eq!(
            parse_admin_args(
                &args(&["--uid", "1000", "--remove-legacy-port", "41088"]),
                true
            )
            .unwrap(),
            (1000, Some(41088))
        );
        assert!(
            parse_admin_args(
                &args(&["--uid", "1000", "--remove-legacy-port", "41088"]),
                false
            )
            .is_err()
        );
        assert!(run_args(&args(&["--config", "untrusted"])).is_err());
    }

    #[test]
    fn firewall_rule_parser_handles_quoted_comments_and_rejects_incomplete_input() {
        assert_eq!(
            shell_words("-A WISPNET-UDP --comment \"wispnet-uid-1000\"").unwrap(),
            words(&["-A", "WISPNET-UDP", "--comment", "wispnet-uid-1000"])
        );
        assert!(shell_words("--comment 'unterminated").is_err());
        assert!(shell_words("--comment trailing\\").is_err());
    }
}

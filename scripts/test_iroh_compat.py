#!/usr/bin/env python3
"""Test mixed Iroh versions on loopback; old dependencies stay in isolated fixtures."""

import contextlib
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
CACHE = Path(os.environ.get("WISPNET_COMPAT_TEST_CACHE", ROOT / "target/iroh-compat"))
SOURCE = ROOT / "scripts/fixtures/iroh_compat.rs"
PREVIOUS = "1.3.0"
CURRENT = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["dependencies"]["iroh"]["version"].removeprefix("=")
CURRENT_QUIC = next(
    package["version"] for package in tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
    if package["name"] == "noq"
)


def write_changed(path, text):
    if not path.exists() or path.read_text() != text:
        path.write_text(text)


def build(version):
    project = CACHE / version
    (project / "src").mkdir(parents=True, exist_ok=True)
    old = version == PREVIOUS
    quic = "1.3.0" if old else CURRENT_QUIC
    dependency = f'version = "={version}"' if old else f'path = {json.dumps(str(ROOT / "vendor/iroh"))}'
    manifest = f'''[package]
name = "wispnet-iroh-compat"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
iroh = {{ {dependency}, default-features = false, features = ["tls-ring"] }}
iroh-base = "={version}"
iroh-dns = "={version}"
iroh-relay = {{ version = "={version}", default-features = false, features = ["server", "tls-ring"] }}
noq = {{ version = "={quic}", default-features = false }}
noq-proto = {{ version = "={quic}", default-features = false }}
noq-udp = "={quic}"
tokio = {{ version = "=1.53.3", features = ["macros", "rt-multi-thread", "net", "time", "io-util"] }}
serde_json = "=1.0.150"
rcgen = "=0.14.8"
rustls = {{ version = "=0.23.46", default-features = false, features = ["ring", "std"] }}
rustls-pki-types = "=1.15.1"
rustls-webpki = "=0.103.16"

[profile.dev]
debug = 0
'''
    write_changed(project / "Cargo.toml", manifest)
    write_changed(project / "src/main.rs", SOURCE.read_text())
    if not (project / "Cargo.lock").exists():
        # Seed compatible transitive versions from the real build. Cargo adjusts
        # the explicitly pinned old fixture only; no old code enters our workspace.
        shutil.copy2(ROOT / "Cargo.lock", project / "Cargo.lock")
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(project / "target")
    subprocess.run(
        ["cargo", "build", "--quiet", "--manifest-path", str(project / "Cargo.toml")],
        cwd=ROOT, env=env, check=True,
    )
    return project / "target/debug/wispnet-iroh-compat"


def read_report(process):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        if not selector.select(25):
            raise AssertionError("compat fixture readiness timed out")
    line = process.stdout.readline()
    if not line:
        raise AssertionError(f"compat fixture exited: {process.stderr.read()}")
    return json.loads(line)


@contextlib.contextmanager
def fixture(binary, *args):
    process = subprocess.Popen(
        [str(binary), *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True,
    )
    try:
        yield process
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()


def exchange(binaries, client, server, mode, relay_url="-", ca_file="-"):
    with fixture(binaries[server], "serve", mode, relay_url, ca_file) as host:
        ready = read_report(host)
        result = subprocess.run(
            [str(binaries[client]), "connect", mode, ready["id"],
             ready["address"] or "-", relay_url, ca_file],
            capture_output=True, text=True, timeout=45,
        )
        if result.returncode:
            raise AssertionError(
                f"{mode} compatibility {client} -> {server} failed: {result.stderr}"
            )
        connected = json.loads(result.stdout)
        accepted = read_report(host)
        assert connected["server_id"] == ready["id"]
        assert connected["client_id"] == accepted["remote_id"]
        assert connected["streams"] == 2
        assert connected["relay_path"] == (mode == "relay")
        assert host.wait(timeout=5) == 0


def main():
    subprocess.run(["rustfmt", "--edition", "2024", "--check", str(SOURCE)], check=True)
    binaries = {version: build(version) for version in (PREVIOUS, CURRENT)}
    for client, server in ((PREVIOUS, CURRENT), (CURRENT, PREVIOUS)):
        exchange(binaries, client, server, "direct")
    with tempfile.TemporaryDirectory(prefix="wispnet-iroh-compat-") as temporary:
        for relay_version in binaries:
            ca_file = str(Path(temporary) / f"{relay_version}.pem")
            with fixture(binaries[relay_version], "relay", ca_file) as relay:
                relay_url = read_report(relay)["relay_url"]
                for client in binaries:
                    for server in binaries:
                        exchange(binaries, client, server, "relay", relay_url, ca_file)
    print("Iroh compatibility: 2 mixed-version direct and 8 endpoint/relay combinations passed")


if __name__ == "__main__":
    main()

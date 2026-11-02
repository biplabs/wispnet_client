"""Exercise an extracted distribution without touching the user's installation."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

from package_linux import package, ROOT


class DistributionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workspace = tempfile.TemporaryDirectory(prefix="wispnet-package-")
        cls.archive = package(
            Path(
                os.environ.get(
                    "WISPNET_PACKAGE_TEST_BINARY", ROOT / "target/debug/wispnet"
                )
            ),
            Path(cls.workspace.name) / "bundle.tar.gz",
        )

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="wispnet-install-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        with tarfile.open(self.archive) as archive:
            archive.extractall(self.root, filter="data")
        self.bundle = self.root / "wispnet-linux-x86_64"
        self.home = self.root / "home"
        self.home.mkdir()
        self.env = dict(os.environ, HOME=str(self.home))
        self.installed = self.home / ".local/libexec/wispnet-node/wispnet"
        self.config = self.home / ".config/wispnet-node/host.toml"
        self.state = self.home / "state"

    def run_command(self, *args, ok=True):
        result = subprocess.run(
            args, env=self.env, capture_output=True, text=True, timeout=30
        )
        self.assertEqual(result.returncode == 0, ok, result.stderr)
        return result

    def install(self, ok=True):
        return self.run_command(
            sys.executable,
            str(self.bundle / "scripts/install_node_desktop.py"),
            "--headless",
            "--no-start",
            ok=ok,
        )

    def initialize(self):
        self.config.parent.mkdir(parents=True, exist_ok=True)
        self.config.write_text(
            f'schema_version=2\ndevice_name="package-test"\n'
            f'state_dir="{self.state}"\nlog_level="info"\n'
            '[transport]\nbind_addr="127.0.0.1:0"\n'
        )
        self.run_command(str(self.installed), "host-init", "--config", str(self.config))

    def status(self):
        return json.loads(
            self.run_command(
                str(self.installed), "host-status", "--config", str(self.config)
            ).stdout
        )

    def test_fresh_install_reinstall_and_missing_authority(self):
        manifest = json.loads((self.bundle / "manifest.json").read_text())
        self.assertEqual(manifest["license"], "Apache-2.0")
        for name in ["LICENSE", "NOTICE", "vendor/iroh/LICENSE-BSD3",
                     "vendor/noq-proto/LICENSE-APACHE", "vendor/noq-proto/LICENSE-MIT",
                     "apps/flutter/assets/fonts/Roboto_LICENSE.txt"]:
            self.assertEqual((self.bundle / name).read_bytes(), (ROOT / name).read_bytes())
            self.assertIn(name, manifest["sha256"])
        self.install()
        self.assertFalse(self.config.exists())
        self.assertFalse(self.state.exists())
        self.assertFalse((self.home / ".local/libexec/wispnet-desktop").exists())
        self.assertFalse(
            (self.home / ".config/systemd/user/default.target.wants").exists()
        )
        self.initialize()
        initial = self.status()
        peer_config = self.root / "peer.toml"
        peer_config.write_text(
            f'schema_version=1\ndevice_name="peer"\nstate_dir="{self.root / "peer"}"\nlog_level="info"\n'
        )
        peer = (
            self.run_command(
                str(self.installed), "identity-init", "--config", str(peer_config)
            )
            .stdout.split("Endpoint ID: ")[1]
            .strip()
        )
        for operation in [
            {"op": "approve", "peer": peer, "name": "Package client"},
            {
                "op": "service",
                "id": "website",
                "backend": "127.0.0.1:8080",
                "allow": [peer],
            },
        ]:
            operation["expected_revision"] = self.status()["revision"]
            request = self.root / "request.json"
            request.write_text(json.dumps(operation))
            self.run_command(
                str(self.installed),
                "host-mutate",
                "--config",
                str(self.config),
                "--request",
                str(request),
            )
        before = self.status()
        files = {p: p.read_bytes() for p in self.state.iterdir() if p.is_file()}
        config = self.config.read_bytes()
        self.install()
        self.assertEqual(self.status(), before)
        self.assertEqual(before["host"], initial["host"])
        self.assertEqual(before["generation"], initial["generation"])
        self.assertEqual(self.config.read_bytes(), config)
        for path, data in files.items():
            self.assertEqual(path.read_bytes(), data, path.name)
        daemon = subprocess.Popen(
            [str(self.installed), "agent", "--config", str(self.config)],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            deadline = time.monotonic() + 10
            while (
                not (self.state / "node.sock").exists()
                and daemon.poll() is None
                and time.monotonic() < deadline
            ):
                time.sleep(0.05)
            result = self.run_command(str(self.home / ".local/bin/wispnet-node"))
            self.assertEqual(json.loads(result.stdout)["host"]["host"], initial["host"])
        finally:
            daemon.terminate()
            daemon.wait(timeout=10)
        (self.state / "host.sqlite3").unlink()
        self.install()
        self.assertFalse((self.state / "host.sqlite3").exists())
        self.run_command(
            str(self.installed), "host-status", "--config", str(self.config), ok=False
        )
        self.run_command(
            str(self.installed), "host-init", "--config", str(self.config), ok=False
        )
        self.assertFalse((self.state / "host.sqlite3").exists())

    def test_gui_files_stage_native_launcher_without_qt(self):
        self.install()
        self.initialize()
        before = self.status()
        # Use an inert native ELF for files-only staging; GUI runtime is tested
        # separately through Flutter adapter/widget and native integration checks.
        archive = package(self.installed, self.root / "desktop.tar.gz", self.installed)
        with tarfile.open(archive) as source:
            source.extractall(self.root, filter="data")
        tools = self.root / "tools"
        tools.mkdir()
        systemctl = tools / "systemctl"
        systemctl.write_text(
            '#!/bin/sh\n[ "$*" = "--user is-active --quiet graphical-session.target" ]\n'
        )
        systemctl.chmod(0o755)
        self.env["PATH"] = str(tools) + os.pathsep + self.env["PATH"]
        self.run_command(
            sys.executable,
            str(self.bundle / "scripts/install_node_desktop.py"),
            "--no-start",
        )
        destination = self.home / ".local/libexec/wispnet-desktop"
        self.assertEqual(
            (self.home / ".local/bin/wispnet-desktop").resolve(),
            destination / "Wispnet",
        )
        self.assertFalse((destination / ".venv").exists())
        self.assertFalse((destination / "wispnet_desktop/app.py").exists())
        self.assertEqual(self.status(), before)
        result = subprocess.run(
            [sys.executable, str(destination / "desktop_data.py")],
            input='{"op":"operations"}',
            env=self.env,
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertEqual(json.loads(result.stdout), [])

    def test_corrupt_payload_is_rejected_before_install_mutation(self):
        self.install()
        self.initialize()
        installed = self.installed.read_bytes()
        status = self.status()
        helper = self.bundle / "scripts/node_request.py"
        helper.write_text(helper.read_text() + "\n# corrupted\n")
        result = self.install(ok=False)
        self.assertIn("checksum mismatch", result.stderr.lower())
        self.assertEqual(self.installed.read_bytes(), installed)
        self.assertEqual(self.status(), status)

    def test_legacy_config_upgrade_preserves_identity_and_installs_named_command(self):
        self.install()
        self.initialize()
        before = self.status()
        legacy = self.home / ".config/wipsnet-node/host.toml"
        legacy.parent.mkdir(parents=True, mode=0o700)
        self.config.rename(legacy)
        self.install()
        self.assertEqual(self.status(), before)
        self.assertEqual(self.config.read_bytes(), legacy.read_bytes())
        self.assertEqual(self.config.stat().st_mode & 0o077, 0)
        command = self.home / ".local/bin/wispnet"
        self.assertIn("--qrcode", self.run_command(str(command), "--help").stdout)
        result = self.run_command(str(command), "--qrcode", ok=False)
        self.assertIn("requires a terminal", result.stderr)

    def test_sidecar_and_payload_exclude_private_state(self):
        expected = (
            self.archive.with_name(self.archive.name + ".sha256").read_text().split()[0]
        )
        self.assertEqual(
            hashlib.sha256(self.archive.read_bytes()).hexdigest(), expected
        )
        manifest = json.loads((self.bundle / "manifest.json").read_text())
        self.assertEqual(manifest["platform"], "linux-x86_64")
        self.assertIn("bin/wispnet", manifest["sha256"])
        self.assertNotIn("apps/node/desktop/wispnet_desktop/app.py", manifest["sha256"])
        self.assertNotIn("apps/node/desktop/uv.lock", manifest["sha256"])
        for name, digest in manifest["sha256"].items():
            self.assertEqual(
                hashlib.sha256((self.bundle / name).read_bytes()).hexdigest(), digest
            )
            for private in [
                ".venv",
                "identity.key",
                "host.sqlite3",
                "host-owner.token",
            ]:
                self.assertNotIn(private, name)


if __name__ == "__main__":
    unittest.main()

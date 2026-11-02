"""Verify macOS packaging/install paths without claiming native macOS GUI coverage."""

import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import install_macos
import package_macos


class MacDistributionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workspace = tempfile.TemporaryDirectory(prefix="wispnet-macos-package-")
        cls.directory = Path(cls.workspace.name)
        app = cls.directory / "Wispnet.app"
        executable = app / "Contents/MacOS/Wispnet"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"inert desktop executable")
        (executable.parent / "build-identity").write_text("0.1.0+g123456789abc\n")
        resources = app / "Contents/Resources"
        resources.mkdir()
        with (app / "Contents/Info.plist").open("wb") as output:
            plistlib.dump({"CFBundleName": "Wispnet"}, output)
        framework = app / "Contents/Frameworks/Example.framework"
        (framework / "Versions/A").mkdir(parents=True)
        (framework / "Versions/A/Example").write_bytes(b"framework payload")
        (framework / "Versions/Current").symlink_to("A")
        (framework / "Example").symlink_to("Versions/Current/Example")
        cls.binary = Path(os.environ.get(
            "WISPNET_PACKAGE_TEST_BINARY", package_macos.ROOT / "target/debug/wispnet"
        ))

        def output(command, **kwargs):
            if command[0] == "lipo":
                return "x86_64\n"
            if command[0] == "python3":
                return "0.1.0+g123456789abc\n"
            if command[1:] == ["--version"]:
                return "wispnet 0.1.0+g123456789abc\n"
            if command[:2] == ["git", "rev-parse"]:
                return "123456789abc" + "0" * 28 + "\n"
            if command[:2] == ["git", "rev-list"]:
                return "42\n"
            if command[:2] == ["git", "show"]:
                return "1700000000\n"
            if command[:2] == ["git", "status"]:
                return b""
            raise AssertionError(command)

        real_run = subprocess.run

        def run(command, **kwargs):
            if command[0] == "codesign":
                return subprocess.CompletedProcess(command, 0)
            return real_run(command, **kwargs)

        # macOS's /var temporary directories resolve through /private/var.
        # Exercise the same aliased parent path on every test host.
        temporary_alias = cls.directory / "temporary"
        temporary_alias.symlink_to(cls.directory, target_is_directory=True)
        with patch.object(package_macos.tempfile, "tempdir", str(temporary_alias)), \
             patch.object(package_macos.platform, "system", return_value="Darwin"), \
             patch.object(package_macos.platform, "machine", return_value="x86_64"), \
             patch.object(package_macos.subprocess, "check_output", side_effect=output), \
             patch.object(package_macos.subprocess, "run", side_effect=run):
            cls.archive = package_macos.package(cls.binary, app, cls.directory / "bundle.tar.gz")

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def setUp(self):
        self.workspace = tempfile.TemporaryDirectory(prefix="wispnet-macos-install-")
        self.addCleanup(self.workspace.cleanup)
        self.directory = Path(self.workspace.name)
        with tarfile.open(self.archive) as archive:
            archive.extractall(self.directory, filter="data")
        self.bundle = self.directory / "wispnet-macos-x86_64"
        self.home = self.directory / "home"
        self.home.mkdir()
        self.installed = self.home / ".local/libexec/wispnet-node/wispnet"
        self.config = self.home / ".config/wispnet-node/host.toml"

    def install(self, *arguments):
        real_run = subprocess.run

        def run(command, **kwargs):
            if Path(command[0]).name == "wispnet":
                return real_run(command, **kwargs)
            if Path(command[0]).name in {"codesign", "sips", "iconutil", "lsregister"}:
                return subprocess.CompletedProcess(command, 0)
            raise AssertionError(f"Unexpected external command: {command}")

        with patch.object(install_macos, "ROOT", self.bundle), \
             patch.object(install_macos.sys, "platform", "darwin"), \
             patch.object(install_macos.platform, "machine", return_value="x86_64"), \
             patch.object(install_macos.sys, "argv", ["install_macos.py", "--no-start", *arguments]), \
             patch.dict(os.environ, HOME=str(self.home)), \
             patch.object(install_macos.subprocess, "check_output", side_effect=AssertionError("Checkout command in packaged installer")), \
             patch.object(install_macos.subprocess, "run", side_effect=run):
            install_macos.main()

    def status(self):
        result = subprocess.run([
            str(self.installed), "host-status", "--config", str(self.config)
        ], text=True, capture_output=True, check=True)
        return json.loads(result.stdout)

    def test_extracted_install_preserves_authority_and_framework_symlinks(self):
        self.install("--init")
        before = self.status()
        config = self.config.read_bytes()
        state = self.home / ".local/state/wispnet-node"
        original = {p: p.read_bytes() for p in state.iterdir() if p.is_file()}
        self.install()
        self.assertEqual(self.status(), before)
        self.assertEqual(self.config.read_bytes(), config)
        for path, data in original.items():
            self.assertEqual(path.read_bytes(), data, path.name)
        app = self.home / "Applications/Wispnet.app"
        framework = app / "Contents/Frameworks/Example.framework"
        self.assertTrue((framework / "Versions/Current").is_symlink())
        self.assertEqual((framework / "Example").read_bytes(), b"framework payload")
        with (app / "Contents/Info.plist").open("rb") as source:
            info = plistlib.load(source)
        self.assertEqual(info["CFBundleVersion"], "42")
        self.assertEqual(info["WispnetBuildIdentity"], "0.1.0+g123456789abc")
        self.assertEqual((app / "Contents/MacOS/python-path").read_text(), str(Path(sys.executable).resolve()))
        self.assertTrue((self.home / "Library/LaunchAgents/app.wispnet.node.plist").exists())

    def test_corrupt_payload_is_rejected_before_initializing_or_replacing_files(self):
        helper = self.bundle / "scripts/node_request.py"
        helper.write_text(helper.read_text() + "\n# corruption\n")
        with contextlib.redirect_stderr(io.StringIO()) as error, self.assertRaises(SystemExit):
            self.install("--init")
        self.assertIn("checksum mismatch", error.getvalue().lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_changed_framework_link_is_rejected_before_installation(self):
        link = self.bundle / "Wispnet.app/Contents/Frameworks/Example.framework/Versions/Current"
        link.unlink()
        link.symlink_to(self.directory)
        with contextlib.redirect_stderr(io.StringIO()) as error, self.assertRaises(SystemExit):
            self.install("--init")
        self.assertIn("checksum mismatch", error.getvalue().lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_missing_authority_is_not_recreated_during_upgrade(self):
        self.install("--init")
        (self.home / ".local/state/wispnet-node/host.sqlite3").unlink()
        installed = self.installed.read_bytes()
        with self.assertRaises(subprocess.CalledProcessError):
            self.install()
        self.assertFalse((self.home / ".local/state/wispnet-node/host.sqlite3").exists())
        self.assertEqual(self.installed.read_bytes(), installed)

    def test_archive_checksum_manifest_and_licenses(self):
        expected = self.archive.with_name(self.archive.name + ".sha256").read_text().split()[0]
        self.assertEqual(hashlib.sha256(self.archive.read_bytes()).hexdigest(), expected)
        manifest = json.loads((self.bundle / "manifest.json").read_text())
        self.assertEqual(manifest["platform"], "macos-x86_64")
        self.assertFalse(manifest["source_dirty"])
        for name in ["LICENSE", "NOTICE", "vendor/iroh/LICENSE-BSD3", "crates/shell/LICENSE",
                     "vendor/noq-proto/LICENSE-APACHE", "vendor/noq-proto/LICENSE-MIT",
                     "crates/shell/NOTICE.md", "apps/flutter/assets/fonts/Roboto_LICENSE.txt"]:
            self.assertEqual((self.bundle / name).read_bytes(), (package_macos.ROOT / name).read_bytes())
            self.assertIn(name, manifest["sha256"])
        for name, digest in manifest["sha256"].items():
            self.assertEqual(hashlib.sha256((self.bundle / name).read_bytes()).hexdigest(), digest)
        self.assertIn("Wispnet.app/Contents/Frameworks/Example.framework/Versions/Current", manifest["symlinks"])


if __name__ == "__main__":
    unittest.main()

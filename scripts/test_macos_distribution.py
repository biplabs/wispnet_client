"""Verify macOS packaging/install paths without claiming native macOS GUI coverage."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

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
        source_binary = Path(os.environ.get(
            "WISPNET_PACKAGE_TEST_BINARY", package_macos.ROOT / "target/debug/wispnet"
        ))
        # Exercise the actual helper while keeping repeated archive/hash checks
        # independent of Cargo's large local debug information.
        cls.binary = cls.directory / "wispnet"
        shutil.copy2(source_binary, cls.binary)
        if strip := shutil.which("strip"):
            subprocess.run([strip, str(cls.binary)], check=True)

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

    def native_install(self, *arguments):
        return subprocess.run([
            str(self.binary), "app-install", "--root", str(self.bundle),
            "--no-start", *arguments,
        ], env={**os.environ, "HOME": str(self.home)}, text=True, capture_output=True)

    def test_native_verifier_accepts_framework_symlinks_without_mutating_home(self):
        result = self.native_install("--verify-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified", result.stdout.lower())
        self.assertEqual(list(self.home.iterdir()), [])
        app = self.bundle / "Wispnet.app"
        framework = app / "Contents/Frameworks/Example.framework"
        self.assertTrue((framework / "Versions/Current").is_symlink())
        self.assertEqual((framework / "Example").read_bytes(), b"framework payload")

    def test_app_and_launcher_ship_native_helper_without_python_runtime(self):
        app = self.bundle / "Wispnet.app/Contents/MacOS"
        self.assertEqual((app / "wispnet").read_bytes(), (self.bundle / "bin/wispnet").read_bytes())
        manifest = json.loads((self.bundle / "manifest.json").read_text())
        self.assertNotIn("python", manifest)
        self.assertFalse(any(p.suffix == ".py" for p in self.bundle.rglob("*")))
        for name in ["python-path", "bridge.py", "desktop_data.py", "wispnet_desktop"]:
            self.assertFalse((app / name).exists(), name)
        launcher = self.bundle / "scripts/install_macos.sh"
        self.assertTrue(os.access(launcher, os.X_OK))
        self.assertIn("app-install", launcher.read_text())

    @unittest.skipIf(sys.platform == "darwin", "Cross-platform rejection is exercised on non-Mac hosts")
    def test_native_installer_rejects_other_platform_before_mutating_home(self):
        result = self.native_install("--init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("platform", result.stderr.lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_corrupt_payload_is_rejected_before_initializing_or_replacing_files(self):
        helper = self.bundle / "scripts/install_macos.sh"
        helper.write_text(helper.read_text() + "\n# corruption\n")
        result = self.native_install("--verify-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr.lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_changed_framework_link_is_rejected_before_installation(self):
        link = self.bundle / "Wispnet.app/Contents/Frameworks/Example.framework/Versions/Current"
        link.unlink()
        link.symlink_to(self.directory)
        result = self.native_install("--verify-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(result.stderr.strip())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_unlisted_payload_is_rejected_before_installation(self):
        (self.bundle / "scripts/unlisted.sh").write_text("#!/bin/sh\nexit 0\n")
        result = self.native_install("--verify-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest", result.stderr.lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_required_installer_cannot_be_removed_even_with_modified_manifest(self):
        self.assert_required_payload("scripts/install_macos.sh")

    def test_required_desktop_helper_cannot_be_removed_even_with_modified_manifest(self):
        self.assert_required_payload("Wispnet.app/Contents/MacOS/wispnet")

    def assert_required_payload(self, name):
        (self.bundle / name).unlink()
        path = self.bundle / "manifest.json"
        manifest = json.loads(path.read_text())
        manifest["sha256"].pop(name)
        path.write_text(json.dumps(manifest))
        result = self.native_install("--verify-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required", result.stderr.lower())
        self.assertEqual(list(self.home.iterdir()), [])

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

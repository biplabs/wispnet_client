"""Casks must pin Apple Silicon to immutable release assets."""
import hashlib
from pathlib import Path
import tempfile
import unittest

from prepare_homebrew import cask


class HomebrewCaskTests(unittest.TestCase):
    def test_pins_apple_silicon_dmg_hash_and_preserves_user_authority(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for arch in ("aarch64",):
                name = f"wispnet-macos-{arch}.dmg"
                payload = arch.encode()
                (root / name).write_bytes(payload)
                (root / (name + ".sha256")).write_text(f"{hashlib.sha256(payload).hexdigest()}  {name}\n")
            content = cask(root, "biplabs/wispnet_client", "development-123-abcdef0")
            self.assertIn('version "123.abcdef0"', content)
            self.assertIn('/download/development-123-abcdef0/', content)
            for arch in ("aarch64",):
                self.assertIn(hashlib.sha256(arch.encode()).hexdigest(), content)
            self.assertIn("depends_on arch: :arm64", content)
            self.assertNotIn("x86_64", content)
            self.assertNotIn("no_check", content)
            self.assertNotIn("python", content)
            self.assertNotIn("~/.config", content)
            self.assertNotIn("~/.local/state", content)
            (root / "wispnet-macos-aarch64.dmg").write_bytes(b"corrupted")
            with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
                cask(root, "biplabs/wispnet_client", "development-123-abcdef0")

    def test_rejects_unversioned_or_injected_urls(self):
        for repository, tag in [("biplabs/wispnet_client", "latest"),
                                ("biplabs/repo\"", "development-1-abcdef0")]:
            with self.assertRaises(ValueError):
                cask(Path("/missing"), repository, tag)

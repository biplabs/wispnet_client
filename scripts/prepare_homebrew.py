#!/usr/bin/env python3
"""Generate a Homebrew cask pinned to a completed release and the Apple Silicon DMG hash."""
import argparse
import hashlib
from pathlib import Path
import re


def cask(directory, repository, tag):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Invalid repository")
    match = re.fullmatch(r"development-([0-9]+)-([0-9a-f]{7})", tag)
    if not match:
        raise ValueError("Expected a versioned development release tag")
    name = "wispnet-macos-aarch64.dmg"
    digest, filename = (directory / (name + ".sha256")).read_text().split()
    if filename != name or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError(f"Invalid checksum sidecar: {name}")
    if hashlib.sha256((directory / name).read_bytes()).hexdigest() != digest:
        raise ValueError(f"Checksum mismatch: {name}")
    return f'''cask "wispnet" do
  version "{match[1]}.{match[2]}"
  sha256 "{digest}"

  url "https://github.com/{repository}/releases/download/{tag}/wispnet-macos-aarch64.dmg"
  name "Wispnet"
  desc "Share and access services across your devices"
  homepage "https://github.com/{repository}"

  depends_on arch: :arm64
  depends_on macos: :ventura

  app "Wispnet.app"
  binary "#{{appdir}}/Wispnet.app/Contents/MacOS/wispnet-node", target: "wispnet"

  uninstall launchctl: ["app.wispnet.desktop", "app.wispnet.node"],
            quit:      "app.wispnet.desktop",
            delete:    [
              "~/Library/LaunchAgents/app.wispnet.desktop.plist",
              "~/Library/LaunchAgents/app.wispnet.node.plist",
              "~/.local/libexec/wispnet-node",
              "~/.local/bin/wispnet",
              "~/.local/bin/wispnet-node",
            ]

  caveats <<~EOS
    Open Wispnet from Applications once to set up its background node.
    Existing identity, permissions and settings are preserved on upgrade.
    This development build is ad hoc signed and is not notarized.
  EOS
end
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    content = cask(args.dist, args.repository, args.tag)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content)


if __name__ == "__main__":
    main()

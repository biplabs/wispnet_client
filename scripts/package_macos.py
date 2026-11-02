#!/usr/bin/env python3
"""Build a checkout-independent native macOS desktop bundle and SHA-256 sidecar."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def architecture():
    if platform.system() != "Darwin":
        raise ValueError("Build macOS bundles on a Mac")
    machine = platform.machine()
    try:
        return {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}[machine]
    except KeyError:
        raise ValueError(f"Unsupported macOS architecture: {machine}") from None


def package(binary, app, output):
    arch = architecture()
    native_arch = "arm64" if arch == "aarch64" else "x86_64"
    for executable in [binary, app / "Contents/MacOS/Wispnet"]:
        architectures = subprocess.check_output(["lipo", "-archs", str(executable)], text=True).split()
        if native_arch not in architectures:
            raise ValueError(f"Missing {native_arch} executable: {executable}")
    subprocess.run([str(binary), "--help"], check=True, stdout=subprocess.DEVNULL)
    build_identity = subprocess.check_output(
        ["python3", "scripts/build_identity.py"], cwd=ROOT, text=True
    ).strip()
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if version != f"wispnet {build_identity}":
        raise ValueError("Node build identity does not match this checkout")
    if (app / "Contents/MacOS/build-identity").read_text().strip() != build_identity:
        raise ValueError("Desktop build identity does not match this checkout")
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    count = subprocess.check_output(["git", "rev-list", "--count", "HEAD"], cwd=ROOT, text=True).strip()
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH") or subprocess.check_output(
        ["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT, text=True
    ))
    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as temporary:
        tree = Path(temporary) / f"wispnet-macos-{arch}"
        tree.mkdir()
        files = [
            "LICENSE", "NOTICE", "vendor/iroh/LICENSE-BSD3",
            "vendor/noq-proto/LICENSE-APACHE", "vendor/noq-proto/LICENSE-MIT",
            "crates/shell/LICENSE", "crates/shell/NOTICE.md",
            "apps/flutter/assets/fonts/Roboto_LICENSE.txt",
            "scripts/install_macos.py", "scripts/node_request.py", "scripts/mcp_headers.py",
            "apps/flutter/desktop/desktop_data.py", "apps/node/desktop/icon.png", "README.md",
        ]
        files.extend(str(p.relative_to(ROOT)) for p in (ROOT / "docs").rglob("*.md"))
        files.extend(f"apps/node/desktop/wispnet_desktop/{name}" for name in [
            "__init__.py", "api.py", "combined.py", "catalog_cache.py",
        ])
        for name in sorted(set(files)):
            destination = tree / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination)
        (tree / "bin").mkdir()
        shutil.copy2(binary, tree / "bin/wispnet")
        shutil.copytree(app, tree / "Wispnet.app", symlinks=True)
        hashes = {}
        symlinks = {}
        for path in sorted(tree.rglob("*")):
            name = str(path.relative_to(tree))
            if path.is_symlink():
                target = os.readlink(path)
                if Path(target).is_absolute() or not path.resolve().is_relative_to(tree.resolve()):
                    raise ValueError(f"App bundle symlink escapes the distribution: {name}")
                symlinks[name] = target
            elif path.is_file():
                hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()
        manifest = {
            "format": 1, "license": "Apache-2.0", "source_commit": revision,
            "source_dirty": bool(subprocess.check_output(
                ["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT
            )),
            "build_identity": build_identity, "source_count": count,
            "version": build_identity.split("+", 1)[0], "platform": f"macos-{arch}",
            "minimum_macos": "13.0", "python": ">=3.11", "desktop": "flutter",
            "sha256": hashes, "symlinks": symlinks,
        }
        (tree / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with output.open("wb") as raw, gzip.GzipFile(
            filename="", mode="wb", fileobj=raw, mtime=epoch
        ) as compressed, tarfile.open(fileobj=compressed, mode="w", dereference=False) as archive:
            for path in [tree, *sorted(tree.rglob("*"))]:
                info = archive.gettarinfo(str(path), arcname=str(path.relative_to(tree.parent)))
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = epoch
                if info.isfile():
                    with path.open("rb") as source:
                        archive.addfile(info, source)
                else:
                    archive.addfile(info)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    output.with_name(output.name + ".sha256").write_text(f"{digest}  {output.name}\n")
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="Use an explicitly prebuilt native release node")
    parser.add_argument("--app", type=Path, help="Use an explicitly prebuilt Wispnet.app")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    arch = architecture()
    if args.binary is None:
        subprocess.run([
            "cargo", "build", "--release", "--locked", "-p", "wispnet-cli", "--bin", "wispnet",
        ], cwd=ROOT, check=True)
    if args.app is None:
        subprocess.run([str(ROOT / "scripts/build_flutter_desktop.sh")], cwd=ROOT, check=True)
    print(package(
        (args.binary or ROOT / "target/release/wispnet").resolve(),
        (args.app or ROOT / "apps/flutter/build/macos/Build/Products/Release/Wispnet.app").resolve(),
        args.output or ROOT / f"target/dist/wispnet-macos-{arch}.tar.gz",
    ))


if __name__ == "__main__":
    main()

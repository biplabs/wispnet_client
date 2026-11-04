# macOS desktop

Wispnet uses the shared Flutter UI in `apps/flutter`, with its native macOS
runner in `apps/flutter/macos`. The node runs independently of the desktop.
The private desktop bridge, persistence, node API, catalog cache and owner
operations are compiled into the Rust executable in `apps/node/cli/src` and
communicate through private pipes and the owner-only node socket.

## Install a release bundle

Download the macOS archive and its `.sha256` sidecar from the GitHub Release.
Use `wispnet-macos-aarch64.tar.gz` for Apple Silicon or
`wispnet-macos-x86_64.tar.gz` for Intel. The installed app requires macOS 13+. Rust, Flutter and a source checkout are unnecessary
when installing a release bundle. Python is used only for development/build
tooling; installation and runtime use native Rust helpers.

For Apple Silicon, verify and extract into a new directory:

```sh
shasum -a 256 --check wispnet-macos-aarch64.tar.gz.sha256
tar -xzf wispnet-macos-aarch64.tar.gz
cd wispnet-macos-aarch64
scripts/install_macos.sh --init
```

Substitute `x86_64` for an Intel Mac. Use `--init` only when creating a new
independent host. For an upgrade, quit any manually opened Wispnet app and run
the new bundle's installer without `--init`. The installer verifies every
payload checksum and framework symlink before changing installed files or
initializing state. `manifest.json` records the source commit, build identity,
architecture, minimum macOS version and payload hashes. Obtain the archive and
checksum from a trusted release source; the checksum does not authenticate its
publisher.

The app is signed ad hoc. Developer ID signing, notarization and App Store
distribution are not implemented; macOS may require approval to open downloaded
software.

## Build and install from source

Build on a Mac with Xcode and its command-line tools, Flutter 3.47.7, the pinned
Rust toolchain, and Python 3.11 or newer. The installed app requires macOS 13+.

```sh
scripts/install-hooks.sh
scripts/install_macos.sh --init
```

Use `--init` only to explicitly initialize a new independent host. For upgrades,
omit it: configuration, identity, grants and pins are preserved. Missing authority
in an existing installation is an error. `--no-start` installs without restarting
jobs; `--skip-build` uses existing build products. Quit a manually opened app
before upgrading.

The source installer builds the Flutter release bundle. Both installation paths install
`~/Applications/Wispnet.app`, the node under `~/.local/libexec/wispnet-node/`,
and the terminal command `~/.local/bin/wispnet`. It also installs the private
`wispnet-node` owner command for explicitly configured SSH owner operations.
Configuration is `~/.config/wispnet-node/host.toml`; default state is
`~/.local/state/wispnet-node`.

Two per-user launch agents start at graphical login:

- `app.wispnet.node`: the independent node.
- `app.wispnet.desktop`: the Flutter desktop and menu bar.

Logs are in `~/Library/Logs/Wispnet/`. The UI uses the same directory, pairing,
service configuration and owner sheets as Android/Linux. Browser handoff uses
the existing private node API and the macOS system browser. Cached catalogs are
display-only; opening services requires a fresh owner check. Closing the desktop
does not remove the node's identity or permissions.

## Development and validation

```sh
scripts/build_flutter_desktop.sh
scripts/test_flutter.sh
scripts/test.sh
```

The desktop build script bundles the Flutter engine/assets and native Rust
adapter with shared owner/cache helpers. The installer stages the app and retains
`Wispnet.previous.app` under `~/Library/Application Support/Wispnet/Backups/`
when replacing an existing installation, keeping backups out of Applications.
Extra launches reveal the existing desktop instead of starting a second copy.
On macOS, left-clicking the menu-bar item opens its menu; right-click has no
action. Choose **Open Wispnet** from the menu to show the desktop window.

To build the same checkout-independent release archive on a Mac:

```sh
python3 scripts/package_macos.py
```

The native architecture determines the output name under `target/dist/`.
The archive contains the release Rust node, complete Flutter app, installer,
helpers, documentation, license notices and a provenance manifest. Framework
symlinks are preserved. An explicitly prebuilt node/app can be supplied with
`--binary` and `--app`; their architecture, build identity and app signature
must match this checkout. A local dirty checkout is recorded in the manifest;
GitHub release builds require clean source provenance.

The local test suite covers shared Rust protocols, node API integration,
Flutter widgets, desktop adapters, cache validation and owner operations.
See [Flutter development and native verification](../apps/flutter/README.md).
Native macOS build, menu-bar, lifecycle and browser verification require a Mac;
Linux checks do not establish macOS runtime coverage.

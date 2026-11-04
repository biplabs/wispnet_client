# macOS desktop

Wispnet uses the shared Flutter UI in `apps/flutter`, with its native macOS
runner in `apps/flutter/macos`. The node runs independently of the desktop.
The private desktop bridge, persistence, node API, catalog cache and owner
operations are compiled into the Rust executable in `apps/node/cli/src` and
communicate through private pipes and the owner-only node socket.

## Install the macOS app

Download `wispnet-macos-aarch64.dmg` for Apple Silicon or
`wispnet-macos-x86_64.dmg` for Intel from the
[latest release](https://github.com/biplabs/wispnet_client/releases/latest).
Open the disk image, drag **Wispnet** to **Applications**, eject the image,
and open Wispnet from Applications. macOS 13+ is required.

First launch initializes a new independent host only when no configuration or
state exists, installs the native node and CLI under `~/.local`, and configures
per-user login agents. Existing configuration, identity, grants, pins and desktop
preferences are preserved. State without its configuration is an error, never a
reason to silently create another identity. The signed application bundle remains
unchanged. No Python, Rust, Flutter or terminal installation steps are required.
Quit Wispnet before replacing it with a newer app; open it again to upgrade the
background node. Applications in `~/Applications` are also supported.

### Homebrew

The release workflow maintains an architecture-specific cask in this repository:

```sh
brew tap biplabs/wispnet https://github.com/biplabs/wispnet_client
brew install --cask biplabs/wispnet/wispnet
open -a Wispnet
```

If Homebrew asks you to trust this third-party cask, run
`brew trust --cask biplabs/wispnet/wispnet` and retry installation.

Upgrade with `brew update` followed by
`brew upgrade --cask biplabs/wispnet/wispnet`, then open Wispnet again.
Each cask pins the release URL and the SHA-256 for both architectures. Homebrew
also exposes `wispnet` in its bin directory. Uninstalling stops the login agents
and removes program files; identity, configuration and user data are retained.
The cask becomes available after the first successful DMG release.

The app is signed ad hoc. Developer ID signing and notarization are not yet
configured; macOS may require approval to open downloaded software. DMG and
Homebrew packaging do not replace notarization. Python remains a development,
test and packaging tool only.

### Advanced archive installation

The release also includes `.tar.gz` bundles and `.sha256` sidecars for scripted
installation. After verifying and extracting the archive, run
`scripts/install_macos.sh --init` for a new host or omit `--init` for an upgrade.
The native installer verifies the manifest before changing installed files.

## Build and install from source

Build on a Mac with Xcode and its command-line tools, Flutter 3.47.7, the pinned
Rust toolchain and Git. Python 3.11+ is needed for development tests and packaging.
The installed app requires macOS 13+.

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

The native architecture determines the archive and DMG names under `target/dist/`.
The DMG contains the complete app and an Applications shortcut; license notices
and build metadata are included inside the signed app.
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

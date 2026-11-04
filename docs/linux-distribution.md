# Linux release bundles

The Linux x86_64 bundle contains the native node, dependency-free owner CLI,
Flutter desktop, service units, native owner helpers and setup guides.
Installing it requires no Git checkout, Cargo or Rust toolchain. Each computer
owns its own identity and authority. Python is used only on development/build
hosts; downloaded bundles install and run without Python. A headless computer needs no display or Qt.

## Build and verify

On Linux x86_64, with Rust/Cargo, Python 3.11+ and binutils (`strip`, `readelf`):

```sh
python3 scripts/package_linux.py
cd target/dist
sha256sum -c wispnet-linux-x86_64.tar.gz.sha256
tar -xzf wispnet-linux-x86_64.tar.gz
cd wispnet-linux-x86_64
```

The builder compiles the locked release binary and copies an explicit set of
application files. It never copies a host config, private key, registry, token,
virtualenv or catalog cache. `manifest.json` records the source commit, whether
the checkout had changes, required glibc version and each payload SHA-256.
The archive normalizes ownership and timestamps; `SOURCE_DATE_EPOCH` can override
the commit timestamp. `--binary /absolute/prebuilt/wispnet` explicitly packages
an existing native executable instead of building; its provenance must be
established separately. A checksum detects corruption; obtain the archive and
checksum from a trusted release source. It is not a signed release attestation.

Only native Linux x86_64/glibc is packaged. The target must meet the binary's
`minimum_glibc` in the manifest and have libgcc and libm. Other architectures,
musl, macOS and Windows are unsupported by this bundle. All targets need a
working systemd user manager. GUI targets additionally need GTK 3, Ayatana AppIndicator and `xdg-open`. No Qt/PySide environment is installed.
Build hosts need Flutter 3.47.7, Clang, CMake, Ninja, the corresponding development packages and `pkg-config`.
`--headless` builds a node-only bundle; its installer refuses GUI installation
before changing installed files. `--desktop-binary /absolute/Wispnet` packages an
explicit prebuilt Flutter executable together with its sibling `lib`/`data` and helper files. Its provenance must be established separately.

## GitHub Actions and development releases

Every push to `main` automatically runs **Actions → Desktop builds**. You can also
use **Run workflow** on `main` to rebuild manually. The workflow builds the Rust
node and Flutter desktop for Linux x86_64 on Ubuntu 24.04, plus native macOS
Apple Silicon and Intel bundles, with Rust 1.95.0 and Flutter 3.47.7. It checks
archive checksums, clean source provenance, platform, desktop and license metadata.
Project tests remain local through the required pre-push hook.

After all three builds succeed, the workflow updates the rolling
[Latest development build](https://github.com/biplabs/wispnet_client/releases/latest)
with the archives and their `.sha256` sidecars. Download
`wispnet-linux-x86_64.tar.gz` and its checksum directly from that release, then
follow the verification and installation steps above. The release notes identify
the source commit. This is a development snapshot, not a stable v1 release.
Failed builds leave the previous release in place. A build superseded by a newer
`main` commit retains its Actions artifacts without replacing the release.

Each run also retains a `wispnet-linux-x86_64-COMMIT` Actions artifact for 14 days.
It contains the same archive and checksum inside an artifact ZIP. Access follows
the repository's GitHub permissions, so private-repository downloads require
access. The workflow does not publish or update an AUR package.

## Fresh host: initialize explicitly

First verify the archive checksum. Extract into a new directory rather than
overlaying an earlier extraction. Stage the node without enabling or starting it:

```sh
scripts/install_node_desktop.sh --headless --no-start
mkdir -p ~/.config/wispnet-node
chmod 700 ~/.config/wispnet-node
cp -n config/node/host.toml ~/.config/wispnet-node/host.toml
chmod 600 ~/.config/wispnet-node/host.toml
```

Edit that config: set a unique device name, an absolute state directory such as
`/home/YOU/.local/state/wispnet-node`, and the intended bind/relay settings.
For the operated public relay, set `relay_url = "https://relay.wispnet.app"`
inside `[transport]`; a direct-only loopback example cannot reach remote hosts.
Use `bind_addr = "0.0.0.0:0"` for ordinary IPv4 direct access plus relay;
the OS chooses the local UDP port, and discovery advertises the reachable address.
Use a fixed port only when deliberately configuring a fixed direct address.
The client's local UDP port is separate from the relay's UDP discovery port (7842).
Do not copy another computer's state, identity or owner token.

```sh
~/.local/libexec/wispnet-node/wispnet host-init --config ~/.config/wispnet-node/host.toml
scripts/install_node_desktop.sh --headless
systemctl --user is-active wispnet-host.service
wispnet-node
```

`--no-start` only stages files; it does not change service enablement or stop an
already running node. Normal installation enables the existing user service.
Unattended boot requires a user manager that runs without login (for example,
administrator-configured user linger). No display, Qt, relay server or backend
application is installed in headless mode. On an actual graphical computer,
run the installer without `--headless` to also install/start the desktop tray.
Approve clients and register services using the [owner workflow](per-host-control.md)
and [desktop/headless workflow](linux-desktop.md). Pairing does not grant services
or reciprocal/admin access automatically.

## Upgrade, rollback and removal

Verify/extract the new bundle and run its installer in the selected mode. Payload
checksums and native executable compatibility are checked before installed files
change. The binary is atomically replaced, and normal installation restarts the
node only when its binary changes. This interrupts current connections; services
must be reopened explicitly. Config, identities, pins, grants, receipts and
display caches are not rewritten by the installer. No failed startup triggers
automatic reinitialization. Graphical dependency/install failure is reported;
installation is not a transactional rollback of all binaries and service files.

Keep the previous bundle before upgrading. Rollback means reinstalling a verified
older bundle whose binary supports the current SQLite schema. Schema-5/6/7 binaries
cannot open newer schema-10 mutual-pairing and deletion state (which also retains schema-8 service metadata). Never downgrade by deleting the
database or restoring an old grant snapshot: that could revive revoked access.
Protected backup/recovery and stale-grant reconciliation are deferred in #19.
Losing a host identity without a usable original backup requires a new identity
and explicit client re-pairing; other hosts remain unchanged. This package
does not implement recovery or promise arbitrary downgrade compatibility.

To remove program files while retaining private state:

```sh
scripts/uninstall_node_desktop.sh
```

Run from the extracted bundle as your regular user. The uninstaller stops and
disables the user services and requests sudo only if the system firewall helper
is installed. It unregisters this user's firewall access. When no users remain,
it disables the helper, removes its rules/chains and installed system files, and
reloads systemd. A shared helper remains installed for other configured users.
Failures stop the uninstall before program files are removed so it can be retried.
No unrelated UFW rules are deleted.

To remove only this user's automatic firewall integration:

```sh
sudo scripts/uninstall_firewall_helper.sh --uid "$(id -u)"
```

These commands leave config and state in place. To reinstall, use the same config
and intact state; do not run an identity reset as an uninstall step. Deleting only
`desktop-catalogs/` under the configured state directory discards rebuildable
display snapshots, preserving pins and authority; refetch requires the real host.
Deleting authority or private keys is a separate destructive reset that requires
explicit migration/re-pairing. It is not a repair or cache-clear operation.

## Evidence and remaining release work

The local pre-push suite tests extracted-bundle installation with an isolated
home, genuine fresh host initialization, registered service/approval preservation
through reinstall, missing-authority refusal and checksum rejection before any
installed-file change. It never initializes or mutates either live host.
Physical GUI/headless browser and restart evidence is recorded in the desktop
guide. Full clean-OS distribution validation, signed public release delivery,
independent relay replacement and protected host recovery remain separate release
acceptance; this document does not claim they were exercised by the package test.

Automatic local firewall setup is available with `--configure-firewall` on the
installer. It requires one administrator authorization and installs the
socket-verified `wispnet-firewall.service`; the Wispnet node stays unprivileged.
See [relay connectivity](relay-connectivity.md#linux-local-firewall-helper) for
verification, migration of old manual rules, and removal behavior.

## Arch Linux / AUR

The `wispnet-bin` package tooling is documented in
[packaging/arch/README.md](../packaging/arch/README.md). It uses pacman-owned files
and an abortable removal hook, rather than running the standalone per-user
installer from package scriptlets. Wispnet license metadata is Apache-2.0; archives include LICENSE, NOTICE and
supplied Iroh/Roboto licenses. A public versioned binary release and an AUR
maintainer account are still required before publication.

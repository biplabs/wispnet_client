# Portable testing

The public test gate runs on one development machine. It creates temporary
identities, hosts, backends, local TLS relays, ports and state. It requires no
phone, second computer, account, signing key, running Wispnet installation or
`wispnet_server` checkout. Dependencies may be downloaded during setup/build;
this is not an offline build guarantee.

## Default suite and pre-push gate

The supported development environment is x86_64 Linux with Rust 1.95.0 (including
rustfmt/Clippy), Python 3.12+, uv, OpenSSL, OpenSSH client, a C/C++ toolchain and
Flutter 3.47.9 or newer compatible with the locked Dart packages. The exact
reference setup uses Python 3.13; see [the test Dockerfile](../scripts/testing/Dockerfile).

```sh
./scripts/install-hooks.sh
./scripts/test.sh
```

The enabled pre-push hook runs this same suite locally. It includes Rust and
vendored transport regressions, Python and Flutter widget/adapter tests, real
local multi-process integration, Shell/OpenSSH, Files, terminal and browser
boundary tests. No graphical desktop or Android SDK is needed. There are no
GitHub Actions test jobs.

Hardware checks cannot be enabled implicitly in the gate: the former
`WISPNET_ANDROID_RELAY_TEST`, `WISPNET_SSH_RELAY_HOST`, Android LAN/port and Wi-Fi
switches produce an error directing users to the separate command below.
`WISPNET_NETWORK_AUDIT=1` remains a separate opt-in local Linux privilege/tool
check, requiring namespaces, nftables and strace; it does not select hardware.

## Clean Linux verification

With Docker available (or `WISPNET_CONTAINER_RUNTIME=podman`):

```sh
./scripts/test_clean.sh
```

This builds the reference tool environment and tests a snapshot of current
tracked files plus non-ignored new files. It initializes fresh Git metadata and
installs the hook inside the container. Host Git configuration, build caches,
home directory, SSH agent, credentials, attached devices and sibling repositories
are not mounted. The container runs on its own network; the source and all build
artifacts are discarded on exit. Allow substantial build time and disk space
for the first Rust/Flutter build. The tool image is cached for subsequent runs.
Tool versions are selected in the Dockerfile; OS repositories are not frozen.

## Disposable Android emulator integration

Install Android SDK/NDK and Java as described in [Android development](android-app.md).
Create and boot a disposable **x86_64 AVD with API 34 or newer**. Set `ANDROID_HOME`,
`JAVA_HOME` and optionally `WISPNET_ANDROID_NDK` for your installation; add the
`x86_64-linux-android` Rust target. With its serial explicitly selected:

```sh
python3 scripts/test_android_emulator.py --serial emulator-5554 --output /tmp/wispnet-android-check
```

This refuses physical devices before building, installing or resetting anything.
It rebuilds the native library, debug app and instrumentation APK, then runs
Files receiving, inbox and share fixtures against isolated local hosts.
Each fixture also creates its own loopback TLS relay, forwards only that port to
the selected emulator and installs its public certificate in private debug-app
state. Debug builds read that fixture configuration; release builds ignore it.
No public relay or external Wispnet service is required. Cleanup stops the debug
app, removes its temporary relay configuration and removes only its ADB mapping.

The command resets
the emulator's Wispnet app data and changes display/animation settings. Use an
AVD created for testing and discard/wipe it afterward. Existing widget tests
remain in the default suite; these Android integration checks run separately.

## Explicit hardware acceptance

For an authorized aarch64 Android device and optional compatible Linux SSH host:

```sh
python3 scripts/test_hardware.py --serial DEVICE_SERIAL
python3 scripts/test_hardware.py --serial DEVICE_SERIAL --ssh-host SECOND_COMPUTER
python3 scripts/test_hardware.py --serial DEVICE_SERIAL --lan-address HOST_LAN_IP --lan-port 60303 --wifi-cycle
```

These rebuild current binaries and use temporary identities, backends and relay
fixtures. They require an explicitly selected ADB device, existing SSH trust and
the NDK; missing prerequisites fail the requested check. Wi-Fi interruption
requires `--wifi-cycle`. See [native relay checks](native-relay-validation.md) and
[two-host checks](physical-relay-validation.md) for cleanup and network scope.

Separate legacy **installed-node** acceptance scripts intentionally test existing
paired installations, preserve their baseline policy and restart their host
services. They require reciprocal approvals, a `website` backend on each node,
a running remote relay unit and a headless remote host. Select both executables
and the SSH host explicitly, for example:

```sh
python3 scripts/test_physical_service_recreation.py --ssh-host SECOND_COMPUTER --local-node /path/to/wispnet-node --remote-node /remote/path/to/wispnet-node
```

The same arguments apply to `test_physical_catalog_snapshots.py` and
`test_physical_gateway_security.py`; the latter also needs the browsers/tools
documented in [gateway security](gateway-security.md). These installed-state
checks are never part of the public gate or clean container run.

Passing local fixtures or an emulator does not establish optical camera scanning,
OEM background behavior, real cellular/Wi-Fi handoff or independent-network
connectivity. Record those against explicitly selected release devices in the
[release acceptance matrix](release-acceptance.md).

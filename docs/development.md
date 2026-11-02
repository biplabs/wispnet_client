# Development foundation (#3)

The current [independent-host checkpoint](per-host-control.md) uses
`config/node/host.toml` (`schema_version = 2`). Schema-1 HTTPS/registry examples below
are preserved legacy foundations. #13/#17 still own relay/pairing integration.

The Rust workspace provides configuration checking and Linux agent/registry
lifecycle shells with persistent Linux device identity and an opt-in authenticated
Iroh service host/raw client. Enrollment, registry persistence, the directory and
production browser gateway remain unimplemented. See the
[identity guide](identity.md) for storage, recovery, and Android strategy.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| wispnet-core | Platform-neutral configuration, service protocol and future policy types |
| wispnet-agent | Agent lifecycle, identity and Iroh tunnel; future browser gateway |
| wispnet-registry | Registry lifecycle and future authenticated API/persistence |
| wispnet-cli | Linux CLI, structured logging, and OS signals |
| wispnet-relay | Standalone packet relay, separate TLS configuration and lifecycle; no node authority |

## Build and verify

Develop directly on main and push to origin/main. Install the local pre-push
hook once per checkout:

```sh
./scripts/install-hooks.sh
./scripts/test.sh
git push
```

The hook runs formatting, Clippy, unit tests, a debug build, CLI lifecycle/tunnel
smoke tests, and the disposable browser-boundary suite. Install OpenSSL for the standalone relay TLS smoke test, and uv and Python 3.11+ for that suite. Node 22+ runs the bundled Sites relay
spike authentication checks without installing npm dependencies. The optional
[live Sites compatibility test](sites-relay-validation.md) reports hosting failures
separately from the local suite.
It blocks a push on failure, a dirty working tree, or a branch other than main,
so checks cover the commit being pushed. Fresh clones must install the hook;
Git does not enable versioned hooks automatically. GitHub Actions is not used.
Release builds and Android checks below remain available as manual checks.

The workspace pins Iroh 1.4.1 and uses the patched source in `vendor/iroh`.
`vendor/iroh/WISPNET-PATCH.md` records the upstream archive and local changes.
Rebase those changes onto a verified upstream crate when upgrading, update the
Iroh/base/relay pins together, and update the Noq test pin with the transport.
Keep a dependency upgrade separate from deliberate changes to relay policy.
The normal suite also compiles all vendored library unit-test modules and runs
the local patch/relay receive regressions using `scripts/test_iroh_vendor.py`.
`scripts/test_iroh_compat.py` verifies authenticated multi-stream exchanges between
isolated 1.3.0 and current endpoints, directly and through either relay version.
Its previous-version dependencies are confined to fixtures under `target/`.

The shared client retains bounded sessions per peer and control/HTTP protocol.
Concurrent dials share one handshake; operations lease stream slots, with a
five-second acquisition deadline. The pool holds at most `max_connections`
entries and permits `max_streams_per_connection` simultaneous operations per
session. Idle sessions expire after 30 seconds, checked every five seconds;
capacity pressure evicts idle sessions without interrupting active ones.
Catalog replies return without waiting for direct-path negotiation, which can
continue on the retained session. Host authorization and service incarnation
checks still run for every stream. Stop, explicit network changes, and unpairing
invalidate retained sessions; application requests are never replayed.
Shell and file transfers retain dedicated connections for independent cancellation
and to keep bulk traffic separate from browser requests. These lifecycle changes
do not establish a throughput or battery improvement without route-specific
measurement.

Install rustup and a Linux C toolchain. Rust 1.95.0, rustfmt and Clippy are pinned
in rust-toolchain.toml. Direct dependencies use exact versions; Cargo.lock pins
the resolved graph. The supported development host is x86_64 Linux.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
python3 scripts/smoke.py
python3 scripts/tunnel_smoke.py
cargo build --workspace --release --locked
rustup target add aarch64-linux-android
cargo check -p wispnet-core --target aarch64-linux-android --locked
```

The Android check type-checks the current shared core and dependencies. Rust
supports this [Android cross-compilation target](https://doc.rust-lang.org/rustc/platform-support/android.html).
This does not link an APK or validate Iroh/JNI, NDK/API versions, key storage,
or background execution. Android integration and physical-device validation remain
#12 work.

## Run

Copy config/node/example.toml to an absolute path and edit state_dir to a private
absolute directory, for example /home/YOUR_USER/.local/state/wispnet/agent.
Use distinct config/state paths for agent and registry.

```sh
mkdir -p "$HOME/.config/wispnet"
cp config/node/example.toml "$HOME/.config/wispnet/agent.toml"
# Edit state_dir before running.
cargo run -p wispnet-cli -- check --config "$HOME/.config/wispnet/agent.toml"
cargo run -p wispnet-cli -- identity-init --config "$HOME/.config/wispnet/agent.toml"
cargo run -p wispnet-cli -- status --config "$HOME/.config/wispnet/agent.toml"
cargo run -p wispnet-cli -- agent --config "$HOME/.config/wispnet/agent.toml"
```

Ctrl-C or SIGTERM stops the process. Use registry instead of agent with its own
configuration to run the registry shell. Agent networking is opt-in through a
validated transport section; the default example stays offline. See the
[service tunnel guide](service-tunnel.md) and config/node/tunnel-host.toml.
There is no installed systemd service yet.

## Configuration and logging

device_id is optional and defaults to a normalized operating system hostname (Linux).
The legacy device_name key is accepted as an alias for the same readable ID.
Legacy names are normalized into one readable ID; omit the field to derive it
from the OS name on startup. Android saves a normalized system/model name once
and lets the owner edit that single Device ID in Wispnet settings.
The other three TOML fields are required. schema_version must be 1; a legacy device_name value must
be nonblank, at most 128 bytes and contain no control characters; state_dir
must be absolute, non-root and contain no parent traversal; log_level accepts
error, warn, info or debug. Optional transport/services define static service
bindings/grants; invalid limits, non-loopback backends, bad IDs and duplicate
registrations/grants fail before startup. Unknown fields fail validation. Files
are limited to 64 KiB. No implicit config/state lookup occurs. The check command is read-only.
Invalid configuration fails before state preparation or component startup.

Linux state directories are created as 0700. Existing shared directories and
final symlinks are rejected; parent directories must be under your control.
The Linux identity store also checks ownership; same-UID malicious processes are
not isolated. Agent startup requires an explicitly initialized identity.

JSON logs go to stderr and contain only fixed lifecycle component/event fields
and networking status; a networked agent also reports its public endpoint ID and
actual UDP bind address. Third-party protocol trace targets are excluded.
Configuration, arguments, device names, raw parser errors, keys, enrollment/browser
tokens and HTTP payloads are not logged. Configuration
errors describe the expected schema without echoing input. RUST_LOG is ignored.

The smoke test starts/stops both shells with SIGINT and SIGTERM, checks private
state permissions and sensitive-input errors, and verifies no sockets are owned.
The separate tunnel suite checks actual Iroh listener/task cleanup, socket reuse,
stream cancellation and two-process streaming. Internet/relay, native Android and
production browser gateway checks remain future release work. The pre-push hook
runs the local Linux checks; Android and release builds remain explicit manual checks.

The [registry API guide](registry-api.md) covers `registry-init`, HTTPS deployment,
owner administration, `registry-directory` and `registry-backup`. The pre-push
suite includes a native-process registry backup/restore and authorization smoke.

The [enrollment guide](enrollment.md) covers invitation codes, explicit owner
approval, signed device proofs and the optional physical Android native harness.
The local registry smoke uses OpenSSL for a disposable test certificate.

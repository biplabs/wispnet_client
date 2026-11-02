# Iroh 1.4.1 compatibility patch

Source: crates.io iroh 1.4.1, archive SHA-256 `e3689cab508af9bcef7816c1201b403595ec6aa20e7d15e1f9683bb4be4a5b17`.
Upstream BSD-3-Clause license is preserved in LICENSE-BSD3.

Local changes: src/net_report.rs, src/net_report/fallback.rs, src/socket.rs and
src/socket/transports/relay/actor.rs. An optional
QUIC observer is tried only after the packet relay's discovery request fails or
returns a private/special-use address. A valid public primary result avoids the
outside request. Each attempt has a three-second timeout. Valid cached public
mappings are reused; unusable LAN mappings are retried on the next report. A LAN
observation survives an outside-service failure.

Observer latency is excluded from home-relay selection unless that URL is also
explicitly present in the packet RelayMap. Existing TLS verification,
peer authentication and UDP socket ownership are unchanged.

Regression coverage: crates/agent/src/relay_tests.rs runs the exact fallback
policy tests from fallback.rs and local TLS relay/QUIC observer fixtures. It checks
that primary failure falls back using the actual peer socket without changing the
home relay. Remove this patch when upstream supports ordered discovery fallback.

The optional `NetReportConfig::prefer_relay` policy carries an explicit primary
URL, failure grace and stable recovery window to the relay actor. Per-connection
health tracks the actual authenticated relay session lifetime, not QAD results.
Latency-based home changes are ignored only when this policy is enabled. The
primary stays connected/retrying while a backup is home, so recovery and flaps
are observed; other idle relay connections retain upstream cleanup behavior.
Wispnet configures five-second failure grace and 30-second stable failback.
Endpoint cancellation drains the same owned relay actor tasks as upstream.
The integration fixtures cover healthy primary without UDP discovery, primary
failure, brief recovery, stable failback, cold starts and unchanged approvals.

`NetReportConfig::keep_all_relays` enables parallel retained packet/QAD sessions.
The opt-in multi-observer implementation is in src/net_report/multi.rs. It keeps
one QUIC observation connection per relay/family and publishes all latest
addresses in Report::discovery_addresses; src/socket.rs advertises the distinct
candidate set. Initial observations are stored immediately for later reports.
The primary preference remains independent of UDP address-discovery success.

Additional changes: src/endpoint.rs exposes a read-only relay session snapshot;
src/net_report/report.rs retains per-observer addresses;
src/socket/biased_rtt_path_selector.rs ranks direct > preferred relay > backup;
src/socket/remote_map/remote_state.rs validates and retries alternate relay paths,
including after a relay-based handshake. This is opt-in and retains upstream
path selection for other endpoints. Existing peer authentication, TLS verification,
bounded candidate exchange and endpoint-owned task cancellation remain in force.
The active-session map is exposed through owner-local redacted diagnostics.

`src/portmapper.rs` exposes bounded gateway protocol probes plus the current
mapping through `Endpoint::port_mapping_status`, retaining the upstream
portmapper service's lease acquisition/renewal. The socket keeps a clone of the
same service handle for status queries and requests deactivation during graceful
close. No second portmapper or independent renewal task is created. Wispnet's
workspace enables the optional feature; disabled configurations still return a
disabled status without probing the gateway.
The existing owned net-report task probes mapping protocols in parallel with
UDP discovery and retries acquisition after that probe. Major link changes
replace the mapping service/cache and reattach its address watcher, so old-network
leases are not advertised or reported on a new network.

`src/runtime.rs` and `src/runtime/linux.rs` use Linux `timerfd` for the shared
Noq protocol timer, including packet pacing, loss recovery and idle deadlines.
The descriptor uses `CLOCK_MONOTONIC`, is nonblocking/close-on-exec, and is owned
by Tokio's read-only `AsyncFd` registration. Rearming replaces the previous
deadline, and polling checks the current deadline before reporting completion.
Timer setup or I/O failure falls back to Noq's existing Tokio timer; paused or
shifted Tokio clocks also retain that timer. Other platforms keep the existing
runtime. `Cargo.toml` adds the Linux-only `libc` dependency.

This applies to every Iroh connection, including Files, Shell and control
traffic. Congestion control, transport windows, packet batching and executor
threading are unchanged. Workspace tests include the exact timer implementation
from `crates/agent/src/lib.rs` because this vendored crate is excluded from the
workspace; they cover deadlines, rearming, fallback and descriptor ownership.

## Upgrade validation

The patches were rebased from pristine Iroh 1.3.0 onto 1.4.1. The workspace
updates Iroh/base/dns/relay together with Noq/proto/udp 1.4.0 and retains the
workspace's rustls 0.23.46; rustls-webpki is updated to 0.103.16.

`scripts/test_iroh_vendor.py` compiles the vendored crate's complete unit-test
modules and runs the local fallback, path-selector, relay receive/queue and
Linux timer regressions. Its checked-in Cargo.lock aligns Tokio and libc with
the workspace. The relay transport's upstream fixture includes our additional
relay-policy fields, preventing those unit tests from silently falling outside
workspace validation.

`scripts/test_iroh_compat.py` builds isolated fixtures against pristine Iroh and
Noq 1.3.0 and the current vendored version. It checks pinned peer identities and
two reliable streams in both mixed-version direct directions, then all eight
old/new client, server and TLS relay combinations on loopback. Old dependencies
exist only in ignored test fixtures and are not linked into Wispnet. Both helpers
run from the required `scripts/test.sh` workflow. These tests establish local
wire compatibility; WAN/mobile transition and throughput tests remain separate.

The workspace also applies a narrow upstream Noq-proto 1.4.0 backport for valid
slow readers being rejected as excessive gaps. See `../noq-proto/WISPNET-PATCH.md`
for immutable source/commit provenance, retained limits and removal criteria.
The standalone Iroh test manifest uses the same Noq-proto override.

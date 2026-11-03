# v1 acceptance evidence — 2026-10-17

This matrix tracks the release criteria in #1 and acceptance work in #14. A
passing fixture does not establish the corresponding physical release scenario.
The exact published release build/device/network combination must be recorded
before a row with unverified release behavior can be accepted. No row claims a
full power-off, cloud VM/disk replacement, or protected recovery test.

| Release criterion / issues | Recorded implementation and evidence | Device/build scope | Still required |
| --- | --- | --- | --- |
| List/use website and Llama across networks (#11/#12/#13) | [Physical desktop browsing](linux-desktop.md#physical-guiheadless-checkpoint--2026-10-17), [Android app](android-app.md), [native public-egress evidence](external-relay-validation.md) | `d0595ad` / `58dc318`: CachyOS/KDE Chromium → headless Ubuntu WispShell; Ubuntu headless Firefox → GIKE/Llama, Llama health HTTP 200. Android guide distinguishes tablet app from native CLI. | Complete Play-phone normal-browser, unrelated-Wi-Fi/cellular/forced-relay matrix on the final build. |
| Symmetric physical power-off, surviving-host cold-start/local changes/fresh pairing (#16/#17) | [Node-offline/restart](linux-desktop.md), [independent tablet pairing](android-app.md#pairing-without-a-camera) | Two physical Linux computers with the opposite **node process stopped**; Android 14 tablet isolated sandbox. Relay continued on desktop.home. | Independent relay, actual opposite-machine power-off and cold starts in both directions. |
| Long unrelated-host outage preserves durable grants (#9/#14) | [Host control](per-host-control.md), local host/relay fixtures | Rust tests and stopped-node checks; no global permission lease. | Declared long-duration physical outage acceptance; explicit expiry/clock boundaries. |
| Composite service IDs, hostile catalogs and owner isolation (#8/#9) | [Host control](per-host-control.md), [desktop cache validation](linux-desktop.md) | Per-host Rust authority and Qt/cache regressions, same local service IDs on distinct hosts. | Service incarnations now reject stale listings/sessions at the identical backend, including physical headless tests; see [lifecycle evidence](service-lifecycle.md). Versioned complete snapshots, typed metadata, durable native high-water and the malformed/partial matrix are now implemented/tested locally and on headless Ubuntu; see [catalog evidence](catalog-snapshots.md). Physical power-off/rebuild acceptance remains. |
| Stale/offline/removal isolation and current host-local enforcement (#9/#10/#11) | [Desktop cache/Open behavior](linux-desktop.md), [Android permission checks](android-app.md) | `4638fd6`, `58dc318`: persistent cache; cached Open blocked; physical offline catalog failure about 5–5.6 s, surviving owner-local browsing works. | Complete snapshot/removal, rollback/generation and cache-deletion checks now have native fixture/headless evidence; see [catalog contract](catalog-snapshots.md). Final measured cancellation/rebuild acceptance remains. |
| No transitive admin; per-target multi-host results (#17) | [Combined owner operations](linux-desktop.md#combined-owner-operations) | `4b1ec0e`: both physical Linux hosts; desktop.home headless; installed Qt and headless CLI. Offline target pending, persistence/retry, rejected/wrong-target and lost-ack replay after later revoke. | Final release-build security/error consolidation. Full power-off/rebuild remains in the rows above/below. |
| Disposable relay/disk and changed-IP rebuild (#18) | [Relay fixtures](native-relay-validation.md), [operated relay](../../wispnet_server/docs/desktop-relay.md) | Local/native changed-address and relay-process restart; desktop-hosted public relay. | Independent relay deployment; destroy/recreate isolated VM and writable disk, same/new IP; verify pins/revocations and reconnect bounds. |
| Independent host recovery and stale backup (#19) | [Identity fail-closed behavior](identity.md), [SQLite foundations](registry-storage.md) | Local protected storage, consistent SQLite backup, restart/missing-state rejection. | Protected host-bound bundle, single-writer fencing, wrong-host/corrupt/key-loss rejection and stale-grant quarantine/reconciliation. |
| Browser/session/security isolation (#10) | [Gateway contract and security matrix](gateway-security.md) | Shared Rust node installed on both hosts; Chromium 154 / headless Firefox 158: cookie/storage/login/SSE/WS isolation and attack rejection, LAN refusal, revocation/client restart, POST interruption. Seven gateway tests also run natively on Ubuntu: expiry/forced port reuse, bounds and total header deadline. | Final Play-phone browser coverage; actual independent cloud replacement stays in #18. HTTP-only v1 deliberately rejects Secure/prefix cookies, service workers and external redirects. |
| Streaming/resources/reconnect without write replay (#10/#13/#14) | [Gateway measured limits and consolidated evidence](gateway-security.md), [native relay validation](native-relay-validation.md), `service_tunnel.rs` / `tunnel_smoke.py` | Both installed Linux nodes: browser SSE/WS and interrupted POST exactly once after fresh Open. Native: slow-reader backpressure/cancellation, QUIC/stream/connection bounds, framing/timeouts, relay replacement/revocation; gateway 16 routes/32 sockets/10 s headers/16 MiB uploads. Combined retries replay receipts, not policy mutations. | Final release-device long-duration/network matrix. Native fixture limits are not an Android memory/latency benchmark; no arbitrary write retry. |
| Android background/lifecycle (#12/#14) | [Android lifecycle evidence](android-app.md), dated UI/emulator limitations | Physical Android 14 tablet; screen-off 60 s including 15 s forced deep Doze; post-wake browser reopen and Stop. Optical Play-phone scan recorded in #17. | Final Play-phone real-service/lifecycle checks, broader OEM restrictions, declared long-duration tests and applicable Android 15/16 acceptance. |
| Fresh install, recovery/rebuild guides and dependencies (#15) | [Linux distribution](linux-distribution.md), [Android internal publishing](android-release.md) | Release x86_64 bundle tested in isolated local/Ubuntu homes: fresh init, state-preserving reinstall, checksum rejection, missing-authority refusal; no Qt on headless Ubuntu. | Clean-OS GUI distribution verification, store declarations/release notes; evidence-backed protected backup and independent cloud rebuild guides (#18/#19). |

## Reproduction and evidence rules

`scripts/test.sh` and the enabled local pre-push hook run the native, Python,
Qt-offscreen, gateway and relay fixture suites. `scripts/test_linux_distribution.py`
checks an extracted bundle in a temporary home; set `WISPNET_PACKAGE_TEST_BINARY`
to an explicitly built release executable to exercise that binary. Build bundles
with `scripts/package_linux.py`; inspect their manifest for source commit, dirty
checkout flag, payload checksums and glibc requirement.

Physical guides retain dated device/build details and limitations. The 2026-10-17
Ubuntu bundle check used a temporary home on desktop.home, staged files with
`--headless --no-start`, initialized only fixture authority and compared state
after reinstall. It did not restart the live node, relay or any desktop GUI.
Future evidence should record commit/version code, OS/device, network/path,
scenario, observed timing, cleanup and remaining uncertainty in its owning issue
and link it here. Deployment/account provisioning, machine shutdown, and live
authority recovery must not be inferred from a fixture's success.

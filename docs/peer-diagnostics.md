# Peer path diagnostics

`peer-probe` authenticates the expected host, fetches its permitted catalog, and
prints one JSON snapshot. `peer-watch` repeats that read-only check using one
persistent identity lease and Iroh endpoint until SIGTERM or Ctrl-C.

```sh
target/debug/wispnet peer-probe --config /absolute/path/client.toml --peer PINNED_HOST_ID
target/debug/wispnet peer-watch --config /absolute/path/client.toml --peer PINNED_HOST_ID
```

Both accept one optional `--address IP:UDP_PORT` hint after the peer ID. Without
that hint, configure `transport.relay_url`; no directory lookup is needed. The
host must independently approve the client before returning a catalog. Probes
never open a backend, mutate policy, enroll a client, or replay a service operation.

Successful reports contain the expected host and own client ID, observed
`selected_path` (`direct`, `relay`, or `unknown`), `relay_status` (`disabled`,
`connected`, `denied`, or `unavailable`), catalog generation/revision, permitted
service count, and elapsed milliseconds. No backend IPs, service payloads, grants,
owner tokens, certificate contents, or raw relay error strings are printed.
The transport pin and catalog host are verified before success is reported.
An API caller supplying a target address for a different ID fails before dialing.

A probe allows the existing five-second connection and five-second control-RPC
deadlines, then up to one second for a direct path to replace an initial relay
path. A path snapshot reflects the time of observation, not an availability or
future-route guarantee. Relay reachability and peer reachability are separate:
a direct peer can work while the configured relay is unavailable.

Watch emits one compact JSON line per completed probe. Successful probes pause
one second; consecutive failures pause 1, 2, 4, 8, 16, then at most 30 seconds
after each completed attempt, in addition to the bounded probe deadlines above.
Each watch owns its retry state independently. Success or a network-change
notification resets the failure backoff; notifications interrupt the wait and
start a fresh probe. Watch reports include `retry_after_ms` for this scheduled
pause (one-shot `peer-probe` does not retry or include that field).
Failures emit fixed redacted errors and omit private catalog
metadata. Pending probes are canceled on shutdown, their connections are closed,
and the endpoint/key lease is released. An unrelated host is not consulted.
Catalog observations are diagnostic snapshots, not service authorization tickets
or a persisted service cache; #9 owns cache/revocation integration.

## Explicit network-change notification

The pinned Iroh 1.4.1 API exposes `Endpoint::network_change()` for platforms whose
native route monitor cannot observe changes. Its Android native monitor is a
stub; the Android app forwards its platform network callback to the
same owned endpoint through AccessService/JNI. Replacing its key or restarting the endpoint is unnecessary.

`peer-watch` provides a local SIGUSR1 bridge for native testing:

```sh
kill -USR1 WATCH_PID
```

This cancels an in-flight read-only diagnostic if needed, notifies the existing
endpoint, and probes again with the same pin and key. It does not accept remote
administrative instructions. The signal bridge demonstrates the transport API;
it is separate from the implemented Android app/JNI callback registration.

The physical test in [native relay validation](native-relay-validation.md) keeps
one Android watch process alive across Wi-Fi off/on, sends notifications after
interface changes, and verifies direct LAN → USB-reachable TLS relay → direct
LAN with unchanged identity and catalog. No seamless application-stream migration
or automatic retry of a potentially state-changing request is promised.

## Android settings diagnostic

In Settings, tap Diagnostic below Save. A separate page opens and automatically
tests each paired host using the existing phone identity and host approval.
Run again and Copy report share the action row. Each host has a 55-second overall
deadline; refreshing network observations can make the test take longer than the
original ten-second path observation. Leaving the page stops subsequent hosts;
an already running host test remains bounded by its deadline.

The phone and target independently watch for a new Iroh network-report publication
for up to 30 seconds while observing the same authenticated control connection.
Iroh periodically runs discovery (normally every 20–26 seconds). A network-change
hint does not guarantee a new discovery run. The report marks an observation
updated during this test only after receiving a later publication; otherwise it
labels the available report cached, or the observations unavailable. A fresh
publication is not a promise that every discovery sub-probe succeeded.

Both sides report selected paths, latency, known peer IP-candidate counts, direct
paths seen/closed, direct-path datagrams, and validation-frame counters. Counts
across all connection paths explicitly include relay paths. Failed candidates
that never became observable paths and their rejection reasons are not exposed
by the pinned transport API; zero observed direct paths does not establish that
no hole-punch attempts occurred. A missed-event warning marks incomplete counts.
The diagnostic does not identify a NAT type without sufficient discovery data.

Host observations use a new bounded `diagnostic` control RPC. Transport identity
and existing host approval authorize it, with approval checked again before the
reply. The response is versioned, host-pinned, bounded to 4096 bytes, and contains
no addresses, backend details, service payloads, or credentials. Older hosts may
reset the unsupported request stream; local results are retained and the host
observation is marked unavailable with an update hint. Catalog, service access,
and permissions remain unchanged. Normal control-RPC deadlines remain bounded
independently from the longer diagnostic request.

Receiving no UDP discovery response does not prove a blanket UDP block; captive-portal detection
is a heuristic. Public-address presence is reported without the addresses.
Device badges continue to show the last normal request's path rather than this
separate diagnostic. For LTE analysis, rerun on a stable cellular connection and
compare the phone and target observations in the copied report.

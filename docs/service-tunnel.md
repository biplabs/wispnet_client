# Authenticated service tunnel (#5)

This is the historical schema-1 static-grant walkthrough. Current schema-2 hosts
use durable live policy with active-stream cancellation and protected browser
access; see [host control](per-host-control.md) and [desktop/headless](linux-desktop.md).

The Rust agent can host fixed loopback TCP services over authenticated Iroh, with
explicit static peer grants. The development client relays stdin/stdout over one
service stream. It does not expose a local browser listener or replace #10's
HTTP-aware security boundary.

## Try two Linux processes

Build with cargo build --workspace --locked. Prepare a host config from
config/node/tunnel-host.toml and a client config from config/node/example.toml. Set different
absolute state directories and config paths; initialize each independently:

    target/debug/wispnet identity-init --config /absolute/host.toml
    target/debug/wispnet identity-init --config /absolute/client.toml

Copy the client's public Endpoint ID into the host service's allow list. Do not
copy its private key. Keep the host's public ID for the client's --peer argument.
Start your backend on the host's configured loopback port, then:

    target/debug/wispnet check --config /absolute/host.toml
    target/debug/wispnet agent --config /absolute/host.toml

The agent's started JSON event on stderr reports its public endpoint_id and actual
bind_addr (useful with UDP port 0). Supply the actual UDP address and host public ID:

    printf 'GET / HTTP/1.1\r
Host: website\r
Connection: close\r
\r
' |
      target/debug/wispnet tunnel --config /absolute/client.toml --peer HOST_PUBLIC_ID --address 127.0.0.1:7779 --service website

stdout is the raw application response; diagnostics go to stderr. The raw client
does not add HTTP authentication, retry requests, resolve names, or open listeners
for browsers. Requests may contain arbitrary application bytes, but the upstream
connection is always the host-configured backend for the authorized service ID.

For a manual LAN trial, explicitly choose the host's LAN UDP bind address and
provide that address to the client. This implementation has no address lookup,
public relay configuration or automatic reachability guarantee; #13 owns them.
The repeatable checks so far use two Linux processes on one machine. They do not
prove different-machine, Wi-Fi, cellular or forced-relay operation.

An absent transport section retains the original offline lifecycle mode. Services
without transport fail configuration validation. The registry remains a lifecycle
shell. Empty allow lists deny all peers. Changing static services/grants requires
an agent restart, which closes all streams. This restriction applies to the
static-grant fixture, not current schema-2 host authority.

The CLI client takes its installation's identity-use lease for the whole session.
Use separate host/client state for the development walkthrough; it cannot take
that same lease while a separate agent process owns it. The library can open
outgoing streams using a hosting agent's existing endpoint, and this simultaneous
host/access behavior is tested in both directions. The installed `wispnet-node` CLI and desktop now attach to the running agent
without starting a duplicate installation.

## Wire protocol

ALPN is wipsnet/service/1. Each QUIC bidirectional stream starts with:

| Field | Request |
| --- | --- |
| Frame length | 2-byte unsigned big-endian payload length; at most 128 |
| Version | 2-byte unsigned big-endian value 1 |
| Service ID | 1-64 ASCII bytes: lowercase letters/digits plus dot, underscore, hyphen; first byte alphanumeric |

There is no wire field for caller identity, IP address, URL, port, or backend.
The authenticated connection's remote ID is the sole device identity input.
Unknown peers are closed at the connection level with application code 1 before
serving any stream. For a known peer, the host validates the frame, service
registration and that service's local allow list before calling TCP connect.

The response is a 2-byte length prefix (3) followed by version (2 bytes) and status
(1 byte). Only status 0 permits application bytes to follow.

| Code | Meaning |
| --- | --- |
| 0 | Accepted, backend connected |
| 1 | Access denied |
| 2 | Unknown service |
| 3 | Backend unavailable |
| 4 | Capacity reached |
| 5 | Malformed handshake |
| 6 | Unsupported version |
| 7 | Handshake timed out |

A frame-level denial finishes its response direction and stops request input.
ALPN mismatch fails transport negotiation. Invalid, truncated and oversized
frames are bounded and cannot select or connect an upstream. The client validates
responses and never automatically reconnects or resends application bytes.

## Bounds and lifecycle

| Resource | Current bound |
| --- | --- |
| TLS/connect/open-service deadline | 5 seconds per operation |
| Host backend TCP connect | 3 seconds, inside the 5-second service-open deadline |
| Handshake payload allocation | At most 128 bytes |
| Default accepted/in-flight connections | 16; configurable 1-128 |
| Default active streams across incoming connections | 64; configurable 1-256 |
| Default streams per incoming connection | 8; configurable 1-128, also enforced through QUIC stream credit |
| QUIC receive window per stream | 64 KiB |
| QUIC connection receive/send windows | 1 MiB each |
| Application copy buffers | 16 KiB per direction, with backpressure |
| Read/write/half-close inactivity deadline | 60 seconds per pending operation |
| Connection without active streams | Closed after 60 seconds idle |
| Maximum individual stream lifetime | 1 hour |
| Unidirectional incoming streams | Disabled |

These bound application concurrency and flow-control buffers, not total process
RSS, kernel buffers, all Iroh internals, or an Internet denial-of-service budget.
Transport-level stream credit can make an excess open wait until the client's
deadline; host application-level overload returns capacity status when a stream
can be accepted. Outgoing sessions added by a future directory still need their
own concurrency management.

EOF half-closes the other write direction and keeps the reverse direction alive.
QUIC write shutdown waits for the peer to acknowledge buffered bytes before a
normal client endpoint close; this is delivery acknowledgement, not proof that
the backend processed a request.
Dropping an unfinished ServiceStream resets/stops its unfinished directions.
Disconnect, connection close or host shutdown cancels owned tasks and drops backend
sockets. The host owns and drains its task sets; no detached tunnel workers remain
after explicit shutdown. The CLI handles SIGINT/SIGTERM and bounds runtime shutdown
so a pending blocking stdio worker cannot indefinitely keep the process alive.

Authorization does not undo backend side effects. A backend closing after receiving
a POST may leave its outcome unknown; the CLI returns the raw EOF/error and makes
no retry. The caller must decide what to do.

## Validation and limitations

The local pre-push suite includes:

- Real Iroh page/SSE and RFC 6455 WebSocket handshake/masked-frame tests.
- Backend accept counters proving denied/unknown/forged/address-like requests open
  no TCP connection; malformed frames, version/ALPN mismatch and deadline checks.
- Global/per-connection capacity, backend-unavailable status, slow readers,
  stream cancellation, repeated slot reuse, backend half-close, drained active
  tasks and UDP listener reuse after shutdown.
- Two actual CLI processes serving HTTP/SSE; first event must arrive before the
  backend is allowed to finish. A POST followed by backend failure is counted once.
- CLI cancellation while stdin remains open, plus existing identity/lifecycle and
  browser-boundary regression suites.

A separate local check retrieved the live GIKE HTML page and llama.cpp health
response through two Rust CLI processes over Iroh (both HTTP 200). No backend
configuration was changed. These are local transport observations, not proof
of browser safety, native Android support, network recovery or throughput targets.

Static grants, local TCP bindings, no discovery/relay, and no protected directory
or browser gateway are intentional stage boundaries. See ADR 0001 and issues
#6/#7/#8/#9/#10/#11/#12/#13/#14 for the remaining release work.

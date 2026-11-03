# Owner-controlled public services

A locally owned HTTP service can be published for anyone to connect over HTTPS:

```
https://j-desktop-3102e0ed3280-llama.wispnet.app/
```

On desktop, open a local service's gear and choose **Enable public access**.
The URL and connection state appear on the card; use the same settings to disable.
The terminal's Local services menu
has **Enable/Disable public access** and displays the URL. Remote service cards
cannot publish: permission to use another host's service is not permission to
publish it. Android currently owns no hosted services, so it has no publishing
switch. All services start private unless their owner explicitly enables this.

Both desktop and Android service cards have one unlabelled link icon that opens
a popup with selectable addresses and copy controls.
It lists the local HTTP URL first, the stable `wispnet://` address second on
Android, and the public URL last when enabled (the third option on Android).
Reading or copying these addresses does not launch the browser. If a local route
has not yet been prepared, an online device resolves it through the normal host
permission checks; failures leave the other addresses available.
Paired devices fetch public URLs and tunnel
state when refreshing a host. Only currently granted HTTP services with the same
incarnation receive this metadata; a removed or disabled publication disappears
on the next successful refresh. Cached links are marked **Public · Cached** when
the host cannot be verified.

This optional, read-only `public_links` peer request is separate from the strict
revisioned service catalog. Older peers can reject it without disrupting normal
service discovery. No backend addresses, gateway configuration, credentials or
publishing controls are shared. The owner-local API exposes the read as
`{"op":"host_public_links","host":"FULL_HOST_ID"}`.

The hostname contains the machine-name slug, the first twelve hexadecimal
characters of the owning endpoint ID, and a service-ID slug. Long or transformed
service IDs include a short hash suffix to avoid slug collisions. The machine
name is saved when enabling publication, so renaming a machine does not silently
change an active public URL. Duplicate names across different owners are distinct.

Public visitors need neither Wispnet nor device pairing. The backend's own login
or API-key requirements still apply. Normal website CORS behavior belongs to that
backend; the private localhost gateway's website allowlist does not gate an
explicitly public service. The HTTPS/public gateway can see HTTP requests and
responses, including LLM prompts. Private paired connections remain encrypted
through the independent packet relay.

## Authority and transport

The owner-only node API handles `configure_public`; HTTP service discovery and
paired-peer control APIs never expose this operation. The request must select a
current HTTP service incarnation in that node's own durable registry. Supplying
another host ID is rejected, and no caller-provided backend address is used.
Publishing adds no entries to private client/device grants.

The owner creates an outgoing authenticated Iroh connection to the pinned public
gateway using `wispnet/public/1`. Each connection registers one service. The public
gateway derives its hostname from the authenticated owner ID rather than accepting
a caller-chosen hostname. Gateway admission permits only configured publisher
endpoint IDs; it cannot request certificate issuance for arbitrary unregistered
names. The deployed allowlist contains the two owning Linux nodes.

The public gateway opens bounded, incarnation-bound streams back over that same
connection. The owning node rechecks its own service binding before connecting
to its loopback backend. A paired client cannot forward a publish request through
its existing access. Disabling publication closes only that publication's tunnel
and active HTTP/SSE/WebSocket streams. Removing a service or changing its backend,
kind or incarnation stops the old publication; a subsequent owner status refresh
or node startup removes stale publication settings. Re-enabling the changed
service is an explicit owner action. Other publications remain independent.

Settings live in owner-only `state_dir/public-services.json`, bound to the node's
identity and service incarnations. Enabled publications reconnect automatically
after node/gateway/network restart. Requests interrupted by a reconnect are not
replayed. Going offline stops the owning node and all its public connections.
The localhost browser-access switch is separate from owner-controlled publishing.
No host private key or owner credential is copied to the public gateway.

## Deployment

The current desktop relay machine runs three separate user services:

- `wispnet-relay.service`: encrypted packet forwarding on loopback port 18443.
- `wispnet-public-gateway.service`: public HTTP routing on loopback port 18080,
  with its own persistent identity and an explicit publisher allowlist.
- `wispnet-public-https.service`: Caddy HTTPS front end on port 8443, reached by
  the existing router mapping from public TCP 443.

The packet relay's explicit `http_only=true` mode requires a loopback listen
address and no TLS configuration. Caddy provides normal verified WebPKI TLS for
`relay.wispnet.app` and proxies packet relay WebSockets. This preserves the client
relay URL. The original packet-only ACME state remains available for rollback.

In Namecheap Advanced DNS, add a CNAME with Host `*`, Value
`relay.wispnet.app`, TTL Automatic, preserving the existing `relay` record.
Caddy obtains individual certificates on demand using TLS-ALPN-01 through public
TCP 443. A wildcard certificate or DNS API token is unnecessary. First access to
a newly published hostname may take a few seconds while the certificate is issued.
The local certificate-permission endpoint accepts only currently registered
public names and is blocked by the public HTTP front end.

Build and deploy from the sibling server checkout:

```sh
cd ../wispnet_server
cargo build -p wispnet-public-gateway -p wispnet-relay --locked
python3 scripts/install_public_gateway.py --ssh-host desktop.home \
  --publisher FULL_OWNER_ID_1 --publisher FULL_OWNER_ID_2
```

The installer preserves the public gateway identity, checks it against the public
server bootstrap in `deploy/public/gateway.json` (the client copy is
`config/public/gateway.json`), saves a rollback snapshot, installs the
HTTPS/public services, and verifies the existing relay's HTTPS endpoint. It
restores the previous installation if startup/health checks fail. It never
publishes any backend. A gateway identity loss requires an explicit bootstrap
update; it does not silently rotate or impersonate the old gateway.

## Owner API and limits

`wispnet-node` accepts these JSON requests on its existing owner-only Unix socket:

```json
{"op":"configure_public","service":"llama","incarnation":"CURRENT_INCARNATION","enabled":true,"name":"J desktop"}
{"op":"public_status"}
{"op":"configure_public","service":"llama","enabled":false}
```

Responses show enabled services, URL and connection state. `online` means the
publishing tunnel is registered; DNS/certificate readiness must also be configured
at deployment. The owner-only `configure_public_gateway` operation overrides the
pinned gateway configuration for controlled deployments/tests and requires all
public services to be disabled first.

Limits: sixteen published services per owner, 256 registration connections,
128 concurrent public requests, sixteen per route, and 32 backend streams per
owning node. WebSockets retain their request permits until closed. Registration
and backend-open handshakes are bounded; uploads are streamed with a 16 MiB limit.
HTTP, streaming/SSE and WebSocket upgrades use bounded buffers and preserve
backpressure. Response-header wait is bounded to two minutes and each backend
stream to one hour. Public gateway and HTTPS front end have separate systemd
memory/task limits. No arbitrary request retries or backend addresses are accepted.

## Validation

The local workspace suite covers owned-service checks, wrong-host rejection,
unauthorized publisher denial, unchanged private grants, normal HTTP, WebSockets,
stream cancellation on disable, binding replacement, and stale-state cleanup.
The live check publishes only a temporary harmless fixture and removes it:

```sh
python3 scripts/test_public_access_live.py --require-dns
```

Without `--require-dns`, a missing wildcard record is explicitly reported and the
check verifies the deployed HTTPS/public route using the existing relay's valid
TLS hostname plus the fixture's Host header. That fallback verifies the tunnel
and routing, not the generated hostname's own DNS/certificate readiness.

# Browser gateway security and compatibility (#10)

## v1 decision

The shared Rust Android/desktop gateway serves **HTTP only on IPv4 loopback**.
Each client installation saves one readable address such as
`desktop-183296aa35df-llama.localhost:<port>` per pinned system and service.
Names contain a lowercase system slug, the first 12 hexadecimal characters of
its host ID, and a service slug. Collisions receive a saved numeric suffix
(`-2`, `-3`). Names follow the current readable device ID when a service is opened;
reconnect order does not affect them. Local services use the node device ID.
Opening a service with an older address replaces its name without keeping an alias;
the reserved port stays unchanged. Open returns the stable root URL, with no capability
in its path, query or fragment. Repeated opens reuse the listener; going online
restores saved listeners on exactly their original ports. A conflicting local
port fails explicitly rather than silently changing a bookmark. Addresses are
saved in owner-only `browser-routes.json`, bound to the installation identity;
no browser credentials are created or persisted.

Native Android and Linux apps resolve URLs through the [shared registry](service-registry.md)
and call them directly using device-level access. No API access token, per-app
consent or credential renewal is required. All website origins are allowed,
including opaque origins serialized as `null`; duplicate or malformed Origin
headers are rejected before forwarding. A service's
own Authorization/API-key requirements remain unchanged. Host revocation still
terminates access. Browser pages open the same stable URL without a Wispnet cookie or launch token.
Native apps share the device's trust boundary.

There is no ten-minute route deadline or Wispnet session cookie. Going offline and process death
close sockets; going online restores the same addresses. Network changes cancel
in-flight requests without replay and restore listeners.
Each request checks current local service state or uses an authenticated peer
connection whose host checks grants and service incarnation. Removing the local
peer pin also denies forwarding. Explicitly opening a recreated service adopts
its new incarnation at the same address; stale bindings stay denied.

Browser pages and same-origin assets/fetches need no Wispnet browser session.
GET navigations to a top-level document are also accepted from external links
and Android app intents. Cross-site frames and requests without an Origin remain
restricted; website API access follows the device-level policy below. Old
`/_wispnet/start` and `/_wispnet/session` endpoints return 404. Obsolete
`_wispnet_session` cookies are ignored and stripped before forwarding; ordinary
application cookies continue to work. Saved URLs, including deep links, are usable
as soon as the app is online without opening the service again.

The trust boundary excludes hostile same-UID/root processes and application
scripts accessing their own origin. A browser secure-context flag does not
establish transport encryption; Iroh encrypts the peer transport.

Applications requiring HTTPS-specific cookie semantics are unsupported in v1.
A response setting Secure, Partitioned, SameSite=None or `__Host-`, `__Secure-`,
`__Http-` (including `__Host-Http-`) cookies is rejected with 502 **before any
upstream headers are emitted**. Those flags/prefixes are never stripped to make
a login appear to work. Ordinary application cookies retain their Path,
HttpOnly and SameSite settings, lose Domain to become host-only, and cannot set
or receive `_wispnet_session`. This is a bounded compatibility decision, not a
claim that arbitrary web apps work over HTTP.

Service workers are unsupported: script requests with `Service-Worker` or
`Sec-Fetch-Dest: serviceworker` get 403 before connecting to the backend.
`Service-Worker-Allowed` is removed from upstream responses. Origins persist for
the same logical system/service, including explicit backend
replacement. Application cookies and localStorage therefore persist at that
origin; offline does not erase rendered pages, downloaded data or application
storage. A different system or service gets a separate hostname and port.
Own-origin scripts remain application code trusted with that application's access.

Relative and same-gateway-origin redirects work. Owner-local absolute redirects
to the **exact current backend origin** are rewritten to the gateway origin.
Remote private backend addresses are intentionally not disclosed in catalogs;
such absolute redirects cannot be safely rewritten and are rejected. Other
origins/schemes, userinfo, backslashes, control characters and Refresh redirects
are rejected with 502. External OAuth/SSO and application-embedded absolute URLs
need an explicit future compatibility design. Applications should use relative
URLs. Ordinary relative fixture login passes in Chromium and Firefox.

## HTTP boundary

- Exact Host, single Origin and Fetch Metadata are checked before backend access.
  Requests carrying a valid website Origin follow the device-level API policy.
  Without Origin, cross-site fetches and frames are rejected; browser writes and
  WebSocket requests require Origin. Native requests use device-level grants.
  Browser pages require no gateway cookie or handoff token. Top-level GET
  navigation is accepted from bookmarks, existing tabs, links and app intents.
- Browser-facing requests must be HTTP/1.1 origin-form. GET/HEAD/POST/PUT/PATCH/
  DELETE/OPTIONS are supported; TRACE/CONNECT and other methods get 405.
  Uploads require a single decimal Content-Length (zero if absent), at most
  16 MiB, within 30 seconds. Chunked uploads, Expect/TE/Trailer and ambiguous
  framing get 400; oversize declared uploads get 413.
- Headers are bounded to 32 KiB/96 fields and a **total** ten-second deadline,
  not a per-byte idle timeout. The gateway allows 16 routes and 32 concurrent
  accepted connections across routes; excess connections close immediately.
  Stop releases active socket capacity. A slow reader applies bounded-copy backpressure;
  there is no whole-response buffer and no total download-size cap.
- HTTP/1.0 and 1.1 backend responses support exact Content-Length,
  close-delimited bodies and plain chunked transfer. Chunk lines are at most
  128 bytes; each chunk is at most 16 MiB; extensions/trailers are unsupported.
  Duplicate/conflicting Content-Length/Transfer-Encoding and unsupported 1xx
  responses get 502. Fixed-length/chunked termination prevents copying a second
  response. Malformed bodies after headers abort the connection; they cannot
  be converted retrospectively into a 502.
- WebSocket requires GET, version 13, a base64 16-byte key, Connection/Upgrade,
  exact Origin and a valid 101/Sec-WebSocket-Accept response. A returned
  subprotocol must have been offered. Non-WebSocket protocol upgrades are
  unsupported. Validated connections stream bidirectionally with backpressure.
- Request forwarding credentials/proxy headers and response hop-by-hop headers
  (including Connection-nominated fields) are removed. Upstream CORS permission
  headers cannot relax the gateway boundary. Responses force no-store,
  no-referrer, frame-ancestors 'none', X-Frame-Options DENY, same-origin CORP/COOP.
  Application CSP headers are retained alongside the frame restriction.
- Peer identity/service incarnation and current host-local grants select the
  backend; catalog display text never selects a URL/backend. Revocation/removal
  cancels active native streams. Owner-local binding changes are polled every
  second with a one-second storage-read deadline. No HTTP write is retried or
  replayed automatically after interruption/reconnect.

## Recorded 2026-10-17 validation

`scripts/test_physical_gateway_security.py` exercises the installed shared Rust
node on CachyOS/KDE and Ubuntu 24.04 `desktop.home` in **both directions**, with
two temporary same-ID services and malicious literal display text. Chromium
154.0.8037.97 locally and existing Firefox 158.0 in headless mode on Ubuntu each
open simultaneous owner-local and remote origins. Checks cover application and
HttpOnly credential cookies, localStorage, relative login, chunked SSE,
WebSocket echo, denied cross-origin fetch/WebSocket/iframe GET/form POST with **zero backend requests**, rejected Secure/prefix
cookies/external redirects and blocked service-worker registration. Raw checks
cover CSRF, null/cross-site Origin, Host mismatch, methods, framing, upload
bounds, bootstrap replay and actual non-loopback interface connection refusal.
Host-local grant withdrawal denies an existing route, remains denied after
client-node restart, and permits a fresh explicit Open after regrant.
An interrupted application POST commits exactly once; a fresh Open does not
replay it. Fixtures restore services/grants/pins/access settings; the relay PID
is unchanged and the remote Qt/tray unit remains absent.

Historical checks before persistent URLs: the seven `wispnet-client` gateway unit tests also ran as a native executable on
headless Ubuntu: strict request/response/cookie/redirect/WebSocket cases;
single-use bootstrap and Stop; short **private test-only** expiry limits and
forced exact port reuse by a different peer; 16-route/32-socket limits and a
continuously trickled header stopped by the ten-second deadline. Expired tokens
and old cookies/Host cannot substitute the newly bound host; fresh bootstrap
gets a different credential. No production TTL control is exposed. This is
same-code expiry validation, not a claim of waiting out a physical ten-minute
browser session. Existing [service recreation](service-lifecycle.md) and
[offline/restart](linux-desktop.md) evidence remains independently scoped.

Run the opt-in physical test only on those approved paired hosts:

```sh
python3 scripts/test_physical_gateway_security.py --ssh-host SECOND_COMPUTER --local-node /path/to/wispnet-node --remote-node /remote/path/to/wispnet-node
python3 scripts/test_physical_service_recreation.py --ssh-host SECOND_COMPUTER --local-node /path/to/wispnet-node --remote-node /remote/path/to/wispnet-node
cargo test -p wispnet-client --lib --locked
```

The enabled `scripts/test.sh` pre-push suite also checks native tunnel SSE early
arrival, slow-reader backpressure/cancellation, bounded QUIC/connection/stream
credit, malformed frames/timeouts, host policy isolation, and no POST replay
(`crates/agent/tests/service_tunnel.rs`, `scripts/tunnel_smoke.py`). These are
consolidated with remaining final-device work in [release acceptance](release-acceptance.md)
and #14. Actual independent relay VM/disk replacement remains #18; this
checkpoint neither destroys the live relay nor claims that deployment test.
The native `explicit_tls_relay_without_udp_pins_identity_enforces_grants_and_recovers`
fixture runs locally and as an executable on headless Ubuntu: rebuilds its own
stateless TLS relay at the same address, recovers an approved client within the
30-second test bound without replaying a service request, restarts a previously
revoked client with the same key, and denies both its catalog and bound service
request. This checks the authority boundary across relay replacement; it does
not establish cloud provisioning or changed-IP VM/disk replacement acceptance.
Fresh Play-phone browser acceptance is separate from Android app/instrumentation
APK build success.

Normative references: [HTTP/1.1 message framing, RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html),
[Set-Cookie semantics](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Set-Cookie),
and [service-worker script requests](https://w3c.github.io/ServiceWorker/#service-worker-script-request).

Android and desktop launch the stable root URL directly. Browser/API access has
no Wispnet session cookie or launch token; backend application authentication is
unchanged.

## Persistent URL regression coverage

Current regressions cover cookie-free browser navigation and assets, saved deep
links immediately after a real node restart without Open, startup restoration,
restoration failures, stable ports, host authorization, local policy withdrawal,
and continued access beyond the former ten-minute boundary. Header and aggregate
socket limits remain bounded. Historical session/bootstrap checks above describe
the earlier design, before the browser session requirement was removed.

## Device-level HTTP API

The [shared service registry](service-registry.md) uses the device's host/service
grants without per-app consent or API access tokens. All valid website origins
can call service APIs directly under the device's host/service grants. Upstream CORS headers remain stripped; the gateway generates its own
approved-origin headers. The registry and service listeners remain loopback-only,
reject unexpected Host headers, and expose no management operations. Ports are
configurable, with stable saved addresses and explicit conflict errors.

# Wispnet

This is the **client project** (desktop, Android, CLI and TUI), in the
[`wispnet_client`](https://github.com/biplabs/wispnet_client) repository and local
`wispnet_client` folder. Server infrastructure lives independently in the sibling
[`wispnet_server`](../wispnet_server/README.md) project. Neither workspace needs
the other to build or run its normal tests.

**Your services. Any device. Wherever you are.**

Project domain: **`wispnet.app`** · [v1 roadmap](https://github.com/biplabs/wispnet_client/issues/1) · [Implementation issues](https://github.com/biplabs/wispnet_client/issues)

Wispnet brings your self-hosted services into one directory and makes them accessible from your computers and phone, without tracking machine IP addresses or manually forwarding each service through your home router.

> **Every host owns its services and permissions. There is no master node. The cloud is disposable.** Machine 1 is authoritative for Machine 1; Machine 2 is authoritative for Machine 2. Devices assemble trusted service information into a shared directory view. The cloud supplies only a transient connection relay, not a registry or authorization database.

> **Status: development implementation.** An installable native Android app now provides independent host pairing, an aggregated service directory, Start/Stop browser access, and normal-browser handoff. See the [Android build and usage guide](docs/android-app.md). Full lifecycle, network-transition, and gateway release acceptance remain tracked in [#12](https://github.com/biplabs/wispnet_client/issues/12) and [#10](https://github.com/biplabs/wispnet_client/issues/10); this is not a production-ready v1 release. [ADR 0002](docs/decisions/0002-per-host-authority.md) records the architecture.

## Repository layout

- `apps/node/daemon`: running Linux/macOS node and private desktop API.
- `apps/node/cli`: owner commands and the node executable.
- `apps/node/desktop`: stdlib Python owner/cache helpers and their tests (no GUI runtime).
- `apps/flutter`: shared Android/desktop Flutter UI and native Linux/macOS runners.
- `apps/flutter/desktop`: private Python bridge, desktop persistence and adapter tests.
- `apps/flutter/android`: Android runner, native services, Gradle wrapper and Rust JNI adapter in `native/`.
- `crates/core`, `crates/agent`, `crates/registry`, `crates/client`: shared protocol, peer transport, host-local authority and protected browser client/gateway.
- `crates/shell`: WispShell-derived shell protocol, PTY sessions and reconnect history.
- `crates/files`: built-in binary file transfers with durable resume and checksum verification.
- `deploy/node`, `config/node`: client/node deployment and example configuration.
- `config/public/gateway.json`: public gateway bootstrap pin consumed by clients.
- `experiments/browser`: browser-boundary investigations.

Standalone relay/public gateway binaries, the public homepage, Caddy/systemd
server deployment, relay configuration and the Sites relay experiment are in
`../wispnet_server`. Host-local services, authority, Shell, Files and publication
controls stay in the client because each device remains a peer and service host.
See [the split guide](docs/project-split.md) for ownership and validation.

[Linux desktop/tray setup](docs/linux-desktop.md) · [macOS app](docs/macos-desktop.md). Installed service names, state paths and Android package ID stay unchanged by the source reorganization.

Download the [development release](https://github.com/biplabs/wispnet_client/releases/latest)
for Linux x86_64 or macOS (Apple Silicon and Intel), including the node/CLI,
Flutter desktop and installer. GitHub Actions publishes a new development release after
successful builds of `main`; see the [Linux bundle guide](docs/linux-distribution.md)
or [macOS guide](docs/macos-desktop.md) for requirements and installation.

## Why Wispnet?

You run a website on one computer and a `llama.cpp` server on another. Both should appear in one place and work from either computer or your phone, whether you are at home, at a coffee shop, or using cellular data.

| Example service | Hosting device | Intended access |
| --- | --- | --- |
| Personal website | Machine 1 | Either computer or the phone, when authorized by Machine 1 |
| Llama UI and API | Machine 2 | Either computer or the phone, when authorized by Machine 2 |
| Another local web application | Either computer | The clients its hosting machine permits |

The initial target is two Linux computers and an Android phone, with one installed Wispnet application per device. Each Linux installation can host and access services. **An installable Android app that both lists and opens services in the normal browser is mandatory for v1.** Android initially accesses services rather than hosting them.

## The intended experience

1. Initialize each computer independently and configure the shared relay. Neither computer must join a registry owned by the other.
2. Pair a client with each host it should access through an explicitly approved, key-bound invitation. A combined setup UI can organize these approvals without making one machine everyone's authority.
3. Register a service locally on its hosting computer. That host controls its name, backend binding, visibility, and access grants.
4. Each device fetches permitted listings from its trusted hosts and combines them into one local directory. Offline hosts do not block updates from online hosts.
5. Select a service to open its normal web UI, or obtain a protected local API endpoint. Wispnet connects to the actual hosting device.

Registration is explicit, not automatic exposure of every listening process. A service still requires its own host and backend to be running.

## Architecture: per-host authority, one directory view

**This supersedes both previous centralized proposals: a cloud registry and a registry on Machine 1 that controls the whole mesh.** Machine 1 and Machine 2 are symmetric peers. There is one writer for each host's own records, not one writer for the entire network.

| Device | Authoritative for | Cached or replicated information |
| --- | --- | --- |
| **Machine 1** | Its identity, local services/backends, client approvals, service grants, and revocations for its services | Permitted service listings and trusted identities from other hosts |
| **Machine 2** | Its identity, local services/backends, client approvals, service grants, and revocations for its services | Permitted service listings and trusted identities from other hosts |
| **Android phone** | Its own identity, trusted peer pins, and local browser/API sessions | Permitted service listings from both machines |
| **Cloud relay** | No Wispnet service or authorization decisions | Transient live-connection and encrypted-packet routing state only |

Every hosting agent has a local registry and policy store. A client-facing directory is an **aggregated view**, not a third authoritative database. Copies of listings never confer permission to use or administer a host, and do not contain other devices' private keys or owner credentials.

There is no master election, required quorum, global registry lease, or mandatory online administrator for unrelated hosts to operate. Owning or compromising Machine 1 does not authorize changes to Machine 2 unless Machine 2 has separately granted such administrative power. v1 does not assume that delegation.

### Service identity and ownership

Use **`(hosting EndpointId, local service ID)`** as the canonical service identity. The readable service ID is also its UI label; devices likewise have one readable ID, with no separate display name. Both machines can publish a local service called `llama` without a global naming lock or collision.

Each host publishes only its own service records and versions them independently. For v1, clients synchronize directly with the authenticated owning host. Cached records retain their host identity, version, and provenance. A peer cannot overwrite another host's records, transfer its services to itself, or restore an old deletion by replaying a cached listing.

Any future forwarding of catalogs between peers needs verifiable origin and freshness, such as host-signed records; a forwarding peer's assertion alone is not authority. Gossip, a shared writable database, and distributed consensus are not prerequisites for the v1 directory.

### Authorization stays at the host

When your phone opens Llama on Machine 2:

1. The phone selects Machine 2's Llama entry from its local directory.
2. It connects to the pinned Machine 2 identity and requests the local service ID.
3. Machine 2 checks the authenticated caller against **its own current, durable access policy**.
4. Only then does Machine 2 open its configured local backend.

Machine 1 and the cloud are not consulted to approve that request. A client cannot supply an arbitrary upstream address, claimed user identity, or cached allowlist to override Machine 2's decision.

```text
Phone browser
    |
    v
Phone's protected local gateway + aggregated service directory
    |
    | Authenticated Iroh connection to Machine 2
    | Direct when possible; relayed when necessary
    v
Machine 2: local service registry + current local permissions
    |
    v
Local llama.cpp backend

Machine 1 is not in this control or application path.
```

A local service can use a local fast path, with the same policy and browser-boundary checks. Registry/control and service protocols share the agent's networking lifecycle without requiring a global registry bootstrap.

## Independent operation and failures

**Machine 2 does not run on a permission lease issued by Machine 1.** Losing contact with Machine 1 must not expire Machine 2's grants, prevent its restart, or stop it approving clients and changing its own services.

| Situation | Required v1 behavior |
| --- | --- |
| Machine 1 is off | Machine 2 continues serving, registering/updating its services, and accepting locally authorized pairing/revocation. Its clients can restart and reconnect. Machine 1's own services are unavailable. |
| Machine 2 is off | The same independence holds for Machine 1. No promotion or replacement master is needed. |
| A cached listing is old | Mark it stale and refresh from that host when reachable. An old listing is not authorization; the host's current policy decides access. |
| A host removes a service or revokes a client | That host rejects new access and cancels affected sessions according to the tested local enforcement bound. Replayed catalogs cannot undo it. |
| A client loses its directory cache | It can refetch from its saved trusted host identities; no central registry is required. Losing a host's authoritative policy store is a different recovery problem. |
| The cloud relay is replaced | Restore connectivity without resetting peer trust, local registries, or grants. |

Catalog freshness and any explicitly expiring host-issued grant are separate concepts; service URLs require no Wispnet browser session. An unrelated host's outage must not expire access. A trusted known host can be contacted directly to refresh missing/expired listing metadata before opening a service; malformed records and unknown identities fail closed.

This removes a **mesh-wide registry/authorization single point of failure**, not every dependency. Each service still depends on its own hosting computer and backend; Wispnet does not replicate application execution or wake sleeping computers. v1 has one relay, so relay-dependent connections and some new connections can be interrupted by its outage. Multi-relay resilience is future work, not a v1 availability guarantee.

## Built-in services

Every desktop host includes the reserved built-in `shell` service. Grant access
to selected paired devices, then open Shell from desktop/Android or run
`wispnet shell DEVICE_ID`. See [shell access](docs/shell.md) for terminal modes,
session persistence and CLI grant management.

Inboxes and shared folders are named folder services with independent permissions.
On desktop or Android, use **Local services → Share a service** and choose **Receive
files** (Inbox) or **Share files** (Shared folder). Create multiple folders and
grant each one to selected devices. The CLI provides `wispnet inbox` and
`wispnet share` for setup, `files put` for inbox uploads, and `files ls|get` for
browsing and downloading shares. Folder downloads use `files get --recursive`
or the app's folder download button. New peers automatically use fast streaming
compression when useful, without creating archives. See
[file transfers](docs/files.md) for
migration details, resumable CLI/SDK use, and throughput targets.

Run `wispnet` for interactive QR pairing, pending approvals, machine removal,
local service grants and owner operations. Use `ssh -t desktop.home wispnet`
on a headless computer. See the [terminal guide](docs/terminal.md).

## Pairing and revocation without a master

An invitation identifies the **target host**, its pinned EndpointId, the configured relay, and a short-lived enrollment capability. The client authenticates that host before sending secrets. Host-specific approval records remain on that host. Pairing with Machine 1 does not automatically approve the phone on Machine 2, nor imply that either computer can administer the other.

The owner may use one UI to add or revoke a phone across several hosts, but each target applies its own authorized operation. The UI must show per-host results, including pending/offline targets. A newly initialized phone can pair directly with Machine 2 while Machine 1 is absent, provided the owner approves at Machine 2.

**An offline host cannot apply a revocation it has not received.** The product must not report a mesh-wide revoke as complete until every selected host has confirmed it. Peer discovery, matching display names, a shared relay, or possession of an old invitation never creates transitive trust. v1 uses direct host approval rather than a mandatory central membership authority. See [#17](https://github.com/biplabs/wispnet_client/issues/17).

## How known peers find each other

Each device saves its trusted peers' identities and relay bootstrap information.
The explicitly configured primary relay is **`relay.wispnet.app`**.

The current relay infrastructure runs on `pi3.home`, with Caddy providing automatic
public TLS certificates. The desktop node remains a separate role. See the
[relay deployment and upgrade guide](../wispnet_server/docs/desktop-relay.md). Native tablet access
through a cellular hotspot has been verified; see [external-network validation](docs/external-relay-validation.md).

```text
Expected peer:  <target host's public EndpointId>
Initial route: https://relay.wispnet.app
Direct hints:  optional; current peer IP is not required for relay reachability
```

The target establishes an outbound connection to the configured relay. The sender uses the target ID and relay address to establish an authenticated connection, with direct peer paths attempted where available. The relay's live connection table supplies reachability, not a service catalog or permission decision. The target must be online and reachable for access to work.

Use the same addressing mechanism for pairing, catalog synchronization, and service access. No discovery request to Machine 1 is required before contacting Machine 2. The implementation must not depend on a cloud EndpointId-to-IP database, public Pkarr/DHT publication, gossip, or default public relays. Ordinary DNS lookup of the configured relay hostname is allowed. See [#13](https://github.com/biplabs/wispnet_client/issues/13).

## Cloud is purely transient and disposable

**The cloud VM and its entire disk must be replaceable without restoring a Wispnet database, losing any host's service registrations or permissions, or re-pairing devices.**

The cloud runs only the connection relay and necessary infrastructure. It must not store a Wispnet service catalog, enrollment/approval database, permission history, revocations, host/owner private credentials, or durable node-discovery records. Registry and service content remain encrypted between the actual endpoints; the relay does not become an application gateway.

The required recovery test is to destroy the isolated test relay and disk, rebuild it from a reproducible recipe, and let agents reconnect. Test both the same IP and a different IP behind the same hostname. Each host retains its own registrations, identity, grants, and revocations; no cloud application database or central-master recovery is involved.

**Transient is not literally zero runtime state.** A relay necessarily handles sockets, encrypted packets, and temporary routing information. Connection identities, addresses, timing, and traffic volume may be visible. Disable payload capture and unnecessary persistent peer metadata. Keep infrastructure configuration, DNS/domain control, TLS provisioning, and any admission/resource-limit configuration recoverable outside the disposable VM. These are infrastructure requirements, not service authorization.

**Disposable is not uninterrupted.** Relay-dependent streams may fail, and some new connections may be unavailable until recovery. Re-resolve DNS and reconnect with bounded backoff; never silently replay a request that may have changed backend state. Relay replacement must not alter trusted host pins or grants. [#18](https://github.com/biplabs/wispnet_client/issues/18) and [#14](https://github.com/biplabs/wispnet_client/issues/14) require these tests. A future relay fleet should preserve independent peer identities and per-host authority.

## Durable state and recovery belong to each host

Back up **each host's** identity, local registry/policy database, administration credentials, service configuration, and recovery/version metadata on protected owner-controlled storage outside the relay. Client directory caches are rebuildable views, not backups of another machine's authority or keys.

Restoring Machine 1 must not reset Machine 2, require its approval, or block it operating. Restore a host under its original identity only with the appropriate protected recovery material; lost or compromised keys require explicit trust repair. A stale backup cannot reveal revocations that happened after it was taken. Recovery must reconcile those decisions or fail closed rather than silently restore old access. Do not clone the same active host identity onto two independent writers. See [#19](https://github.com/biplabs/wispnet_client/issues/19).

## Browser and application security

The [browser spike](experiments/browser/README.md), [ADR 0001](docs/decisions/0001-browser-access.md), and [evidence matrix](docs/browser-compatibility.md) establish the tested browser direction: service-specific hostnames and an HTTP-aware session/origin boundary. Per-host authority does not make an unrestricted local byte proxy safe.

Bind gateways only to loopback, enforce device pairing and host/service grants, validate exact Host and appropriate Origin/Fetch Metadata, and protect WebSocket upgrades. Browser pages and APIs require no Wispnet cookie or launch token. Different ports are not cookie isolation. Local service origins must be bound to the full host/service identity, so two peers publishing `llama` cannot collide or share application cookies.

Production cookie-prefix/Secure-cookie, HTTPS, hostname reuse, redirect/login, service-worker, streaming, and Android handoff decisions remain release requirements in [#10](https://github.com/biplabs/wispnet_client/issues/10). A directory entry or authenticated device connection is not proof of the human user's intent. Backend applications may still require their own authentication.

`wispnet.app` names the project; it does not require public service subdomains, a cloud application proxy, or public HTTPS controllers at home. Browser HTTPS and relay TLS are separate from host registry transport.

## v1 roadmap

[Issue #1](https://github.com/biplabs/wispnet_client/issues/1) tracks the accepted architecture. Existing completed foundations remain useful; their completion does not prove the per-host migration has shipped.

| Workstream | Tasks |
| --- | --- |
| Foundations | [#2 Browser spike](https://github.com/biplabs/wispnet_client/issues/2), [#3 Workspace/local checks](https://github.com/biplabs/wispnet_client/issues/3), [#4 Identities](https://github.com/biplabs/wispnet_client/issues/4), [#5 Tunnel](https://github.com/biplabs/wispnet_client/issues/5), [#6 Registry foundation](https://github.com/biplabs/wispnet_client/issues/6), [#7 Enrollment foundation](https://github.com/biplabs/wispnet_client/issues/7) |
| Independent hosts and connectivity | [#16 Per-host registry/control](https://github.com/biplabs/wispnet_client/issues/16), [#13 Known-peer relay transport](https://github.com/biplabs/wispnet_client/issues/13), [#17 Pairwise approval](https://github.com/biplabs/wispnet_client/issues/17), [#18 Disposable relay](https://github.com/biplabs/wispnet_client/issues/18) |
| Shared directory and access | [#8 Host-owned service records](https://github.com/biplabs/wispnet_client/issues/8), [#9 Catalog synchronization and host-local enforcement](https://github.com/biplabs/wispnet_client/issues/9), [#10 Browser/API gateway](https://github.com/biplabs/wispnet_client/issues/10), [#11 Linux directory](https://github.com/biplabs/wispnet_client/issues/11), [#12 Mandatory Android app](https://github.com/biplabs/wispnet_client/issues/12) |
| Recovery and release | [#19 Per-host backup/recovery](https://github.com/biplabs/wispnet_client/issues/19), [#14 Independence/security/recovery tests](https://github.com/biplabs/wispnet_client/issues/14), [#15 Packaging/documentation](https://github.com/biplabs/wispnet_client/issues/15) |

Start per-host control/storage and explicit-relay transport first, then integrate pairing, registration, and synchronization. Android scaffolding and relay deployment can proceed in parallel. Feature tests accompany implementation; final end-to-end validation is not a prerequisite to starting those features.

### Release gates that prove independence

- Both computers and the Android app list and use the website and Llama through ordinary browsers across LAN, unrelated Wi-Fi, cellular, and forced relay, without manual router forwarding.
- Shut down Machine 1. Restart Machine 2 and the phone, use Llama, change/register a Machine 2 service, and pair a fresh client with Machine 2. None of this needs Machine 1 or a Machine 1-issued lease. Repeat symmetrically with Machine 2 shut down.
- Both hosts publish the same local service ID without collision. One host cannot forge another's listing, grants, deletion, or peer approval. Stale catalogs cannot restore removed services or revoked access.
- Host-local revocation cancels affected sessions within a measured bound. Multi-host actions show partial/offline results instead of falsely claiming global completion.
- Delete and recreate the relay and disk, including at a changed IP. Existing pairings and every host's state survive; clients revoked at each host stay denied there.
- Restore one host from protected state without resetting the others. Exercise wrong identities, stale backups, malformed catalogs, browser attacks, stream cancellation, Android lifecycle, and network changes.

Use isolated fixtures for destructive tests. A fresh user must be able to reproduce installation and the per-host-backup/cloud-rebuild workflows from the published guide.

### Outside v1

A required central registry/authorization server, master node, global permission lease, and durable cloud application/discovery database are outside the accepted architecture. Multi-relay automatic failover, application replication, shared writable records requiring consensus, transitive/delegated mesh-wide administration, gossip/DHT discovery, VPN/TUN, arbitrary LAN routing, process auto-discovery, enterprise roles, path ACLs, identity-header injection, and automatic wildcard certificates are not default v1 scope.

Independent single-writer records from many hosts are part of v1; they must not be confused with conflicting writers to one global record. Future relay redundancy must not reintroduce centralized service authority.

## Development and migration status

Follow [AGENTS.md](AGENTS.md) and [the contributor guide](docs/development.md): direct work on `main`, local pre-push checks, no mandatory PR/GitHub Actions workflow. Link implementation commits to issues and preserve existing security/storage tests.

The existing development foundations include [Linux identity storage](docs/identity.md), the [bounded authenticated service tunnel](docs/service-tunnel.md), [registry persistence/API](docs/registry-api.md), [key-bound enrollment](docs/enrollment.md), and the browser spike. Native CLI/USB evidence is not a shipping Android app or proof of cross-network relay recovery.

The first [independent-host control checkpoint](docs/per-host-control.md) now provides explicit schema-2 configuration, host-local administration/state, and a filtered Iroh catalog protocol sharing the service endpoint. Symmetric local-process tests exercise cold startup, owner changes, service access, failure isolation, and separate local owner credentials. #16 remains open for relay/pairing integration and full acceptance evidence; this is not a completed migration or release.

The [explicit relay checkpoint](docs/relay-connectivity.md) adds validated HTTPS relay configuration and pinned-ID catalog/service connections without a peer IP. Local TLS fixtures exercise relay-only traffic, native CLI use, grant denial, relay restart and direct-path survival during outage. Isolated Linux fixtures also test hostname-preserving DNS/IP replacement, block all peer UDP, and audit attempted network destinations with public access blocked. #13 remains open for physical app/network transitions and deployed-relay evidence.

The [pairing bootstrap checkpoint](docs/pairing-bootstrap.md) adds a pinned pre-approval hello on the shared endpoint, including relay-only access without a catalog or peer IP. It exposes only the authenticated host ID and protocol version. A shared host-bound invitation/proof contract and protected `pairing-check` preflight validate pins, relay configuration and expiry without sending capabilities. Host-local issuance, signed proof exchange and pending owner approval now run over that same relay path, with single-use state and separate service grants. Acknowledged pairing saves protected, secret-free per-host bootstrap records for later connections without an invitation. Multi-target workflows and Android app integration remain #17/#12 work.

The [native relay lifecycle harness](docs/native-relay-validation.md) also exercises separate Linux processes and the authorized Android native CLI over USB, including independent host approvals, HTTP service bytes, offline startup, relay recovery and persistent local revocation. A [peer path diagnostic](docs/peer-diagnostics.md) also verifies physical tablet LAN/HTTP access and direct → USB-relay → direct recovery on one endpoint through Wi-Fi off/on with explicit network notifications. The [physical two-host harness](docs/physical-relay-validation.md) also verifies two independent Linux computers and the tablet with the opposite host agent stopped, including actual local backends, restart, relay rebuild and isolated revocation. The Android app, cellular/unrelated-Wi-Fi handoff and deployed-relay validation remain open.

Older implementation guides describe earlier HTTPS-controller prototypes, not the accepted architecture. Development now uses fresh per-host state; production migration/import compatibility is not current work. #16/#17 focus on independent host control and target-bound pairing without a global owner credential. #9 supplies per-host catalog synchronization and live local authorization. The checkpoint above does not mark these complete.

**One service directory. Independent hosts. No master node. A disposable cloud relay.**

Owner nodes can optionally [publish a local HTTP service publicly](docs/public-access.md)
with an HTTPS URL under `wispnet.app`; private device grants do not permit
republishing another node's service.

## License

Wispnet's original code is licensed under [Apache-2.0](LICENSE). Vendored code,
fonts and other third-party components retain their own licenses; see [NOTICE](NOTICE).

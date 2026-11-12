# Linux desktop and KDE tray

The desktop window and tray attach to the running node through its owner-only `node.sock` in the configured state directory. The node retains the one identity lease and Iroh endpoint; UI operations do not start a second endpoint or send owner credentials over the network. Relay and node remain independent roles.

## Build and install

For installation without a checkout or Rust toolchain, use the
[Linux distribution bundle](linux-distribution.md). Files-only staging keeps
first-time initialization explicit and supports headless hosts.

For an already initialized schema-2 node using `~/.config/wispnet-node/host.toml`:

```sh
cargo build --workspace --locked
scripts/install_node_desktop.sh
wispnet-desktop
```

The default desktop is the shared Flutter app in `apps/flutter`. It uses the same
launch screen, typography, machine cards and settings sheets as Android. There is
no sidebar: the gear next to the device name opens desktop owner tools. Local
services have a separate section with service management; remote machines and
their Manage/refresh controls appear under Remote sector; service cards use extra columns
when the window is wide.

Build prerequisites are Flutter 3.47.9, Clang, CMake, Ninja, pkg-config, GTK 3 and
Ayatana AppIndicator development libraries. Runtime needs GTK/AppIndicator,
`xdg-open`. The private node/cache/owner-operation adapter is compiled into the
native Rust executable. WebKitGTK, Tauri, Qt and PySide are not used by the installed Flutter UI.
The installer builds and stages the complete Flutter release bundle before
switching the launcher. Identity, configuration, grants, pairings, catalog cache,
private machine names and card ordering are retained. Tauri preferences are read
without modifying the old WebKit database. Previous runtime files are retained
for recovery; they are not started alongside Flutter.
It requires an active `graphical-session.target` for graphical installation.

Installed GUI files remain `~/.local/libexec/wispnet-desktop`, launcher
`~/.local/bin/wispnet-desktop`, app entry `~/.local/share/applications/wispnet.desktop`
and session service `wispnet-desktop.service`. The node remains `wispnet-host.service`.
Closing the window hides it to the tray; quitting leaves the node running.
Relaunching reveals the existing window. The app uses the installed default host
configuration. Alternate-config automation remains available through `wispnet-node --config`.
Existing Qt peer names and ordering are imported on first use; private catalog
snapshots and combined-operation journals are reused directly. The Qt UI, PySide dependency and obsolete UI tests have been removed.

The header’s **Go offline** button stops `wispnet-host.service`, closing the node endpoint, peer connections and local browser routes. The desktop stays running; **Go online** starts the same node and restores saved browser listeners before reporting online. Desktop startup does the same; if the node restarts while the UI remains open, its next successful snapshot restores the listeners. Identity and grants persist, and the independent relay service is untouched. This lifecycle control uses the installed default node configuration; alternate-config desktops do not control the default service. Browser access has no separate UI switch or success message.

## Headless hosts

Run `wispnet`, or `ssh -t desktop.home wispnet`, for interactive pairing,
approvals, service grants and machine removal. See the [terminal guide](terminal.md)
for controls and feature details.


The same installer automatically selects node-only mode when `graphical-session.target` is inactive. Force it explicitly with:

```sh
scripts/install_node_desktop.sh --headless
~/.local/libexec/wispnet-node/wispnet host-status --config ~/.config/wispnet-node/host.toml
# Query the running node without a second identity lease:
printf '%s\n' '{"op":"catalog","host":"PINNED_HOST_ID"}' | ~/.local/bin/wispnet-node
systemctl --user status wispnet-host.service
```

This mode installs/enables the node and preserves its existing configuration, identity and grants. It skips the desktop launcher and tray service; it does not require a display server or a graphical login. The dependency-free `~/.local/bin/wispnet-node` command reads one bounded JSON request from stdin (an empty request returns status) and talks to the running node's owner-only socket. It verifies socket permissions and OS peer credentials. Pairing capabilities and other request material stay out of process arguments. Use operations such as `catalog`, `pair`, `mutate`, `enable`, `open` and `stop` from headless automation; `open` returns a protected browser bootstrap URL for the caller. Treat that URL as a short-lived secret. This reuses the daemon identity/endpoint; the older standalone `host-catalog`/`pairing-request` transport probes require a separate identity lease and are not the live-node management interface. Run the graphical mode later only if a desktop session is actually wanted. For unattended boot, the user service manager must run without a login (for example, an already enabled user linger setting); the installer does not change that host policy.

## Views and actions

- My services: actual host-local backend bindings and client allowlists; share/edit/remove with revision conflict enforcement. Click a local service tile to open it; browser access starts automatically. Owner-local browsing uses the protected gateway and a direct loopback backend connection, without self-pairing, self-grants or relay traffic. It resolves the registered binding from current durable state, refuses missing/corrupt authority, and interrupts active responses on removal or backend replacement. Existing browser routes cannot switch to a different backend binding. Local owner access does not widen the remote client allowlist. Backend addresses remain loopback-only. Editing replaces the complete selected allowlist and requires confirmation.
- Devices: approved client identities and current authenticated inbound QUIC connection counts. Approved devices with a successful catalog or permitted service request in the last 60 seconds remain in the active-client total even after their connection closes. The Activity column distinguishes a live connection, recent activity and idle. Multiple connections from one peer count once in the tray total. These are transport connections, not a count of people, browser tabs, or service sessions. Short catalog checks may end between tray polls. No extra heartbeat traffic is sent. The timer uses the node’s monotonic clock, starts on an authenticated approved catalog or permitted service request, and refreshes at service-stream completion. Live connections count throughout their lifetime. An idle phone with Wisp space enabled disappears after its requests/connections end and the activity window expires; this is recent activity rather than proof of continuously enabled access. Node restart clears the in-memory activity. Revoked clients are excluded from recent activity immediately. Transport stream totals include control traffic.
- Peer services: independently refreshed catalogs from saved target pins; click an available service tile to open it; browser access starts automatically. Private persistent snapshots in `desktop-catalogs/<host-id>.json` retain host provenance, revision, generation and last-refresh time. Restart loads entries only for saved pins, labels them cached, and independently refreshes each host; cached/denied/offline/rejected entries cannot Open. Each remote Open still fetches a live owning-host catalog and the host enforces its current grants. Lower revisions, changed generations and inconsistent same-revision contents remain rejected across desktop restarts until explicit authenticated target re-pairing resets that baseline. The shared native client also persists this evidence separately in `catalog-baselines/`, including for headless callers. Snapshots are bounded to 128 KiB, validated, mode 0600, and atomically replaced in a mode-0700 directory; corrupt/unsafe files are ignored and refetched. Cache write failure is reported. Deleting this display-cache directory leaves pins and local authority intact and permits a fresh authenticated refetch; the native `catalog-baselines/` high-water source remains intact. No LAN scan or central discovery is implied. Empty peers means this node has no saved host pins yet. Approving a mutual desktop/headless request also saves the requesting host; phone approvals remain client-only.
- Pairing requests: approve/reject a specific proved request on this host. Choose service grants while approving; approval and the selected grants commit together. Pair with a host accepts a pairing code, shows the full host ID for comparison and explicitly requests target-owner approval. Between two updated desktop/headless nodes this single request pairs both machines. The scanning machine grants no new service access to the QR host; allow it later through the local service editor. Each machine controls its own allowlists. Existing pairings are preserved and are not automatically converted; phone clients keep their existing client-only flow.

Each remote browser Open rechecks the target's permitted catalog; local Open checks the owner's current registry. Both use the same loopback HTTP gateway as Android and open the stable service URL directly. No Wispnet session cookie, launch token or temporary handoff file is required. Saved routes are restored when the app comes online, so existing tabs and bookmarks do not need another service click. The gateway still validates Host, browser request metadata and the current host/service grant. Taking the node offline closes its listeners and active connections. Backend applications retain their own login cookies and API authentication.

The tray reports node availability, the active-client total (live connections or approved activity within 60 seconds), approved count, pending requests and configured relay connection state. It never counts saved approvals as activity; recent activity is UI metadata and grants no permissions. This does not inspect a separately operated relay's server process.

## Flutter validation

Settings → Access & pairing opens the single Paired devices list, including
approved inbound clients without a saved remote-host entry. Pending requests
appear at the top and refresh while the list is open. Each device's settings
contains permissions for this machine's services and removal; changing one
permission preserves pairing and other devices' grants. Owner operations across
hosts are under Settings → Advanced → Owner operations.

`scripts/test_flutter.sh` checks shared narrow/wide layouts, owner sheets,
explicit grants/public-link confirmation, input retention, preference migration,
cache provenance, stale-incarnation rejection, and a real isolated-node HTTP route.
`apps/flutter/integration_test/desktop_test.dart` exercises the native window,
settings, online/offline lifecycle and a real browser handoff using the installed
node. It does not grant, remove or recreate any existing service or device.
The release installer is checked with the existing identity and authority data,
registered tray, single-instance reopening and engine loaded from its installed
bundle. See `apps/flutter/README.md` for the native test command.

## Tests and current limits

`scripts/test.sh` includes real two-node local-socket tests for pairing/pins, target-owner approval, permissions, conflict rejection, browser bootstrap/service access, live connection increase/decrease, recent-activity refresh/expiry and revocation filtering, and native Stop, plus real owner-local HTTP/bootstrap/origin/removal/Stop checks with no self-pin or self-grant. Flutter widget and adapter tests cover selected approval grants, revision capture, settings, service edits, public access and stale Open blocking. Cache regressions cover corrupt/oversized/foreign-host files, permissions, symlinks and deletion independent of pins. All original Rust/relay/browser tests remain local; the pre-push hook runs them. The remaining Python tests exercise the stdlib owner/cache helpers without Qt. Native installation is checked against KDE's registered StatusNotifierItem and actual node services/client records.

The version-1 catalog supplies complete sorted records with service IDs, incarnations, derived legacy display-name fields and explicit HTTP/TCP types; see [catalog snapshots](catalog-snapshots.md). This desktop uses the existing HTTP browser gateway. Own-service local browsing and persistent desktop catalog caching are implemented. The broader full-power-off/rebuild acceptance under #11 remains separate work. Combined multi-host operation status is implemented below. The second physical computer can be headless; it does not require another desktop GUI. Service incarnation IDs now distinguish removal/recreation even at the identical backend address; see [service lifecycle safety](service-lifecycle.md). Versioned complete snapshots and native durable high-water checks are implemented. A checkout-independent Linux x86_64 bundle and installer are available; see
[distribution](linux-distribution.md). Full release distribution acceptance remains #15. This checkpoint does not close those issues.

## Physical GUI/headless checkpoint — 2026-10-17

Installed the updated desktop/node on CachyOS/KDE (`j-strix`) and the same node on headless Ubuntu 24.04 (`desktop.home` / `j-desktop`). The remote host has no graphical session, no Wispnet tray unit or Qt app installation, and its already enabled user linger setting keeps the node independent of SSH sessions. Both nodes were independently paired and explicitly approved through their own local administration; each peer was added to the relevant existing service allowlists, preserving the phone/tablet grants and both host identities/generations.

- Local GUI → headless host: a Qt interaction check using the installed desktop selected the actual remote `website` row and double-clicked it. The real private handoff HTML opened the remote WispShell site in Chromium with an HttpOnly gateway session. The GUI displayed the authenticated peer catalog and persisted its private snapshot.
- Headless host → local computer: the existing Firefox installation, run headlessly through its existing geckodriver, loaded the actual GIKE and Llama UIs through the headless node's protected browser gateway. The Llama health API returned HTTP 200 through that browser session. No display server, Qt GUI, USB forwarding or SSH port forwarding carried the service traffic.

Browser access returned to its original disabled setting on both nodes; temporary browser profiles, handoff files and test processes were cleaned. The paired peer identities and explicit grants remain for normal use. This proves physical GUI/headless peer browsing in both directions, not machine power-off, deployed relay replacement, a second visible GUI, or a new Llama generation/streaming run.

## Combined owner operations

The **Owner operations** screen adds or revokes a specified client independently on selected hosts and shows one **applied**, **pending** or **rejected** row per host. Adding a client does not grant services automatically. Current service grants and local approvals remain owned by each host. This is an optional owner workflow, not a required controller or delegated mesh-wide administration.

The local host uses its existing private node socket. Other hosts require an explicitly configured, already trusted SSH owner connection; service pairing alone never supplies administrative rights. Store the selected owner routes in the private (mode 0600) `~/.config/wispnet-node/admin-targets.json`, for example:

```json
[
  {"host": "FULL_LOCAL_HOST_ID"},
  {"host": "FULL_REMOTE_HOST_ID", "ssh": "desktop.home"}
]
```

The UI lists only these owner routes (or just this host if no file exists), requires target selection and confirmation, and shows pending/rejected results without claiming global completion. SSH uses existing strict host-key verification and noninteractive authentication; it does not copy host keys, Wispnet owner tokens, or global credentials. The contacted node checks its own full host ID before touching policy. There is no new public or Iroh admin RPC.

Headless clients use the same workflow without Qt. A private plan file contains an action and its selected targets:

```json
{
  "action": {"op": "revoke", "peer": "FULL_CLIENT_ID"},
  "targets": [
    {"host": "FULL_LOCAL_HOST_ID"},
    {"host": "FULL_REMOTE_HOST_ID", "ssh": "desktop.home"}
  ]
}
```

```sh
chmod 600 /private/revoke-plan.json
wispnet-node --batch /private/revoke-plan.json
wispnet-node --operations
wispnet-node --retry OPERATION_ID
```

For explicit owner approval, use `{"op":"approve","peer":"FULL_CLIENT_ID","name":"client-id"}` as the action. This is the existing owner-local known-key approval decision, not an invitation proof, reciprocal grant or admin delegation. Target service grants still require separate host-owner decisions.

An intent is atomically saved before dispatch in private `combined-operations/` journals. Up to four targets execute independently. An unreachable target, failed owner connection, timeout or unconfirmed reply remains **pending**; a negative host acknowledgement is **rejected**. Only all-applied targets make `complete` true. A batch with rejections can be `finished` without being complete. Retry acts only on pending targets; rejected targets need a newly reviewed operation. No background retries or automatic replay of arbitrary HTTP/application writes occur.

Each host stores the operation ID, immutable action hash and acknowledgement atomically with its own authority mutation in SQLite schema 10 (receipts introduced in schema 6). A repeated ID returns the saved receipt; changing its action fails closed. If a reply was lost after commit, retry confirms the earlier result without re-executing it. An old approval receipt cannot recreate approval after a later local revocation. Applied revisions are historical acknowledgements, not a promise that current permissions still match that earlier action; the serving host's current policy always wins. New-access policy is durable at acknowledgement; affected active streams close under the existing bounded host-policy checks, not through catalog propagation.

Limits are 32 selected hosts, four concurrent attempts, 128 local operation journals and 10,000 retained target receipts. Receipt replay protection is not automatically evicted; hitting its limit rejects new operations. Completed local journals can be archived explicitly without changing host permissions or deleting target receipts. Host recovery must preserve receipts along with authority; stale-backup recovery remains #19 work. Older binaries that support only schema 5/6/7/8/9 cannot open schema 10; the new installer preserves identities/configuration rather than resetting authority.

Observed on both physical computers on 2026-10-17: combined approval applied at both hosts; revocation with headless desktop.home stopped showed local applied / remote pending, while its offline registry still contained the client. Pending status survived a local node restart and completed after the remote node restarted, without repeating the local mutation. Already-revoked and wrong-host actions produced explicit rejection. A deliberately discarded real applied reply remained pending; after remote restart and a later revocation, retry returned its earlier applied revision without restoring access or advancing the current revision. The headless CLI also performed a target-local combined operation and reported its rejection. The installed GUI displayed actual applied/pending/rejected journals, including a pending host, and its rendered table was inspected. Tests used a temporary client without service grants; fixture approvals were removed, phone/tablet grants and backend bindings were preserved, and original browser settings were restored.
## Generate a pairing QR

After installing, run `wispnet --qrcode` in a terminal, or
`ssh -t desktop.home 'wispnet --qrcode'` for a headless computer.
An alternate configuration can be selected with `--config /absolute/path/host.toml`.
The terminal must be wide enough for the QR; the command reports the required
width if it would wrap. Reduce the font size or widen the window and retry.

In the desktop app, select **Pair this machine** in the local machine header. The
dialog offers **Copy pairing code**, its matching QR, the host ID, and a five-minute countdown. It clears the code and image on
expiry or when the invitation is used. Close the dialog to review incoming
requests. Generating the QR runs in the background; an offline node reports an
error and can be started with **Go online**.

On Android, select **Scan pairing QR**. Scanning submits a request; the target
owner still approves that request and grants individual services. Keep the QR
private: it contains a short-lived, single-use enrollment capability. No image
or invitation file is written by these two actions.

## Naming compatibility

The product, executables, crates and user services are named **Wispnet** /
`wispnet`. The installer upgrades the historical misspelled node configuration
path and service name while retaining the configured state directory, identity,
owner credentials and grants. The relay installer similarly preserves TLS/ACME
state while upgrading its paths and service. Old `wipsnet/...` wire protocol IDs
and signing-domain bytes remain frozen so installed clients can still connect;
those strings are compatibility identifiers, not product names.

## Approve a device and select services

The incoming-request **Approve** action opens a review with the full device ID
and local-service checkboxes. New grants start unchecked. Existing grants appear
checked and locked, since this action adds access; use service configuration to
withdraw existing grants. Confirm **Approve device** to pair and grant the selected
services together in one policy revision. Cancelling changes nothing. No selection
pairs the device without adding access. A concurrent policy change, removed service,
or expired request rejects the entire operation and requires a fresh review.
The terminal approval flow offers the same selection and atomic behavior.

## Removal learned on contact

The machine-card trash button atomically revokes local access and forgets the
host. The other machine learns this during its next service-directory refresh or
Open operation that checks the catalog. It queries the pinned host over the
identity-authenticated pairing protocol, which remains reachable after service
access is revoked. An explicit **removed** response causes local cleanup of that
same pairing and all its grants. Pending approval, an empty service catalog,
unknown status, generic denial and transport failure keep the record intact.

There is no outgoing deletion queue, acknowledgement retry loop or mailbox.
Both machines must eventually be reachable at the same time. Desktop catalog
refresh discovers removal automatically while the app is running; in the TUI,
browsing or refreshing a host's services performs the check. New pairings share
a unique session ID, and cleanup compares it again under the local transaction
so a delayed response cannot delete a newer pairing. Existing saved file pins
are indexed without adding clients or widening grants. Local removal markers
prevent those pins reappearing after a restart.

Both desktop/headless nodes need this update. Updated Android clients implement the same check in both directions. Older phone clients that do not implement it may retain stale entries. SQLite schema 10 stores
session IDs and local removal state; older binaries must not be used against the
upgraded state. Individual service grants remain independently controlled.

Refreshing the GUI/TUI device directory also checks authenticated pairing status
for legacy client-only phones, without creating saved host pins or granting any
services. These probes are demand-driven, bounded to four concurrent attempts,
and rate limited to one per device every 30 seconds. Offline and older devices
remain listed. An updated Android app answers while its process is reachable.

## Release version

The shared product version is `workspace.package.version` in `Cargo.toml`.
The Rust helper `scripts/build_identity.sh` supplies version metadata to Cargo,
Flutter and Android builds without Python. Python remains a development dependency
for integration tests, packaging and release automation. All three interfaces show
it with a commit SHA at the bottom right; `wispnet --version` also prints it. Android derives
its default version code as `major * 1000000 + minor * 1000 + patch`.
Debug identifiers append `.dirty.<source fingerprint>` for uncommitted changes,
so the same label identifies the same source snapshot across machines. Rebuild
and reinstall after editing or committing to update the displayed identifier.
Explicit Android release-publishing version overrides remain available.

## Shared registry and service port

**Settings → Wispnet port** configures one loopback HTTP port for the registry and all services (7842 on new installs). HTTP Host headers select each service. The same HTTP API is available on
Android; see the [registry contract and examples](service-registry.md).

## Public access

Local HTTP service settings offer **Enable/Disable public access**. Service cards
show the public tunnel state and include the public URL in the link popup when
enabled, including on paired desktop and Android devices after refreshing the
host. Remote cards show links without publishing controls. See
[owner-controlled public services](public-access.md) for authority, deployment,
privacy and owner API details.

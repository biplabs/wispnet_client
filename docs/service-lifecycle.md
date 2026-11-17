# Service removal and recreation safety

A dynamic service has identity `(host EndpointId, local service ID, incarnation)`.
The incarnation is a host-generated 32-byte random value encoded as 64 lowercase
hex characters. It is public identity metadata, not a credential or permission.
Creating a service saves it atomically with its backend and grants; editing the
existing row preserves it, except an HTTP/TCP type change rotates it. Removing that row cascades its grants. Registering the
same local ID again generates a new incarnation even if its backend address and
explicitly supplied grants are identical. Another host's same local ID remains
independent. Incarnations persist in the host's SQLite authority across restart.

The desktop Edit service dialog can rename the service ID while retaining the
backend port field and selected device grants. Its owner-only `edit_service`
operation takes the current `id`, `new_id`, `backend`, and `expected_revision`;
optional `allow` replaces the grant list, and omitting it preserves existing
grants. The rename, grants and backend/protocol edits commit in one transaction
and advance the revision once. The source must exist and another service cannot
already use the destination ID. A rename preserves the incarnation unless the
protocol also changes. Pairings and host identity remain unchanged.

Other devices see the new name on catalog refresh and keep their access. URLs
contain the service ID, so saved links and browser routes using the old ID need
updating; there is no old-name alias. Existing public publications are also bound
to the old ID and must be explicitly published again under the new name.

## Listings, sessions and enforcement

Filtered catalog records now contain `id` and `incarnation`. Desktop and Android
Open actions send the incarnation from the selected listing. The owner-local node
API and Android native client require that value and check the current permitted
catalog (or current owner-local registry) before creating a browser route. A stale
listing cannot silently resolve to a newer incarnation. Refresh the directory and
explicitly Open the newly listed service instead.

The browser route retains that same incarnation for its entire lifetime. Remote
service requests use the bounded version-2 open frame: two-byte version, 32-byte
incarnation, then the service ID, inside the existing length-delimited frame.
The host checks the ID, incarnation and current grant before backend connection.
Existing remote streams and owner-local responses also compare their captured
incarnation when checking current policy. Removal/recreation between two checks
therefore cannot be missed just because the same backend address returned.

Active response cancellation retains the existing one-second policy-check cadence
and one-second authority-read timeout. This is bounded runtime enforcement, not
atomic cancellation of every in-flight byte at mutation commit. New requests
using an old incarnation are denied; an already active stream is closed by that
policy loop. Local tests require cancellation within three seconds. Browser URLs
and cookies never select the current incarnation by service name alone. Neither
application HTTP requests nor denied Open actions are automatically replayed.

## Upgrade and compatibility

SQLite schema 7 adds durable incarnations to existing services in one migration
transaction. If existing services gain this metadata, the host revision advances
once so an older same-revision catalog does not appear inconsistent. Host identity,
authority generation, owner credential, clients, backend bindings, grants, pins
and combined-operation receipts are preserved. Invalid incarnation state fails
closed. Schema-5/6 binaries cannot open schema 7; do not delete authority to
downgrade. Protected recovery and stale-backup reconciliation remain #19.

Upgrade clients and hosts together. Older catalogs without incarnations are not
valid desktop snapshots; they must be refetched. Android legacy rows cannot Open
without the metadata. A current client rejects a host catalog missing the field.
An old client may read the additional public catalog field, but its unbound service
request is rejected by a current dynamic host. The original version-1 service
frame remains only for schema-1 immutable/static tunnel fixtures. There is no
unbound fallback granting access to a dynamic service. Current browser clients
use bound requests exclusively. The diagnostic `tunnel` CLI resolves a fresh
authenticated catalog for each explicit manual open; its static-fixture fallback
cannot open a dynamic host if catalog resolution fails.

Headless owner automation uses the same selected catalog record:

```json
{"op":"open","host":"FULL_HOST_ID","service":"website","incarnation":"64_HEX_CHARACTERS_FROM_SELECTED_LISTING"}
```

## Verification

The enabled pre-push suite covers protocol bounds, schema-6 migration,
edit/restart persistence, same-ID/same-backend recreation, rejection of unbound
and old-incarnation native streams before backend acceptance, active stream
cancellation, and owner-local browser response/session rejection after rapid
recreation. Qt tests check that Open passes listing incarnation metadata.
Android app and instrumentation APKs are built with the updated contract; a
successful build is not a fresh Play-phone runtime acceptance result.

Physical Linux checkpoint on 2026-10-17: installed the updated desktop/node on
CachyOS/KDE and the node-only bundle on Ubuntu desktop.home. The remote migration
preserved its host identity/generation, clients, backends, grants and pins. In
each direction, a temporary service used the target's existing website backend
and the already approved reciprocal client. Its original browser session returned
HTTP 200. After removal and immediate recreation at the identical address with
identical explicit grants, the old session returned 403, a stale listing Open was
rejected, and an Open missing an incarnation was rejected. After target-node
restart, the new incarnation was unchanged, old access still failed, and an
explicit fresh Open returned HTTP 200.

Reproduce this opt-in physical check on those already configured hosts with:

```sh
python3 scripts/test_physical_service_recreation.py --ssh-host SECOND_COMPUTER --local-node /path/to/wispnet-node --remote-node /remote/path/to/wispnet-node
```

The script restarts both nodes, temporarily enables browser access and creates
only uniquely named service fixtures. It removes them and restores original
access settings, checking existing identities/generations, approvals, service
incarnations/backends/grants and pins afterward. The relay PID remained unchanged;
desktop.home retained no GUI/tray unit. HTTP checks run headlessly against the
real protected browser-session protocol; this checkpoint is not a new visual
Chromium/Firefox interaction test. Full-machine power-off, relay/disk replacement,
stale-backup recovery remain separate. Versioned complete catalog/types and
native high-water checks are now documented in [catalog snapshots](catalog-snapshots.md).

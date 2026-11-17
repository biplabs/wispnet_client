# Complete versioned peer catalogs

Each host returns one complete snapshot of the services currently permitted to
the authenticated requesting client. Header and records come from the same local
SQLite transaction. No global revision, leader, permission lease, clock-based
catalog expiry or application database at the relay is involved.

## Contract version 1

```json
{
  "version": 1,
  "complete": true,
  "service_count": 1,
  "host": "64_HEX_HOST_ENDPOINT_ID",
  "generation": "64_HEX_HOST_AUTHORITY_GENERATION",
  "revision": 42,
  "services": [
    {
      "id": "website",
      "incarnation": "64_HEX_SERVICE_INCARNATION",
      "display_name": "website",
      "kind": "http"
    }
  ]
}
```

The illustrative placeholder strings above must be real 64-character lowercase
hex values on the wire. Only version 1 and `complete: true` are supported. The
count describes this client's filtered records, not the host's private total.
Services must be strictly sorted by local ID, with no duplicates and at most 128
records. Revisions are nonnegative integers no greater than SQLite's signed
64-bit maximum. A display name must contain non-whitespace text, fit in 128 UTF-8
bytes and contain no control characters. Supported types are `http` and `tcp`.
Unknown versions, fields/types, foreign owners, missing fields, inconsistent
counts, invalid metadata and noncanonical records are rejected. JSON duplicates,
truncated frames and trailing stream bytes are also rejected by the native parser.
The existing 128 KiB response limit and control deadlines still apply.

There is no pagination or delta merge in this version. A partial/failed response
never changes the accepted snapshot, even if it contains plausible service IDs.
After a newer complete snapshot is durably accepted, its list replaces the old
list atomically. Missing IDs then disappear from that client's directory; this
can mean removal or grant withdrawal, not necessarily deletion from the owner's
private registry. A complete empty list removes all that peer's filtered rows.
Other peers' snapshots remain independent. A dishonest owner could intentionally
omit its own services while claiming completeness; the protocol cannot prove an
owner's undisclosed private inventory. No catalog can mutate another host's state.

## Durable client version baseline

The shared native client used by Linux desktop, headless `wispnet-node` and Android
keeps a private `catalog-baselines/<host-id>.json` under its existing state
directory. This is version evidence, not an ACL, owner credential or backup of
host authority. It is separate from rebuildable desktop/Android display copies.
Only an authenticated, pinned, structurally valid complete response can reach
this step. Successful baseline persistence happens **before** the snapshot is
returned to the UI or headless caller.

- Same generation, higher revision: accept the whole new snapshot.
- Same generation and revision, identical records/header: accept idempotently.
- Lower revision, different same-version content, or changed authority generation:
  reject without replacing the baseline or last display copy.
- Failed authentication, partial/malformed data, disk failure or corrupt/unsafe
  baseline storage: fail closed for that peer; no implicit fresh baseline.

Mode-0700 directories and mode-0600, current-user-owned regular files protect the
baseline. Per-host OS locks, temporary-file write/fsync, atomic rename and
directory fsync serialize acceptance across processes and restarts. Symlinks,
hard links and publicly readable baseline/lock files are rejected. The pairing
epoch is captured before a request, so a late pre-pairing reply cannot seed a
baseline after an explicit re-pair. A busy same-host writer returns a scoped retry
error rather than blocking unrelated hosts.

Deleting only `desktop-catalogs/` or the Android UI preference copies does not
delete this baseline, pins, local grants or host identity. The native client still
rejects rollback after a display-cache deletion or node/app restart. Display
cache corruption is refetched; native baseline corruption is not treated as a
cache miss. Same-UID filesystem compromise and deliberate deletion of version
evidence are outside this protected-file boundary, as with device identity files.

Generation changes require an explicit verified target re-pairing. Only a
successful authenticated pairing acknowledgement resets that host's baseline;
it does not approve the client or grant services automatically. Recovery cannot
infer later revocations from an old backup. Host-bound protected restore and
stale-grant quarantine/reconciliation remain #19; these client checks do not
implement or certify authoritative recovery.

No host-supplied wall-clock timestamp participates in version acceptance. UI fetch
times are display metadata. Backwards/forwards clocks cannot lower a baseline or
turn cached data into a permission grant; invitation expiry and browser/session
deadlines retain their separate existing contracts.

## Service metadata and upgrade

SQLite schema 11 removes the stored `display_name` column. Existing service
IDs, incarnations, pairings and grants remain unchanged. Catalog version 1 keeps
`display_name` as a compatibility field derived from `id`; clients display `id`.
The migration advances the revision when the compatibility field changes, so
existing durable baselines accept the new snapshot. Legacy owner mutations can
still send a bounded `display_name`, which is ignored. Optional `kind` preserves
an existing type when omitted; changing it rotates the incarnation.

TCP services are listed but cannot Open in
the HTTP browser gateway; explicitly use the native tunnel CLI for them. No
backend address, grants or admin information are published in peer catalogs.

Hosts and clients must be updated together. Old catalog shapes are rejected and
old display copies must be refetched. The first valid version-1 snapshot seeds a
new native baseline on upgrade; earlier unversioned cache contents are not
silently promoted to trusted version evidence. Diagnostic `host-catalog` and raw
`peer-probe` validate the response contract but do not maintain a client directory
or this durable baseline; installed node/app browsing uses the shared client.

## Verification — 2026-10-17

`cargo test -p wispnet-client --test catalog_snapshots --locked` uses real pinned
Iroh endpoints in temporary private state. It exercises incomplete/missing flags,
count mismatches, unknown fields/versions, stale/negative/oversized revisions,
changed generation, foreign owner, conflicting same-version content, duplicate
or unsorted records, malformed metadata, duplicate JSON keys, truncation and
trailing data. Rejections leave baseline bytes unchanged while a separate healthy
host remains usable. Further checks cover corrupt-baseline refusal, complete
empty-snapshot removal, node/client restart, display-cache deletion, past/future
file timestamps, and a delayed response after pairing-epoch reset. Qt tests also
simulate past/future UI wall-clock values. This does not change either computer's
real system clock.

The same native test executable passed on headless Ubuntu desktop.home using only
temporary identities, loopback Iroh fixtures and private files. It does not borrow
the live node's key or owner credentials, install Qt or mutate live grants.
For installed physical nodes, the opt-in check is:

```sh
python3 scripts/test_physical_catalog_snapshots.py --ssh-host SECOND_COMPUTER --local-node /path/to/wispnet-node --remote-node /remote/path/to/wispnet-node
```

Both local → headless Ubuntu and reverse passed: complete sorted/count-matched
snapshots, HTTP/TCP metadata, TCP browser denial, independently filtered grant
withdrawal, complete removal, and unchanged native baseline across source-node
restart. Fixtures were removed and existing authority/services/grants/pins and
original access settings were preserved. The relay PID stayed unchanged and
desktop.home retained no GUI unit. Malformed responses were injected only by
isolated native fixtures, never by corrupting the live host's authoritative data.
Full machine power-off, deployed relay/disk replacement and protected host recovery
remain separate acceptance work in #14/#18/#19.

# Folder services

Inboxes and shared folders are special local services. Each has a name, one
folder, and its own list of permitted paired devices. Pairing, Shell access,
and access to another folder never grant access to this folder.

| Mode | Local badge | What permitted devices can do |
| --- | --- | --- |
| Receive files | Inbox | Upload files into this folder |
| Share files | Shared folder | Browse and download files or subfolders |

On desktop or Android, open **Local services → Add folder**, choose **Receive
files** or **Share files**, name the service, select a folder, and choose its
allowed devices. Repeat to add more folders. Each folder has independent
settings, enable/disable controls, and removal. Removing a service keeps its
files. Editing permissions applies only to that service; changing its folder
invalidates previous transfer bindings. The mode of an existing service is fixed.

Folder services use binary frames over authenticated native Iroh/QUIC tunnels;
file bytes never pass through MCP JSON, Shell output, or the HTTP gateway.
Filesystem paths stay on the hosting device. Peers see service names and
relative file names. Roots must be existing absolute directories without
symlink components, outside Wispnet's private state. Android uses the system
folder picker and persistent document-provider permissions instead of raw paths.

## Configure an inbox

Create a receiving folder, then grant only the devices allowed to upload into it:

```sh
mkdir -p /srv/training
wispnet inbox training-inputs --path /srv/training --allow FULL_CLIENT_PUBLIC_ID
wispnet inbox phone-downloads --path /srv/phone-downloads --allow OTHER_PUBLIC_ID
```

`wispnet inbox` lists local inboxes; `wispnet inbox training-inputs` inspects one.
`--allow` replaces only that inbox's device list, and `--allow none` revokes its
senders. Omitted path and permission options preserve existing settings. Use
`wispnet inbox training-inputs --remove` to remove the service without deleting
its files. Management commands accept `--config /absolute/path/host.toml` for
nondefault nodes.

Inbox access is ongoing permission to upload without confirming every transfer.
It permits no reads or downloads. Transfers cannot overwrite existing files or
escape the selected inbox. Revoking access stops active streams and prevents
new transfers without affecting other inboxes or shared folders.

## Existing inbox configurations

Upgrading converts each previously configured receiving export into a named
Inbox, preserving its folder, allowed devices, and enabled state. Names use the
old export alias when available, or an available `inbox-` name if it conflicts
with another service. The old global exports and grants are cleared, preventing
a legacy grant from bypassing the new inbox permissions. Existing complete
files are retained. Incomplete transfers using the old service binding must be
started against the migrated inbox.

The reserved legacy `files` service remains for old owner APIs and
`files-access` commands, but its empty placeholder is hidden from service cards
and remote catalogs. Explicitly configuring it through the legacy API still
uses the original global export/grant model. New folder services use independent
per-folder permissions. The owner-only `wispnet_set_files_access` MCP tool
continues to manage that legacy configuration; see [MCP configuration](service-registry.md).

## Transfer a file

The node must be running. Device names must resolve unambiguously; a full public
host ID always works. The sender chooses a local file and its destination path
inside one of the receiving host's configured folders:

```sh
wispnet files put GPU_HOST training-inputs ./dataset.npz jobs/run-1/dataset.npz
```

Transfers resume by default. Repeat the same command after a disconnect, client
restart, or node restart. Ctrl+C pauses a transfer and retains its resumable
bytes. `--no-resume` starts with fresh client resume state. Existing destinations
are never overwritten, including files with identical content. Retrying your own
completed transfer can recover its receipt while its client journal and the
receiving host's retained transfer state remain available (24 hours by default).

Progress is JSON on stderr, and completion or a typed transfer error is JSON on
stdout. The owner-local API starts bounded background jobs with paths and target
metadata only (`file_put`), then exposes `file_status` and
`file_cancel`. The CLI also supports `files status TRANSFER` and
`files cancel TRANSFER`. Background job handles are ephemeral; repeat put to
recover durable resume state after a node restart.

On desktop, drop one or more files into the Wispnet pop-up or onto a remote
device's named **Inbox** service, then select **Send files**. The receiving device
chooses the destination; the sender neither selects nor sees its receiving
folder path. The sender selects the named inbox service.
The transfer panel shows progress, cancellation and resume actions. Hiding the
pop-up or closing the panel keeps submitted transfers running. Files are queued
one at a time and retain their original names; folders must first be archived.
The recipient must already have granted access to the selected inbox.

Active uploads appear in a dock for each receiving computer, using the same
controls as the shell dock. Expand a dock to view that computer's transfers;
closing it asks before cancelling its uploads. The dock disappears when none
of that computer's uploads remain active. Transfer progress shows a percentage,
sent/total bytes, and current upload speed; preparation and verification have
their own status. Speed is measured from newly acknowledged bytes and falls to
zero when progress stalls.

On Android, tap a device's named **Inbox** service and use **Choose files** to select
one or more documents in Android's system picker.
Select **Send files** to queue the batch. Two files can run concurrently; each
has its own progress, cancel and resume controls. The network service retains
the queue when the panel closes or the UI is recreated. Going offline cancels
its active and queued transfers. After a network-process restart, select the
original documents again to recover their durable resume state.

To receive on Android, open **Settings → Services → Local services → Add folder**, choose
**Receive files**, pick a writable folder, and select the devices allowed to
send into this inbox. Repeat for additional inboxes. The gear edits only that
inbox's folder and permissions. Pairing never grants receiving permission.
Android restricts selecting storage and Downloads roots; select or create a
subfolder such as `Downloads/Wispnet` instead. Folder access uses a persisted
Storage Access Framework grant without broad storage permissions.

Receiving runs in the existing foreground network service, so it continues when
the UI is in the background. Go online before receiving; going offline refuses
incoming transfers and preserves resumable native data. Android process death
requires going online again, using the same identity and saved folder/grants.
If folder access is withdrawn, choose the folder again in settings.

Native uploads first verify their SHA-256 in private resumable storage. The
Android adapter then streams the verified bytes into the chosen provider; the
sender receives success only after the provider stream closes successfully and
the publication receipt is durable. A provider file may be visible while this
final copy is in progress. Receipts bind retries to the same document, and
existing destinations are never replaced. Cancelled/failed copies are removed
when provider access permits; abandoned provider copies and private receipts
are cleaned after 48 hours. Successfully received documents are retained.

Regular Android documents stream directly through their read-only descriptors.
Providers that expose a stream or a file slice are prepared in private temporary
storage with bounded buffers and cancellation, then sent through the same native
protocol. Temporary copies are removed when the transfer finishes or is cancelled.
File contents never cross Flutter or the UI's Binder connection.

App uploads carry the selected inbox service identity and use an empty workspace
selector. The receiver binds it only to that inbox's configured root; another
inbox cannot be selected through the workspace field. CLI and SDK callers may
also use the `incoming` workspace alias for named inboxes. Legacy Files callers
retain explicit export selectors for a deliberately configured legacy service.

The Rust SDK exposes `Client::put_file`, `FileTarget`,
`TransferOptions`, progress callbacks, and `FileCancellation` in
`wispnet_client::files`.

## Read-only shares

Read-only shares are separate services with a device allow list for each share.
Inbox permission still only permits uploads; it never permits listing or reading
files. A share grant permits browsing and downloading only that share, without
granting uploads, Shell access, or access to other shares. Pairing alone grants
neither capability. Existing inbox settings and grants are preserved.

For example, a rented training computer can expose its checkpoints to your
laptop. Your laptop grants the rented computer no inbox permission:

```sh
# On the training computer, after pairing with your laptop:
wispnet share checkpoints --path /workspace/checkpoints --allow FULL_LAPTOP_PUBLIC_ID

# On your laptop:
wispnet files ls GPU_HOST checkpoints
wispnet files ls GPU_HOST checkpoints run-1
wispnet files get GPU_HOST checkpoints run-1/model.safetensors ./model.safetensors
wispnet files get GPU_HOST checkpoints run-1 ./run-1 --recursive
```

Use `wispnet share` to list local shares and `wispnet share checkpoints` to
inspect one. `--allow` replaces that share's device list; `--allow none` revokes
all readers. Existing shares preserve omitted path and permission options.
Creating a share without readers exposes it to no remote devices. To remove
the share, use `wispnet share checkpoints --remove`; source files are retained.
These commands accept `--config /absolute/path/host.toml` for nondefault nodes.

Each share has a unique service name and points to an existing absolute folder.
Source paths stay private to the host; peers see share names and relative entries.
Sharing a folder exposes its current and future contents to its
allowed devices. Traversal, symlinks, special files, reserved partial names, and
roots overlapping Wispnet's private state are rejected. Other shares remain
private unless independently granted. Removing, disabling, changing a share's
root, or revoking its reader closes active access through current policy checks.

On desktop or Android, use **Local services → Add folder → Share files**.
The resulting **Shared folder** card has its own folder and reader permissions.
Inbox services continue to authorize uploads independently.

Open a remote shared-files service in the desktop or Android app to browse its
folders and choose a file or folder to download. Tapping a folder's name opens
it; its download button downloads the whole subtree. **Download this folder**
in the browser header downloads the current folder, including the whole share
at its root. Choose one local destination; the selected folder's name and its
nested and empty folders are preserved. Folder downloads appear as one task
with total bytes and completed-file counts.
Downloads show progress, cancellation, and resume controls and
continue when the browser closes. On Android, choose a writable destination
with the system folder picker; no inbox configuration or grant is needed.
Android downloads are verified in private storage before copying into the
selected document provider. The provider may show a file during this final
copy, and success is reported only after it closes. The verified staging copy
is removed after publication. Downloads and cancellation remain in the Android
network process if the UI is recreated; after a network-process restart,
select the same source and destination to recover durable transfer state.
While downloading, progress cards show a smoothed speed and an approximate time
remaining once enough samples and the total size are available. Speed counts
newly received original file bytes, excluding saved resume data and completed
files. Preparation, verification and Android publication have their own status;
they do not advertise a transfer speed. Stalled or unavailable updates clear the
estimate. Older local nodes without the fresh-byte counter cannot show reliable
folder speeds, but updated local clients measure downloads from older sources.
On Android, open **Settings → Services → Local services → Add folder → Share files**. Choose a
folder with Android's system picker, then select the paired devices allowed to
browse and download that share. Each share has its own device list; receiving
files through the inbox is configured independently. Wispnet persists only the
selected folder's read permission and streams requested files through Android's
document APIs, without copying the folder or requesting broad storage access.
Removing a share does not delete its files. If Android or the document provider
withdraws access, select the folder again. Shares are served by the foreground
network process while Wispnet is online, including when the UI is closed.

The CLI downloads one regular file per `files get`, or an entire folder with
`--recursive`. For a whole share, pass `""` as the remote path and choose a new
local folder name. The destination's parent must exist. It resumes by default;
repeat the command after interruption or use the app's **Resume download**.
`files ls` emits one JSON object per page, with up to 32 entries per page.
Resume binds the source host, share incarnation, relative path, file version,
checksum, and local destination. A changed source returns `STALE_VERSION`;
restart deliberately with `--no-resume` or the app's **Restart download** after confirming the checkpoint is
complete. Downloads verify the SHA-256 before publication and never overwrite
an existing destination. A matching completed destination can recover success
with resume enabled. Source changes invalidate partial data rather than mixing
versions. Publish training checkpoints only after they are complete, and keep
all shards of a checkpoint together; downloads do not create a transactional
snapshot across multiple files.

Folder jobs retain a manifest of the selected relative paths and file versions.
Resume keeps verified progress and rejects detected source changes instead of
silently combining checkpoint versions. Existing unrelated destination folders
are never merged or overwritten. Desktop builds the folder in a private staging
directory beside the destination and publishes it after verification. Android
copies one verified file at a time into its selected document provider, so its
owned destination folder and completed children can be visible before the task
finishes. No archive is created on the source device.

### Automatic download compression

New peers negotiate streaming Zstandard compression for file payloads, with
uncompressed transfer as the fallback. Compression uses a fast level, samples
data from different parts of a chunk, and avoids compression when its measured
cost or small size reduction makes it unsuitable. Poorly compressible data is
periodically sampled again, so one header does not decide an entire large file.
Desktop folder jobs reuse the authenticated session across listings and files.
Android publishes one verified child at a time through its document provider and
currently starts a native file job for each child.
Older source devices continue to serve ordinary uncompressed downloads.

Compression changes only bytes on the wire: original file offsets, source
versions, checksums and resume data remain authoritative. The receiving device
decodes bounded chunks and writes the original files. Files remain byte-for-byte
identical, and neither device needs a ZIP file or an extraction step. Actual
speed depends on the source data, connection, CPU and storage; compression is
not a promise of faster transfers on every link.

Owner-local `file_list` returns one bounded page of entries with an optional
`next_cursor`. `file_stat` starts a cancellable metadata job whose completed
status includes the shared file's version metadata. `file_get`
starts a download job; `recursive: true` selects a folder job, and
`expected_version` can pin an individual file to previously inspected metadata.
`file_status` and `file_cancel`
use the same bounded job lifecycle as uploads. Only paths, metadata, and job
handles cross the owner API; file bytes use the native binary tunnel. The Rust
SDK exposes `Client::list_shared_files`, `Client::get_file` and
`Client::get_folder` with the existing
`FileTarget`, `TransferOptions`, and cancellation/progress callbacks.

## Reliability and limits

Uploads use a persistent manifest bound to the authenticated client, service
incarnation, workspace, destination, size, SHA-256, and idempotency key. A chunk
acknowledgement reports a contiguous durable prefix. Duplicate acknowledged
chunks are verified against stored bytes. Upload commit verifies the complete
file and publishes it atomically without replacing an existing destination.
Retries recover lost acknowledgements and commit responses.

Pipelined upload chunks can share a durable flush in groups of up to eight.
Each chunk still receives a response; its acknowledged offset may include later
chunks in the same group. Single-request callers remain supported.

Only regular files are supported. Absolute remote paths, traversal, symlinks,
and reserved partial-file names are rejected. Disk/hash work runs outside the
async I/O executor. Limits bound chunk size, pending bytes, upload count, bulk
operations, client background jobs, and retained upload state. Expiry/abort only
remove owned partial data; completed artifacts remain.

Defaults allow files up to 1 TiB, 2 TiB of declared pending uploads, and 1,024
retained upload manifests, with a 24-hour retention period. Four disk-heavy
operations can run concurrently; the client allows two background transfers.
Idle maintenance checks expiry every minute. Each frame read/write has a
300-second deadline; the complete transfer has no fixed session lifetime.

## Throughput target

The initial target is **1 Gbit/s sustained file payload** on a direct path with
sufficient source upload, destination download, CPU, and disk capacity. Report
steady-state payload throughput and total time through verification/publication
separately. Relay and WAN results require their own measurements.

Frames carry up to 4 MiB of binary data. Helpers pipeline eight chunks, allowing
32 MiB of application data in flight while preserving ordered writes. Transport
windows are 64 MiB per stream, 128 MiB receive per connection, and 64 MiB send per
connection. These are bounded ceilings rather than eager allocations. At
100 ms RTT, 1 Gbit/s needs approximately 12.5 MB in flight; at 200 ms, 25 MB.

Linux hosts use a high-resolution monotonic timer for shared Iroh/QUIC protocol
deadlines, reducing late pacing wakeups during sustained transfers. This applies
to Shell, control and other native services as well as Files. Timer failures fall
back to the existing Tokio timer, which other platforms continue to use. Files
still runs inside the existing Wispnet daemon.

Run the optional 1 GiB native upload/restart/resume benchmark:

```sh
cargo test -p wispnet-agent --release --test files native_files_bulk -- --ignored --nocapture
cargo test -p wispnet-client --release --test files sdk_large_file -- --ignored --nocapture
```

This transfers real binary payload over authenticated Iroh/QUIC, checks a durable
offset after restarting the host, and verifies the final SHA-256. Loopback results
demonstrate local implementation capacity, not a guaranteed Internet rate.

The SDK benchmark uses two native peers, pauses an upload, restarts both nodes,
and resumes the upload through verification and commit.
Set `TMPDIR` to a directory on the desired filesystem to include that storage
device in the measurement; `/tmp` may be backed by memory.

An initial 1 GiB run on the development machine's normal filesystem measured
3.77 Gbit/s of resumed native upload payload, with verification/publication
complete in 3.00 seconds. The same run checked status responsiveness during the
transfer. These are local loopback measurements; Internet and relay paths still
need separate validation.

The full SDK run on that filesystem measured 3.27 Gbit/s for upload including
source hashing and commit.

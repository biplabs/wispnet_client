# Shell access

Every desktop host includes a built-in service with the reserved ID `shell`.
It runs the host account's default shell in a PTY over Wispnet's existing Iroh
connection. SSH and a separate shell daemon are not required. Android accesses
desktop shells; it does not offer a host shell from the application sandbox.

The optional [local SSH adapter](ssh-adapter.md) lets OpenSSH-based tools use
the same native Shell service through an SSH configuration alias.

The built-in service is always present and available to the local owner. New
hosts grant no remote shell access. Pairing alone does not grant access: select
Shell when approving a device or edit Shell's permitted devices afterwards.
The host checks the current grant and exact service incarnation before starting
a session, and revocation ends access and terminates that peer's sessions.
`shell` cannot be used for an HTTP/TCP service, renamed, or removed.

## Desktop and terminal

Select Shell under a device in the desktop app to open that host's built-in
terminal sheet. Each host has its own isolated terminal workspace with multiple
tabs, side-by-side and top-and-bottom split panes, an input mode selector
(Auto, Predictive, Sync, Line), a button to hand off the active pane to the
system terminal, a popup `−` button that minimizes the sheet into a docked bar
at the bottom of the window while keeping sessions connected, and a separate
popup `×` button that closes all sessions for that host after confirmation.
The `×` on a tab or split pane terminates that session; the docked bar’s `×`
closes all sessions for that host after confirmation.
In the TUI, selecting Shell temporarily hands the terminal to the remote shell;
detaching or exiting returns to Wispnet.

```sh
wispnet shell desktop.home
wispnet shell --host FULL_PUBLIC_ID --session main
wispnet shell desktop.home --config /absolute/path/host.toml
```

Device names must resolve unambiguously among known devices. Public IDs pin the
target independently of its readable name. The CLI uses the running node's
private API and existing identity. Colours, interactive terminal applications,
input bytes, resize events and process exit status pass through the session.
The local terminal must support the remote application's terminal features.

`Ctrl+]` detaches without ending the shell. `exit` ends the remote shell, and its
status becomes the CLI exit status. `--session NAME` selects a separate named
session. Sessions are separate for each requesting device. They survive client
disconnects and network changes, but not a
host-node restart. Reconnecting replays up to 64 KiB of recent terminal output;
this is byte history, not a serialized terminal-screen snapshot.

To inspect or change grants from the CLI:

```sh
wispnet shell-access
wispnet shell-access --allow FULL_PAIRED_PUBLIC_ID,ANOTHER_PAIRED_PUBLIC_ID
wispnet shell-access --allow none
```

Grant changes replace the allow list using the reviewed host revision. Only
already approved devices can be selected. Shell access runs with the host
account's permissions.

On Linux, the host service allows `sudo` according to the host account's normal
sudo policy. It must run with `NoNewPrivileges=false`; setting it to `true`
blocks privilege changes in every shell it starts. After changing this service
setting, run `systemctl --user daemon-reload` followed by
`systemctl --user restart wispnet-host.service`, then open a new shell session.
Existing sessions end when the host service restarts.

MCP clients can add a device without replacing other grants by calling
`wispnet_grant_service_access` with `service: "shell"`, the paired device's
`peer` ID, and the current `expected_revision` from `wispnet_status`. The same
tool works for HTTP and TCP services. It does not create services or pair devices.
Use `wispnet_revoke_service_access` with the same fields to remove just that
device's shell grant while keeping it paired and preserving its other access.

## MCP commands

`wispnet_run_shell_command` runs a command on a paired desktop host using the
same Shell grant as an interactive terminal. Supply `host` (a full public ID or
an unambiguous paired device ID, such as `j-desktop`) and `command`, for example
`pwd; git status --short`. Optional `cwd` must be an absolute remote directory;
the default is the remote account's HOME.

Each call starts a separate `/bin/sh -c` process with stdin closed and no PTY.
Commands do not reuse interactive sessions or retain shell variables between
calls. The result contains separate stdout/stderr strings, the resolved host
ID, exit status, and timeout/truncation flags; nonzero exits remain result data.
Output uses lossy UTF-8 decoding. `timeout_seconds` defaults to 30 (1–300), and
`max_output_bytes` limits combined output to 65536 bytes by default (1–1048576).
A timeout, disconnected MCP request, Shell revocation, or local unpairing ends
the command and its children. Commands run with the remote host account's
permissions, and revoking Shell access prevents further calls.
MCP permits four concurrent command requests, reserving capacity for status and
access controls. The native host's shared limit remains 16 shell operations.

## Android

Tap Shell under a paired desktop host to slide up that host's isolated terminal
sheet. Tap the sheet's `−` button (or swipe down) to minimize it into a docked
bar for that host at the bottom of the screen while keeping its sessions
connected (when multiple hosts have minimized terminals, each host gets its own
dock bar), tap a docked bar to expand that host's terminal again, or tap `×` on
a tab or docked bar (with confirmation) to terminate the corresponding remote
shell session(s). The native terminal retains WispShell's
Termux renderer, per-host tabs, Android keyboard composition handling, shortcut
keys, keyboard resize smoothing, and resume/network reconnect handling.

Input modes are Sync, Predictive, Line, and Auto. Predictive typing displays
supported input locally and reconciles remote echoes; Line offers a local
command composer. Auto selects Sync up to 80 ms, Predictive through 220 ms, and
Line on slower paths. Local input modes fall back to Sync when the host reports
no-echo or noncanonical input, including password prompts and raw terminal apps.

## Implementation and attribution

`crates/shell` contains the WispShell-derived, transport-independent protocol and
host session engine. `crates/agent` applies service authorization to Iroh streams;
`crates/client` provides bounded terminal sessions to the CLI and Android Binder
bridge. The Android terminal is native; Flutter owns device navigation.

Imported WispShell code retains its AGPL-3.0-only notices. The Android Termux
terminal dependency retains its GPL notices. See `crates/shell/NOTICE.md` and the
Android terminal's bundled third-party notices for the included license texts.

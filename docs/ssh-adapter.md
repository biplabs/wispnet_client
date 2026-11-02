# SSH compatibility adapter

The optional SSH adapter lets an OpenSSH client open Wispnet's built-in Shell
service. SSH runs between the client and a local `wispnet ssh-bridge` process;
the remote connection uses Wispnet's existing Iroh identity and Shell grant.
The adapter opens no TCP listener, and the remote host needs no SSH daemon.

## Connect

Pair the devices and grant the requesting device Shell access on the target.
Both nodes must support streaming Shell execution for remote SSH commands.
Print an OpenSSH configuration entry:

```sh
wispnet ssh-bridge --host j-desktop --print-config
```

Review the entry and add it to `~/.ssh/config`, then connect using its `Host`
alias, for example:

```sh
ssh shell-j-desktop.localhost
ssh shell-j-desktop.localhost 'git status --short'
printf 'example\n' | ssh shell-j-desktop.localhost cat
```

`--config /absolute/path/host.toml` selects a different local node. The printed
entry pins the resolved device's public ID, so renaming a device does not route
an existing alias to another machine. `.localhost` is an SSH configuration alias;
OpenSSH invokes `ProxyCommand` instead of resolving a remote address. Printing
the entry does not edit SSH configuration or change device permissions.

The bridge speaks SSH on stdin/stdout, so its diagnostic output goes to stderr.
Run it through the printed `ProxyCommand`, rather than in an interactive
terminal. The local node remains responsible for service lookup, current
permissions, service incarnation checks and remote transport.

## Supported behavior

Interactive shells support PTY input, output, terminal resizing and exit status.
Each SSH channel uses a new shell session; closing the channel terminates that
session rather than leaving a resumable Wispnet terminal behind.

Remote commands use native streaming execution with binary stdin/stdout,
separate stderr, stdin EOF and exit status. They are separate processes and do
not share interactive shell state. Streaming commands use bounded in-flight
buffers and continue until exit, disconnect or revocation, without the bounded
MCP command API's five-minute and 1 MiB limits. The existing native tunnel policy
still limits each remote Shell stream to one hour; this prototype preserves
that policy, so remote commands and interactive sessions end at that limit.
Commands run with the target Wispnet account's permissions. The SSH username
does not select another target OS account.

Remote command strings are limited to 8192 bytes and run through `/bin/sh -c`.
PTYs currently use `TERM=xterm-256color`; SSH environment requests are rejected.
The native process engine bounds stalled input/output writes to ten seconds.

This adapter is an initial compatibility implementation. SFTP subsystems, TCP
port forwarding, SSH agent forwarding and X11 forwarding are not supported.
Remote-project clients that require those capabilities need additional work;
a successful SSH terminal connection alone does not establish application
compatibility.

## Local authentication and host keys

The SSH endpoint is a stdio child process running as the local account. It
accepts SSH's `none` authentication on that inherited local connection; remote
access still requires the existing Wispnet Shell grant. The bridge verifies the
local node socket's owner and permissions. This stdio mode must not be wrapped
in a publicly accessible socket server.

Separate persistent SSH host keys are stored under the local node's
`state_dir/ssh-adapter` directory. They identify the local compatibility
endpoint for a pinned peer and do not replace Iroh's verification of the remote
peer. Host-key material is separate from the Wispnet identity. OpenSSH's normal
known-host checking remains enabled.

## Module boundary and removal

All SSH protocol and configuration code lives in `crates/ssh-adapter`. The CLI
integrates it through the optional `ssh-adapter` Cargo feature. Build without
that feature to omit the SSH dependency and command:

```sh
cargo build -p wispnet-cli --no-default-features --locked
```

The native Shell streaming mode and owner-only local stream API are reusable
Shell capabilities. They contain no SSH protocol dependency and preserve the
existing interactive and bounded MCP paths. To remove the adapter entirely,
remove the crate and its workspace entry, the CLI feature/dependency/dispatch,
and this documentation and its smoke test (including the `scripts/test.sh`
invocation); ordinary Shell access remains available. Remove any generated
SSH configuration entries separately. Deleting adapter host keys makes
OpenSSH report a changed key if the adapter is later re-enabled.

## Validation

`python3 scripts/ssh_smoke.py` uses the system OpenSSH client against two
temporary Wispnet nodes with separate identities and explicit Shell grants.
It checks grant denial, binary streaming and EOF, separate stderr, exit codes,
output beyond 1 MiB, stable host keys, multiplexed SSH channels, PTY resizing,
disconnect cleanup and revocation. It does not touch installed nodes or the
user's SSH configuration.

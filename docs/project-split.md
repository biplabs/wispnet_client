# Client and server projects

The client repository and local folder are named `wispnet_client`.
`wispnet_server` is its sibling folder and separate repository. Both projects
work on main; no client package has a path dependency on the server folder.

| Client (`wispnet_client`) | Infrastructure (`wispnet_server`) |
| --- | --- |
| Flutter desktop and Android | Standalone packet relay |
| Node daemon, CLI and TUI | Public HTTP ingress/gateway |
| Local authority, pairing, catalogs and grants | Public homepage |
| Local browser gateway, Shell, Files, SSH adapter | Caddy/systemd server deployment and installers |
| Publication settings and outbound publisher | Relay config, UDP-discovery installer, Sites relay experiment |

A client still hosts local services and owns its own permissions. The
`crates/registry` library is host-local authority and remains necessary to client
operation; moving it wholesale to infrastructure would break independent peers.
The public gateway needs the existing endpoint identity/transport implementation.
The server project therefore carries a pinned source snapshot of those shared
runtime crates and the vendored transport. Its SOURCE.md records provenance.
Neither workspace references the other's source or Cargo workspace. Coordinate
protocol/security fixes until a separately versioned shared library exists.

Client gateway bootstrap information now lives in `config/public/gateway.json`.
The deployment copy remains in `wispnet_server/deploy/public/gateway.json`. Update
both deliberately if the public gateway pin changes. Installed names, package
IDs, node state, pairing data and service configuration are unchanged.

Run `./scripts/test.sh` independently in each project. Client publication tests
use a frozen test peer in `crates/client/src/test_support/public_ingress.rs`,
compiled only for tests; production ingress lives in the server project. The
server also provides an opt-in test of real binaries from both projects:

```sh
# Build the client in this folder first.
cargo build -p wispnet-cli --locked
cd ../wispnet_server
cargo build --workspace --locked
python3 scripts/test_client_interop.py --client-root ../wispnet_client
```

The cross-project test uses only private temporary identities and loopback ports.
It does not use installed services, SSH, ADB, public DNS or deployed hosting.

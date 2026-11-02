# Temporary desktop relay, independent of the node role

`desktop.home` temporarily hosts the standalone `wispnet-relay` binary. It is a
separate process/systemd unit, not the desktop node's registry or transport
endpoint. The pinned Iroh 1.4.1 server authenticates relay clients and forwards
end-to-end encrypted packets. Relay admission never approves an application
client, grants service access, or owns a node identity.

## Installed layout

| Item | Desktop path |
| --- | --- |
| Relay executable | `~/.local/libexec/wispnet-relay/wispnet-relay` |
| Relay-only configuration | `~/.config/wispnet-relay/relay.toml` |
| Public CA certificate (client trust) | `~/.local/share/wispnet-relay/relay-ca.pem` |
| Server TLS certificate | `~/.local/share/wispnet-relay/relay-cert.pem` |
| CA private key (desktop only) | `~/.local/share/wispnet-relay/relay-ca-key.pem` |
| Private-CA server TLS key (retained fallback) | `~/.local/share/wispnet-relay/relay-key.pem` |
| Automatic public TLS account/certificate cache | `~/.local/share/wispnet-relay/acme/` |
| User service | `~/.config/systemd/user/wispnet-relay.service` |

The protected relay directories contain no `host.sqlite3`, owner token, node
secret key, service configuration or pairing capabilities. Relay TLS/account keys
are generated on desktop and never copied to clients or committed. The current
public endpoint uses automatic Let's Encrypt certificates and normal WebPKI
verification. Clients no longer need `relay_ca_file` for this public endpoint.
The original private CA/server files remain protected for an explicit manual-mode
fallback; selecting that fallback would require clients to configure its trust
root again. There is no silent TLS fallback or verification bypass.

Install from this checkout with a trusted SSH alias:

```sh
python3 scripts/install_desktop_relay.py --acme --ssh-host desktop.home \
  --hostname relay.wispnet.app --lan-ip 192.168.1.26
```

The installer builds the locked source locally, copies a stripped Linux binary,
and enables only `wispnet-relay.service` in the SSH user's systemd account. Desktop
has user lingering enabled, so this service can run independently of login. No
sudo is needed. Existing Caddy/WispShell services and node state are untouched.
The source unit is `deploy/relay/wispnet-relay.service`; systemd applies a 256 MiB
memory limit, restart-on-failure and filesystem hardening. Only the protected
ACME cache is writable inside the home directory; the relay and node configuration
remain read-only to the service sandbox. Relay admission is
limited to 64 authenticated connections, with a default 1 MiB/s incoming rate per
connection and a 2 MiB burst. This is not a claim of pre-authentication DoS protection.
Third-party payload/identity trace logging is not enabled; lifecycle logs contain
fixed role/event labels and the listening address.

The relay configuration can raise this allowance for relayed file transfers:

```toml
[client_rx]
enabled = true
bytes_per_second = 33554432 # 32 MiB/s per relay client connection
burst_bytes = 67108864     # 64 MiB burst
```

Omitting the table or any field preserves its default. Rates and bursts are
integer byte counts from 1 through 4294967295; zero, negative values and unknown
options are rejected. Set `enabled = false` explicitly to remove the per-connection
ingress cap. The connection-count limit remains in force. These settings limit
traffic the relay reads from each client connection, including protocol overhead;
they are neither a total server bandwidth budget nor a guaranteed file-transfer
speed. Direct peer connections do not pass through this limiter. Restart only
`wispnet-relay.service` after changing its protected configuration; this interrupts
current relay sessions. Re-running the historical desktop-relay installer rewrites
its configuration, so retain intentional overrides when using it.

## Network and client configuration

The actual DNS name is **relay.wispnet.app**.
At installation it resolved to `162.156.47.159`. Reserve desktop's LAN address
`192.168.1.26` and forward **external TCP 443 → 192.168.1.26 TCP 8443**.
The relay listens on `0.0.0.0:8443`; auxiliary HTTP is loopback-only, and relay
QUIC/metrics listeners are disabled. No UDP or port 80 forwarding is required for
this TLS WebSocket relay. Existing desktop listeners on 80/443 are preserved.

For the current public-certificate deployment, configure each node separately:

```toml
[transport]
bind_addr = "0.0.0.0:7779"
relay_url = "https://relay.wispnet.app"
relay_only = false
```

Remove an old `relay_ca_file` override when selecting public WebPKI trust; that
override deliberately replaces the normal trust roots. A manual/private-CA relay
still requires the explicit public CA certificate at a protected client path.

Use `relay_only=true` to verify the relay path with no peer UDP transport. There
is no automatic node config rewrite or global authority configuration. Relocating
the relay changes only transport configuration/DNS and TLS trust as necessary;
it does not migrate identities, catalogs, owner approvals or grants.

## Verification and limits

The local pre-push suite builds/tests the relay and checks verified TLS, wrong
hostname rejection, unsafe TLS-key rejection, graceful shutdown and absence of
node/application state. To run the physical application test against this
already-installed relay, build the current Linux and Android CLI first, then run:

```sh
python3 scripts/physical_relay_smoke.py --ssh-host desktop.home \
  --serial AUTHORIZED_ADB_SERIAL --relay-url https://relay.wispnet.app \
  --restart-desktop-relay
```

The existing-relay mode creates no local relay, SSH reverse forward or USB
reverse. Each computer generates independent temporary host/client identities.
The tablet proves its invitation to each host, remains denied until target-local
owner approval, gets a separate service grant, and exercises catalog plus HTTP
bytes. Each host restarts with its own state; the relay restarts independently,
and read-only reconnects must recover within 30 seconds without replacing pins,
permissions or node processes. Fresh Linux clients exercise both cross-computer
catalog/HTTP directions, and tablet revocation remains host-local. Stopping a
node leaves the relay service active. Temporary **node test resources** are
removed; the intentionally installed relay service/TLS files remain.

Android native CLI hostname lookups use the OS `getaddrinfo` resolver without a
JVM, public fallback or TXT peer discovery. The OS owns caching and network DNS
policy; the asynchronous lookup has a three-second deadline. A timed-out OS
lookup may finish later on its blocking worker. This does not implement APK/JNI
lifecycle, Keystore or cellular acceptance.

Home-LAN tests through the public hostname exercise public-address/hairpin
forwarding. The subsequent [phone-hotspot test](external-relay-validation.md)
verified the tablet through an unrelated mobile network. Desktop's **node** can stop
while its relay keeps running; powering off desktop removes the temporary relay
and therefore breaks relay-dependent paths, the expected single-relay v1
limitation. Whole-machine-off independence requires relocating that relay first.

Inspect/restart/remove only the relay role:

```sh
ssh desktop.home systemctl --user status wispnet-relay.service
ssh desktop.home systemctl --user restart wispnet-relay.service
# When relocating/removing the temporary relay:
ssh desktop.home systemctl --user disable --now wispnet-relay.service
```

Do not stop the node to administer the relay, or stop the relay as part of node
shutdown. Retain TLS files until clients have switched to the replacement relay.

The complete existing-relay scenario passed on 2026-10-16 using the two physical
Linux computers and the arm64 tablet, with `https://relay.wispnet.app` and explicit
CA trust. Both target-local tablet pairing/approval/service cases, both Linux
client directions, host restart, independent relay restart/recovery and local
revocation isolation passed. No SSH/USB traffic forwarding was used. The server
certificate installed during this run expires 2026-11-15; the temporary CA must
also be checked before renewal. Existing WispShell services remained active.


The relay was subsequently switched explicitly to ACME/WebPKI on 2026-10-16 for
external-network testing. Let's Encrypt issued a certificate for
`relay.wispnet.app` (issuer YE1), initially valid until 2027-01-14. The pinned Iroh
ACME implementation renews and caches certificates using TLS-ALPN-01 through the
existing external TCP 443 → internal 8443 mapping; port 80 and DNS API credentials
are not needed. Keep that forwarding and DNS valid for renewal. Protected cached
account/certificate files were reused across independent relay restarts.

Manual mode is still available by omitting `--acme`. Relay config accepts either
`certificate` plus `private_key`, or `acme_hostname` plus `acme_cache`, never a
partial/mixed configuration. ACME starts asynchronously; service startup alone
is not certificate issuance proof. Confirm `curl --fail https://relay.wispnet.app/ping`
without custom trust after installing/updating public mode.

## Public HTTPS front end

The owner-controlled [public service gateway](public-access.md) adds a separate
application ingress beside this packet relay. In that deployment Caddy owns port
8443 and normal public TLS, while `wispnet-relay` uses its explicit loopback-only
HTTP mode on 18443. Clients retain `https://relay.wispnet.app`. The packet relay
still has no node/application identity or service registry; those belong to the
separate public gateway and individual owning nodes. The deployment installer
preserves a rollback snapshot of the earlier packet-only configuration.

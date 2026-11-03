# External-network relay validation (#13)

A successful home-LAN request to the router's public address only proves hairpin
forwarding. To test inbound Internet access without a VM, connect the Android
tablet to a phone hotspot **with the phone's Wi-Fi disabled**. Leave both Linux
computers and the independent desktop relay on the home network. A VM/VPN exit
could also provide an external vantage, but neither is required for this test.

## Verified run, 2026-10-16

The owner moved the tablet to a phone hotspot. Its Wi-Fi address changed from the
home `192.168.1.x` subnet to a `10.x` hotspot subnet. The fixed native HTTPS
preflight observed different public egress addresses for tablet and home computer
and verified `https://relay.wispnet.app/ping` using ordinary public certificate
roots from both networks. It prints only the comparison/result, not public IPs.
Different IPs are supporting evidence; the owner-confirmed cellular hotspot and
network change distinguish this test from different address families/proxies on
the same home network.

Against that public relay, the actual native Wispnet application scenario passed:

- Tablet invitation proof, pending/ungranted denial, target-local owner approval,
  service grants, filtered catalog and HTTP bytes against **each** home computer.
- Cold CLI starts and each host's independent restart preserved pins/permissions.
- Restarting only desktop's relay recovered within the 30-second read-only retry
  bound; the active node process did not change. No application write was replayed.
- Linux clients exercised both cross-computer catalog/HTTP directions.
- Tablet revocation remained local to the selected host; the other still served
  it. Stopping a node left the separately installed relay active.

The runner used `relay_only=true`, with no peer IP hint, default public relay,
USB reverse, SSH reverse, local relay or proxy. SSH/ADB only orchestrated/copied
binaries/configs/intended invitations; keys were generated independently on their
own devices. Temporary node/process/tablet resources were removed. The deployed
relay intentionally remains enabled/running. This proves native relay access from
the tested mobile network, not from every carrier/network or an Android APK.

## Reproduce

Build the Linux CLI, standalone relay and preflight example:

```sh
cargo build --workspace --locked
cargo build -p wispnet-cli --example external_network_probe --locked
```

With the existing Android NDK linker/CC/AR environment configured, build current
Android artifacts (see the native relay validation instructions for NDK setup):

```sh
cargo build -p wispnet-cli --bin wispnet --example external_network_probe \
  --target aarch64-linux-android --locked
```

Move the tablet to the cellular hotspot, then run:

```sh
python3 scripts/physical_relay_smoke.py --require-external-tablet \
  --ssh-host desktop.home --serial AUTHORIZED_ADB_SERIAL \
  --relay-url https://relay.wispnet.app --restart-desktop-relay
```

`--require-external-tablet` runs the HTTPS egress/relay preflight before **and after**
the full scenario and fails when either probe fails or the observed public egress
is the same. An explicitly requested external run is never silently downgraded to
a LAN run. Existing local/private-CA fixture modes remain available. The fixed
preflight contacts only Cloudflare's public HTTPS `/cdn-cgi/trace` endpoint and
the deployed relay `/ping`; it accepts no arbitrary URLs, sends no credentials,
verifies TLS, bounds response sizes/time, and cleans its temporary tablet binary.
The preflight example itself prints public IPs for direct operator diagnostics;
the Python runner captures them and emits only the comparison.

The full hardware/network scenario is opt-in. The local pre-push hook runs the
normal regression suite plus offline preflight-parser tests and standalone relay
TLS tests. It does not require a hotspot or contact a public CA to run tests.

## Public TLS and remaining work

The relay now uses pinned Iroh's built-in Let's Encrypt TLS-ALPN-01 support. The
existing external port 443 mapping suffices; no extra forwarding, DNS API token or
VM was introduced. Public issuance and normal TLS verification were checked, and
cached certificate/account files stay protected on desktop. See
[desktop deployment](../../wispnet_server/docs/desktop-relay.md) for renewal and manual fallback details.

A private Sites Worker was considered as an external wire client. Once the
physical hotspot became available, no new Site code/deployment was needed: the
native application test provides stronger evidence than an HTTP-only cloud
health check or a hand-built wire probe. Sites was not used as a traffic proxy.

Whole-desktop power-off still removes the current single relay, as expected for
this temporary co-location. Testing that scenario requires relocating the relay.
APK/Keystore/JNI lifecycle (#12), arbitrary Wi-Fi/cellular transitions in the app,
and deployed relay replacement under a changed IP remain distinct acceptance
work. This run does not overstate those remaining items or close #13 prematurely.

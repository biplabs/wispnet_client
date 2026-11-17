# Native relay lifecycle validation (#13)

`scripts/relay_smoke.py` runs the real Linux host/CLI and an ephemeral TLS Iroh
relay as separate processes. The default local hook includes its Linux mode.
With `--android`, it additionally runs the same native CLI on an explicitly
selected authorized aarch64 Android device over a temporary ADB TCP reverse.
The device creates its own private identity; no host key or owner token is copied.

## Run locally

```sh
cargo build --workspace --locked
cargo build -p wispnet-cli --example relay_fixture --locked
python3 scripts/relay_smoke.py
```

The example is a loopback-only disposable **test fixture**, not the deployed relay
from #18. It generates an ephemeral TLS certificate/key, writes only the public
certificate to a newly created private file, reports its HTTPS origin, and drains
on SIGTERM. SIGUSR1 replaces the relay server instance at the same address using
the in-memory certificate/key, retaining no relay client/application database.
Stopping the fixture discards that TLS key. Nothing is provisioned on the cloud.

For the authorized Android device, use an installed NDK:

```sh
export WISPNET_NDK="$HOME/Android/Sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CC_aarch64_linux_android="$WISPNET_NDK/aarch64-linux-android34-clang"
export AR_aarch64_linux_android="$WISPNET_NDK/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
cargo build -p wispnet-cli --target aarch64-linux-android --locked
python3 scripts/relay_smoke.py --android
# When multiple authorized devices exist:
python3 scripts/relay_smoke.py --android --serial DEVICE_SERIAL
```

Use `--ndk /absolute/path/to/ndk` when the strip tool is elsewhere. The default
path matches the NDK example above. The harness refuses an ambiguous/unauthorized
device and uses ADB `reverse --no-rebind` so it cannot replace an existing mapping.
Only a uniquely named `/data/local/tmp/wispnet-relay-*` directory and its own
mapping are removed afterward. Remote CLI processes also have a 12-second
on-device timeout. The default run changes no browser, Wi-Fi, cellular or display settings.
Wi-Fi interruption is separately opt-in with `--wifi-cycle`; cleanup restores
Wi-Fi and waits for its IPv4 connection.

Android mode builds the native CLI from current source with that NDK before
copying it to the selected device. Hardware acceptance runs separately from the
public pre-push suite:

```sh
python3 scripts/test_hardware.py --serial DEVICE_SERIAL
```

Use `--ndk /absolute/path/to/ndk` or `WISPNET_ANDROID_NDK` for a different NDK.
An explicitly selected hardware test fails if its build, tools or device are
missing. `scripts/test.sh` and the hook always run the local Linux relay fixture.
The old hardware environment switches are rejected with a migration message.
See [portable testing](testing.md) for all three test layers.

## Physical LAN and Wi-Fi interruption

Connect the authorized device to the computer's LAN. Select a computer IPv4
address reachable on that LAN and a free UDP port already allowed by its local
firewall; the harness never changes firewall rules. For example:

```sh
python3 scripts/relay_smoke.py --android --lan-address HOST_LAN_IP --lan-port 60303 --wifi-cycle
# Faster fresh-state LAN/handoff iteration, skipping earlier lifecycle cases:
python3 scripts/relay_smoke.py --android --lan-address HOST_LAN_IP --lan-port 60303 --wifi-cycle --lan-only
```

The direct test disables client relays and checks both the selected UDP path and
actual HTTP response bytes. The subsequent watch enables direct and relay paths,
keeps one process/endpoint/key alive, briefly disables Wi-Fi, verifies relay
fallback, restores Wi-Fi, and verifies a selected direct path again. It sends
SIGUSR1 after each interface transition to exercise Iroh's required Android
network-change notification API. The backup relay remains reachable through USB;
this is not a cellular or unrelated-Wi-Fi relay test. Direct service bytes are
checked again after recovery. Watch is bounded by an on-device 180-second timeout
and its exact owned PID is terminated during cleanup.

A failed test also restores Wi-Fi and removes only owned temporary resources.
Recovery is checked within 30 seconds after each interface state becomes usable;
Wi-Fi reconnection itself has a separate 45-second fixture bound. No user key,
pin, grant, or authority generation/revision is replaced. A selected direct path
can coexist with an unavailable relay; diagnostics report these separately.

Run the separately selected Wi-Fi case with:

```sh
python3 scripts/test_hardware.py --serial DEVICE_SERIAL --lan-address HOST_LAN_IP --lan-port 60303 --wifi-cycle
```

## What the harness verifies

- Fresh A and B own distinct local identities, owner state, generations and policy.
  Only one host process runs at a time. Linux and Android clients start with no
  directory/controller state and connect using the expected pin plus relay URL,
  never a peer IP hint. Both sides explicitly disable peer UDP transports.
- Unknown clients cannot fetch catalogs or open backends. A's approval cannot
  authorize B. Separate local owner approval and a service grant on each target
  enable catalog and actual HTTP response bytes over the relay. Wrong pins fail
  without reaching the live host backend.
- Client identities survive CLI cold starts; private Android key permissions are
  checked. Host restart preserves policy, generation/revision and catalog.
- A separate endpoint probes offline A while B serves a healthy catalog within
  four seconds. The CLI key lease is respected by using independent identities.
- With the relay deliberately stalled, the host still starts within two seconds
  and its local authority can be read. Remote catalog reachability fails through
  the normal bounded connection path. Resuming/replacing the relay restores a
  read-only catalog probe within a 30-second harness deadline using unchanged
  host pins, catalog and approvals. Service operations are never retried by the
  harness; only read-only catalog recovery probes are retried.
- B's local revocation persists across B restart while the same client can still
  access A. Backend request counts exclude denied accesses and catalog probes.

## Hardware evidence and limits

The authorized tablet (`arm64-v8a`, Android API 34) passed native USB-relay
lifecycle checks and physical LAN direct control/HTTP access. A single native
watch process then selected direct → relay → direct during a Wi-Fi off/on cycle
with explicit network callbacks, unchanged identity and catalog, followed by
successful direct HTTP access. Wi-Fi was restored and temporary files/USB mappings
were removed. The chosen UDP port was already permitted by the host's UFW rules;
an arbitrary ephemeral port was not reliably reachable through that firewall.

This remains native Android CLI evidence. It does not implement or validate an
APK, Android foreground/background lifecycle, Keystore/JNI DNS integration,
automatic app network-callback registration, QR pairing, cellular or unrelated
Wi-Fi handoff, or a real two-computer power-off test. Both target host processes
ran on one Linux computer and were alternately absent. The fallback relay was a
local TLS server reached over USB, not a cloud deployment or a relay-hostname/DNS
test on Android. Those remain #12, #17, #18 and #13/#14 acceptance work; #13 is
not closed by this harness.

The optional [physical two-host harness](physical-relay-validation.md) extends the
native checks to a second SSH-accessible Linux computer. Both physical hosts have
their own keys, authority state and local backend, and the tablet connects with
the opposite host's agent stopped. Enable it with `python3 scripts/test_hardware.py --serial DEVICE_SERIAL
--ssh-host SECOND_COMPUTER`. Its documented SSH/USB relay forwarding keeps the
orchestration computer powered on; it is not a cloud deployment or APK test.

### Owner-local connection tracing

For a running installed node, capture its last authenticated incoming connection
per approved peer without loading an identity into a second endpoint:

```sh
printf '%s\n' '{"op":"transport_status"}' | python3 scripts/node_request.py --config "$HOME/.config/wispnet-node/host.toml"
```

Each sample includes its age and connection ID. Short presence/catalog RPCs can
replace an earlier diagnostic sample, so capture while reproducing the failure.
Counters belong to that connection and include retransmitted frames. They do not
contain IP addresses or application payloads. `ADD_ADDRESS` advertisements
normally flow from server to client; `REACH_OUT` requests flow from client to
server to coordinate hole punching. Successful advertisement/request exchange
with unanswered path challenges places the failure after coordination, but does
not identify which network rejected the packets.

The diagnostic's transport-cache address count is not the QUIC NAT traversal
candidate list. In particular, a zero cache count does not establish that no
addresses were advertised or that no hole punching was attempted.

A successful UDP discovery response does not imply a public address was found.
When the discovery server and endpoint sit behind the same router, hairpin NAT
can make the server observe a private LAN address. Such an address is usable on
the LAN but cannot advertise an Internet route to a cellular peer. The public-IP
checks reject private, loopback and other special-use address ranges. Internet
reachability needs an observation from outside that NAT, a working router port
mapping, or an explicitly configured public address backed by a UDP forward.

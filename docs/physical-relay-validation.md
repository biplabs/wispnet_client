# Two physical hosts and Android native relay validation

The optional harness runs independent authorities on this computer and a second
Linux computer reached through an already trusted SSH alias. It uses the real
Android native CLI through authorized ADB. It does not install packages, modify
existing project checkouts, or touch system services. Each computer generates its
own identity/owner credentials in fresh protected temporary state; neither host's
key or owner token is copied. Only binaries, public CA/configuration and an
intended client's invitation are transferred.

Run the full local hook suite with the current Android binary rebuilt first:

```sh
WISPNET_ANDROID_RELAY_TEST=1 WISPNET_SSH_RELAY_HOST=SECOND_COMPUTER scripts/test.sh
```

Or after building/validating both native binaries and the relay fixture:

```sh
python3 scripts/physical_relay_smoke.py --ssh-host SECOND_COMPUTER --serial AUTHORIZED_ADB_SERIAL
```

Omit `--serial` only with exactly one authorized device. The second computer needs
compatible Linux binary/runtime support, Python 3, `timeout`, SCP and permission
for an SSH reverse forward. SSH runs in batch mode with strict existing host-key
checking. The harness probes the copied CLI before initializing any host state;
it fails on an incompatible runtime rather than installing dependencies.

Each physical host runs its own loopback HTTP backend. With the other host's
Wispnet agent stopped, the tablet uses only the expected host ID and configured
TLS relay to exercise unapproved denial, invitation proof, pending denial, local
owner approval, separate service grants, filtered catalog and actual HTTP bytes.
Client invitation copies are removed after the proof acknowledgement. The tablet
then cold-starts fresh CLI processes, reconnects after the host process restarts,
and recovers its catalog within a 30-second read-only retry window after rebuilding
the stateless relay. Identity, catalog generation/revision and saved pins remain
unchanged. Symmetric host tests finish with remote-host revocation remaining local:
its client is denied after restart while the other host still serves that client.

The fixture's loopback TLS relay is reached from the second computer through an
owned SSH reverse forward and from the tablet through an owned USB reverse. Thus
the primary computer remains powered on to provide the test relay and orchestration.
The opposite host's **Wispnet authority process** is stopped, not the whole computer.
This proves no dependency on that host's registry/approval or backend. It does not
prove availability after powering off the machine providing the test relay.
Deployed disposable-relay validation belongs to #18; APK/QR/UI, real cellular and
unrelated-Wi-Fi behavior remain #12/#14 work. Existing LAN/Wi-Fi and isolated
DNS/IP-replacement harnesses provide their separately documented native evidence.

All fixture processes are owned/drained by the harness; agent supervisors stop
their children on stdin closure or termination. Cleanup removes only the owned
ADB reverse, unique private tablet directory, SSH forward and remote/local
temporary directories. Remote fallback cleanup validates an agent's command path
against the unique fixture root before signaling it. No existing user state or
unrelated listeners are removed. Cleanup errors fail the run.

This scenario was verified using two physical x86-64 Linux computers and the
authorized arm64 Android tablet, with current native binaries. The fixture still
uses a local TLS relay and native CLI, so it is not production deployment or an
Android application acceptance claim.

An [existing desktop relay](../../wispnet_server/docs/desktop-relay.md) can now be selected with `--relay-url`
and optional `--relay-ca-file`. That mode skips all local relay and SSH/USB
forwarding fixtures. `--restart-desktop-relay` restarts only `wispnet-relay.service`
on the selected SSH host and verifies the active node process remains unchanged.
Both modes also test fresh Linux clients in both cross-computer directions.

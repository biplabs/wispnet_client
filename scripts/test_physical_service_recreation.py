#!/usr/bin/env python3
"""Opt-in two-host recreation test; the selected remote stays headless, relay stays up.

Uses existing reciprocal pins/approvals and each host's website backend. Only
temporary service registrations are mutated. Restarts both nodes, never the relay.
"""

import argparse
import json
import shlex
import subprocess
import sys
import time
import uuid

SSH = []
NODE = {}


def configure(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ssh-host", required=True, help="Trusted SSH alias for the headless Linux host")
    parser.add_argument("--local-node", required=True, help="Local installed wispnet-node executable")
    parser.add_argument("--remote-node", required=True, help="Remote installed wispnet-node executable")
    args = parser.parse_args(argv)
    if args.ssh_host.startswith("-") or not all(c.isalnum() or c in ".-_@" for c in args.ssh_host):
        parser.error("--ssh-host must be a trusted SSH alias or user@host")
    SSH[:] = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", args.ssh_host]
    NODE.update({False: args.local_node, True: args.remote_node})
    return args



def execute(remote, args, data=None, check=True):
    if not SSH or not NODE:
        raise RuntimeError("Select hosts with configure() before running installed-node acceptance")
    return subprocess.run(
        SSH + [shlex.join(list(map(str, args)))] if remote else args,
        input=data,
        text=True,
        capture_output=True,
        check=check,
        timeout=40,
    )


def request(remote, value, ok=True):
    result = execute(
        remote, [NODE[remote]], json.dumps(value), check=False
    )
    if (result.returncode == 0) != ok:
        raise RuntimeError("Unexpected owner-local node result: " + result.stderr)
    return json.loads(result.stdout) if ok else None


def snapshot(remote):
    return request(remote, {"op": "status"})


def mutate(remote, value):
    request(
        remote,
        {
            "op": "mutate",
            "operation": {
                **value,
                "expected_revision": snapshot(remote)["host"]["revision"],
            },
        },
    )


HTTP = """import http.client,json,sys,urllib.parse
d=json.load(sys.stdin);u=urllib.parse.urlsplit(d['url'])
c=http.client.HTTPConnection('127.0.0.1',u.port,timeout=15)
h={'Host':u.netloc,'Sec-Fetch-Site':'same-origin','Sec-Fetch-Mode':'navigate','Sec-Fetch-Dest':'document'}
if d.get('cookie'):h['Cookie']=d['cookie']
c.request('GET','/',headers=h)
r=c.getresponse();body=r.read()
print(json.dumps({'status':r.status,'bytes':len(body),'cookie':r.getheader('Set-Cookie','').split(';')[0]}))
c.close()
"""


def http(remote, url, cookie=None):
    return json.loads(
        execute(
            remote,
            [sys.executable if not remote else "python3", "-c", HTTP],
            json.dumps(
                {"op": "get" if cookie else "bootstrap", "url": url, "cookie": cookie}
            ),
        ).stdout
    )


def stable(state):
    host = state["host"]
    return {
        "host": host["host"],
        "generation": host["generation"],
        "clients": host["clients"],
        "services": host["services"],
        "pins": state["peer_pins"],
    }


def restart(remote):
    execute(remote, ["systemctl", "--user", "restart", "wispnet-host.service"])
    deadline = time.monotonic() + 15
    while True:
        try:
            return snapshot(remote)
        except (RuntimeError, subprocess.SubprocessError):
            if time.monotonic() > deadline:
                raise
            time.sleep(0.2)


def main():
    configure()
    baseline = {remote: snapshot(remote) for remote in [False, True]}
    fixture = "recreation-" + uuid.uuid4().hex[:12]
    relay_pid = execute(
        True,
        [
            "systemctl",
            "--user",
            "show",
            "wispnet-relay.service",
            "-p",
            "MainPID",
            "--value",
        ],
    ).stdout.strip()
    assert relay_pid != "0"
    try:
        for target in [True, False]:
            source = not target
            host = baseline[target]["host"]["host"]
            peer = baseline[source]["host"]["host"]
            backend = next(
                s["backend"]
                for s in baseline[target]["host"]["services"]
                if s["id"] == "website"
            )
            registration = {
                "op": "service",
                "id": fixture,
                "backend": backend,
                "allow": [peer],
            }
            mutate(target, registration)
            catalog = request(source, {"op": "catalog", "host": host})
            old = next(
                s["incarnation"] for s in catalog["services"] if s["id"] == fixture
            )
            request(source, {"op": "enable"})
            opening = {
                "op": "open",
                "host": host,
                "service": fixture,
                "incarnation": old,
            }
            url = request(source, opening)["url"]
            bootstrap = http(source, url)
            assert bootstrap["status"] == 200
            assert not bootstrap["cookie"]
            cookie = bootstrap["cookie"]
            assert http(source, url, cookie)["status"] == 200
            # Same ID, address and grants, recreated before any one-second tick.
            mutate(target, {"op": "remove", "id": fixture})
            mutate(target, registration)
            fresh = next(
                s["incarnation"]
                for s in snapshot(target)["host"]["services"]
                if s["id"] == fixture
            )
            assert fresh != old
            assert http(source, url, cookie)["status"] == 403
            request(source, opening, ok=False)
            request(source, {"op": "open", "host": host, "service": fixture}, ok=False)
            restarted = restart(target)
            assert (
                next(
                    s["incarnation"]
                    for s in restarted["host"]["services"]
                    if s["id"] == fixture
                )
                == fresh
            )
            assert http(source, url, cookie)["status"] == 403
            request(source, opening, ok=False)
            opening["incarnation"] = fresh
            fresh_url = request(source, opening)["url"]
            fresh_bootstrap = http(source, fresh_url)
            assert fresh_bootstrap["status"] == 200
            assert not fresh_bootstrap["cookie"]
            assert http(source, fresh_url, fresh_bootstrap["cookie"])["status"] == 200
            mutate(target, {"op": "remove", "id": fixture})
            request(source, {"op": "stop"})
            print(
                f"{'local → remote host' if target else 'remote host → local'}: stale listing/session denied, unbound Open denied, incarnation survived target restart, fresh Open HTTP 200"
            )
    finally:
        for remote in [False, True]:
            state = snapshot(remote)
            if any(s["id"] == fixture for s in state["host"]["services"]):
                mutate(remote, {"op": "remove", "id": fixture})
            request(
                remote,
                {"op": "enable" if baseline[remote]["access_enabled"] else "stop"},
            )
            assert stable(snapshot(remote)) == stable(baseline[remote]), (
                "Existing authority/pins changed"
            )
    assert (
        execute(
            True,
            [
                "systemctl",
                "--user",
                "show",
                "wispnet-relay.service",
                "-p",
                "MainPID",
                "--value",
            ],
        ).stdout.strip()
        == relay_pid
    )
    assert (
        execute(
            True,
            [
                "systemctl",
                "--user",
                "show",
                "wispnet-desktop.service",
                "-p",
                "LoadState",
                "--value",
            ],
        ).stdout.strip()
        == "not-found"
    )
    print(
        "Fixture services removed; existing host identities/generations, services/incarnations, approvals/grants and pins preserved; relay PID unchanged; selected remote remained headless"
    )


if __name__ == "__main__":
    main()

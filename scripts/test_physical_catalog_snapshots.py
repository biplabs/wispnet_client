#!/usr/bin/env python3
"""Opt-in complete catalogs/typed metadata/removal/restart checks on installed nodes.

Uses the existing reciprocal owner routes and loopback website backends. Creates
temporary service rows, never resets identities/pins or restarts the relay/GUI.
"""

import json
import uuid
from test_physical_service_recreation import (
    configure,
    execute,
    request,
    snapshot,
    mutate,
    stable,
    restart,
)


def baseline(remote, host):
    code = """import json,pathlib,sys,tomllib
config=pathlib.Path.home()/'.config/wispnet-node/host.toml'
state=pathlib.Path(tomllib.loads(config.read_text())['state_dir'])
print((state/'catalog-baselines'/(sys.argv[1]+'.json')).read_text())
"""
    return json.loads(execute(remote, ["python3", "-c", code, host]).stdout)


def main():
    configure()
    original = {r: snapshot(r) for r in [False, True]}
    prefix = "snapshot-" + uuid.uuid4().hex[:10]
    ids = [prefix + "-a", prefix + "-b"]
    relay = execute(
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
    ).stdout
    try:
        for target in [True, False]:
            source = not target
            host = original[target]["host"]["host"]
            peer = original[source]["host"]["host"]
            backend = next(
                s["backend"]
                for s in original[target]["host"]["services"]
                if s["id"] == "website"
            )
            for id, kind in zip(ids, ["http", "tcp"]):
                mutate(
                    target,
                    {
                        "op": "service",
                        "id": id,
                        "backend": backend,
                        "allow": [peer],
                        "display_name": "Snapshot fixture " + kind,
                        "kind": kind,
                    },
                )
            catalog = request(source, {"op": "catalog", "host": host})
            assert catalog["version"] == 1 and catalog["complete"] is True
            assert catalog["service_count"] == len(catalog["services"])
            assert [s["id"] for s in catalog["services"]] == sorted(
                s["id"] for s in catalog["services"]
            )
            records = {s["id"]: s for s in catalog["services"]}
            assert (
                records[ids[0]]["kind"] == "http" and records[ids[1]]["kind"] == "tcp"
            )
            assert records[ids[1]]["display_name"] == ids[1]
            request(source, {"op": "enable"})
            request(
                source,
                {
                    "op": "open",
                    "host": host,
                    "service": ids[1],
                    "incarnation": records[ids[1]]["incarnation"],
                },
                ok=False,
            )
            # Omitting metadata on an ordinary owner edit preserves it; withdrawing
            # grants changes the complete filtered listing without changing authority elsewhere.
            mutate(
                target, {"op": "service", "id": ids[0], "backend": backend, "allow": []}
            )
            current = request(source, {"op": "catalog", "host": host})
            assert current["revision"] > catalog["revision"]
            assert ids[0] not in [s["id"] for s in current["services"]]
            assert ids[1] in [s["id"] for s in current["services"]]
            for id in ids:
                mutate(target, {"op": "remove", "id": id})
            removed = request(source, {"op": "catalog", "host": host})
            assert not set(ids).intersection(s["id"] for s in removed["services"])
            before = baseline(source, host)
            assert before["catalog"] == removed
            restart(source)
            assert baseline(source, host) == before
            assert request(source, {"op": "catalog", "host": host}) == removed
            print(
                f"{'local → remote host' if target else 'remote host → local'}: complete canonical count, typed metadata, TCP browser denial, filtered withdrawal/removal and durable baseline across source restart passed"
            )
    finally:
        for remote in [False, True]:
            for id in ids:
                if any(s["id"] == id for s in snapshot(remote)["host"]["services"]):
                    mutate(remote, {"op": "remove", "id": id})
            request(
                remote,
                {"op": "enable" if original[remote]["access_enabled"] else "stop"},
            )
            assert stable(snapshot(remote)) == stable(original[remote])
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
        ).stdout
        == relay
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
        "Fixture rows removed; existing authority/pins/services preserved; relay unchanged; the selected remote remains headless"
    )


if __name__ == "__main__":
    main()

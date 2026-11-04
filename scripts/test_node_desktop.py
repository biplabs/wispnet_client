"""Real two-node IPC, permission, browser-gateway and live-connection integration."""

import json
import os
from pathlib import Path
import signal
import sys
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.request
import threading
import http.server
import http.client

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/wispnet"

class NativeRejected(ValueError):
    pass

class NativeNodeAPI:
    def __init__(self, config):
        self.config = config

    def request(self, **request):
        result = subprocess.run(
            [BINARY, "node-request", "--config", self.config, "--response"],
            input=json.dumps(request), text=True, capture_output=True, timeout=75,
        )
        reply = json.loads(result.stdout)
        if not reply["ok"]:
            kind = NativeRejected if reply["kind"] == "rejected" else OSError
            raise kind(reply["error"])
        return reply["result"]


class NodeIntegration(unittest.TestCase):
    def test_private_api_pair_catalog_gateway_counts_and_revision(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            processes = []

            def cli(config, *args):
                return subprocess.run(
                    [BINARY, args[0], "--config", config, *args[1:]],
                    capture_output=True,
                    text=True,
                    check=True,
                    timeout=15,
                ).stdout

            def host(name):
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                    udp.bind(("127.0.0.1", 0))
                    port = udp.getsockname()[1]
                config = root / (name + ".toml")
                state = root / name
                config.write_text(
                    f'schema_version=2\ndevice_name="{name}"\nstate_dir="{state}"\nlog_level="info"\n[transport]\nbind_addr="127.0.0.1:{port}"\nrelay_url="https://relay.example"\n'
                )
                peer = cli(config, "host-init").split("Endpoint ID: ")[1].strip()
                process = subprocess.Popen(
                    [BINARY, "agent", "--config", config],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                processes.append(process)
                deadline = time.monotonic() + 10
                while not (state / "node.sock").exists():
                    self.assertIsNone(process.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.05)
                self.assertEqual((state / "node.sock").stat().st_mode & 0o777, 0o600)
                return config, state, peer, port, NativeNodeAPI(config)

            release = threading.Event()

            class Backend(http.server.BaseHTTPRequestHandler):
                def do_GET(self):
                    self.send_response(200)
                    self.end_headers()
                    if self.path == "/hold":
                        release.wait(8)
                    try:
                        self.wfile.write(b"node-gateway-ok")
                    except (BrokenPipeError, ConnectionResetError):
                        pass

                def log_message(self, *args):
                    pass

            backend = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
            thread = threading.Thread(target=backend.serve_forever, daemon=True)
            thread.start()
            try:
                a, sa, pa, porta, api = host("a")
                b, sb, pb, portb, bapi = host("b")
                # The desktop IPC exposes the shared diagnostic, while its
                # native pairing check still rejects an unpaired target.
                with self.assertRaisesRegex(NativeRejected, "Pair with this host first"):
                    api.request(op="diagnose_connection", host=pb)
                # Target acknowledgements are durable and scoped to this owner-local host.
                _, _, pc, _, capi = host("combined")
                approval = {
                    "op": "apply_operation",
                    "host": pc,
                    "id": "1" * 64,
                    "action": {"op": "approve", "peer": pa, "name": "Combined fixture"},
                }
                receipt = capi.request(**approval)
                self.assertEqual(receipt["state"], "applied")
                self.assertEqual(receipt["host"], pc)
                self.assertEqual(capi.request(**approval), receipt)
                capi.request(
                    op="apply_operation",
                    host=pc,
                    id="2" * 64,
                    action={"op": "revoke", "peer": pa},
                )
                self.assertEqual(capi.request(**approval), receipt)
                self.assertEqual(capi.request(op="status")["host"]["clients"], [])
                self.assertEqual(capi.request(op="transport_status")["peers"], {})
                with self.assertRaisesRegex(ValueError, "Wrong target host"):
                    capi.request(**{**approval, "host": pb})
                rejected = capi.request(
                    op="apply_operation",
                    host=pc,
                    id="3" * 64,
                    action={"op": "revoke", "peer": pa},
                )
                self.assertEqual(rejected["state"], "rejected")
                self.assertEqual(capi.request(op="status")["host"]["revision"], 2)

                self.assertEqual(api.request(op="status")["connected_peers"], {})
                self.assertEqual(bapi.request(op="status")["host"]["clients"], [])
                with self.assertRaises(ValueError):
                    api.request(op="shell", command="echo bad")
                with self.assertRaises(ValueError):
                    api.request(op="catalog", host=pb)
                invitation_file = root / "invitation.json"
                cli(b, "host-invite", "--output", invitation_file)
                invitation = json.loads(invitation_file.read_text())
                invitation["direct_hint"] = f"127.0.0.1:{portb}"
                self.assertEqual(
                    api.request(op="pair", host=pb, invitation=json.dumps(invitation))[
                        "state"
                    ],
                    "pending_owner_approval",
                )
                pending = bapi.request(op="status")
                self.assertEqual(pending["host"]["clients"], [])
                self.assertEqual(pending["invitations"][0]["peer"], pa)
                self.assertTrue(pending["invitations"][0]["mutual"])
                self.assertNotIn(pa, pending["peer_pins"])
                self.assertEqual(api.request(op="status")["host"]["clients"][0]["peer"], pb)
                bapi.request(
                    op="mutate",
                    operation={
                        "op": "approve_invitation",
                        "expected_revision": 0,
                        "invitation": invitation["invitation"],
                    },
                )
                bapi.request(
                    op="mutate",
                    operation={
                        "op": "service",
                        "expected_revision": 1,
                        "id": "website",
                        "backend": f"127.0.0.1:{backend.server_port}",
                        "allow": [pa],
                    },
                )
                with self.assertRaises(ValueError):
                    bapi.request(
                        op="mutate",
                        operation={
                            "op": "remove",
                            "expected_revision": 0,
                            "id": "website",
                        },
                    )
                # Mutual bootstrap is durable and secret-free in the protected policy DB.
                import sqlite3
                database = sa / "host.sqlite3"
                with sqlite3.connect(database) as db:
                    saved = db.execute("SELECT bootstrap FROM host_pairings WHERE peer=?", (pb,)).fetchone()[0]
                self.assertNotIn(invitation["code"], saved)
                self.assertEqual(json.loads(saved)["host"], pb)
                self.assertIn(pa, bapi.request(op="status")["peer_pins"])
                self.assertEqual(bapi.request(op="catalog", host=pa)["services"], [])
                catalog = api.request(op="catalog", host=pb)
                self.assertEqual(catalog["host"], pb)
                self.assertEqual([s["id"] for s in catalog["services"]], ["website"])
                # Owner-local tracing survives a short authenticated RPC closing.
                # It exposes frame counts, never peer addresses or payloads.
                transport = bapi.request(op="transport_status")
                self.assertEqual(transport["host"], pb)
                observed = transport["peers"][pa]
                self.assertGreaterEqual(observed["age_ms"], 0)
                counters = observed["connection"]
                self.assertIn(counters["path"], ("direct", "relay", "unknown"))
                for field in ("address_advertisements_sent", "address_advertisements_received",
                              "holepunch_requests_sent", "holepunch_requests_received",
                              "path_challenges_sent", "path_responses_received"):
                    self.assertIsInstance(counters[field], int)
                    self.assertGreaterEqual(counters[field], 0)
                self.assertNotIn("127.0.0.1", json.dumps(transport))
                remote_incarnation = catalog["services"][0]["incarnation"]
                # The headless command reuses the daemon, without a key lease or Qt.
                command = [
                    sys.executable,
                    ROOT / "scripts/node_request.py",
                    "--config",
                    a,
                ]
                environment = {
                    **os.environ,
                    "PYTHONPATH": str(ROOT / "apps/node/desktop"),
                }
                result = subprocess.run(
                    command,
                    input=json.dumps({"op": "catalog", "host": pb}),
                    capture_output=True,
                    text=True,
                    check=True,
                    env=environment,
                    timeout=30,
                )
                self.assertEqual(json.loads(result.stdout), catalog)
                result = subprocess.run(
                    command,
                    input="",
                    capture_output=True,
                    text=True,
                    check=True,
                    env=environment,
                    timeout=30,
                )
                self.assertEqual(json.loads(result.stdout)["host"]["host"], pa)
                for invalid in ["[]", "x" * 16385]:
                    result = subprocess.run(
                        command,
                        input=invalid,
                        capture_output=True,
                        text=True,
                        env=environment,
                        timeout=5,
                    )
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "")

                self.assertIn(pa, bapi.request(op="status")["recent_peers"])
                # Keep the fixture independent of an installed node's registry.
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as registry:
                    registry.bind(("127.0.0.1", 0))
                    registry_port = registry.getsockname()[1]
                api.request(
                    op="configure_access",
                    settings={
                        "registry_port": registry_port,
                        "allowed_origins": [],
                        "service_ports": [],
                    },
                )
                api.request(op="enable")
                url = api.request(
                    op="open",
                    host=pb,
                    service="website",
                    incarnation=remote_incarnation,
                )["url"]
                # Script commands share the same authenticated API and route state.
                self.assertEqual(json.loads(cli(a, "node-status"))["host"]["host"], pa)
                settings = json.loads(cli(a, "settings", "--port", str(registry_port)))
                self.assertEqual(settings["settings"]["registry_port"], registry_port)
                links = json.loads(cli(a, "service-links", "--host", pb, "--service", "website"))
                self.assertEqual(links["local"], url)
                self.assertEqual(links["wispnet"], f"wispnet://{pb}/website")
                self.assertIsNone(links["public"])
                resolved = json.loads(cli(a, "resolve-link", "--link", links["wispnet"]))
                self.assertEqual(resolved["url"], url)
                request_file = root / "cli-request.json"
                request_file.write_text(json.dumps({"op": "catalog", "host": pb}))
                request_file.chmod(0o600)
                self.assertEqual(json.loads(cli(a, "node-request", "--request", request_file)), catalog)
                request_file.chmod(0o644)
                denied = subprocess.run([BINARY, "node-request", "--config", a, "--request", request_file], capture_output=True, text=True)
                self.assertNotEqual(denied.returncode, 0)
                self.assertIn("owner-only", denied.stderr)
                from urllib.parse import urlsplit

                parsed = urlsplit(url)

                def exchange(path, headers=None, body=None):
                    import http.client

                    conn = http.client.HTTPConnection(
                        "127.0.0.1", parsed.port, timeout=10
                    )
                    conn.request(
                        "POST" if body is not None else "GET",
                        path,
                        body=body,
                        headers={"Host": parsed.netloc, **(headers or {})},
                    )
                    response = conn.getresponse()
                    data = response.read()
                    result = (response.status, dict(response.getheaders()), data)
                    conn.close()
                    return result

                self.assertEqual(exchange("/")[0], 200)
                status, headers, _ = exchange(
                    "/",
                    {
                        "Sec-Fetch-Site": "none",
                        "Sec-Fetch-Mode": "navigate",
                        "Sec-Fetch-Dest": "document",
                    },
                )
                self.assertEqual(status, 200)
                self.assertNotIn("Set-Cookie", headers)
                browser_headers = {"Sec-Fetch-Site": "same-origin"}
                self.assertEqual(
                    exchange("/", browser_headers)[2], b"node-gateway-ok"
                )
                # Hold a real service stream open and distinguish approved from connected.
                responses = []
                held = threading.Thread(
                    target=lambda: responses.append(
                        exchange("/hold", browser_headers)
                    ),
                    daemon=True,
                )
                held.start()
                try:
                    deadline = time.monotonic() + 8
                    while pa not in bapi.request(op="status")["connected_peers"]:
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.05)
                    self.assertGreaterEqual(
                        bapi.request(op="status")["connected_peers"][pa], 1
                    )
                finally:
                    release.set()
                    held.join(timeout=10)
                self.assertEqual(responses[0][2], b"node-gateway-ok")
                deadline = time.monotonic() + 8
                while bapi.request(op="status")["transport_streams"]:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.05)
                # Completed operations release their streams, while reusable
                # authenticated sessions remain connected until Stop or idle expiry.
                self.assertIn(pa, bapi.request(op="status")["connected_peers"])
                self.assertEqual(len(bapi.request(op="status")["host"]["clients"]), 1)
                api.request(op="stop")
                deadline = time.monotonic() + 8
                while bapi.request(op="status")["connected_peers"]:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.05)
                self.assertFalse(api.request(op="access_status")["enabled"])
                with self.assertRaises(OSError):
                    socket.create_connection(("127.0.0.1", parsed.port), timeout=1)
                # Owner-local browsing needs no self-pin/self-grant or transport.
                # Duplicate local IDs on different hosts do not change the route.
                api.request(
                    op="mutate",
                    operation={
                        "op": "service",
                        "expected_revision": 1,
                        "id": "website",
                        "backend": f"127.0.0.1:{backend.server_port}",
                        "allow": [],
                    },
                )
                local_incarnation = next(s["incarnation"] for s in
                    api.request(op="status")["host"]["services"] if s["id"] == "website")
                with self.assertRaises(ValueError):
                    api.request(
                        op="open",
                        host=pa,
                        service="website",
                        incarnation=local_incarnation,
                    )
                api.request(op="enable")
                with self.assertRaises(ValueError):
                    api.request(op="open", host=pa, service="missing")
                parsed = urlsplit(
                    api.request(
                        op="open",
                        host=pa,
                        service="website",
                        incarnation=local_incarnation,
                        host_name="Local computer",
                    )["url"]
                )
                self.assertEqual(parsed.hostname, f"a-{pa[:12]}-website.localhost")
                self.assertEqual(
                    urlsplit(api.request(
                        op="open", host=pa, service="website", incarnation=local_incarnation
                    )["url"]).netloc,
                    parsed.netloc,
                )
                self.assertEqual(exchange("/")[0], 200)
                self.assertEqual(exchange("/", {"Host": "other.localhost"})[0], 421)
                status, headers, _ = exchange(
                    "/",
                    {
                        "Sec-Fetch-Site": "none",
                        "Sec-Fetch-Mode": "navigate",
                        "Sec-Fetch-Dest": "document",
                    },
                )
                self.assertEqual(status, 200)
                self.assertNotIn("Set-Cookie", headers)
                browser_headers = {"Sec-Fetch-Site": "same-origin"}
                self.assertEqual(
                    exchange("/", browser_headers)[2], b"node-gateway-ok"
                )
                status, headers, body = exchange(
                    "/", {"Origin": "https://unlisted.example"}
                )
                self.assertEqual(status, 200)
                self.assertEqual(body, b"node-gateway-ok")
                self.assertEqual(
                    headers["Access-Control-Allow-Origin"], "https://unlisted.example"
                )
                # The other host's idle control session may still be connected.
                # Owner-local browsing must not create a self-connection.
                self.assertNotIn(pa, api.request(op="status")["connected_peers"])
                self.assertEqual(api.request(op="status")["host"]["clients"][0]["peer"], pb)
                self.assertNotIn(pa, api.request(op="status")["peer_pins"])
                # Removing the binding interrupts an active local response and
                # rejects subsequent requests using the saved service URL.
                release.clear()
                held_connection = http.client.HTTPConnection(
                    "127.0.0.1", parsed.port, timeout=4
                )
                held_connection.request(
                    "GET", "/hold", headers={"Host": parsed.netloc, **browser_headers}
                )
                held_response = held_connection.getresponse()
                started = time.monotonic()
                api.request(
                    op="mutate",
                    operation={"op": "remove", "expected_revision": 2, "id": "website"},
                )
                # Recreate immediately, before the one-second policy tick, at
                # exactly the same address. Missing the deleted row is harmless.
                api.request(
                    op="mutate",
                    operation={
                        "op": "service",
                        "expected_revision": 3,
                        "id": "website",
                        "backend": f"127.0.0.1:{backend.server_port}",
                        "allow": [],
                    },
                )
                self.assertEqual(held_response.read(), b"")
                self.assertLess(time.monotonic() - started, 3)
                held_connection.close()
                release.set()
                self.assertEqual(exchange("/", browser_headers)[0], 403)
                with self.assertRaises(ValueError):
                    api.request(
                        op="open",
                        host=pa,
                        service="website",
                        incarnation=local_incarnation,
                    )
                new_incarnation = next(s["incarnation"] for s in
                    api.request(op="status")["host"]["services"] if s["id"] == "website")
                self.assertNotEqual(new_incarnation, local_incarnation)
                api.request(
                    op="open", host=pa, service="website", incarnation=new_incarnation
                )
                # A remote rename keeps the pairing and grants, but moves URLs
                # to the new ID. The old saved route must no longer serve it.
                before_rename = bapi.request(op="status")["host"]
                bapi.request(
                    op="mutate",
                    operation={
                        "op": "edit_service",
                        "expected_revision": before_rename["revision"],
                        "id": "website",
                        "new_id": "renamed-website",
                        "backend": f"127.0.0.1:{backend.server_port}",
                        "allow": [pa],
                        "kind": "http",
                    },
                )
                renamed = api.request(op="catalog", host=pb)
                self.assertEqual([s["id"] for s in renamed["services"]], ["renamed-website"])
                self.assertEqual(renamed["services"][0]["incarnation"], remote_incarnation)
                after_rename = bapi.request(op="status")["host"]
                self.assertEqual(after_rename["clients"], before_rename["clients"])
                self.assertEqual(next(s["allow"] for s in after_rename["services"]
                                      if s["id"] == "renamed-website"), [pa])
                self.assertIn(pb, api.request(op="status")["peer_pins"])
                with self.assertRaises(ValueError):
                    api.request(op="resolve_service", host=pb, service="website")
                old_address = urlsplit(url)
                connection = http.client.HTTPConnection("127.0.0.1", old_address.port, timeout=5)
                connection.request("GET", "/", headers={"Host": old_address.netloc})
                response = connection.getresponse()
                self.assertEqual(response.status, 403)
                response.read()
                connection.close()
                renamed_url = api.request(op="resolve_service", host=pb, service="renamed-website")["url"]
                renamed_address = urlsplit(renamed_url)
                connection = http.client.HTTPConnection("127.0.0.1", renamed_address.port, timeout=5)
                connection.request("GET", "/", headers={"Host": renamed_address.netloc})
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(response.read(), b"node-gateway-ok")
                connection.close()
                api.request(op="stop")
                with self.assertRaises(OSError):
                    socket.create_connection(("127.0.0.1", parsed.port), timeout=1)
                # Successful requests retain recent activity after Stop closes
                # reusable sessions, without emitting background network traffic.
                status = bapi.request(op="status")
                self.assertEqual(status["presence_timeout_seconds"], 60)
                self.assertEqual(status["connected_peers"], {})
                self.assertIn(pa, status["recent_peers"])
                deadline = time.monotonic() + 65
                while pa in bapi.request(op="status")["recent_peers"]:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.2)
                self.assertEqual(len(bapi.request(op="status")["host"]["clients"]), 1)
                api.request(op="catalog", host=pb)
                self.assertIn(pa, bapi.request(op="status")["recent_peers"])
                bapi.request(
                    op="mutate",
                    operation={"op": "revoke", "expected_revision": 3, "peer": pa},
                )
                self.assertNotIn(pa, bapi.request(op="status")["recent_peers"])
                with self.assertRaises(ValueError):
                    api.request(op="catalog", host=pb)
                self.assertNotIn(pa, bapi.request(op="status")["recent_peers"])
                self.assertIn(pb, api.request(op="status")["peer_pins"])
                api.request(op="unpair", host=pb)
                self.assertNotIn(pb, api.request(op="status")["peer_pins"])
                with self.assertRaises(ValueError):
                    api.request(op="catalog", host=pb)
                # Forgetting locally never modifies remote owner permissions.
                self.assertEqual(bapi.request(op="status")["host"]["clients"], [])
                badpath = sa / "node.sock"
                badpath.chmod(0o666)
                with self.assertRaisesRegex(OSError, "owner-only"):
                    api.request(op="status")
                badpath.chmod(0o600)
            finally:
                for p in processes:
                    p.send_signal(signal.SIGINT)
                for p in processes:
                    p.wait(timeout=10)
                backend.shutdown()
                backend.server_close()
                thread.join(timeout=5)
            self.assertFalse((sa / "node.sock").exists())
            self.assertFalse((sb / "node.sock").exists())


if __name__ == "__main__":
    unittest.main()

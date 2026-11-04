"""Exercise installed native helper commands with no executables on PATH."""

import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/wispnet"


class BridgeClient:
    def __init__(self, config, environment, error_log):
        self.process = subprocess.Popen(
            [BINARY, "desktop-bridge", "--config", config],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=error_log,
            text=True,
            env=environment,
        )
        self.sequence = 0
        self.replies = queue.Queue()
        self.pending = {}

        def read_replies():
            try:
                for line in self.process.stdout:
                    self.replies.put(json.loads(line))
            finally:
                self.replies.put(None)

        self.reader = threading.Thread(target=read_replies, daemon=True)
        self.reader.start()

    def send(self, method, arguments=None):
        self.sequence += 1
        identifier = self.sequence
        self.process.stdin.write(json.dumps({
            "id": identifier, "method": method, "arguments": arguments or {},
        }) + "\n")
        self.process.stdin.flush()
        return identifier

    def receive(self, identifier):
        deadline = time.monotonic() + 30
        while identifier not in self.pending:
            reply = self.replies.get(timeout=max(.01, deadline - time.monotonic()))
            if reply is None:
                raise AssertionError("Native desktop bridge exited before replying")
            if reply["id"] in self.pending:
                raise AssertionError("Native desktop bridge returned a duplicate ID")
            self.pending[reply["id"]] = reply
        return self.pending.pop(identifier)

    def request(self, method, arguments=None):
        reply = self.receive(self.send(method, arguments))
        if "error" in reply:
            raise ValueError(reply["error"])
        return reply["result"]

    def close(self):
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.process.stdout.close()


class NativeDesktopIntegration(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wispnet-native-desktop-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.state = self.root / "state"
        self.config = self.root / "host.toml"
        self.config.write_text(
            'schema_version=2\ndevice_id="native-fixture"\nstate_dir='
            + json.dumps(str(self.state))
            + '\nlog_level="error"\n[transport]\nbind_addr="127.0.0.1:0"\n'
            'relay_url="https://relay.example"\n'
        )
        # All processes under test are absolute Rust executables. An empty PATH
        # catches an accidental interpreter/subprocess runtime dependency.
        self.environment = {
            **os.environ,
            "PATH": "",
            "HOME": str(self.root),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_DATA_HOME": str(self.root / "data"),
        }
        self.environment.pop("PYTHONPATH", None)
        subprocess.run(
            [BINARY, "host-init", "--config", self.config],
            check=True, capture_output=True, text=True,
            timeout=15, env=self.environment,
        )
        self.agent_log = (self.root / "agent.log").open("wb")
        self.addCleanup(self.agent_log.close)
        self.agent = subprocess.Popen(
            [BINARY, "agent", "--config", self.config],
            stdout=subprocess.DEVNULL, stderr=self.agent_log,
            env=self.environment,
        )
        self.addCleanup(self.stop_agent)
        deadline = time.monotonic() + 15
        while True:
            self.assertIsNone(self.agent.poll(), "Native agent failed to start")
            try:
                self.initial = self.node_request({"op": "status"})
                break
            except (ValueError, subprocess.CalledProcessError):
                self.assertLess(time.monotonic(), deadline, "Native node socket was not ready")
                time.sleep(.03)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            registry_port = listener.getsockname()[1]
        self.node_request({"op": "configure_access", "settings": {
            "registry_port": registry_port, "service_ports": [],
        }})
        self.bridge_log = (self.root / "bridge.log").open("wb")
        self.addCleanup(self.bridge_log.close)
        self.bridge = BridgeClient(self.config, self.environment, self.bridge_log)
        self.addCleanup(self.bridge.close)
        self.own = self.initial["host"]["host"]

    def stop_agent(self):
        if self.agent.poll() is None:
            self.agent.terminate()
            try:
                self.agent.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.agent.kill()
                self.agent.wait(timeout=5)

    def node_command(self, request):
        return subprocess.run(
            [BINARY, "node-request", "--config", self.config, "--response"],
            input=json.dumps(request), text=True, capture_output=True,
            timeout=30, env=self.environment,
        )

    def node_request(self, request):
        outcome = self.node_command(request)
        reply = json.loads(outcome.stdout)
        if not reply["ok"]:
            raise ValueError(reply["error"])
        self.assertEqual(outcome.returncode, 0, outcome.stderr)
        return reply["result"]

    def mutate(self, operation):
        status = self.bridge.request("ownerStatus")
        operation = {**operation, "expected_revision": status["host"]["revision"]}
        return self.bridge.request("ownerRequest", {"payload": {
            "op": "mutate", "operation": operation,
        }})

    def service(self, identifier):
        return next(service for service in self.bridge.request("ownerStatus")["host"]["services"]
                    if service["id"] == identifier)

    def wait_transfer(self, result):
        deadline = time.monotonic() + 15
        while result["state"] == "running":
            self.assertLess(time.monotonic(), deadline, result)
            time.sleep(.02)
            result = self.bridge.request("fileStatus", {"transfer": result["transfer"]})
        self.assertEqual(result["state"], "complete", result)
        return result

    def test_native_stdio_ids_preferences_owner_revisions_and_validation(self):
        calls = [
            ("snapshot", {}),
            ("ownerStatus", {}),
            ("ownerData", {"op": "operations"}),
            ("ownerData", {"op": "preferences"}),
            ("methodThatDoesNotExist", {}),
            ("setServiceEnabled", {"enabled": "false"}),
        ]
        identifiers = [self.bridge.send(method, arguments) for method, arguments in calls]
        replies = [self.bridge.receive(identifier) for identifier in identifiers]
        self.assertEqual([reply["id"] for reply in replies], identifiers)
        snapshot = replies[0]["result"]
        self.assertTrue(snapshot["enabled"])
        self.assertTrue(snapshot["desktop"])
        self.assertEqual(snapshot["peer"], self.own)
        self.assertTrue(snapshot["hosts"][self.own]["local"])
        self.assertEqual(replies[1]["result"]["host"]["host"], self.own)
        self.assertEqual(replies[2]["result"], {"value": []})
        self.assertIsInstance(replies[3]["result"]["value"]["order"], dict)
        self.assertIn("Unsupported desktop UI operation", replies[4]["error"])
        self.assertIn("Invalid service state", replies[5]["error"])
        self.bridge.request("saveOrder", {"group": "machines", "ids": [self.own]})
        self.assertEqual(self.bridge.request("snapshot")["order"]["machines"], [self.own])
        self.mutate({"op": "service", "id": "fixture", "backend": "127.0.0.1:9", "allow": []})
        service = self.service("fixture")
        old_revision = self.bridge.request("ownerStatus")["host"]["revision"]
        self.bridge.request("setServiceEnabled", {
            "service": "fixture", "incarnation": service["incarnation"], "enabled": False,
        })
        self.assertFalse(self.service("fixture")["enabled"])
        new_revision = self.bridge.request("ownerStatus")["host"]["revision"]
        self.assertGreater(new_revision, old_revision)
        with self.assertRaises(ValueError):
            self.bridge.request("ownerRequest", {"payload": {"op": "mutate", "operation": {
                "op": "remove", "id": "fixture", "expected_revision": old_revision,
            }}})
        self.assertEqual(self.bridge.request("ownerStatus")["host"]["revision"], new_revision)
        with self.assertRaises(ValueError):
            self.bridge.request("setServiceEnabled", {
                "service": "fixture", "incarnation": "f" * 64, "enabled": True,
            })
        self.bridge.request("setServiceEnabled", {
            "service": "fixture", "incarnation": service["incarnation"], "enabled": True,
        })
        self.assertTrue(self.service("fixture")["enabled"])
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            configured_port = listener.getsockname()[1]
        self.bridge.request("saveSettings", {
            "host": self.own, "service": "fixture", "port": configured_port,
        })
        self.assertEqual(self.bridge.request("settings", {
            "host": self.own, "service": "fixture",
        })["port"], configured_port)
        settings = self.node_request({"op": "gateway_settings"})["settings"]
        self.assertEqual(settings["registry_port"], configured_port)
        self.assertEqual(settings["service_ports"], [])
        # Arbitrary owner commands never become a passthrough to the node.
        with self.assertRaisesRegex(ValueError, "Unsupported owner operation"):
            self.bridge.request("ownerRequest", {"payload": {"op": "shell"}})
        unavailable = self.node_command([])
        self.assertEqual(unavailable.returncode, 1)
        self.assertEqual(json.loads(unavailable.stdout)["kind"], "unavailable")
        rejected = self.node_command({"op": "not_a_node_operation"})
        self.assertEqual(rejected.returncode, 1)
        self.assertEqual(json.loads(rejected.stdout)["kind"], "rejected")
        # Lifecycle calls cannot touch the user's services: this is a separate,
        # non-installed configuration under a temporary HOME.
        with self.assertRaises(ValueError):
            self.bridge.request("setAccess", {"enabled": False})
        self.assertIsNone(self.agent.poll())

    def test_native_file_jobs_inbox_browse_and_recursive_download(self):
        receiving = self.root / "receiving"
        receiving.mkdir()
        self.mutate({"op": "file_inbox", "id": "incoming", "root": str(receiving), "allow": []})
        inbox = self.service("incoming")
        content = os.urandom(64 * 1024 + 137)
        source = self.root / "dataset $(literal); example.bin"
        source.write_bytes(content)
        sent = self.wait_transfer(self.bridge.request("fileSend", {
            "host": self.own, "service": "incoming", "incarnation": inbox["incarnation"],
            "local_path": str(source), "remote_path": "nested/data.bin",
        }))
        self.assertEqual(sent["verified_bytes"], len(content))
        self.assertEqual(sent["file"]["sha256"], hashlib.sha256(content).hexdigest())
        self.assertEqual((receiving / "nested/data.bin").read_bytes(), content)
        self.mutate({"op": "file_share", "id": "models", "root": str(receiving), "allow": []})
        shared = self.service("models")
        target = {"host": self.own, "service": "models", "incarnation": shared["incarnation"]}
        listing = self.bridge.request("shareList", target)
        self.assertIn("nested", json.dumps(listing))
        destination = self.root / "downloaded folder"
        downloaded = self.wait_transfer(self.bridge.request("shareGet", {
            **target, "path": "", "local_path": str(destination), "recursive": True,
        }))
        self.assertEqual(downloaded["verified_bytes"], len(content))
        self.assertEqual((destination / "nested/data.bin").read_bytes(), content)
        with self.assertRaises(ValueError):
            self.bridge.request("shareList", {**target, "incarnation": "e" * 64})
        with self.assertRaises(ValueError):
            self.bridge.request("shareList", {**target, "path": "../outside"})
        symlink = self.root / "source-link"
        symlink.symlink_to(source)
        with self.assertRaises(ValueError):
            self.bridge.request("fileSend", {
                "host": self.own, "service": "incoming", "incarnation": inbox["incarnation"],
                "local_path": str(symlink), "remote_path": "link.bin",
            })
        self.assertFalse((receiving / "link.bin").exists())

    def test_malformed_and_oversized_stdio_fail_with_bounded_native_processes(self):
        for payload in ("{malformed json\n", "x" * 65537 + "\n"):
            with self.subTest(size=len(payload)):
                outcome = subprocess.run(
                    [BINARY, "desktop-bridge", "--config", self.config],
                    input=payload, text=True, capture_output=True,
                    timeout=10, env=self.environment,
                )
                self.assertEqual(outcome.returncode, 1)
                self.assertEqual(outcome.stdout, "")
                self.assertIn("desktop request", outcome.stderr.lower())
        self.assertEqual(self.bridge.request("ownerStatus")["host"]["host"], self.own)


if __name__ == "__main__":
    unittest.main()

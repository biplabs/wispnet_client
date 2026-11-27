#!/usr/bin/env python3
"""Real CLI/PTY shell checks over two isolated Wispnet nodes.

Uses private temporary identities, policies, HOME directories, and loopback
discovery hints. No installed node, user grants, or shell profiles are touched.
"""

from contextlib import closing
import errno
import fcntl
import http.client
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import socket
import sqlite3
import struct
import subprocess
import tempfile
import termios
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/wispnet"
spec = importlib.util.spec_from_file_location(
    "shell_node_api", ROOT / "apps/node/desktop/wispnet_desktop/api.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Terminal:
    """Run the real CLI with a controlling terminal and retain byte output."""

    def __init__(self, command, environment, initial_size=(80, 24)):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(BINARY, [str(arg) for arg in command], environment)
        self.original = termios.tcgetattr(self.fd)
        self.output = bytearray()
        self.exited = False
        self.resize(*initial_size)

    def resize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def read(self, seconds=0.05):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if not select.select([self.fd], [], [], max(0, deadline - time.monotonic()))[0]:
                return
            try:
                data = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            if not data:
                return
            self.output.extend(data)

    def expect(self, needle, after=0, timeout=12):
        needle = needle.encode() if isinstance(needle, str) else needle
        deadline = time.monotonic() + timeout
        while needle not in self.output[after:]:
            self.read()
            if time.monotonic() >= deadline:
                raise AssertionError(f"Terminal never showed {needle!r}: {bytes(self.output)!r}")

    def send(self, data):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)

    def wait(self, expected, timeout=12):
        deadline = time.monotonic() + timeout
        while True:
            self.read()
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.exited = True
                code = os.waitstatus_to_exitcode(status)
                if code != expected:
                    raise AssertionError(f"Shell CLI exited {code}, expected {expected}: {bytes(self.output)!r}")
                flags = termios.ICANON | termios.ECHO
                if termios.tcgetattr(self.fd)[3] & flags != self.original[3] & flags:
                    raise AssertionError("Shell CLI did not restore terminal settings")
                return
            if time.monotonic() >= deadline:
                raise AssertionError(f"Shell CLI did not exit: {bytes(self.output)!r}")

    def close(self):
        if not self.exited:
            os.kill(self.pid, signal.SIGTERM)
            try:
                self.wait(0, timeout=4)
            except AssertionError:
                if not self.exited:
                    os.kill(self.pid, signal.SIGKILL)
                    os.waitpid(self.pid, 0)
                    self.exited = True
        os.close(self.fd)


class ShellSmoke(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wispnet-shell-")
        self.root = Path(self.temporary.name)
        home = self.root / "home"
        home.mkdir(mode=0o700)
        self.environment = dict(os.environ, HOME=str(home), SHELL="/bin/sh", ENV="",
                                TERM="xterm-256color", NO_COLOR="1")
        self.processes = []
        self.terminals = []

    def tearDown(self):
        try:
            for terminal in self.terminals:
                terminal.close()
        finally:
            for process in self.processes:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            self.temporary.cleanup()

    def cli(self, config, *args, input=None, success=True):
        result = subprocess.run([BINARY, *args, "--config", config], input=input,
                                capture_output=True, text=True, timeout=20,
                                env=self.environment)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def host(self, name):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            udp.bind(("127.0.0.1", 0))
            port = udp.getsockname()[1]
        config, state = self.root / (name + ".toml"), self.root / name
        config.write_text(
            f'schema_version=2\ndevice_name="{name}"\nstate_dir="{state}"\nlog_level="info"\n'
            f'[transport]\nbind_addr="127.0.0.1:{port}"\nrelay_url="https://relay.example"\n'
        )
        # The low-level management parser requires --config immediately after op.
        initialized = subprocess.run([BINARY, "host-init", "--config", config],
                                     capture_output=True, text=True, check=True,
                                     env=self.environment, timeout=15)
        peer = initialized.stdout.split("Endpoint ID: ")[1].strip()
        daemon = subprocess.Popen([BINARY, "agent", "--config", config],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  env=self.environment)
        self.processes.append(daemon)
        deadline = time.monotonic() + 12
        api = module.NodeAPI(config)
        while True:
            self.assertIsNone(daemon.poll(), "Fixture node exited before its API was ready")
            try:
                api.request(op="status")
                break
            except (FileNotFoundError, ConnectionRefusedError):
                self.assertLess(time.monotonic(), deadline, "Fixture node did not start")
                time.sleep(0.03)
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as registry:
            registry.bind(("127.0.0.1", 0))
            registry_port = registry.getsockname()[1]
        api.request(op="configure_access", settings=dict(
            registry_port=registry_port, allowed_origins=[], service_ports=[]))
        api.request(op="enable")
        return config, api, peer, port

    def mutate(self, api, **operation):
        operation["expected_revision"] = api.request(op="status")["host"]["revision"]
        return api.request(op="mutate", operation=operation)

    def mcp(self, api, name, **arguments):
        port = api.request(op="gateway_settings")["settings"]["registry_port"]
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
        try:
            connection.request("POST", "/mcp", json.dumps({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }), headers={"Host": f"wispnet.localhost:{port}",
                         "Content-Type": "application/json",
                         "Accept": "application/json, text/event-stream"})
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            result = json.loads(response.read())
        finally:
            connection.close()
        self.assertNotIn("error", result)
        self.assertFalse(result["result"]["isError"], result)
        return result["result"]["structuredContent"]

    def terminal(self, config, host, incarnation, session="main"):
        terminal = Terminal([BINARY, "shell", "--host", host, "--service", "shell",
                             "--incarnation", incarnation, "--session", session,
                             "--config", config], self.environment)
        self.terminals.append(terminal)
        terminal.expect("Ctrl+] detaches")
        return terminal

    def wait_for_shell_exit(self, pid):
        deadline = time.monotonic() + 5
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            self.assertLess(time.monotonic(), deadline, "Revoked detached shell survived")
            time.sleep(0.03)

    def test_bulk_input_and_output_make_progress_together(self):
        config, _, _, _ = self.host("shell-bulk")
        # More than the host's 1 MiB history budget: draining stdin before
        # polling output deadlocks or overruns, even though each RPC is bounded.
        # The expected marker is absent from the echoed command itself.
        count = 50_000
        payload = "printf '%s%s\\n' REVIEW_ PASTE\n" * count + "exit 0\n"
        result = self.cli(config, "shell", "localhost", "--session", "bulk", input=payload)
        self.assertEqual(result.stdout.count("REVIEW_PASTE"), count)

    def test_zero_initial_and_transient_terminal_sizes_keep_shell_usable(self):
        config, _, _, _ = self.host("shell-size")
        # Desktop terminal emulators can launch a child before assigning its
        # PTY dimensions, or briefly report zero during a window transition.
        terminal = Terminal([BINARY, "shell", "localhost", "--session", "size",
                             "--config", config], self.environment, initial_size=(0, 0))
        self.terminals.append(terminal)
        terminal.expect("Ctrl+] detaches")
        terminal.send("printf 'WISP_INITIAL_'; stty size\r")
        terminal.expect("WISP_INITIAL_24 80")

        terminal.resize(93, 31)
        time.sleep(0.15)  # Allow the CLI's resize polling loop to transmit it.
        terminal.send("printf 'WISP_RESIZED_'; stty size\r")
        terminal.expect("WISP_RESIZED_31 93")

        terminal.resize(0, 0)
        time.sleep(0.15)
        terminal.send("printf 'WISP_TRANSIENT_'; stty size; exit 0\r")
        terminal.expect("WISP_TRANSIENT_31 93")
        terminal.wait(0)  # Also verifies the controlling TTY modes are restored.

    def test_permissions_persistent_session_resize_exit_and_revocation(self):
        client, api, client_id, _ = self.host("shell-client")
        target, target_api, target_id, target_port = self.host("shell-target")
        builtin = next(s for s in target_api.request(op="status")["host"]["services"] if s["id"] == "shell")
        self.assertEqual(builtin["kind"], "shell")
        self.assertEqual(builtin["allow"], [])
        for operation in [dict(op="service", id="shell", kind="http", backend="127.0.0.1:8000", allow=[]),
                          dict(op="remove", id="shell")]:
            with self.assertRaises(ValueError):
                self.mutate(target_api, **operation)
        invitation_path = self.root / "invitation.json"
        subprocess.run([BINARY, "host-invite", "--config", target, "--output", invitation_path],
                       capture_output=True, check=True, env=self.environment, timeout=15)
        invitation = json.loads(invitation_path.read_text())
        invitation["direct_hint"] = f"127.0.0.1:{target_port}"
        api.request(op="pair", host=target_id, invitation=json.dumps(invitation))
        self.mutate(target_api, op="approve_invitation", invitation=invitation["invitation"])

        # Pairing alone must not enable remote command execution.
        self.cli(client, "shell", "--host", target_id, success=False)
        self.assertEqual(api.request(op="catalog", host=target_id)["services"], [])
        self.cli(client, "shell", "--host", target_id, success=False)
        self.mutate(target_api, op="shell", allow=[client_id])
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual(catalog["services"][0]["kind"], "shell")
        incarnation = catalog["services"][0]["incarnation"]
        failure = self.cli(client, "shell", "--host", target_id, "--incarnation", "f" * 64,
                           success=False)
        self.assertIn("changed", failure.stderr)

        terminal = self.terminal(client, target_id, incarnation)
        terminal.send("WISP_SMOKE_VALUE=kept; printf '%s%s\\n' WISP_ READY\r")
        terminal.expect("WISP_READY")
        terminal.resize(93, 31)
        time.sleep(0.15)  # Let the CLI observe and transmit the changed dimensions.
        terminal.send("printf 'WISP_SIZE_'; stty size\r")
        terminal.expect("WISP_SIZE_31 93")
        terminal.send(b"\x1d")
        terminal.wait(0)

        resumed = self.terminal(client, target_id, incarnation)
        resumed.send("printf 'WISP_RESUME_%s\\n' \"$WISP_SMOKE_VALUE\"; exit 7\r")
        resumed.expect("WISP_RESUME_kept")
        resumed.wait(7)

        # Owner-local desktop/CLI access needs no self-grant or second identity.
        local = self.cli(target, "shell", "localhost", "--session", "owner",
                         input="printf '%s%s\\n' WISP_ LOCAL; exit 0\n")
        self.assertIn("WISP_LOCAL", local.stdout)

        # Service-specific MCP revocation must leave pairing and other service
        # grants intact while terminating attached and detached remote PTYs.
        self.mutate(target_api, op="service", id="other", kind="http",
                    backend="127.0.0.1:18888", allow=[client_id])
        detached_pid = self.root / "detached-shell.pid"
        detached = self.terminal(client, target_id, incarnation, session="detached")
        detached.send("WISP_SMOKE_REVOKED=stale; printf '%s\\n' \"$$\" > "
                      + shlex.quote(str(detached_pid))
                      + "; printf '%s%s\\n' WISP_ DETACHED\r")
        detached.expect("WISP_DETACHED")
        detached.send(b"\x1d")
        detached.wait(0)
        pid = int(detached_pid.read_text().strip())

        active = self.terminal(client, target_id, incarnation, session="revoked")
        active.send("printf '%s%s\\n' WISP_ ACTIVE\r")
        active.expect("WISP_ACTIVE")
        # Hold the authority DB past several watcher ticks and its SQLite busy timeout.
        with closing(sqlite3.connect(self.root / "shell-target" / "host.sqlite3")) as store:
            store.execute("BEGIN EXCLUSIVE")
            time.sleep(3.5)
            active.send("printf '%s%s\\n' WISP_ DURING_STORE_LOCK\r")
            active.expect("WISP_DURING_STORE_LOCK")
            os.kill(pid, 0)
            store.rollback()

        before = target_api.request(op="status")["host"]
        self.mcp(target_api, "wispnet_revoke_service_access",
                 expected_revision=before["revision"], service="shell", peer=client_id)
        active.wait(1)
        self.cli(client, "shell", "--host", target_id, success=False)
        after = target_api.request(op="status")["host"]
        self.assertEqual(after["clients"], before["clients"])
        self.assertEqual(after["generation"], before["generation"])
        for service in before["services"]:
            current = next(s for s in after["services"] if s["id"] == service["id"])
            self.assertEqual(current, dict(service, allow=[]) if service["id"] == "shell" else service)
        self.assertEqual([s["id"] for s in api.request(op="catalog", host=target_id)["services"]], ["other"])
        self.wait_for_shell_exit(pid)

        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        fresh = self.terminal(client, target_id, incarnation, session="detached")
        fresh.send("printf '%s\\n' \"$$\" > " + shlex.quote(str(detached_pid))
                   + "; printf 'WISP_REVOKED_STATE_%s\\n' \"${WISP_SMOKE_REVOKED-unset}\"\r")
        fresh.expect("WISP_REVOKED_STATE_unset")
        fresh.send(b"\x1d")
        fresh.wait(0)

        # A detached PTY must be swept even when no attached Shell stream can close its scope.
        self.mutate(target_api, op="revoke_service", service="shell", peer=client_id)
        self.wait_for_shell_exit(int(detached_pid.read_text().strip()))
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)

        # Forgetting a host locally must also close an already-open outgoing
        # terminal, without relying on the remote owner learning the removal.
        removed = self.terminal(client, target_id, incarnation, session="unpaired")
        removed.send("printf '%s%s\\n' WISP_ UNPAIR_READY\r")
        removed.expect("WISP_UNPAIR_READY")
        api.request(op="unpair", host=target_id)
        self.assertNotIn(target_id, api.request(op="status")["peer_pins"])
        self.assertTrue(any(c["peer"] == client_id for c in target_api.request(op="status")["host"]["clients"]))
        removed.wait(1)
        self.cli(client, "shell", "--host", target_id, success=False)


if __name__ == "__main__":
    unittest.main()

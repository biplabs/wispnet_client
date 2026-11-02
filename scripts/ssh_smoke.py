#!/usr/bin/env python3
"""Real OpenSSH clients over isolated Wispnet identities and Shell grants.

This uses the same private two-node fixture as shell_smoke.py. It never reads or
writes the user's SSH configuration, known_hosts, or installed Wispnet state.
"""

import json
import os
from pathlib import Path
import pty
import shlex
import shutil
import subprocess
import time
import unittest

import shell_smoke as fixture


SSH = shutil.which("ssh")
BINARY = fixture.BINARY


class SSHTerminal(fixture.Terminal):
    def __init__(self, command, environment, initial_size=(80, 24)):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                os.execve(SSH, [str(arg) for arg in command], environment)
            finally:
                # An exec error must end this child rather than returning to
                # unittest with a second copy of the fixture's cleanup state.
                os._exit(127)
        self.original = fixture.termios.tcgetattr(self.fd)
        self.output = bytearray()
        self.exited = False
        self.resize(*initial_size)


class SSHSmoke(unittest.TestCase):
    # Reuse setup and node management without discovering ShellSmoke's tests.
    setUp = fixture.ShellSmoke.setUp
    host = fixture.ShellSmoke.host
    mutate = fixture.ShellSmoke.mutate

    def tearDown(self):
        errors = []
        try:
            for terminal in self.terminals:
                try:
                    terminal.close()
                except Exception as error:
                    errors.append(error)
            # Stop SSH clients before their fixture nodes, even after an
            # assertion/timeout, and drain/close every captured pipe.
            for process in reversed(self.processes):
                try:
                    process.terminate()
                    try:
                        process.communicate(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate(timeout=5)
                except Exception as error:
                    errors.append(error)
                finally:
                    for stream in (process.stdin, process.stdout, process.stderr):
                        if stream is not None:
                            stream.close()
        finally:
            self.temporary.cleanup()
        if errors:
            raise errors[0]

    def pair(self, client_api, target_config, target_api, target_id, target_port):
        invitation_path = self.root / "ssh-invitation.json"
        subprocess.run(
            [BINARY, "host-invite", "--config", target_config,
             "--output", invitation_path],
            capture_output=True, check=True, env=self.environment, timeout=15,
        )
        invitation = json.loads(invitation_path.read_text())
        invitation["direct_hint"] = f"127.0.0.1:{target_port}"
        client_api.request(op="pair", host=target_id, invitation=json.dumps(invitation))
        self.mutate(target_api, op="approve_invitation", invitation=invitation["invitation"])

    def ssh_arguments(self, config, peer, options=()):
        self.assertIsNotNone(SSH, "OpenSSH's ssh executable is required for ssh_smoke.py")
        proxy = shlex.join([
            str(BINARY), "ssh-bridge", "--host", peer,
            "--config", str(config), "--stdio",
        ])
        return [
            SSH, "-F", "/dev/null", *options, "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10", "-o", "ConnectionAttempts=1",
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", f"UserKnownHostsFile={self.root / 'known_hosts'}",
            "-o", "GlobalKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR",
            "-o", f"HostKeyAlias=wispnet-{peer}", "-o", f"ProxyCommand={proxy}",
            "shell-fixture.localhost",
        ]

    def ssh(self, config, peer, command, *, input=b"", options=()):
        return subprocess.run(
            [*self.ssh_arguments(config, peer, ("-T", *options)), command],
            input=input, capture_output=True, env=self.environment, timeout=30,
        )

    def fixture_pair(self):
        config, client_api, client_id, _ = self.host("ssh-client")
        target, target_api, peer, port = self.host("ssh-target")
        self.pair(client_api, target, target_api, peer, port)
        return config, client_api, client_id, target_api, peer

    def wait_pid(self, path):
        deadline = time.monotonic() + 8
        while True:
            try:
                return int(path.read_text())
            except (FileNotFoundError, ValueError):
                self.assertLess(time.monotonic(), deadline, "SSH command did not start")
                time.sleep(.03)

    def assert_stopped(self, pid, marker, cancelled_at):
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            self.assertLess(time.monotonic() - cancelled_at, 4,
                            "Closing an SSH channel left its remote shell running")
            time.sleep(.03)
        # The child would create this file after two seconds if it survived.
        time.sleep(max(0, cancelled_at + 2.3 - time.monotonic()))
        self.assertFalse(marker.exists(), "Closing SSH left a remote child running")

    def test_permissions_binary_streams_exit_and_persistent_host_key(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        forbidden = self.root / "forbidden"
        denied = self.ssh(config, peer, "printf forbidden > " + shlex.quote(str(forbidden)))
        self.assertNotEqual(denied.returncode, 0, denied.stderr)
        self.assertFalse(forbidden.exists(), "Pairing alone granted SSH shell execution")
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)

        result = self.ssh(config, peer,
                          "printf 'ssh stdout\\n'; printf 'ssh stderr\\n' >&2; exit 7")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(result.stdout, b"ssh stdout\n")
        self.assertEqual(result.stderr, b"ssh stderr\n")

        # Concurrent input/output with every byte value, followed by true stdin
        # EOF; this also exceeds the existing one-shot Exec's 1 MiB ceiling.
        payload = bytes(range(256)) * 8192
        result = self.ssh(config, peer, "cat", input=payload)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, payload)
        self.assertEqual(result.stderr, b"")

        result = self.ssh(config, peer,
                          "dd if=/dev/zero bs=65536 count=20 2>/dev/null")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, bytes(65536 * 20))

        # Repeated adapters use the same per-peer key with strict checking.
        result = self.ssh(config, peer, "printf SSH_KEY_REUSED",
                          options=("-o", "StrictHostKeyChecking=yes"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"SSH_KEY_REUSED")
        keys = (self.root / "known_hosts").read_text().splitlines()
        self.assertEqual(len(keys), 1, "Repeated connections changed the adapter host key")

        # The user-facing alias works from the generated entry and stays pinned
        # to the public peer ID instead of performing mutable name lookup.
        generated = subprocess.run(
            [BINARY, "ssh-bridge", "--host", "ssh-target", "--config", config,
             "--print-config"],
            capture_output=True, text=True, env=self.environment, timeout=15,
        )
        self.assertEqual(generated.returncode, 0, generated.stderr)
        self.assertIn(peer, generated.stdout)
        alias = next(line.split()[1] for line in generated.stdout.splitlines()
                     if line.strip().startswith("Host "))
        self.assertEqual(alias, "shell-ssh-target.localhost")
        generated_path = self.root / "ssh_config"
        generated_path.write_text(generated.stdout)
        result = subprocess.run(
            [SSH, "-F", generated_path, "-o", "BatchMode=yes", "-o", "LogLevel=ERROR",
             "-o", "StrictHostKeyChecking=accept-new",
             "-o", f"UserKnownHostsFile={self.root / 'generated_known_hosts'}",
             "-o", "GlobalKnownHostsFile=/dev/null", alias, "printf SSH_CONFIG_ALIAS"],
            input=b"", capture_output=True, env=self.environment, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"SSH_CONFIG_ALIAS")
        self.assertFalse((Path(self.environment["HOME"]) / ".ssh/config").exists(),
                         "Printing an SSH entry modified the user's SSH config")

    def test_multiple_channels_share_an_openssh_master(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        control = self.root / "ssh-control"
        master = subprocess.Popen(
            self.ssh_arguments(config, peer, ("-M", "-N", "-S", str(control))),
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, env=self.environment,
        )
        self.processes.append(master)
        deadline = time.monotonic() + 10
        while not control.exists():
            self.assertIsNone(master.poll(), "OpenSSH multiplexing master exited")
            self.assertLess(time.monotonic(), deadline, "OpenSSH control socket not ready")
            time.sleep(.03)
        for index in range(3):
            result = self.ssh(config, peer, f"printf channel-{index}",
                              options=("-S", str(control)))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, f"channel-{index}".encode())
        closed = subprocess.run(
            self.ssh_arguments(config, peer, ("-S", str(control), "-O", "exit")),
            capture_output=True, env=self.environment, timeout=5,
        )
        self.assertEqual(closed.returncode, 0, closed.stderr)
        master.wait(timeout=5)

    def test_interactive_pty_resize_exit_and_cleanup(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        terminal = SSHTerminal(self.ssh_arguments(config, peer, ("-tt",)), self.environment)
        self.terminals.append(terminal)
        terminal.send("printf 'SSH_INITIAL_'; stty size\r")
        terminal.expect("SSH_INITIAL_24 80")
        terminal.resize(93, 31)
        time.sleep(.15)
        terminal.send("printf 'SSH_RESIZED_'; stty size; exit 7\r")
        terminal.expect("SSH_RESIZED_31 93")
        terminal.wait(7)

    def test_revocation_and_disconnect_stop_remote_commands(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        for reason in ("disconnect", "revocation"):
            pidfile = self.root / (reason + ".pid")
            marker = self.root / (reason + ".survived")
            command = ("printf '%s\\n' \"$$\" > " + shlex.quote(str(pidfile))
                       + "; (sleep 2; printf leaked > " + shlex.quote(str(marker)) + ") & wait")
            process = subprocess.Popen(
                [*self.ssh_arguments(config, peer, ("-T",)), command],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=self.environment,
            )
            self.processes.append(process)
            pid = self.wait_pid(pidfile)
            cancelled_at = time.monotonic()
            if reason == "disconnect":
                process.terminate()
            else:
                self.mutate(target_api, op="shell", allow=[])
            process.communicate(timeout=8)
            self.assert_stopped(pid, marker, cancelled_at)
        self.assertNotEqual(self.ssh(config, peer, "true").returncode, 0)

    def test_disconnect_cancels_a_command_with_blocked_stdin(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        pidfile = self.root / "blocked-stdin.pid"
        marker = self.root / "blocked-stdin.survived"
        command = ("printf '%s\\n' \"$$\" > " + shlex.quote(str(pidfile))
                   + "; (sleep 2; printf leaked > " + shlex.quote(str(marker))
                   + ") & sleep 30; wait")
        # A regular input file lets OpenSSH fill every transport buffer without
        # introducing a producer thread that could itself hide a cleanup hang.
        # The remote command deliberately never reads this input.
        payload_size = 192 * 1024 * 1024
        with (self.root / "blocked-stdin.input").open("w+b") as payload:
            payload.truncate(payload_size)
            payload.seek(0)
            process = subprocess.Popen(
                [*self.ssh_arguments(config, peer, ("-T",)), command],
                stdin=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=self.environment,
            )
            self.processes.append(process)
            pid = self.wait_pid(pidfile)
            deadline = time.monotonic() + 4
            # The child shares this file description, so tell() observes how
            # much input ssh has actually read rather than merely queued here.
            while payload.tell() < 128 * 1024:
                self.assertIsNone(process.poll(), "SSH exited before filling remote stdin")
                self.assertLess(time.monotonic(), deadline, "SSH made no stdin progress")
                time.sleep(.03)
            time.sleep(.2)
            self.assertIsNone(process.poll(), "SSH exited before testing blocked-input cancellation")
            cancelled_at = time.monotonic()
            process.terminate()
            process.communicate(timeout=4)
            # Cancellation must beat the native ten-second stalled-input
            # timeout, including while its child stdin write is blocked.
            self.assert_stopped(pid, marker, cancelled_at)

    def test_full_duplex_command_drains_output_before_reading_stdin(self):
        config, _, client_id, target_api, peer = self.fixture_pair()
        self.mutate(target_api, op="grant_service", service="shell", peer=client_id)
        # Sparse input exceeds the native transport's 64 MiB receive window
        # without allocating a matching Python buffer. The remote process lets
        # stdin back up, emits more than an SSH output window, then reads stdin.
        # Serializing either direction deadlocks before wc can count the input.
        payload_size = 192 * 1024 * 1024
        with (self.root / "duplex.input").open("w+b") as payload:
            payload.truncate(payload_size)
            payload.seek(0)
            result = subprocess.run(
                [*self.ssh_arguments(config, peer, ("-T",)),
                 "sleep 0.3; dd if=/dev/zero bs=65536 count=128 2>/dev/null; wc -c"],
                stdin=payload, capture_output=True, env=self.environment, timeout=30,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, b"")
        prefix_size = 8 * 1024 * 1024
        self.assertEqual(result.stdout[:prefix_size], bytes(prefix_size))
        # BSD wc pads the count; the payload and its byte count stay exact.
        self.assertEqual(result.stdout[prefix_size:].strip(), str(payload_size).encode())


if __name__ == "__main__":
    unittest.main(verbosity=2)

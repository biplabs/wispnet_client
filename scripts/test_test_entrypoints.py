"""Keep hardware selection explicit and out of the public test gate."""

import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import test_android_emulator as emulator
import test_hardware as hardware
import test_physical_service_recreation as installed


class EntrypointTests(unittest.TestCase):
    def test_public_gate_rejects_legacy_hardware_switches_before_running_tools(self):
        root = Path(__file__).resolve().parents[1]
        switches = {
            'WISPNET_ANDROID_RELAY_TEST': '1',
            'WISPNET_SSH_RELAY_HOST': 'fixture-host',
            'WISPNET_ANDROID_LAN_ADDRESS': '192.0.2.5',
            'WISPNET_ANDROID_LAN_PORT': '60303',
            'WISPNET_ANDROID_WIFI_CYCLE': '1',
        }
        environment = {key: value for key, value in os.environ.items() if key not in switches}
        for name, value in switches.items():
            with self.subTest(name=name):
                result = subprocess.run(['sh', str(root / 'scripts/test.sh')], cwd=root,
                                        env={**environment, name: value}, capture_output=True,
                                        text=True, timeout=5)
                self.assertEqual(result.returncode, 1)
                self.assertIn('scripts/test_hardware.py --serial', result.stderr)

    def test_hardware_requires_explicit_serial_before_building(self):
        with patch.object(sys, 'argv', ['test_hardware.py']), patch.object(subprocess, 'run') as run:
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                hardware.main()
            run.assert_not_called()

    def test_selected_device_and_host_reach_both_disposable_harnesses(self):
        with patch.object(sys, 'argv', ['test_hardware.py', '--serial', 'selected-device',
                                      '--ssh-host', 'selected-host']), patch.object(subprocess, 'run') as run:
            hardware.main()
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(commands[-2][1:], ['scripts/relay_smoke.py', '--android', '--serial', 'selected-device'])
        self.assertEqual(commands[-1][1:], ['scripts/physical_relay_smoke.py', '--ssh-host',
                                          'selected-host', '--serial', 'selected-device'])

    def test_emulator_refuses_a_physical_device_before_build_or_reset(self):
        with patch.object(sys, 'argv', ['test_android_emulator.py', '--serial', 'phone', '--output', '/tmp/check']), \
                patch.object(subprocess, 'check_output', return_value='0\n'), \
                patch.object(subprocess, 'run') as run:
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                emulator.main()
            run.assert_not_called()

    def test_emulator_builds_x86_and_runs_all_three_fixtures(self):
        with patch.object(sys, 'argv', ['test_android_emulator.py', '--serial', 'avd', '--output', '/tmp/check']), \
                patch.object(subprocess, 'check_output', side_effect=['1\n', 'x86_64\n']), \
                patch.object(subprocess, 'run') as run:
            emulator.main()
        self.assertEqual(run.call_args_list[2].kwargs['env']['WISPNET_ANDROID_ABI'], 'x86_64')
        for call, name in zip(run.call_args_list[-3:], ['files', 'inboxes', 'shares']):
            self.assertIn(f'scripts/test_android_{name}.py', call.args[0])
            self.assertIn('avd', call.args[0])

    def test_installed_acceptance_has_no_implicit_host(self):
        with patch.object(subprocess, 'run') as run:
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                installed.configure([])
            run.assert_not_called()

    def test_installed_paths_are_passed_as_arguments_and_remote_shell_quoted(self):
        self.addCleanup(installed.SSH.clear)
        self.addCleanup(installed.NODE.clear)
        installed.configure(['--ssh-host', 'fixture', '--local-node', '/local node',
                             '--remote-node', '/remote node'])
        with patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='{}')) as run:
            installed.request(False, {'op': 'status'})
            self.assertEqual(run.call_args.args[0], ['/local node'])
            installed.request(True, {'op': 'status'})
            self.assertEqual(run.call_args.args[0][-2:], ['fixture', "'/remote node'"])


if __name__ == '__main__':
    unittest.main()

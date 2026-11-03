import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch, Mock

import uninstall_firewall_helper as firewall
import uninstall_node_desktop as node


class FirewallUninstall(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for name in ('CONFIG', 'BINARY', 'UNIT'):
            path = self.root / name
            path.write_text('installed')
            context = patch.object(firewall, name, path)
            context.start()
            self.addCleanup(context.stop)

    def test_last_user_removes_only_managed_rules_and_files_and_is_repeatable(self):
        backend = Mock()
        with patch.object(firewall, 'configured_uids', return_value={1000}), \
             patch.object(firewall, 'Firewall', return_value=backend), \
             patch.object(firewall.subprocess, 'run', return_value=subprocess.CompletedProcess([], 3)) as run:
            firewall.uninstall(1000)
            firewall.uninstall(1000)
        backend.reconcile.assert_called_with(set(), cleanup=True)
        self.assertFalse(any(self.root.iterdir()))
        self.assertEqual(sum('disable' in c.args[0] for c in run.call_args_list), 1)

    def test_other_users_keep_the_helper_and_their_authorization(self):
        with patch.object(firewall, 'configured_uids', return_value={1000, 1001}), \
             patch.object(firewall, 'write_root', side_effect=lambda p,s,m:p.write_text(s)), \
             patch.object(firewall.subprocess, 'run') as run:
            firewall.uninstall(1000)
        self.assertEqual(json.loads(firewall.CONFIG.read_text())['uids'], [1001])
        self.assertTrue(firewall.BINARY.exists())
        self.assertEqual(run.call_args.args[0][1], 'restart')

    def test_failed_firewall_cleanup_preserves_installed_files(self):
        with patch.object(firewall, 'configured_uids', return_value={1000}), \
             patch.object(firewall, 'Firewall') as backend, \
             patch.object(firewall.subprocess, 'run', return_value=subprocess.CompletedProcess([], 3)):
            backend.return_value.reconcile.side_effect = RuntimeError('Foreign rule')
            with self.assertRaises(RuntimeError):
                firewall.uninstall(1000)
        self.assertTrue(firewall.CONFIG.exists())
        self.assertTrue(firewall.UNIT.exists())


class UserUninstall(unittest.TestCase):
    def test_ssh_cleanup_preserves_unrelated_entries_and_is_repeatable(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            directory = home / '.ssh'
            directory.mkdir(mode=0o700)
            path = directory / 'config'
            unrelated = 'Host personal\n HostName personal.example\n'
            block = ('# Wispnet bridge ' + 'a' * 64 + ' begin\n'
                     'Host shell-desktop.localhost\n ProxyCommand wispnet ssh-bridge --stdio\n'
                     'Host *\n# Wispnet bridge ' + 'a' * 64 + ' end\n')
            path.write_text(block + unrelated)
            known = directory / 'known_hosts'
            known.write_text('preserve trusted hosts')
            node.remove_ssh_aliases(home)
            node.remove_ssh_aliases(home)
            self.assertEqual(path.read_text(), unrelated)
            self.assertEqual(known.read_text(), 'preserve trusted hosts')

    def test_ssh_cleanup_refuses_incomplete_blocks_and_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            (home / '.ssh').mkdir(mode=0o700)
            path = home / '.ssh/config'
            text = '# Wispnet bridge ' + 'a' * 64 + ' begin\nHost personal\n'
            path.write_text(text)
            with self.assertRaises(ValueError):
                node.remove_ssh_aliases(home)
            self.assertEqual(path.read_text(), text)
            path.unlink()
            target = home / 'other'
            target.write_text('preserve')
            path.symlink_to(target)
            with self.assertRaises(OSError):
                node.remove_ssh_aliases(home)
            self.assertEqual(target.read_text(), 'preserve')

    def test_removes_programs_preserves_identity_and_unrelated_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            paths = ['.config/systemd/user/wispnet-host.service',
                     '.local/bin/wispnet', '.local/libexec/wispnet-node/wispnet',
                     '.local/share/applications/wispnet.desktop',
                     '.config/wispnet-node/host.toml', '.local/state/wipsnet-node/identity',
                     '.local/bin/unrelated']
            for name in paths:
                p = home / name
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(name)
            with patch.object(node.subprocess, 'run'):
                node.uninstall(home, 1000)
                node.uninstall(home, 1000)
            for name in paths[:4]:
                self.assertFalse((home / name).exists())
            for name in paths[4:]:
                self.assertEqual((home / name).read_text(), name)

    def test_stop_failure_leaves_programs_available_for_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            unit = home / '.config/systemd/user/wispnet-host.service'
            unit.parent.mkdir(parents=True)
            unit.write_text('unit')
            with patch.object(node.subprocess, 'run', side_effect=subprocess.CalledProcessError(1, 'systemctl')):
                with self.assertRaises(subprocess.CalledProcessError):
                    node.uninstall(home, 1000)
            self.assertTrue(unit.exists())


if __name__ == '__main__':
    unittest.main()

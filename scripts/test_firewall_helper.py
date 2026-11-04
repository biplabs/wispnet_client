import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('firewall_helper', Path(__file__).with_name('firewall_helper.py'))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)


class SocketVerification(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.uid = os.getuid()
        self.proc = self.root/'proc'
        self.runtime = self.root/'run'
        self.process = self.proc/'4242'
        for directory in [self.process/'fd', self.process/'net', self.process/'ns', self.proc/'self/ns', self.runtime/str(self.uid)]:
            directory.mkdir(parents=True, exist_ok=True)
        (self.runtime/str(self.uid)).chmod(0o700)
        (self.proc/'self/ns/net').touch()
        os.link(self.proc/'self/ns/net', self.process/'ns/net')
        (self.process/'status').write_text('Uid:\t'+'\t'.join([str(self.uid)]*4)+'\n')
        (self.process/'stat').write_text('4242 (wispnet host) '+' '.join(['S']+['0']*18+['42']))
        (self.process/'cgroup').write_text(f'0::/user.slice/user-{self.uid}.slice/user@{self.uid}.service/app.slice/wispnet-host.service\n')
        (self.process/'fd/10').symlink_to('socket:[1234]')
        (self.process/'net/udp').write_text('header\n'+f'0: 00000000:A080 00000000:0000 07 0:0 0:0 0 {self.uid} 0 1234\n')
        (self.process/'net/udp6').write_text('header\n')
        self.report = self.runtime/str(self.uid)/'wispnet-firewall.json'
        self.value = {'version':1,'pid':4242,'start_time':'42','ports':[{'family':'ipv4','port':41088}]}
        self.save()

    def save(self):
        self.report.write_text(json.dumps(self.value))
        self.report.chmod(0o600)

    def verify(self):
        return h.verified_ports(self.uid, self.proc, self.runtime)

    def test_live_service_socket_and_empty_relay_only_report(self):
        self.assertEqual(self.verify(), {(self.uid,'ipv4',41088)})
        self.value['ports'] = []
        self.save()
        self.assertEqual(self.verify(), set())

    def test_rejects_forged_port_stale_pid_wrong_unit_and_foreign_descriptor(self):
        for file, replacement in [('cgroup','0::/unrelated.service'),('stat','4242 (other) '+' '.join(['S']+['0']*19)),('status','Uid: 0 0 0 0')]:
            with self.subTest(file=file):
                old = (self.process/file).read_text()
                (self.process/file).write_text(replacement)
                with self.assertRaises(ValueError): self.verify()
                (self.process/file).write_text(old)
        self.value['ports'][0]['port'] = 443
        self.save()
        with self.assertRaises(ValueError): self.verify()
        self.value['ports'][0]['port'] = 41089
        self.save()
        with self.assertRaises(ValueError): self.verify()
        self.value['ports'][0]['port'] = 41088
        self.save()
        (self.process/'fd/10').unlink()
        with self.assertRaises(ValueError): self.verify()

    def test_rejects_symlinks_public_reports_and_other_namespaces(self):
        self.report.chmod(0o644)
        with self.assertRaises(ValueError): self.verify()
        self.report.chmod(0o600)
        self.report.rename(self.report.with_suffix('.old'))
        self.report.symlink_to(self.report.with_suffix('.old'))
        with self.assertRaises(OSError): self.verify()
        self.report.unlink()
        self.save()
        (self.process/'ns/net').unlink()
        (self.process/'ns/net').touch()
        with self.assertRaises(ValueError): self.verify()


class FakeFirewall(h.Firewall):
    def __init__(self):
        self.backends = {'ipv4':'fake','ipv6':'fake6'}
        self.chains = {f:{'INPUT':[['-j','ufw-before-input']]} for f in self.backends}

    def call(self, family, args, check=True):
        chains = self.chains[family]
        op, chain, *rest = args
        code, output = 0, ''
        if op == '-S':
            if chain not in chains: code = 1
            else: output = '\n'.join((['-N '+chain] if chain != 'INPUT' else []) + [shlex.join(['-A',chain,*r]) for r in chains[chain]])
        elif op == '-N': chains[chain] = []
        elif op == '-C': code = 0 if rest in chains[chain] else 1
        elif op == '-A': chains[chain].append(rest)
        elif op == '-I': chains[chain].insert(int(rest[0])-1,rest[1:])
        elif op == '-D': chains[chain].remove(rest)
        elif op == '-X':
            assert not chains[chain]
            del chains[chain]
        else: raise AssertionError(args)
        if check and code: raise RuntimeError('fake backend failure')
        return subprocess.CompletedProcess(args, code, output, '')


class RuleLifecycle(unittest.TestCase):
    def test_port_change_stop_and_reload_preserve_ufw_and_other_users(self):
        fw = FakeFirewall()
        first = {(1000,'ipv4',41088),(1001,'ipv4',43000),(1000,'ipv6',41088)}
        fw.reconcile(first)
        fw.reconcile(first)  # Idempotent; no duplicate jump/rules.
        self.assertEqual(len(fw.chains['ipv4'][h.CHAIN]),2)
        fw.chains['ipv4']['INPUT'].reverse()  # UFW reload reordered hooks.
        fw.reconcile({(1000,'ipv4',42000),(1001,'ipv4',43000)})
        self.assertEqual(fw.chains['ipv4']['INPUT'][0],['-j',h.CHAIN])
        self.assertIn(['-j','ufw-before-input'],fw.chains['ipv4']['INPUT'])
        self.assertEqual(fw.chains['ipv4'][h.CHAIN],[fw.rule(1001,43000),fw.rule(1000,42000)])
        self.assertEqual(fw.chains['ipv6'][h.CHAIN],[])
        fw.reconcile(set())
        self.assertEqual(fw.chains['ipv4'][h.CHAIN],[])
        fw.reconcile(set(),cleanup=True)
        self.assertNotIn(h.CHAIN,fw.chains['ipv4'])
        self.assertEqual(fw.chains['ipv4']['INPUT'],[['-j','ufw-before-input']])

    def test_foreign_rules_are_not_flushed(self):
        fw = FakeFirewall()
        fw.chains['ipv4'][h.CHAIN] = [['-j','DROP']]
        with self.assertRaises(ValueError): fw.reconcile(set())
        self.assertEqual(fw.chains['ipv4'][h.CHAIN],[['-j','DROP']])


class IsolatedKernelRules(unittest.TestCase):
    def test_real_netfilter_port_change_and_cleanup(self):
        import shutil
        unshare, iptables = shutil.which('unshare'), shutil.which('iptables')
        if not unshare or not iptables:
            self.skipTest('unshare/iptables unavailable')
        probe = subprocess.run([unshare,'--user','--map-root-user','--net','true'],capture_output=True)
        if probe.returncode:
            self.skipTest('Unprivileged network namespaces unavailable')
        code = '''
import importlib.util, socket, subprocess, sys
spec=importlib.util.spec_from_file_location('fw',sys.argv[1]);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
f=m.Firewall.__new__(m.Firewall);f.backends={'ipv4':sys.argv[2]}
subprocess.run(['ip','link','set','lo','up'],check=True)
f.call('ipv4',['-P','INPUT','DROP'])
a=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);a.bind(('127.0.0.1',41088));a.settimeout(.15)
b=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);b.bind(('127.0.0.1',41089));b.settimeout(.15)
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
f.reconcile({(1000,'ipv4',41088)})
s.sendto(b'allowed',a.getsockname());assert a.recv(100)==b'allowed'
f.reconcile({(1000,'ipv4',41089)})
s.sendto(b'stale',a.getsockname())
try:a.recv(100);raise AssertionError('stale port still allowed')
except socket.timeout:pass
s.sendto(b'changed',b.getsockname());assert b.recv(100)==b'changed'
f.reconcile(set(),cleanup=True)
assert f.call('ipv4',['-S',m.CHAIN],check=False).returncode!=0
assert m.CHAIN not in f.call('ipv4',['-S','INPUT']).stdout
'''
        result = subprocess.run([unshare,'--user','--map-root-user','--net','python3','-c',code,
                                 str(Path(__file__).with_name('firewall_helper.py')),iptables],
                                text=True,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)


class InstallerBoundaries(unittest.TestCase):
    def test_files_only_install_cannot_request_privileged_setup(self):
        result = subprocess.run([str(Path(__file__).with_name('install_node_desktop.sh')),
                                 '--no-start','--configure-firewall','--headless'],
                                capture_output=True,text=True,timeout=5)
        self.assertNotEqual(result.returncode,0)
        self.assertIn('cannot be combined with --no-start',result.stderr)


if __name__ == '__main__': unittest.main()

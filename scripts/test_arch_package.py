"""Verify AUR generation and the real makepkg payload without touching live installs."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

from prepare_aur import prepare, ROOT


class ArchPackaging(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bundle = self.root/'wispnet-linux-x86_64'
        names = ['bin/Wispnet','bin/wispnet',
                 'bin/lib/libflutter_linux_gtk.so',
                 'bin/data/flutter_assets/AssetManifest.bin','bin/icon.png',
                 'scripts/install_node_desktop.sh',
                 'LICENSE','NOTICE','vendor/iroh/LICENSE-BSD3',
                 'vendor/noq-proto/LICENSE-APACHE','vendor/noq-proto/LICENSE-MIT',
                 'apps/flutter/assets/fonts/Roboto_LICENSE.txt']
        for name in names:
            p=self.bundle/name; p.parent.mkdir(parents=True,exist_ok=True);p.write_text('test payload\n')
        (self.bundle/'LICENSE').write_bytes((ROOT/'LICENSE').read_bytes())
        (self.bundle/'NOTICE').write_bytes((ROOT/'NOTICE').read_bytes())
        for name in ['vendor/noq-proto/LICENSE-APACHE', 'vendor/noq-proto/LICENSE-MIT']:
            (self.bundle/name).write_bytes((ROOT/name).read_bytes())
        self.manifest = {'license':'Apache-2.0','platform':'linux-x86_64','desktop':'flutter','source_dirty':False,
                         'minimum_glibc':'2.39','sha256':{n:hashlib.sha256((self.bundle/n).read_bytes()).hexdigest() for n in names}}
        self.archive = self.root/'bundle.tar.gz'
        self.license = ROOT/'LICENSE'
        self.pack()

    def pack(self):
        (self.bundle/'manifest.json').write_text(json.dumps(self.manifest))
        with tarfile.open(self.archive,'w:gz') as tar:tar.add(self.bundle,arcname=self.bundle.name)

    def generate(self, **changes):
        args = dict(archive=self.archive, output=self.root/'aur', version='0.1.0',
                    url='https://example.invalid/releases/v0.1.0/wispnet.tar.gz',
                    license_id='Apache-2.0',license_file=self.license)
        args.update(changes)
        return prepare(**args)

    def test_pinned_sources_srcinfo_and_real_makepkg_layout(self):
        if not shutil.which('makepkg') or not shutil.which('fakeroot'):
            self.skipTest('Arch makepkg/fakeroot unavailable')
        out=self.generate()
        shutil.copy(self.archive,out/'wispnet-0.1.0-linux-x86_64.tar.gz')
        subprocess.run(['makepkg','--nodeps','--nocheck','--noconfirm'],cwd=out,check=True,capture_output=True)
        package=next(out.glob('*.pkg.tar.zst'))
        names=subprocess.check_output(['bsdtar','-tf',str(package)],text=True).splitlines()
        for name in ['usr/bin/wispnet','usr/bin/wispnet-setup','usr/lib/systemd/system/wispnet-firewall.service',
                     'usr/share/libalpm/hooks/wispnet-remove.hook','opt/wispnet/bin/Wispnet']:
            self.assertIn(name,names)
        self.assertFalse(any(n.startswith(('home/','usr/local/','etc/')) for n in names))
        info=(out/'.SRCINFO').read_text()
        self.assertIn('pkgbase = wispnet-bin',info)
        self.assertIn('license = Apache-2.0',info)
        self.assertIn('usr/share/licenses/wispnet-bin/NOTICE',names)
        self.assertIn('usr/share/licenses/wispnet-bin/iroh-LICENSE-BSD3',names)
        for license_name in ['LICENSE-APACHE', 'LICENSE-MIT']:
            installed = 'usr/share/licenses/wispnet-bin/noq-proto-' + license_name
            self.assertIn(installed, names)
            self.assertEqual(
                subprocess.check_output(['bsdtar', '-xOf', str(package), installed]),
                (ROOT/'vendor/noq-proto'/license_name).read_bytes(),
            )
        self.assertEqual((out/'LICENSE').read_bytes(), (ROOT/'LICENSE').read_bytes())
        self.assertIn(hashlib.sha256(self.archive.read_bytes()).hexdigest(),info)
        self.assertNotIn('SKIP',info)
        self.assertNotIn('depends = python', info)
        self.assertFalse(any(n.endswith('.py') or '/__pycache__/' in n for n in names))
        for name, command in [
            ('usr/bin/wispnet-node', 'exec /opt/wispnet/bin/wispnet node-request'),
            ('usr/bin/wispnet-setup', 'exec /opt/wispnet/bin/wispnet package-setup'),
            ('usr/lib/systemd/system/wispnet-firewall.service',
             'ExecStart=/opt/wispnet/bin/wispnet firewall-helper'),
            ('usr/share/libalpm/hooks/wispnet-remove.hook',
             'Exec = /opt/wispnet/bin/wispnet package-setup --remove'),
        ]:
            installed = subprocess.check_output(['bsdtar', '-xOf', str(package), name], text=True)
            self.assertIn(command, installed)
            self.assertNotIn('python', installed)

    def test_rejects_corrupt_dirty_and_incomplete_releases(self):
        (self.bundle/'bin/wispnet').write_text('tampered')
        self.pack()
        with self.assertRaisesRegex(ValueError,'checksum'):self.generate()
        self.manifest['source_dirty']=True;self.pack()
        with self.assertRaisesRegex(ValueError,'dirty'):self.generate()
        self.manifest['source_dirty']=False
        self.manifest['desktop']='headless';self.pack()
        with self.assertRaisesRegex(ValueError,'desktop bundle'):self.generate()

    def test_rejects_wrong_license_metadata_and_mislabeled_license_payload(self):
        self.manifest['license'] = 'MIT'
        self.pack()
        with self.assertRaisesRegex(ValueError, 'declare Apache-2.0'):
            self.generate()
        self.manifest['license'] = 'Apache-2.0'
        (self.bundle/'LICENSE').write_text('different terms')
        self.manifest['sha256']['LICENSE'] = hashlib.sha256((self.bundle/'LICENSE').read_bytes()).hexdigest()
        self.pack()
        with self.assertRaisesRegex(ValueError, 'LICENSE does not match'):
            self.generate()

    def test_rejects_missing_vendored_transport_licenses(self):
        for name in ['vendor/noq-proto/LICENSE-APACHE', 'vendor/noq-proto/LICENSE-MIT']:
            with self.subTest(name=name):
                checksum = self.manifest['sha256'].pop(name)
                self.pack()
                with self.assertRaisesRegex(ValueError, 'Missing desktop release payload: ' + name):
                    self.generate()
                self.manifest['sha256'][name] = checksum

    def test_rejects_unpinned_urls_and_shell_injection(self):
        for changes in [dict(url='https://example.com/latest/binary.tar.gz'),dict(url='https://example.com/$(touch-x)'),
                        dict(version="1'; touch x"),dict(license_id="MIT';touch x"),dict(license_id="MIT")]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):self.generate(**changes)


if __name__=='__main__':unittest.main()

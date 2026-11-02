"""Verify AUR generation and the real makepkg payload without touching live installs."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import importlib.util
import sys

from prepare_aur import prepare, SUPPORT, ROOT


class ArchPackaging(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bundle = self.root/'wispnet-linux-x86_64'
        names = ['bin/Wispnet','bin/wispnet','bin/bridge.py','bin/desktop_data.py',
                 'bin/lib/libflutter_linux_gtk.so','bin/wispnet_desktop/api.py',
                 'bin/data/flutter_assets/AssetManifest.bin','bin/icon.png',
                 'scripts/firewall_helper.py','scripts/install_firewall_helper.py','scripts/node_request.py',
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


class PackageCleanup(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        sys.path.insert(0,str(SUPPORT.parents[2]/'scripts'))
        spec=importlib.util.spec_from_file_location('wispnet_package_system',SUPPORT/'package_system.py')
        cls.module=importlib.util.module_from_spec(spec);spec.loader.exec_module(cls.module)

    def test_cleanup_keeps_user_state_and_aborts_if_rules_cannot_be_removed(self):
        m=self.module
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'firewall.json';config.write_text('{}')
            with patch.object(m.os,'geteuid',return_value=0),patch.object(m,'CONFIG',config), \
                 patch.object(m.Path,'iterdir',return_value=iter([])),patch.object(m,'run'), \
                 patch.object(m.subprocess,'run',return_value=subprocess.CompletedProcess([],3)), \
                 patch.object(m,'Firewall') as fw:
                fw.return_value.reconcile.side_effect=RuntimeError('Foreign rule')
                with self.assertRaises(RuntimeError):m.remove()
                self.assertTrue(config.exists())
                fw.return_value.reconcile.side_effect=None
                m.remove()
                self.assertFalse(config.exists())


if __name__=='__main__':unittest.main()

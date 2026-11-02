#!/usr/bin/env python3
"""Generate a pinned wispnet-bin AUR submission from a verified desktop release."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
SUPPORT = ROOT / 'packaging/arch/wispnet-bin'
FILES = ['wispnet-host.service', 'wispnet-desktop.service', 'wispnet-firewall.service',
         'wispnet.desktop', 'wispnet-setup', 'package_system.py', 'wispnet-remove.hook']


def prepare(archive, output, version, url, license_id="Apache-2.0", license_file=None, allow_dirty=False):
    license_file = license_file or ROOT / "LICENSE"
    if not re.fullmatch(r'[0-9][A-Za-z0-9.+_]*', version):
        raise ValueError('Use an Arch-compatible version without hyphens')
    parsed = urlsplit(url)
    if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment or not re.fullmatch(r'https://[A-Za-z0-9._:/%+~-]+', url):
        raise ValueError('Use a pinned public HTTPS archive URL without credentials/query/fragment')
    if '/latest/' in parsed.path:
        raise ValueError('Pin a release URL, not latest')
    if license_id != 'Apache-2.0' or license_file.read_bytes() != (ROOT/'LICENSE').read_bytes():
        raise ValueError('Wispnet packaging requires Apache-2.0 and the repository LICENSE')
    with tempfile.TemporaryDirectory() as temporary:
        with tarfile.open(archive) as tar:
            tar.extractall(temporary, filter='data')
        bundle = Path(temporary)/'wispnet-linux-x86_64'
        manifest = json.loads((bundle/'manifest.json').read_text())
        if manifest.get('license') != 'Apache-2.0':
            raise ValueError('Release manifest must declare Apache-2.0')
        if manifest['platform'] != 'linux-x86_64' or manifest['desktop'] != 'flutter':
            raise ValueError('wispnet-bin requires the Linux x86_64 Flutter desktop bundle')
        if manifest['source_dirty'] and not allow_dirty:
            raise ValueError('Do not publish a release from a dirty checkout')
        if not re.fullmatch(r'\d+\.\d+', manifest['minimum_glibc']):
            raise ValueError('Invalid glibc version')
        for name, digest in manifest['sha256'].items():
            path = bundle/name
            if Path(name).is_absolute() or '..' in Path(name).parts or path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
                raise ValueError('Bundle checksum mismatch: '+name)
        for name in ['bin/Wispnet', 'bin/wispnet', 'bin/bridge.py', 'bin/desktop_data.py',
                     'bin/lib/libflutter_linux_gtk.so', 'bin/wispnet_desktop/api.py',
                     'bin/data/flutter_assets/AssetManifest.bin',
                     'LICENSE', 'NOTICE', 'vendor/iroh/LICENSE-BSD3',
                     'vendor/noq-proto/LICENSE-APACHE', 'vendor/noq-proto/LICENSE-MIT',
                     'apps/flutter/assets/fonts/Roboto_LICENSE.txt',
                     'scripts/firewall_helper.py', 'scripts/install_firewall_helper.py', 'scripts/node_request.py']:
            if name not in manifest['sha256']:
                raise ValueError('Missing desktop release payload: '+name)
        if (bundle/'LICENSE').read_bytes() != license_file.read_bytes():
            raise ValueError('Release LICENSE does not match Apache-2.0')
        notice = (bundle/'NOTICE').read_bytes()
    if output.exists():
        raise ValueError('Output directory already exists; use a fresh staging directory')
    output.mkdir(parents=True)
    for name in [*FILES, 'wispnet-bin.install']:
        shutil.copy2(SUPPORT/name, output/name)
    shutil.copy2(license_file, output/'LICENSE')
    (output/'NOTICE').write_bytes(notice)
    checksums = [hashlib.sha256(archive.read_bytes()).hexdigest()]
    checksums += [hashlib.sha256((output/name).read_bytes()).hexdigest() for name in [*FILES, 'LICENSE', 'NOTICE']]
    template = (SUPPORT/'PKGBUILD.in').read_text()
    for key, value in {'VERSION': version, 'URL': url,
                       'GLIBC': manifest['minimum_glibc'],
                       'CHECKSUMS': '\n            '.join(repr(x) for x in checksums)}.items():
        template = template.replace('@'+key+'@', value)
    (output/'PKGBUILD').write_text(template)
    info = subprocess.check_output(['makepkg', '--printsrcinfo'], cwd=output, text=True)
    (output/'.SRCINFO').write_text(info)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--url', required=True)
    parser.add_argument('--license-id', default='Apache-2.0', choices=['Apache-2.0'])
    parser.add_argument('--license-file', default=ROOT/'LICENSE', type=Path)
    args = parser.parse_args()
    prepare(args.archive, args.output, args.version, args.url, args.license_id, args.license_file)
    print('AUR submission staged at '+str(args.output))


if __name__ == '__main__':
    main()

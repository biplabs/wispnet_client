#!/usr/bin/env python3
"""Build and run Android Files, inbox and share integration on a disposable x86_64 emulator.

Resets the selected emulator's Wispnet app data and changes its display settings.
Never selects a device automatically. SDK, NDK, Java and an already booted AVD are required.
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    def property(name):
        return subprocess.check_output(['adb', '-s', args.serial, 'shell', 'getprop', name],
                                       text=True, timeout=15).strip()
    if property('ro.kernel.qemu') != '1' or property('ro.product.cpu.abi') != 'x86_64':
        parser.error('select a disposable x86_64 emulator; physical devices are refused')
    output = args.output.resolve()
    env = dict(os.environ, WISPNET_ANDROID_ABI='x86_64',
               WISPNET_ANDROID_PROFILE='debug', WISPNET_ANDROID_ARTIFACT='apk')
    subprocess.run(['cargo', 'build', '--workspace', '--locked'], cwd=ROOT, check=True)
    subprocess.run(['cargo', 'build', '-p', 'wispnet-cli', '--example', 'relay_fixture', '--locked'],
                   cwd=ROOT, check=True)
    subprocess.run(['bash', 'scripts/build_android.sh'], cwd=ROOT, env=env, check=True)
    subprocess.run(['./gradlew', '-Ptarget-platform=android-x64', ':app:assembleDebugAndroidTest'],
                   cwd=ROOT / 'apps/flutter/android', env=env, check=True)
    for name, extra in [('files', ['--receiving']), ('inboxes', []), ('shares', [])]:
        subprocess.run([sys.executable, f'scripts/test_android_{name}.py',
                        '--serial', args.serial, '--output', str(output / name), *extra],
                       cwd=ROOT, check=True)


if __name__ == '__main__':
    main()

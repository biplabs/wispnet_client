#!/usr/bin/env python3
"""Opt-in relay acceptance on a selected Android device and optional SSH host.

Creates disposable nodes and backends; does not use installed Wispnet identities.
Wi-Fi interruption requires --wifi-cycle and a selected LAN address.
"""

import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', required=True, help='Authorized aarch64 Android ADB serial')
    parser.add_argument('--ssh-host', help='Trusted compatible Linux SSH host for two-host acceptance')
    parser.add_argument('--ndk', type=Path)
    parser.add_argument('--lan-address')
    parser.add_argument('--lan-port', type=int, default=0)
    parser.add_argument('--wifi-cycle', action='store_true')
    args = parser.parse_args()
    if args.wifi_cycle and not args.lan_address:
        parser.error('--wifi-cycle requires --lan-address')
    if not 0 <= args.lan_port <= 65535:
        parser.error('--lan-port must be between 0 and 65535')
    if args.ssh_host and (args.ssh_host.startswith('-') or not all(
            c.isalnum() or c in '.-_@' for c in args.ssh_host)):
        parser.error('--ssh-host must be a trusted SSH alias or user@host')
    subprocess.run(['cargo', 'build', '--workspace', '--locked'], cwd=ROOT, check=True)
    subprocess.run(['cargo', 'build', '-p', 'wispnet-cli', '--example', 'relay_fixture', '--locked'],
                   cwd=ROOT, check=True)
    options = ['--serial', args.serial]
    if args.ndk:
        options += ['--ndk', str(args.ndk.resolve())]
    local = [sys.executable, 'scripts/relay_smoke.py', '--android', *options]
    if args.lan_address:
        local += ['--lan-address', args.lan_address, '--lan-port', str(args.lan_port)]
    if args.wifi_cycle:
        local += ['--wifi-cycle']
    # Android relay_smoke always rebuilds the device CLI before either harness.
    subprocess.run(local, cwd=ROOT, check=True)
    if args.ssh_host:
        subprocess.run([sys.executable, 'scripts/physical_relay_smoke.py',
                        '--ssh-host', args.ssh_host, *options], cwd=ROOT, check=True)


if __name__ == '__main__':
    main()

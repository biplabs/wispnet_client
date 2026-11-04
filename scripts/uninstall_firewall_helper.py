#!/usr/bin/env python3
"""Remove one user's Wispnet firewall access; remove the helper when unused."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from firewall_helper import configured_uids, Firewall
from install_firewall_helper import CONFIG, BINARY, UNIT, write_root


def uninstall(uid):
    remaining = configured_uids() - {uid} if CONFIG.exists() or CONFIG.is_symlink() else set()
    if remaining:
        write_root(CONFIG, json.dumps({'version': 1, 'uids': sorted(remaining)})+'\n', 0o600)
        # Restart also removes the departing user's rules before rebuilding the
        # remaining users' rules. A failure is reported, not silently ignored.
        subprocess.run(['/usr/bin/systemctl', 'restart', 'wispnet-firewall.service'], check=True)
        return
    active = ['/usr/bin/systemctl', 'is-active', '--quiet', 'wispnet-firewall.service']
    if UNIT.exists() or UNIT.is_symlink() or subprocess.run(active).returncode == 0:
        subprocess.run(['/usr/bin/systemctl', 'disable', '--now', 'wispnet-firewall.service'], check=True)
    if subprocess.run(active).returncode == 0:
        raise RuntimeError('Firewall helper is still running; files were preserved')
    # Explicitly clean up even if the service had previously crashed. The
    # reconciler refuses to delete chains containing foreign rules.
    Firewall().reconcile(set(), cleanup=True)
    for path in (UNIT, BINARY, CONFIG):
        path.unlink(missing_ok=True)
    subprocess.run(['/usr/bin/systemctl', 'daemon-reload'], check=True)


def main():
    # Legacy developer entry point; target installations run the native helper.
    import sys
    script = Path(__file__).with_suffix(".sh")
    os.execv(str(script), [str(script), *sys.argv[1:]])


if __name__ == '__main__':
    main()

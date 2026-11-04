#!/usr/bin/env python3
"""Uninstall Wispnet programs and managed SSH aliases, preserving identity and other settings."""
import argparse
import fcntl
import os
from pathlib import Path
import shutil
import subprocess
import sys
import re
import stat
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def remove_ssh_aliases(home):
    """Remove only explicitly marked Wispnet blocks, under the adapter's lock."""
    directory = home / '.ssh'
    if not directory.exists() and not directory.is_symlink():
        return
    meta = directory.lstat()
    if not stat.S_ISDIR(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o022:
        raise ValueError('Unsafe SSH directory; SSH configuration was not changed')
    path = directory / 'config'
    if not path.exists() and not path.is_symlink():
        return
    lock_fd = os.open(directory / '.wispnet.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(lock_fd, 'r+') as lock:
        meta = os.fstat(lock.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o077:
            raise ValueError('Unsafe SSH configuration lock')
        fcntl.flock(lock, fcntl.LOCK_EX)
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'r', newline='') as config:
            meta = os.fstat(config.fileno())
            if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o022 or meta.st_size > 1024 * 1024:
                raise ValueError('Unsafe or oversized SSH configuration')
            text = config.read(1024 * 1024 + 1)
        if len(text) > 1024 * 1024:
            raise ValueError('SSH configuration grew while reading')
        lines, kept, peer = text.splitlines(keepends=True), [], None
        for line in lines:
            marker = re.fullmatch(r'# Wispnet bridge ([0-9a-f]{64}) (begin|end)\r?\n?', line)
            if marker:
                identity, kind = marker.groups()
                if kind == 'begin' and peer is None:
                    peer = identity
                elif kind == 'end' and peer == identity:
                    peer = None
                else:
                    raise ValueError('Malformed Wispnet SSH block; configuration was not changed')
            elif line.startswith('# Wispnet bridge '):
                raise ValueError('Malformed Wispnet SSH marker; configuration was not changed')
            elif peer is None:
                kept.append(line)
        if peer is not None:
            raise ValueError('Incomplete Wispnet SSH block; configuration was not changed')
        updated = ''.join(kept)
        if updated == text:
            return
        fd, temporary = tempfile.mkstemp(prefix='.wispnet-uninstall-', dir=directory)
        try:
            with os.fdopen(fd, 'w', newline='') as output:
                output.write(updated)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            Path(temporary).unlink(missing_ok=True)


def uninstall(home, uid):
    units = home / '.config/systemd/user'
    for name in ('wispnet-desktop.service', 'wispnet-host.service', 'wipsnet-host.service'):
        unit = units / name
        if unit.exists() or unit.is_symlink():
            subprocess.run(['systemctl', '--user', 'disable', '--now', name], check=True)
    if Path('/etc/wispnet-firewall.json').exists() or Path('/etc/systemd/system/wispnet-firewall.service').exists():
        subprocess.run(['sudo', sys.executable, str(ROOT / 'scripts/uninstall_firewall_helper.py'), '--uid', str(uid)], check=True)
    # Do not remove launchers/binaries if stopping services or privileged cleanup
    # failed: leaving files intact makes a retry possible.
    remove_ssh_aliases(home)
    for name in ('wispnet-desktop.service', 'wispnet-host.service', 'wipsnet-host.service'):
        (units / name).unlink(missing_ok=True)
    for name in ('wispnet', 'wispnet-node', 'wispnet-desktop'):
        (home / '.local/bin' / name).unlink(missing_ok=True)
    (home / '.local/share/applications/wispnet.desktop').unlink(missing_ok=True)
    for name in ('wispnet-node', 'wispnet-desktop'):
        directory = home / '.local/libexec' / name
        if directory.is_symlink():
            directory.unlink()
        elif directory.exists():
            shutil.rmtree(directory)
    subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)


def main():
    # Legacy developer entry point; target installations run the native helper.
    import sys
    script = Path(__file__).with_suffix(".sh")
    os.execv(str(script), [str(script), *sys.argv[1:]])


if __name__ == '__main__':
    main()

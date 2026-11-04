#!/usr/bin/env python3
"""One-time administrator installation of Wispnet's socket-verified Linux firewall helper."""
import argparse
import json
import os
from pathlib import Path
import pwd
import shutil
import stat
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
CONFIG = Path('/etc/wispnet-firewall.json')
BINARY = Path('/usr/local/libexec/wispnet-firewall.py')
UNIT = Path('/etc/systemd/system/wispnet-firewall.service')


def write_root(path, content, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    # Never follow an administrator destination replaced by a symlink.
    for parent in [path.parent, *path.parent.parents]:
        m = parent.lstat()
        if not stat.S_ISDIR(m.st_mode) or m.st_uid != 0 or m.st_mode & 0o022:
            raise SystemExit('Unsafe destination directory: ' + str(parent))
    if path.is_symlink():
        raise SystemExit('Refusing symlink destination: ' + str(path))
    fd, temporary = tempfile.mkstemp(prefix='.wispnet-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as output:
            os.fchmod(output.fileno(), mode)
            output.write(content)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def main():
    # Legacy developer entry point; target installations run the native helper.
    import sys
    script = Path(__file__).with_suffix(".sh")
    os.execv(str(script), [str(script), *sys.argv[1:]])


if __name__ == '__main__':
    main()

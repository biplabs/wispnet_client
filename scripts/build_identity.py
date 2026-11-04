#!/usr/bin/env python3
"""Stable build label shared by all clients; never includes private machine state."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def identity():
    """Compatibility entry point for development tools; Rust owns the identity."""
    return subprocess.check_output(
        [str(ROOT / 'scripts/build_identity.sh')], cwd=ROOT, text=True
    ).strip()


if __name__ == '__main__':
    print(identity())

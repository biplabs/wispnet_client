#!/usr/bin/env python3
"""Development compatibility entry point; installation is implemented by Rust."""
import os
from pathlib import Path
import sys
if __name__ == "__main__":
    script = Path(__file__).with_suffix(".sh")
    os.execv(str(script), [str(script), *sys.argv[1:]])

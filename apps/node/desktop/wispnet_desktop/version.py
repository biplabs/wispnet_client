"""Development compatibility access to the canonical Cargo workspace version."""
from pathlib import Path
import tomllib

VERSION = tomllib.loads(
    (Path(__file__).resolve().parents[4] / 'Cargo.toml').read_text()
)['workspace']['package']['version']

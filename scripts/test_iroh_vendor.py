#!/usr/bin/env python3
"""Compile the vendored crate's unit tests and run bounded, local regressions."""

import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]


def main():
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = env.get(
        "WISPNET_VENDOR_TEST_TARGET_DIR", str(ROOT / "target/iroh-vendor-tests")
    )
    command = [
        "cargo", "test", "--manifest-path", "vendor/iroh/Cargo.toml", "--locked",
        "--lib", "--no-default-features", "--features",
        "tls-ring,portmapper,unstable-net-report",
    ]
    # Every invocation compiles all upstream unit-test modules. This catches
    # fixtures broken by our extra configuration fields, even outside workspace
    # membership. These selected tests need no external relay or privileged netns.
    filters = [
        "net_report::fallback::tests",
        "socket::biased_rtt_path_selector::tests",
        "socket::transports::relay::tests",
    ]
    if sys.platform.startswith("linux"):
        filters.append("runtime::linux::tests")
    for test_filter in filters:
        subprocess.run(command + [test_filter], cwd=ROOT, env=env, check=True)
    # Exercise both the upstream slow-reader regression and the malicious-gap
    # cap retained by its narrow backport. This also compiles all proto unit tests.
    subprocess.run(
        ["cargo", "test", "--manifest-path", "vendor/noq-proto/Cargo.toml", "--locked",
         "--lib", "--no-default-features", "--features", "rustls-ring",
         "connection::assembler::test"],
        cwd=ROOT, env=env, check=True,
    )


if __name__ == "__main__":
    main()

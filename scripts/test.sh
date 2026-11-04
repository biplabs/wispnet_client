#!/bin/sh
set -eu
cd "$(git rev-parse --show-toplevel)"
if { [ -n "${WISPNET_ANDROID_LAN_ADDRESS:-}" ] || [ "${WISPNET_ANDROID_WIFI_CYCLE:-0}" = 1 ]; } && [ "${WISPNET_ANDROID_RELAY_TEST:-0}" != 1 ]; then
    echo "Android LAN/Wi-Fi checks require WISPNET_ANDROID_RELAY_TEST=1." >&2
    exit 1
fi
if [ -n "${WISPNET_SSH_RELAY_HOST:-}" ] && [ "${WISPNET_ANDROID_RELAY_TEST:-0}" != 1 ]; then
    echo "Physical SSH/tablet checks require WISPNET_ANDROID_RELAY_TEST=1 to rebuild the current Android CLI." >&2
    exit 1
fi
# Darwin's per-user temporary path can exceed the Unix-domain socket limit.
if [ "$(uname -s)" = Darwin ]; then
    # Share roots reject symlink components; /tmp is a symlink on macOS.
    TMPDIR=$(cd /tmp && pwd -P)
    export TMPDIR
fi
cargo fmt --all --check
# The patched Iroh runtime is excluded from workspace formatting.
rustfmt --edition 2024 --check vendor/iroh/src/runtime.rs
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/test_iroh_vendor.py
python3 scripts/test_iroh_compat.py
cargo test -p wispnet-cli --example external_network_probe --locked
cargo build --workspace --locked
cargo build -p wispnet-cli --example relay_fixture --locked
python3 -m unittest discover -s scripts -p test_firewall_helper.py
python3 -m unittest discover -s scripts -p test_uninstall.py
python3 -m unittest discover -s scripts -p test_arch_package.py
python3 -m unittest discover -s scripts -p test_macos_distribution.py
python3 -m unittest discover -s scripts -p test_homebrew.py
python3 -m unittest discover -s scripts -p test_relay_network_audit.py
python3 scripts/smoke.py
python3 scripts/registry_smoke.py
python3 scripts/host_smoke.py
python3 -m unittest discover -s scripts -p test_mcp.py
python3 -m unittest discover -s scripts -p test_mcp_exec.py
python3 scripts/test_node_desktop.py
python3 scripts/test_native_desktop.py
python3 scripts/shell_smoke.py
python3 scripts/ssh_smoke.py
python3 scripts/files_smoke.py
python3 scripts/test_mutual_deletion.py
uv run --with pyte==0.8.2 --with zxing-cpp==2.3.0 --with pillow==11.3.0 python scripts/test_tui.py
if [ "$(uname -s)" = Linux ]; then
    python3 -m unittest discover -s scripts -p test_linux_distribution.py
fi
PYTHONPATH=apps/node/desktop python3 -m unittest discover -s apps/node/desktop/tests
scripts/test_flutter.sh
python3 -m unittest discover -s scripts -p test_android_release.py
python3 -m unittest discover -s scripts -p test_android_publish.py
uv run --with qrcode==8.2 --with zxing-cpp==2.3.0 --with pillow==11.3.0 python -m unittest discover -s scripts -p test_pairing_qr.py
if [ "${WISPNET_ANDROID_RELAY_TEST:-0}" = 1 ]; then
    set -- --android
    if [ -n "${WISPNET_ANDROID_LAN_ADDRESS:-}" ]; then
        set -- "$@" --lan-address "$WISPNET_ANDROID_LAN_ADDRESS" --lan-port "${WISPNET_ANDROID_LAN_PORT:-0}"
    fi
    if [ "${WISPNET_ANDROID_WIFI_CYCLE:-0}" = 1 ]; then
        set -- "$@" --wifi-cycle
    fi
    python3 scripts/relay_smoke.py "$@"
else
    python3 scripts/relay_smoke.py
fi
if [ -n "${WISPNET_SSH_RELAY_HOST:-}" ]; then
    python3 scripts/physical_relay_smoke.py --ssh-host "$WISPNET_SSH_RELAY_HOST"
fi
python3 scripts/tunnel_smoke.py
uv run --project experiments/browser --locked python -m unittest discover -s experiments/browser -p 'test_*.py'

# Opt in on Linux hosts with namespaces, nftables and strace. An explicitly
# requested network audit fails if unavailable; it is never silently skipped.
if [ "${WISPNET_NETWORK_AUDIT:-0}" = 1 ]; then
    if [ -n "${WISPNET_STRACE:-}" ]; then
        python3 scripts/relay_network_audit.py --strace "$WISPNET_STRACE"
    else
        python3 scripts/relay_network_audit.py
    fi
fi

#!/bin/sh
set -eu
case "$0" in
    */*) wispnet_script_dir=${0%/*} ;;
    *) wispnet_script_dir=. ;;
esac
wispnet_root=$(CDPATH= cd -- "$wispnet_script_dir/.." && pwd)
if [ -x "$wispnet_root/bin/wispnet" ]; then
    wispnet_native="$wispnet_root/bin/wispnet"
elif [ -x "$wispnet_root/target/debug/wispnet" ]; then
    wispnet_native="$wispnet_root/target/debug/wispnet"
elif [ -f "$wispnet_root/Cargo.toml" ]; then
    (cd "$wispnet_root" && cargo build --locked -p wispnet-cli)
    wispnet_native="$wispnet_root/target/debug/wispnet"
else
    echo 'The native Wispnet executable is missing from this bundle.' >&2
    exit 1
fi
exec "$wispnet_native" app-install --root "$wispnet_root" "$@"

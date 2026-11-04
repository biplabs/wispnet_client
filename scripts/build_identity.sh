#!/bin/sh
set -eu
case "$0" in */*) wispnet_script_dir=${0%/*} ;; *) wispnet_script_dir=. ;; esac
wispnet_root=$(CDPATH= cd -- "$wispnet_script_dir/.." && pwd -P)
cd "$wispnet_root"
exec cargo run --quiet --locked -p wispnet-build-info -- "$@"

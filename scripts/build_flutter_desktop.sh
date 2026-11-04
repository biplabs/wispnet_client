#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(scripts/build_identity.sh)
case "$(uname -s)" in
 Linux) platform=linux; bundle=apps/flutter/build/linux/x64/release/bundle ;;
 Darwin) platform=macos; bundle=apps/flutter/build/macos/Build/Products/Release/Wispnet.app/Contents/MacOS ;;
 *) echo 'Build on Linux or macOS; Windows node integration is not implemented.' >&2; exit 1 ;;
esac
(cd apps/flutter; ../../scripts/flutter.sh build "$platform" --release --dart-define="WISPNET_BUILD_ID=$version")
# The native node also provides the private desktop stdio adapter.
# Rebuild it for this checkout before bundling so UI and adapter stay in sync.
cargo build --release --locked -p wispnet-cli --bin wispnet
helper=wispnet
if [ "$platform" = macos ]; then helper=wispnet-node; fi
cp target/release/wispnet "$bundle/$helper"
assets="$bundle"
if [ "$platform" = macos ]; then
 assets="$bundle/../Resources"
 mkdir -p "$assets"
 # Contents/MacOS must contain code only. Data signed via extended attributes
 # loses its detached signature when transported through a tar archive.
 rm -f "$bundle/icon.png" "$bundle/build-identity"
fi
cp apps/node/desktop/icon.png "$assets/"
printf '%s\n' "$version" > "$assets/build-identity"
# Flutter's build directory can retain assets from earlier Python releases.
rm -f "$bundle/bridge.py" "$bundle/desktop_data.py" "$bundle/python-path"
rm -rf "$bundle/wispnet_desktop" "$bundle/__pycache__"
if [ "$platform" = macos ]; then
 codesign --force --deep --sign - apps/flutter/build/macos/Build/Products/Release/Wispnet.app
fi
printf 'Flutter desktop bundle: %s\n' "$bundle"

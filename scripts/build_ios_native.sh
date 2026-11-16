#!/usr/bin/env bash
# Xcode invokes this for both device and simulator builds.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
case "${PLATFORM_NAME:-iphonesimulator}" in
  iphoneos) target=aarch64-apple-ios ;;
  iphonesimulator) target=aarch64-apple-ios-sim ;;
  *) echo "Unsupported Apple platform: $PLATFORM_NAME" >&2; exit 1 ;;
esac
if [[ "${CURRENT_ARCH:-arm64}" != arm64 && "${CURRENT_ARCH:-arm64}" != undefined_arch ]]; then
  echo 'Wispnet iOS builds currently require arm64.' >&2; exit 1
fi
profile=debug
set --
if [[ "${CONFIGURATION:-Debug}" != Debug ]]; then profile=release; set -- --release; fi
# Xcode injects host SDK/toolchain variables that must not reach Cargo build scripts.
env -u SDKROOT -u IPHONEOS_DEPLOYMENT_TARGET -u MACOSX_DEPLOYMENT_TARGET \
  cargo build -p wispnet-client --lib --target "$target" --locked "$@"
mkdir -p "${BUILT_PRODUCTS_DIR:-apps/flutter/build/ios/native}"
output="${BUILT_PRODUCTS_DIR:-apps/flutter/build/ios/native}/libwispnet_client.a"
if ! cmp -s "target/$target/$profile/libwispnet_client.a" "$output"; then
  cp "target/$target/$profile/libwispnet_client.a" "$output"
fi

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
if [[ "$(uname -s)" != Darwin ]]; then echo 'iOS builds require macOS and Xcode.' >&2; exit 1; fi
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
scripts/flutter.sh precache --ios
version=$(scripts/build_identity.sh --version)
build=$(scripts/build_identity.sh --android-version-code)
cd apps/flutter
../../scripts/flutter.sh pub get --enforce-lockfile
exec ../../scripts/flutter.sh build ios --build-name "$version" --build-number "$build" "$@"

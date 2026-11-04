#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export JAVA_HOME="${JAVA_HOME:-/opt/android-studio/jbr}"
ndk_bin="${WISPNET_ANDROID_NDK:-$ANDROID_HOME/ndk/27.2.12479018}/toolchains/llvm/prebuilt/linux-x86_64/bin"
android_abi="${WISPNET_ANDROID_ABI:-arm64-v8a}"
case "$android_abi" in
  arm64-v8a)
    native_target=aarch64-linux-android
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ndk_bin/aarch64-linux-android34-clang"
    export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
    export AR_aarch64_linux_android="$ndk_bin/llvm-ar"
    ;;
  x86_64)
    native_target=x86_64-linux-android
    export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$ndk_bin/x86_64-linux-android34-clang"
    export CC_x86_64_linux_android="$CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER"
    export AR_x86_64_linux_android="$ndk_bin/llvm-ar"
    ;;
  *) echo 'Supported Android ABIs: arm64-v8a, x86_64' >&2; exit 1 ;;
esac
# NDK r27 needs both flags: segment alignment and RELRO boundaries.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384"
android_profile="${WISPNET_ANDROID_PROFILE:-debug}"
android_artifact="${WISPNET_ANDROID_ARTIFACT:-apk}"
case "$android_profile:$android_artifact" in
  debug:apk) cargo_flags=() ;;
  release:apk) cargo_flags=(--release) ;;
  release:bundle) cargo_flags=(--release) ;;
  *) echo 'Supported builds: debug/apk, release/apk, release/bundle' >&2; exit 1 ;;
esac
cargo build -p wispnet-android --target "$native_target" --locked "${cargo_flags[@]}"
mkdir -p "apps/flutter/android/app/src/main/jniLibs/$android_abi"
cp "target/$native_target/$android_profile/libwispnet_android.so" "apps/flutter/android/app/src/main/jniLibs/$android_abi/"
"$ndk_bin/llvm-strip" --strip-debug "apps/flutter/android/app/src/main/jniLibs/$android_abi/libwispnet_android.so"
shared_version="$(scripts/build_identity.sh --version)"
shared_code="$(scripts/build_identity.sh --android-version-code)"
case "$android_abi" in arm64-v8a) flutter_target=android-arm64 ;; x86_64) flutter_target=android-x64 ;; esac
case "$android_artifact" in apk) flutter_artifact=apk ;; bundle) flutter_artifact=appbundle ;; esac
flutter_name="${WISPNET_ANDROID_VERSION_NAME:-$shared_version}"
flutter_code="${WISPNET_ANDROID_VERSION_CODE:-$shared_code}"
(cd apps/flutter && ../../scripts/flutter.sh build "$flutter_artifact" "--$android_profile" --target-platform "$flutter_target" --build-name "$flutter_name" --build-number "$flutter_code")

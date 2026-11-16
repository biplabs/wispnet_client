# iPhone and iPad

The iOS runner uses the existing shared Flutter UI and embedded Rust client on
both iPhone and iPad. It targets arm64 devices and Apple Silicon simulators.
No desktop daemon, SSH installation, or Android service is launched on iOS.

## Build

Install Xcode with the iOS platform and simulator runtime, Flutter 3.47.9
(Dart 3.13.7), and Rust 1.95. From the repository root:

```sh
scripts/build_ios.sh --simulator --debug
# Build an unsigned device app for compilation validation:
scripts/build_ios.sh --release --no-codesign
```

The Xcode `Build Rust client` phase runs `scripts/build_ios_native.sh` for the
selected platform, then links the static library into Runner. Both device
families and iPad landscape orientations are enabled. Open
`apps/flutter/ios/Runner.xcworkspace` to select your signing team, connect a
trusted device with Developer Mode enabled, and run Runner. The bundle ID is
`app.wispnet.ios`. Signing credentials are local configuration, not committed.
See [Flutter's iOS setup](https://docs.flutter.dev/platform-integration/ios/setup)
for the Apple signing and device prerequisites.

## Platform behavior

- The device secret is generated natively and stored in Keychain with
  `AfterFirstUnlockThisDeviceOnly`. It never crosses the Flutter channel.
  Catalogs, owner state, and display ordering live in private Application Support.
- Pairing invitations, approval, catalog refresh, HTTP/TCP services, local service
  management, public links, file transfers, and remote Shell use the shared Rust
  permission and incarnation checks. Cached catalogs cannot authorize an open.
- QR-image import uses Vision. Camera capture supplies bounded JPEG frames to the
  shared Flutter QR scanner and requests camera permission only when needed.
- File selection copies documents to app-private temporary storage. Selected
  folders retain security-scoped bookmarks; stale access requires reselection.
  Folder sharing uses filesystem-backed Rust storage within the selected scope.
- Remote shells use the shared Flutter terminal renderer, with a software
  keyboard editor and control-key bar for touch input. iOS cannot host an OS
  account Shell service or launch an external desktop terminal application.
- Browser services use separate saved `127.0.0.1` ports on iOS, preserving distinct
  origins without requiring installation of desktop `.localhost` DNS aliases.
  Browsing opens an in-app Safari view so the embedded client remains foregrounded.
  The registry retains the configured shared port and is available at
  `http://127.0.0.1:7842/v1/services` by default.
- `NWPathMonitor` forwards network transitions to the client, closing interrupted
  streams before restoring listeners. iOS may suspend networking after the app
  enters the background; this is not an always-on background daemon or VPN.
- The `wispnet://` URL scheme enters the shared service-link flow. Receiving files
  from another app's Share Sheet is not implemented; use the in-app file picker.
  Native upload staging is temporary, and transfer handles are process-local.
  After process termination, select the source/destination again to resume.

## Verification

Run the standard local checks with `scripts/test.sh`, using Python 3.12 or
newer on PATH (the macOS system Python does not honor the test fixture CA
consistently). When sharing the Mac
loopback with simulators, use `RUST_TEST_THREADS=1 scripts/test.sh` to avoid
concurrent tests reusing ports that another test expects to be closed. With a booted iPhone or
iPad simulator, also run:

```sh
cd apps/flutter
../../scripts/flutter.sh drive -d SIMULATOR_UDID \
  --driver=test_driver/ios_test.dart \
  --target=integration_test/ios_test.dart
```

The native integration test checks identity, QR invitation generation and preview,
service creation, a real HTTP request through the numeric loopback gateway, the
shared directory and settings, and cleanup. Simulators share the Mac loopback;
if the desktop app owns the simulator's original port, the test retains its free
fixture port instead of restoring an unusable setting. Screenshots are written to
`apps/flutter/build/ios-review`, or `WISPNET_IOS_SCREENSHOTS` when set. Run on both device families. A
simulator run does not validate physical camera hardware, Files providers,
cellular transitions, background behavior, or distribution signing.

The vendored `netdev` and `netwatch` patches avoid unavailable iOS routing/IPv6
ioctl ABI declarations. Interface enumeration still uses `getifaddrs`; route
notifications come from the native `NWPathMonitor`. Unknown route/IPv6 metadata
is left unavailable. Other platforms retain upstream behavior.

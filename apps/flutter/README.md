# Wispnet Flutter UI

Desktop is the UI standard for every platform. Android and desktop use the same
Flutter pages for the home screen, paired machines, machine settings, pairing,
device settings, service links, service management, folder editors, sending files,
browsing shared files, and transfer progress. Responsive layouts adapt that page
to the available width; do not create parallel mobile pages.

`ServiceSettings` is the shared service-management page, using the same owner
status/request interface on every platform. Android supports HTTP/TCP services,
public-link controls, pairing invitations and approval, device grants, and durable
owner operations as well as provider-backed folders. Folder IDs can be renamed
atomically without losing their capability, grants, or paused state. The legacy
Files inbox uses the same staged editor on every platform and preserves existing
exports during grant-only saves.

The remaining differences are native platform integrations, not alternate pages:

- Android folder access uses its Storage Access Framework, persisted URI grants,
  and folder labels. Desktop uses native filesystem paths and pickers. An expired
  provider grant requires reselecting that folder before access can be enabled.
- Android cannot host the desktop operating-system-account Shell service. Remote
  Shell access uses the same Flutter terminal page, tabs, splits, input modes and
  session controls on every platform. Android embeds the Termux renderer and IME
  input component inside that page; desktop uses the Flutter terminal renderer.
  Only desktop can hand a session to an installed external terminal application.
- Pairing uses the same QR and code tabs. QR-image import works everywhere. The
  camera action depends on actual camera availability. The shared camera page
  receives frames from Android Camera2, Linux V4L2/FFmpeg, or macOS AVFoundation.
- Diagnostics uses the same page and probe on every platform. Device-specific
  network/VPN observations are reported when available; unknown fields remain
  unavailable rather than being guessed. The device itself is not probed as a
  remote peer.
- Touch reordering uses a long press; mouse reordering uses a drag. Desktop also
  accepts file drops, while Android accepts attachments from the OS share sheet.
- Owner operations require explicit configured owner access on each target.
  Desktop uses its configured SSH targets. Android offers this device and paired
  hosts with an already granted Shell service, checked again when executing the
  fixed owner command. The same review/confirmation/history page is used; no
  service grant or owner authority is inferred or added automatically.

Local service switches pause serving and close active connections while keeping
folder settings and grants for re-enabling. Switch state survives a node restart.
Settings groups controls into Device, Services, Access & pairing, and Connections.
Device contains the name and an Advanced port setting; Services contains local
services and the registry URL; Connections contains MCP setup and diagnostics.
Access & pairing opens the Paired devices popup.
Pair another device scans or imports a pairing QR or accepts a pairing code.
Pair this device displays this device’s invitation. Device access opens the
local approval list, pairing requests, and owner-operation history.

Popup navigation uses a pinned header. Back returns to the previous page and
preserves its draft, while X dismisses the entire popup flow. Save and Cancel
return from the current form.

The macOS runner and installer use these widgets too, but require native build
and runtime validation on a Mac. Windows/iOS node adapters are not implemented.

## Build and run

Use Flutter **3.47.9** (Dart 3.13.7), Android SDK 36, and the existing Rust/Android
prerequisites. `scripts/flutter.sh` resolves `FLUTTER_ROOT`, `PATH`, then
`~/.local/share/flutter`. The Flutter runner uses the SDK's NDK; the Rust library
continues to use the existing NDK 27 build and 16 KB alignment flags.

From the repository root:

```sh
scripts/build_android.sh
adb install -r apps/flutter/build/app/outputs/apk/debug/app-debug.apk
adb shell am start -n app.wispnet.android/com.biplabs.wispnet.FlutterMainActivity
scripts/test_flutter.sh
```

The Android build/release scripts build the Flutter UI with the existing version
identity, upload signing variables and release checks. Artifacts live under
`apps/flutter/build/app/outputs`. Never uninstall/clear the app to migrate it.

## Boundaries and upgrade compatibility

- `lib/src/backend.dart`: UI-facing platform interface, without Android imports.
- `lib/ui.dart`: shared UI library, implemented in `lib/src/ui/`. Screens import
  this library for the theme, panels/sheets, headers, status badges, links, copy
  buttons, toggles, and destructive button styles.
- `WispListItem` keeps identity on the left and independent controls on the right,
  moving controls below the label on narrow layouts. `WispServiceItem` adds a
  service icon and orders its controls as toggle, extra actions, then settings.
  Feature screens own backend calls, permissions, and busy state.
- `lib/src/cards.dart`, `space.dart`, `sheets.dart`: directory cards, background,
  and feature-specific sheet content built from the shared UI library.
- `lib/src/app.dart`: responsive directory and orchestration.
- `android/.../FlutterMainActivity.java`: Android platform adapter. Reuses the
  existing `Native`, `HostStore`, `DeviceProfile`, `AccessService`, and bounded QR
  decoder from the same Android runner and the shared Rust client/owner core.

The application ID remains `app.wispnet.android`, with the same signing settings,
Keystore alias, private state paths, preferences, host validation, card ordering,
and backup exclusions. Notification taps resolve the Flutter launcher. Flutter never receives the device key. Opening cached services still
requires the Rust client's fresh owner/permission/incarnation check.

The legacy Java UI and its instrumentation harness have been removed. Flutter
is the only launcher and text-share entry point. Native Android services and
resources live in `android/app/src/main`; the JNI crate lives in `android/native`.

## Verification

`test/widget_test.dart` covers service handoff, denied cached entries, failed
pairing/input retention, settings validation, saved ordering, and narrow/wide
layouts. To render review images with the bundled fonts:

```sh
cd apps/flutter
WISPNET_SCREENSHOTS=/tmp/wispnet-flutter ../../scripts/flutter.sh test
```

## Desktop

```sh
scripts/build_flutter_desktop.sh
scripts/install_node_desktop.sh   # existing initialized Linux node
# On a Mac: scripts/install_macos.sh
```

Flutter desktop bundles include the engine, assets and the native Rust adapter.
`wispnet desktop-bridge` uses the shared node API, catalog cache and durable owner
operations through private stdin/stdout pipes. It creates no management listener
and never imports the node key. The adapter keeps polling independent of bounded
network operations, checks the socket's owner/permissions and preserves the
existing lifecycle (switching Online off stops the node; quitting the UI leaves it running).

Version metadata is generated by the Rust helper `scripts/build_identity.sh`.
Python remains a development dependency for integration tests, packaging and
release automation; it is not installed or launched by the desktop bundles.

Private labels and ordering are imported read-only from Tauri WebKit local storage
and the earlier desktop preference files. New display preferences live in
owner-only `desktop-flutter.json` in the existing state directory; cached catalogs
remain display-only. Opening any service asks its current owner again with the
exact incarnation. The installer stages complete runtimes and switches the
launcher only after the replacement builds, retaining previous Flutter runtimes for recovery.
The private desktop persistence helper and its tests live in
`apps/node/cli/src/owner.rs` and `desktop.rs`.

Linux build prerequisites: Flutter, Clang, CMake, Ninja, pkg-config, GTK 3 and
Ayatana AppIndicator development libraries. Runtime also requires xdg-open. Linux camera QR scanning uses FFmpeg with V4L2; the Arch package
includes FFmpeg as a dependency. Camera availability follows detected hardware,
and image import remains available without a camera. macOS uses AVFoundation for
camera capture, native Rust helpers, launch agents and menu bar.

`integration_test/desktop_test.dart` runs the actual native Linux window against
an already online node, checks sheets and registry settings, captures review
images, and opens an existing local HTTP service in the real browser. Stop the
installed desktop UI before running (the runner is single-instance), then restart
it afterwards:

```sh
systemctl --user stop wispnet-desktop.service
(cd apps/flutter && ../../scripts/flutter.sh drive -d linux \
  --driver=test_driver/integration_test.dart --target=integration_test/desktop_test.dart)
systemctl --user start wispnet-desktop.service
```

Run fixture-only adapter and widget checks with `scripts/test_flutter.sh`.
Live native coverage is distinct from a wide widget rendering.

For the connected paired debug app, run the upgrade-only device checks:

```sh
python3 scripts/test_flutter_android.py --serial DEVICE_SERIAL
```

This checks identity/profile/host preservation, settings loading, online/offline
controls, real catalogs, and browser handoff. It captures the Flutter layer for
visual review and restores the normal app in a `finally` block. It uses explicit
`adb install -r` and attaches a test driver to the installed app; it never invokes
Flutter's uninstall-on-install-failure fallback.

## Android visual reference

The header retains the established device-header geometry: settings follows the
intrinsic device-label width. The docked control retains the native 6 dp downward
offset, 38 dp pill within a 44 dp control, layered cyan indicator, and fill/border
colors. Its caption is now **Online**, measured at its natural text width using
the shared bundled font on Android and desktop. Clicking it still switches access
off. Widget checks cover shared labels, toggling, phone density, settings adjacency,
and narrow/wide layouts. The installed-app check captures the isolated control.

`SpaceChecks.buttonLayout` in the existing Android instrumentation runner renders
the original control to private `files/button-layout.png` and reports its metrics
without changing pairings or services. This is a native reference, not a Flutter
coverage claim. Bundled Roboto 3.005 (2022) Regular/Medium/Bold are static instances
of the reference Android system font (weight 400/500/700, width 100, italic 0),
under the included Apache 2.0 license. Bundling them keeps that typography shared
across future desktop targets.

Android networking is owned by `AccessService` in `:network`; `NetworkController`
contains the private native operations and `NetworkBridge` is the UI's Binder
adapter. The activity never initializes the native client. All platforms use one
shared HTTP service/registry port, defaulting to 7842 for new settings. See
`docs/service-registry.md` for migration and stable `wispnet://` links.

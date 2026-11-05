# Android app development

The shared Flutter UI and its Android platform integration live in `apps/flutter`.
The Java backend, QR scanner, foreground service and resources are in
`apps/flutter/android/app/src/main`; the Rust JNI crate is in
`apps/flutter/android/native`. The old Java UI and its instrumentation runner
have been removed. Application ID, signing and private storage are unchanged.
Use `adb install -r` to preserve identity and paired hosts.

## Build and install

Requirements: Rust with `aarch64-linux-android`, Android SDK platform 36, NDK 27.2.12479018, Java 17+, and an authorized arm64 Android 14+ device. `scripts/build_android.sh` defaults to `$HOME/Android/Sdk` and Android Studio's JBR; override `ANDROID_HOME`, `JAVA_HOME`, or `WISPNET_ANDROID_NDK` as needed. The checked-in Gradle wrapper pins its distribution checksum.

```sh
scripts/build_android.sh
adb install -r apps/flutter/build/app/outputs/apk/debug/app-debug.apk
```

The APK currently targets API 36, builds for arm64 by default (optional x86_64 for emulator acceptance), and uses the configured public relay `https://relay.wispnet.app`.

## Pair and browse

1. Start a host agent on each computer. `scripts/install_app_hosts.py` installs independent `wispnet-host.service` user units on this computer and `desktop.home`, preserving existing identities/configuration. Its node state is separate from the desktop relay state.
2. Issue a target-host invitation with the CLI's `host-invite` command. Verify that computer's public Endpoint ID independently.
3. In Wispnet, paste its pairing code, review the target machine, and request pairing. This proves the tablet identity; it does not grant access yet.
4. On that computer, inspect `host-invitations`, approve the pending invitation using `host-mutate`, and grant the tablet identity access to the intended services. Repeat independently for the other computer.
5. Refresh hosts, select **Start browser access**, then select a service's **Open** button. Its owning host is shown even when both publish `website`.
6. **Stop browser access**, including the notification action, closes the app's gateway listeners and active connections.

Identity is wrapped using a non-exportable Android Keystore AES-GCM key. Pins and cached catalogs live in private app storage; Android backup is disabled. Imported invitations are cleared after the request. No host owner token or private host key is copied into the app. A corrupt identity envelope fails closed rather than silently replacing the device identity.

Catalog refreshes run independently. Cached listings show their age/status and cannot authorize access: opening fetches the owning host's catalog again and every new tunnel is checked by that host. Catalog rollback or a changed host generation requires explicit attention/re-pairing.


## Receive files

Open **Settings → Receive files**. Choose a writable receiving folder with the
system folder picker, select the paired computers allowed to send, then save.
Use the gear to change the folder. The destination stays private to Android;
senders only see their Files permission. Keep Wispnet online to receive while
other apps are open. See [Files](files.md) for resume and storage behavior.

On a disposable emulator, build an x86_64 app and instrumentation APK, then run:

```sh
WISPNET_ANDROID_ABI=x86_64 scripts/build_android.sh
(cd apps/flutter/android && ./gradlew -Ptarget-platform=android-x64 :app:assembleDebugAndroidTest)
python3 scripts/test_android_files.py --serial emulator-5554 --receiving --output /tmp/wispnet-receiving-check
```

This uses a real system folder grant and an isolated desktop identity to verify
background QUIC uploads, contents, safe retries, offline resume and revocation.
The fixture creates a unique `Documents/WispnetReceiveTest-*` folder on the
disposable emulator; it refuses physical devices.

## Verification

Run `scripts/test_flutter.sh` for adapter and widget tests. Build the real Android
app with `scripts/build_android.sh`, then run
`python3 scripts/test_flutter_android.py --serial DEVICE_SERIAL` against an
authorized device. This checks Flutter semantics and real Android handoff;
the retired Java-view test harness is no longer supported.

The native QR camera checks cover JPEG selection, including cameras whose smallest
JPEG is 1080p. On a disposable emulator with the app and instrumentation APK
installed, run the bounds/lifecycle checks without opening a camera:

```sh
adb -s emulator-5554 shell am instrument -w -e cameraChecks true app.wispnet.android.test/com.biplabs.wispnet.FilesTransferInstrumentation
```

To opt into real capture and close/reopen checks, pregrant `android.permission.CAMERA`
and `android.permission.POST_NOTIFICATIONS` to `app.wispnet.android`, then add
`-e cameraLive true`. This opens the emulator camera and validates two JPEG frames;
it does not verify optical QR scanning on a physical phone.

See [Flutter development and verification](../apps/flutter/README.md),
[release checks](android-release.md) and [service discovery](android-service-discovery.md).

## Networking process

The Flutter activity is a controller and browser launcher. `AccessService` runs
in the private `:network` process and owns JNI initialization, the protected
identity, peer/catalog storage, shared HTTP listener and network recovery.
`NetworkBridge` uses private, UID-checked Messenger/Binder RPC; the service is not
exported. UI teardown unbinds only and does not stop an online service.
All network-owned SharedPreferences are read and written in that one process.

Online access is a user-started `connectedDevice` foreground service with an
ongoing notification and **Go offline** action. Its declared network permission
satisfies the connected-device service prerequisite. It does not use a VPN slot
or the time-limited `dataSync` service type. Play foreground-service declarations
must describe the actual connected-device use when publishing this update.

Android may still terminate the process, and force-stop ends access. The existing
`START_NOT_STICKY` policy is retained: process death requires explicit restart;
UI recreation alone does not create a second endpoint. Binder disconnection fails
in-flight UI operations without replaying mutations and reconnects on a new service
binding. Network changes and port changes never replay HTTP requests.

`wispnet://HOST_ID/SERVICE_ID` links are handled by the activity, resolved by the
network process and opened in the external browser. Ordinary HTTP service traffic
continues after switching away from the UI while the foreground service is active.

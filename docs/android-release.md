# Android internal-testing build

Internal Play publishing and a real Play-phone optical scan/pending owner approval are recorded in
[issue #17](https://github.com/biplabs/wispnet_client/issues/17). Camera-free code, invitation-file and QR-image paths are also implemented. See [app camera evidence](android-app.md#automatic-camera-qr-pairing) and the dated publishing checkpoint below; real-phone browser/lifecycle and broader #12/#10 acceptance remain open. Earlier preparation sections retain their dated scope.

## Build and inspect

Requirements: the Android build prerequisites in [Android app](android-app.md), SDK platform 36, `uv` for the local suite, and network access on the first bundletool download. Default distribution ABI is arm64-v8a; x86_64 is available for emulator checks.

```sh
# Produce an explicitly unsigned, optimized release AAB:
scripts/build_android_release.sh
# Independent validation of an existing bundle:
python3 scripts/check_android_release.py apps/flutter/build/app/outputs/bundle/release/app-release.aab
```

Output: `apps/flutter/build/app/outputs/bundle/release/app-release.aab`. Unsigned output is for inspection, not Play upload. The builder uses Cargo's release profile, target API 36, a non-debuggable Flutter application with the existing Java/Rust backend, backup disabled, and optional camera hardware. Per-build version overrides are `WISPNET_ANDROID_VERSION_CODE` and `WISPNET_ANDROID_VERSION_NAME`; the defaults derive from the shared Wispnet version (`1000` and `0.1.0` for version 0.1.0). Increase the version code for every new Play upload.

The build uses both NDK r27 linker flags for 16 KB LOAD and RELRO alignment, and uncompressed native-library packaging. The checker downloads checksum-pinned Google bundletool 1.18.3 into ignored `target/android-tools`, validates the AAB, reads its actual protobuf manifest/config, and checks the expected application ID, release flags, API target, optional camera, native ABI, ELF LOAD/RELRO boundaries and bundle ZIP-alignment request. It rejects unexpected native libraries and verifies any present bundle signature. It also inspects the compiled DEX definitions for the Java/JNI interface, including the native-to-Java receiving callback and cancellation method; missing, renamed, or incompatible methods fail the release check. `apps/flutter/android/app/proguard-rules.pro` preserves the callback that Rust invokes by name, so R8 cannot remove it as unused.
The Flutter bundle contains exactly the selected ABI's Rust library, Flutter engine,
and Dart AOT snapshot. The snapshot can omit RELRO; the engine's separate RELRO
LOAD segment is accepted only when page rounding cannot overlap another writable
segment, following Android's documented separate-segment rule. Unsafe shared
RELRO/data pages remain rejected. Unit regressions for unsafe manifest/alignment/signing cases run through `scripts/test.sh` and the pre-push hook.

Policy references: [Play target API requirements](https://support.google.com/googleplay/android-developer/answer/11926878), [16 KB native alignment and testing](https://developer.android.com/guide/practices/page-sizes), and [bundletool](https://developer.android.com/tools/bundletool). Structural alignment is separate from runtime compatibility.

## Upload signing

Use your chosen upload keystore outside the repository. Supply all four variables through a local secret environment, without placing passwords in command arguments or committed files:

- `WISPNET_UPLOAD_KEYSTORE`: absolute path to the upload keystore.
- `WISPNET_UPLOAD_STORE_PASSWORD`.
- `WISPNET_UPLOAD_KEY_ALIAS`.
- `WISPNET_UPLOAD_KEY_PASSWORD`.

Then run:

```sh
WISPNET_ANDROID_VERSION_CODE=2 scripts/build_android_release.sh --signed
python3 scripts/check_android_release.py apps/flutter/build/app/outputs/bundle/release/app-release.aab --require-signed
```

The signed builder fails before compilation if credentials are missing; partial signing configuration is rejected by Gradle. It never falls back to a debug key. Keystores and signing properties are ignored, but keep real credentials outside the checkout. The initial release-preparation checkpoint did not create a permanent key. The subsequent signing setup below provisions a separate Wispnet upload key; Console access remains user-managed. See [Play App Signing and upload keys](https://developer.android.com/studio/publish/app-signing).

A Play-signed app cannot update the existing debug-signed tablet APK in place when certificates differ. Keep the current installation intact until the internal-testing switch is intentional; a fresh install loses that sandbox's identity and requires fresh host-specific pairing/approval. Do not uninstall it as a build step. Production migration is not part of this development workflow.

## Internal-testing verification

Once a signed artifact and Play app are available, upload through the internal track and complete the required app/foreground-service/store declarations for the actual behavior. Use the opt-in install link on the intended devices. The internal-only uploader below automates subsequent uploads; Console declarations remain user-managed.

Verify live optical scan, permission denial/cancellation and camera-free fallback on the Play-installed app. Repeat independent pairing and the real-service/browser lifecycle checks after fresh installation. Record the version code, Android/device versions and outcomes in #12; camera code being present is not evidence of an optical scan. Remaining public relay/cellular, OEM/long-duration and #10 browser compatibility work remains separate.

## Observed local evidence

On 2026-10-16, ARM and x86 unsigned bundles passed bundletool, manifest and 16 KB ELF/RELRO/ZIP-config validation. A disposable local signing key exercised the signed Gradle build and signature checker successfully, then was deleted; no real upload identity was provisioned. The final distribution artifact was rebuilt unsigned.

An APK generated from the optimized x86 release AAB by bundletool, signed only with the existing local debug certificate for this test, ran on a disposable Android 16/API 36 emulator reporting `getconf PAGE_SIZE = 16384`. The application remained non-debuggable. Matching debug instrumentation exercised protected identity initialization, native access Start, the actual shortened framework timeout, native Stop, explicit foreground restart, and Stop with unchanged identity. No host approval or live tunnel was exercised on this emulator. The test override was restored; this was not a six-hour soak or Play-signed install.

The tablet remains on its existing debug certificate and protected identity. API 36 targeting and explicit system-bar/cutout/keyboard insets are included in the rebuilt app; the tablet's Android 14 runtime does not establish Android 16 behavior by itself. Live camera verification is deferred as requested.

Release lint passed with warnings for the pinned tool/library versions, ARM-only distribution/ChromeOS ABI coverage, explicit backup-rule resources, missing launcher artwork and hard-coded UI text. Those warnings were not suppressed or presented as Play acceptance. Store artwork/declarations and remaining UI polish are still preparation work.

The rebuilt API 36-targeted debug APK, using byte-for-byte the optimized ARM native library from the release AAB, passed the physical tablet normal-app harness: preserved identity/pairings, real GIKE, desktop WispShell, Llama chat UI and streamed API reply, gateway denial cases, 30-second browser handoff and Stop. The emulator settings and disposable AVD were removed afterward; SDK packages remain available for repeat builds.

## Configured signing and your first Console upload

A separate Wispnet upload key is now configured locally at `~/.config/wispnet/play-upload-keystore.p12`, with a private mode-0600 `~/.config/wispnet/play-upload.env`. WispShell's key and workflows were inspected but not changed. Wispnet's four signing secrets are configured in `biplabs/wispnet_client`; the Google publisher credential still needs to be supplied. Keep a private backup of the keystore and its environment file together.

A preserved copy at `target/play-internal/wispnet-app.wispnet.android-v1-signed.aab` (also in `apps/flutter/build/app/outputs/bundle/release/app-release.aab`) is **signed**, version code **1**, package **app.wispnet.android**, and passed the signature/native/bundle checks. Earlier unsigned artifacts described above were the preceding checkpoint.

To rebuild with this key:

```sh
source "$HOME/.config/wispnet/play-upload.env"
scripts/build_android_release.sh --signed
```

Do these steps in your Play Console account:

1. Create **Wispnet** as a free app, with your chosen default language. The first uploaded bundle identifies package `app.wispnet.android`; it is separate from WispShell's `com.biplabs.wisp`.
2. Open **Test and release → Testing → Internal testing**, create a release, enable/configure Play App Signing, and upload `target/play-internal/wispnet-app.wispnet.android-v1-signed.aab`. Complete any blocking tasks the Console displays, then roll out to internal testing. This is an internal testing track, not Internal app sharing or production. Internal tests can start before the full store listing is complete; see [Google's internal-testing instructions](https://support.google.com/googleplay/android-developer/answer/9845334?hl=en).
3. Under **Internal testing → Testers**, add the Google account used on the tablet/phone, save the tester list and copy its opt-in link. Open that link while signed into the tester account, opt in and install from Play. A different Play certificate cannot update the existing debug-signed tablet app in place; the existing installation has not been removed. Switching that tablet requires a deliberate fresh install and new independent host approvals.
4. For later automated uploads, grant the existing WispShell publishing service account access to **Wispnet**, including app visibility and release-to-testing permission. Reuse the Google Cloud project where Play Developer API is already enabled. In Google Cloud, obtain the original service-account JSON you kept, or create an additional JSON key for that same service account if the original is unavailable. Download a new JSON key through **Google Cloud Console → IAM & Admin → Service Accounts → [the publishing account] → Keys → Add key → Create new key → JSON** ([Google instructions](https://docs.cloud.google.com/iam/docs/keys-create-delete)). GitHub does not reveal an existing secret's value. Do not put the JSON in this repo or chat.
5. Add that raw JSON as `ANDROID_PUBLISHER_CREDENTIALS` in **biplabs/wispnet_client → Settings → Secrets and variables → Actions → Repository secrets**. The four `WISPNET_UPLOAD_*` signing secrets are already installed; do not replace them with WispShell's key. Alternatively, from the local machine:

   ```sh
   gh secret set ANDROID_PUBLISHER_CREDENTIALS --repo biplabs/wispnet_client < /private/play-service-account.json
   ```

Google's [Publisher API setup](https://developers.google.com/android-publisher/getting_started) describes service-account access through Play Console Users and permissions. The first app/Console setup and credential grant are user-managed here. No upload has been claimed successful before a committed, verified track response.

## Subsequent internal-track uploads

The **Play internal upload** GitHub workflow builds/signs/validates and publishes only to `internal`; it cannot select production. It does not run the test suite on GitHub. Local `scripts/test.sh` and the pre-push hook remain the test gate.

After first Console setup and the publisher secret:

```sh
# Default next version: 1000 + workflow run number.
gh workflow run play-internal.yml --repo biplabs/wispnet_client
# For an app still requiring draft releases:
gh workflow run play-internal.yml --repo biplabs/wispnet_client -f release_status=draft
```

Automatic upload on relevant `main` pushes is initially disabled. To opt in after the first successful manual workflow, set repository variable `WISPNET_PLAY_AUTO_UPLOAD=true` (or `gh variable set WISPNET_PLAY_AUTO_UPLOAD --repo biplabs/wispnet_client --body true`). Publishing jobs serialize; version codes are configurable and monotonic by default. A manually supplied code must be unused and higher than earlier uploads.

Local upload also works with a private service-account JSON file:

```sh
# Default is dry-run: validates the signed artifact without any Play API calls.
uv run scripts/publish_android_internal.py apps/flutter/build/app/outputs/bundle/release/app-release.aab
# Once the app/account setup is complete, publish a new unused-version bundle:
GOOGLE_APPLICATION_CREDENTIALS=/private/play-service-account.json \
  uv run scripts/publish_android_internal.py apps/flutter/build/app/outputs/bundle/release/app-release.aab --publish
```

The uploader creates an edit, uploads the bundle, updates only the internal track, validates, commits, then checks the committed version/status in a fresh edit. Failed pre-commit work attempts to discard its edit. A commit timeout or failed post-commit verification is reported as uncertain, with no automatic retry. Inspect the Console before retrying such an outcome. Local regressions cover success, draft bootstrap status, aborted upload cleanup, unsupported statuses, uncertain commit and post-commit verification failure. Signed dry-run passed; live upload awaits the user's Console/credential setup.

The Play application ID is `app.wispnet.android`. The Java/JNI namespace remains `com.biplabs.wispnet`; it is an internal implementation name, not the installed package. The former development package is a separate installation; changing the application ID does not transfer its sandbox or identity.

## Active internal publishing and installation state

On 2026-10-16, the publisher credential was configured and version 1 was rolled out: the internal track reports `completed`, and Console reports available to internal testers. Automatic relevant-main-push uploads are enabled with repository variable `WISPNET_PLAY_AUTO_UPLOAD=true`. Documentation-only pushes do not trigger this workflow. These current settings supersede the bootstrap instructions above; do not re-upload version 1.

The app explicitly excludes all standard credential/device-protected private storage domains and external app storage from both cloud backup and device transfer, in addition to `allowBackup=false`. This protects the installation-specific identity, host pins and cached directory against platform-supported copying. New installations pair independently. This is not a claim about privileged OEM tools ignoring Android extraction rules; actual cross-device restore testing remains outstanding. See [Android backup behavior](https://developer.android.com/identity/data/autobackup).

The first internal test store link can take several hours to propagate. Join with the tester account and use that same account in Play Store. Internal testing allows testers in Canada or any other country, independent of production country availability. A completed upload does not prove a successful Play installation or optical camera check.

# Android and iOS viewer

The mobile app uses the shared React viewer and the native Rust `hub-remote`
client. Machines opens remote Hub connections; the paired computer owns session
history, indexing, and agent runtimes. The phone stores its own device identity,
saved machines, and preferences inside the app sandbox. Local history scanning,
hosting, project creation, and desktop translation are unavailable on mobile.

Pair with the Hub URL, the host's trusted machine reference, and its current
authenticator code. Returning to a saved machine reuses the device identity.
Remote message input remains subject to the host's control permission and the
selected session's capabilities. Mobile passkey sign-in is not implemented;
authenticator pairing is the available native onboarding flow.

## Shared prerequisites

Install Rust, Node.js, pnpm, and the relevant
[Tauri mobile prerequisites](https://v2.tauri.app/start/prerequisites/#configure-for-mobile-targets).
The shared frontend build also needs the browser crypto target and its pinned
bindings generator:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
cd apps/viewer
pnpm install --frozen-lockfile
pnpm run check
```

Tauri automatically merges `tauri.android.conf.json` or `tauri.ios.conf.json`
with the base config. Mobile windows are created by the Rust mobile entry point
without desktop minimum dimensions. The development CSP permits the Vite HMR
WebSocket on the development computer's LAN address; release builds retain the
base CSP and connect to Hub from Rust.

## Android

Install Android Studio or the command-line SDK tools, a compatible JDK, an SDK
platform, Build-Tools, Platform-Tools, and the side-by-side NDK. Set `JAVA_HOME`,
`ANDROID_HOME`, and `NDK_HOME` to those installations. Android API 24 is the
minimum runtime version; the checked-in Gradle project compiles against API 37.
Keep Android System WebView up to date for the shared UI's modern CSS and dialog
APIs.
Android 17 asks for nearby-device/local-network access so the app can reach a
paired computer directly. Denying it keeps public Hub relay access available;
a Hub or development server on the LAN requires that permission. The native
activity declares and requests the
[Android local-network permission](https://developer.android.com/privacy-and-security/local-network-permission).
Install the Rust targets before running the app:

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
pnpm run android:dev
```

The Android project lives in `src-tauri/gen/android` and is ready to use from a
fresh checkout. Reserve `pnpm run android:init` for deliberate regeneration;
preserve the native local-network permission handling, disabled device backup,
and keyboard resize settings when doing so. A connected device with USB
debugging enabled or an Android emulator is required for `android:dev`. Use the
Tauri CLI for builds so that Cargo, Gradle, frontend assets, and JNI configuration
stay coordinated:

```sh
pnpm run android:build -- --debug --apk --target aarch64
pnpm run android:build -- --aab
```

The first command creates a development APK for an ARM64 device; use `x86_64`
for an x86 emulator. Release distribution needs your own Android signing setup;
no signing keys or store credentials are included. See
[Tauri Android signing](https://v2.tauri.app/distribute/sign/android/).

## iOS

Use macOS with Xcode, its iOS SDK/simulator, and the Tauri iOS prerequisites.
iOS 16.4 is the minimum runtime version, covering the shared UI's native dialogs,
`inert` interaction blocking, `:has()` selectors, and color mixing. See the
[WebKit 16.4 feature notes](https://webkit.org/blog/13966/webkit-features-in-safari-16-4/).
From `apps/viewer` on that Mac:

```sh
pnpm run ios:init
pnpm run ios:dev
pnpm run ios:build
```

Initialization generates `src-tauri/gen/apple`. Choose a simulator for initial
development. A real device or distribution build requires the appropriate Apple
development team and provisioning; configure those locally using
[Tauri iOS signing](https://v2.tauri.app/distribute/sign/ios/). Neither a team ID
nor signing material is committed. iOS generation/build commands are only
available from the Tauri CLI on macOS.

## Device verification

Native verification on Linux passed the Android ARM64 Rust cross-check and full
debug APK build (frontend assets, Rust linking, Kotlin, resources, and packaging)
with NDK 29 and JDK 17. The development APK is
`apps/viewer/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`;
this build contains the ARM64 ABI. No emulator or physical-device pairing was
performed. iOS project generation, compilation, and device checks still require
a Mac with Xcode and have not been verified here.

Chromium layout checks passed at 320×568, 390×844, 412×915, 768×1024,
and 844×390. The checks exercised the shared conversation, session drawer
(search focus and Escape), Settings bounds, and expanded saved-machine details
with long addresses. No horizontal overflow remained. A simulated 380px visual
viewport with 44px top and 34px bottom insets kept the composer and Send visible.
These checks used component fixtures with mocked native API/crypto boundaries;
they did not exercise pairing or native WebViews. Viewport and model-picker
tests cover keyboard resize/panning, cleanup, and preserving pinch zoom.

Frontend tests and browser viewport checks are useful, but do not establish a
working native package. Before a release, verify these flows on both platforms:

- Start on a fresh installation, pair to an HTTPS Hub, open a remote session,
  restart the app, and reconnect using the saved machine.
- Browse long titles, expanded tools, images, code blocks, and Settings at phone
  widths in portrait and landscape; content must stay within the viewport.
- Open and close the session drawer, switch machines, and return to a selected
  session without leaving stale machine content visible.
- Type a multiline prompt with the software keyboard visible; the composer,
  send action, and last messages must remain reachable above the keyboard and
  home indicator. Send a prompt and answer an allowed host interaction.
- Background and restore the app, then switch between Wi-Fi and cellular.
  Reconnection must not replay submitted prompts. Mobile operating systems may
  suspend networking while the app is in the background.
- Revoke the device from the host and confirm that access stops; pairing a fresh
  device must require a new valid authenticator code.

Android/iOS signing, App Store/Play publication, push notifications, camera QR
scanning, and background execution are separate follow-ups.

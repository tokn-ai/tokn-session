# Apell viewer

iOS and Android use the Tokn Sessions `.apel` applet inside Apell. The standalone
Tokn mobile Tauri targets and their packaging workflow are removed. Pair the
applet as a new device; there is no import of the retired application's private
identity. Desktop remains a native Tauri application, and the browser entry
retains its existing passkey policy.

## Shared code

`apps/viewer/src` contains one React viewer: screens, timeline rendering,
composer, question cards, images, and bounded session display caches. An explicit
`ViewerHost` supplies device commands/events and capabilities independently of
viewport size. Desktop uses Tauri; `apps/apell` installs an Apell bridge adapter
and mounts that same viewer. The applet does not initialize local histories,
hosting, translation or provider processes.

`hub-client-core` owns shared pairing, Noise, request/response framing and SSE
validation. Native `hub-remote` uses its existing network and filesystem adapters.
`crates/apell-client` builds as a Wasmi-compatible Rust guest with host-injected
cryptographic entropy and no browser/WASI runtime imports. Apell provides generic
WebSockets, applet-private secure storage, randomness and timers. No Tokn-specific
service is added to the Apell host.

Device secrets and trusted machine pins stay in backend-only secure storage.
Viewer preferences use applet config. The frontend receives public metadata and
decrypted viewer responses, never the device private key. The applet uses durable
native-device trust rather than browser document/passkey sessions.

## Data and lifecycle

The host remains authoritative for session histories, RPC and permissions. The
existing subscription, latest-three-turn backward load, complete-group details,
tool details and inspector APIs are preserved. The shared frontend continues to
own display coverage and caches.

The guest decrypts bounded records and forwards acknowledged chunks across the
bridge instead of retaining full histories. One chunk per transfer can await the
frontend; response bodies and SSE frames retain viewer bounds without increasing
the guest's 64 MiB heap or the 8 MiB ABI limit. Failed or uncertain input is never
automatically replayed. Only the read-only event stream reconnects automatically.

Apell can keep the backend alive while its view is detached. A new view loads
preferences and reconnects to a remembered host; a fresh stream generation
replaces the previous view's delivery state. Process termination ends active
operations. Saved identity and trust survive restart, subject to the platform's
secure-store policy. Mobile OS background networking is not guaranteed.

The initial applet uses the encrypted Hub relay. Native desktop direct WebRTC
continues to work; adding a generic Apell direct carrier is a follow-up. Remote
viewing, images, permitted message/steer/cancel, model inspection and supported
question replies use the existing host capabilities. Local session creation and
runtime ownership remain desktop functionality.

## Build and install

Use an Apell checkout containing the generic backend network/secure-storage
services. Set `APELL_SOURCE` locally to that checkout; no machine paths enter the
package or repository. Cargo and CI pin the same merged Apell revision for the
guest/runtime and frontend SDK.

```sh
rustup target add wasm32-unknown-unknown
cd apps/viewer
pnpm install --frozen-lockfile
APELL_SOURCE=/path/to/apell pnpm run build:apell
```

The build produces `artifacts/tokn-session.apel` and its SHA-256 beside the
repository root. It bundles the existing viewer, remaps compiler source paths,
rejects unexpected WASM imports, and uses Apell's package validator/packer.
Install the file through Apell's **+ → Install from file**, then pair using the
Hub URL, trusted machine reference and authenticator code. Applet code updates
take effect in a fresh Apell run; restart Apell if that applet already ran.

For nightly installs and updates, choose **Install from GitHub** and enter
`tokn-ai/tokn-session@nightly`. Main CI publishes the tested package and SHA-256
after its checks pass. The stable asset name and prerelease channel let Apell
replace builds even when their applet version is unchanged.

## Verification

Run the shared viewer tests/build and Rust workspace checks. The portable core
tests native/guest entropy interoperability, framing, credits and SSE boundaries.
The Wasmi integration test uses the compiled guest with a synthetic Hub and
generic host services; it requires no provider credentials or real session data.
CI builds and executes this test explicitly:

```sh
cargo build --locked --release --target wasm32-unknown-unknown -p tokn-session-apell
TOKN_APELL_TEST_WASM=target/wasm32-unknown-unknown/release/tokn_session_apell.wasm \
  cargo test --locked -p tokn-session-apell --test wasmi -- --ignored
```

`TOKN_APELL_TEST_WASM` accepts an absolute path or a path relative to the workspace
root, independently of Cargo's per-crate test working directory.

On iOS/Android verify fresh pairing, remembered reconnect, session loading,
images, input, question replies, view reload, OS resume, network changes and host
revocation. Reconnect must obtain a fresh baseline without duplicating input.

# Connect your own machines

Open the Hub in a browser, or choose **Hub** in the Tokn app. Both connect
directly to a host through the Hub; a viewing-device daemon is not required.
The host verifies an authenticator code or its own passkey, then remembers the
device's encryption key. Remembered devices reconnect without another prompt.
Guest sharing is outside this flow.

```text
Browser (Rust/WASM) ─┐
                    ├⇄ Hub ⇄ outbound host connector → loopback viewer-api
App (Rust) ─────────┘
       └──────── end-to-end encrypted payloads ────────┘
```

The Hub routes encrypted records. Device private keys, authenticator seeds,
host passkey credentials, and content authorization stay on the endpoints.
The browser decrypts content itself. Browser code delivery trusts the Hub: a
malicious publisher of that UI could read decrypted content or pairing input.
Encryption protects traffic from the relay when the endpoint code is trusted.

## Build and run

From this checkout, build the host binaries and browser UI. The UI build also
compiles the shared Rust crypto crate to WASM; it needs the Rust WASM target and
the matching `wasm-bindgen-cli` version.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
cargo build -p tokn-session-hub -p tokn-viewer-api
pnpm --dir apps/viewer install --frozen-lockfile
pnpm --dir apps/viewer build
```

Start the Hub behind your HTTPS reverse proxy:

```sh
tokn-session-hub serve --public-url https://hub.example.com \
  --web-root ./apps/viewer/dist
```

The listener defaults to `127.0.0.1:5559`. Proxy WebSocket upgrades on
`/hub/v1/tunnel` and `/hub/v1/secure/*`. The public URL must match the browser
origin exactly. Hub administration at `/admin` is optional and uses separate
passkeys; it does not grant access to encrypted machine content. Paired hosts
register without administrator approval. See [Hub administration](hub.md).

On each host, run the API and connector in separate terminals:

```sh
tokn-viewer-api --api-only
tokn-session-hub connect --hub https://hub.example.com --name "Workstation"
```

First setup generates a UUID, dedicated keys, and an authenticator seed. Scan
the terminal QR, then copy the printed machine ID or full machine reference
(`UUID@host_public_key`). Manual authenticator setup uses SHA1, six digits, and
a 30-second period. Secrets print only to an interactive terminal. To display
them later over a trusted terminal or SSH:

```sh
tokn-session-hub authenticator
```

Open the Hub URL, or enter it in the app's Hub mode. Under **Connect a machine**,
enter the machine ID and current authenticator code. The host verifies the code
through password-authenticated key exchange; the relay does not receive the raw
code. The device saves the verified host encryption key only after authenticated
acknowledgment. Save the full reference from the connector or connection settings
for passkey sign-in on another device.

The connector saves its configuration, so later starts need only:

```sh
tokn-session-hub connect
```

The host's viewer API must still be running. Browsers remember device keys and
host pins in IndexedDB, scoped to their Hub origin; the native app keeps private
files. Clearing that storage makes it a new device. **Forget** removes a local
saved machine; it does not revoke that device on the host. **Change machine**
cancels requests and streams before selecting another machine.

For local development, serve with `--public-url http://localhost:5559` and connect
with `--hub http://localhost:5559 --insecure-loopback`. The saved development
setting permits HTTP only on loopback. From a checkout, use the binaries under
`./target/debug/` if they are not on `PATH`.

## Machine passkeys

After authenticator pairing, choose **Add a passkey**. The host stores and verifies
this credential, independently of Hub administrator passkeys. On another device,
enter the complete machine reference and choose **Sign in with machine passkey**.
The reference supplies the host key needed to authenticate the encrypted channel
before passkey login; a bare UUID does not establish that trust. Transfer the
reference through a trusted channel. The Hub cannot replace a saved host pin.

The host defaults the WebAuthn origin to the saved Hub URL. Use
`connect --passkey-origin https://hub.example.com` to configure it explicitly.
Passkeys require a stable HTTPS DNS hostname, or `http://localhost` for development;
numeric IP origins cannot be used as a passkey RP. Once credentials are enrolled,
the host rejects changing or removing their origin. Browser passkey availability
and credential synchronization depend on the browser and authenticator.

In the native app, Rust starts the ceremony on its encrypted host channel and
opens the trusted Hub `/passkey` page in the system browser. A user gesture runs
WebAuthn under that Hub origin. The page returns only the credential or cancellation
via a top-level form POST to the app's one-shot loopback callback. Rust finishes
the ceremony on the original encrypted channel; the browser does not proxy app
session traffic. The callback verifies its unpredictable path and the exact Hub
Origin. `/passkey` uses `Referrer-Policy: origin` for this form; other Hub pages
use `no-referrer`. Ceremony options travel in a URL fragment that the page clears.

## Host configuration and revocation

Host state defaults to `~/.tokn/hub`; use `--state-dir PATH` consistently for a
separate installation. Host files include `host.json`, `host-enrollment.key`,
`host-noise.key`, and `host-access.json` (authenticator, authorized devices,
passkeys, and persisted attempt limits). Back up identity and trust state with
the device's storage controls. Corrupt, missing, or insecure saved state fails
closed; Unix private files must have mode 0600 and belong to the current user.

Authorized devices can view all host sessions. Agent input requires the host's
saved control setting:

```sh
tokn-session-hub connect --allow-control
tokn-session-hub connect --allow-control=false
```

Use `--viewer-url` for another numeric loopback API origin, and provide
`TOKN_VIEWER_TOKEN` if the API requires it. That local token is not saved in
configuration or transmitted to the Hub or remote device.

List and remove authorized device keys on the host:

```sh
tokn-session-hub devices
tokn-session-hub forget-device --public-key CLIENT_PUBLIC_KEY
```

Removal denies new requests and closes live streams when the host checks its
state, every second. It cannot recall delivered content or undo accepted input.
An authenticator code or enrolled machine passkey can authorize a device again.

## One authenticator for several hosts

Hosts keep distinct UUIDs and asymmetric keys. To share only the authenticator
seed, export it and transfer the private file through a trusted channel:

```sh
tokn-session-hub authenticator --export-file ./authenticator.secret
# On a new host:
tokn-session-hub connect --hub https://hub.example.com \
  --totp-secret-file ./authenticator.secret
```

Exports create a new mode-0600 file and refuse overwrites. Imports require a
private Base32 file and work only before authenticator setup; alternatively use
`authenticator --import-file FILE` before the first `connect`. The Hub does not
synchronize seeds or trust records. Compromising any seed-holding host threatens
first pairing across that group; existing host pins remain fixed. Each host
consumes a successful code once per time step. Keep endpoint clocks correct.

## Protocol and limits

Signed host registration proves UUID/key possession and establishes routing,
not device authorization. UUID ownership and Hub removals persist. New host
registrations are limited to eight per minute and 64 active hosts, with a bounded
revocation ledger; reconnects bypass the new-host limit. An exposed Hub still
needs admission and availability controls at its reverse proxy.

Pairing uses RustCrypto `spake2` 0.4 and HKDF/HMAC confirmations binding the
machine ID, both Noise keys, time step, and handshake. Hosts persist five OTP
attempts per five minutes and consume successful time steps atomically. Passkey
ceremonies have a separate persisted limit of 20 starts per five minutes, expire
after five minutes, and bind a single-use challenge to the machine, device key,
and original Noise channel. The host accepts at most 64 device keys and 32 passkeys.
An unpaired encrypted peer may only attempt passkey authentication; it cannot
read content or register credentials.

Normal requests use fresh `Noise_IK_25519_ChaChaPoly_BLAKE2s` channels and host-local
authorization. There is no plaintext fallback or automatic request retry. The
SPAKE2 composition is experimental, unaudited, and not RFC 9382 wire-compatible.

The installed `tokn-session-hub client` helper is optional compatibility. In
paired mode, it terminates encryption in a local Rust process and serves
`/connect` with a local bearer link. The [owner-signed grant workflow](hub-e2ee.md) and explicit
`connect --trusted-hub` plaintext mode also remain available.

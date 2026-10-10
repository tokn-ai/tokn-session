# Connect your own machines

Open the Hub in a browser, or choose **Hub** in the Tokn app. Both connect
directly to a host through the Hub; a viewing-device daemon is not required.
The native app pairs with an authenticator code and retains its device authorization.
Browsers use that code to enroll a machine passkey, then require a passkey to sign
in. Each new tab, reload, or reopened window requires another browser sign-in.
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
(`UUID@host_public_key`). Manual authenticator setup uses SHA-256, six digits, and
a 30-second period. Secrets print only to an interactive terminal. To display
them later over a trusted terminal or SSH:

```sh
tokn-session-hub authenticator
```

Open the Hub URL, or enter it in the app's Hub mode. On **Connections**, choose
**Authenticator code** and enter the machine address or UUID and current code. The host verifies the code
through password-authenticated key exchange; the relay does not receive the raw
code. The device saves the verified host encryption key only after authenticated
acknowledgment. Save the full reference from the connector or connection settings
for passkey sign-in on another device.

The connector saves its configuration, so later starts need only:

```sh
tokn-session-hub connect
```

The host's viewer API must still be running. Browsers save only machine metadata
and verified host pins in IndexedDB. A tab holds its private encryption key in
memory and destroys it on navigation or close; restored back/forward-cache pages
reload before sign-in. Backgrounding a tab does not lock it. A live tab can
reconnect without another prompt until its eight-hour authorization expires.
The native app keeps its persistent private key in owner-only files. Clearing
that storage makes it a new device. **Forget** removes a local
saved machine; it does not revoke that device on the host. **Manage connections**
cancels requests and streams before selecting another machine.

## Readable machine addresses

The Hub administrator can create a namespace such as `clouds` at `/admin`, then
assign a registered encrypted machine a name such as `macbook`. Its address is
`clouds:macbook`, scoped to that Hub. Names use 1–63 lowercase ASCII letters or
digits with internal hyphens. A namespace is a managed name, not a user account;
the Hub still has one administrator. Connector `--name` remains its display name.

Addresses are permanent: an address cannot move to a different UUID, and revoking
the host does not free its name. Only administrator authentication can assign
names; registering a connector cannot claim another namespace. Public lookup is
exact, works while a host is offline, and reveals the address, UUID, display name,
and online status to anyone who knows the address. There is no public machine list.

The Hub directory is trusted to associate an address with a UUID. Lookup supplies
no host encryption key. OTP pairing still verifies the host through PAKE, and
remembered devices retain their UUID and verified key. Opening a saved machine
uses that identity directly; it does not need another directory lookup. Saved
address mappings cannot silently change to another machine.

For local development, serve with `--public-url http://localhost:5559` and connect
with `--hub http://localhost:5559 --insecure-loopback`. The saved development
setting permits HTTP only on loopback. From a checkout, use the binaries under
`./target/debug/` if they are not on `PATH`.

## Machine passkeys

Browser pairing automatically enrolls a passkey and then performs a separate
passkey sign-in. A TOTP-paired browser key can only enroll credentials, for five
minutes; it cannot read sessions. In the app, **Add a passkey** is optional.
The host stores and verifies credentials independently of Hub administrator
passkeys. On another device,
choose **Passkey**, enter the complete machine reference, and sign in.
The reference supplies the host key needed to authenticate the encrypted channel
before passkey login; a bare UUID or readable address does not establish that trust.
The reference may use `clouds:macbook@host_public_key` after an address is assigned.
An already remembered machine can use its readable address with its saved pin. Transfer the
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
An authenticator code can pair an app again or bootstrap browser enrollment.
An enrolled machine passkey can issue another expiring session authorization.
Native keys already paired with TOTP retain persistent access after passkey use.
Passkey-only sign-in on an unpaired app issues an expiring grant too; use
authenticator pairing to establish persistent app authorization.

## Upgrading existing hosts

Pairing protocol v2 uses HMAC-SHA-256 and explicit authorization purposes. Upgrade
both host and clients together. Legacy host state fails closed because its
unclassified keys could belong to a browser. From a trusted local terminal:

```sh
tokn-session-hub authenticator --upgrade-sha256
```

Use the same `--state-dir` as the connector. This rotates the authenticator seed
and removes all legacy device grants. Rescan the new SHA-256 QR and pair app
devices again. Host UUID/keys, enrolled passkeys, their origin, and attempt limits
are preserved; existing passkeys can sign in browsers. Repeating the upgrade on
v2 state does not rotate it again. For noninteractive setup, supply
`--export-file NEW_PRIVATE_FILE`, then import it with SHA-256 explicitly selected.
Browser storage migration deletes old persisted private keys but retains host
pins and machine names. Bare Base32 seeds do not encode the algorithm; setup QR
URIs include `algorithm=SHA256`.

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
machine ID, both Noise keys, client authorization purpose, time step, and handshake.
Purpose is authenticated enrollment intent, not platform attestation: possessing
a TOTP code permits persistent app enrollment. Passkey login cannot promote a
browser/session grant into persistent app trust. Hosts persist five OTP
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

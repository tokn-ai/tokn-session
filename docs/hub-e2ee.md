# Encrypted Hub access and sharing

For normal setup, use [machine pairing](hub-pairing.md). A browser or native app
connects directly through the Hub; shared Rust crypto runs in browser WASM or
the native app. The host verifies an authenticator code or a host-owned passkey
and remembers the device key. New-device passkey login requires a trusted
machine reference containing the host's encryption public key. Hub administrator
passkeys at `/admin` remain separate from host content authorization.

```text
Browser or app ⇄ Hub ⇄ outbound host connector → loopback viewer-api
       └──── end-to-end encrypted payloads ────┘           ↓
                                                 viewer-core / Relay
```

The Hub sees enrollment metadata, IP addresses, connection timing, and
ciphertext sizes. The relay cannot decrypt protected requests, responses,
credentials, or session content. Browser code is served by the Hub and is a
trusted endpoint dependency: a malicious UI publisher could access browser
plaintext. The app encrypts and decrypts its own traffic. Its passkey ceremony
uses the trusted Hub browser origin and a one-shot loopback form callback;
session traffic remains on the app's encrypted host connection. Neither path
requires a viewing-device proxy. The Hub can deny service.

Sharing is deferred from normal pairing. The remainder of this page documents
the earlier explicit owner-key and signed-grant interface, retained for
compatibility. This interface uses the optional installed Rust `client` helper
as its encryption endpoint; a locally served browser UI talks to that helper
using a loopback bearer credential. The host verifies an owner-signed grant
bound to the recipient's device key. Start the Hub and set up its administrator
using [hub.md](hub.md) for this legacy enrollment workflow.

## Establish trust

Examples use `tokn-session-hub`; from this checkout, substitute
`cargo run -p tokn-session-hub --`. Build the local UI with
`pnpm --dir apps/viewer build`. For a local development Hub, use
`--hub http://localhost:5559 --insecure-loopback` on the host and client.

On the trusted owner device:

```sh
tokn-session-hub keygen --kind owner --key-file ./owner.key
```

Only the public key is printed. Keep `owner.key` on the owner device, never
the Hub. Independently transfer and verify its public key on each host and
recipient (for example through trusted SSH or an in-person comparison).
Accepting the trust root only from the VPS would permit key substitution.

On each session host, initialize revocations **once**, then run the API and
connector in separate terminals:

```sh
printf '[]\n' > ./revoked-grants.json
tokn-viewer-api --api-only
tokn-session-hub connect --hub https://hub.example.com --name "Workstation" \
  --owner-public-key OWNER_PUBLIC_KEY \
  --identity-file ./host-enrollment.key --noise-key-file ./host-noise.key \
  --revocations-file ./revoked-grants.json
```

Never replace an existing revocation file with `[]`. A configured missing or
invalid file fails closed. Compare the printed pairing code in the Hub and
approve its connection. Separately verify the printed host ID and encryption
public key on the owner device before signing grants. Hub pairing is relay
admission, not an independent verification of encryption identity.

The API stays on loopback. Use `--viewer-url` for another numeric loopback HTTP
address, and provide `TOKN_VIEWER_TOKEN` if the API requires it. That local
token never reaches the Hub or recipient. Forwarding disables redirects,
environment proxies, and automatic retries.

On the recipient's machine:

```sh
tokn-session-hub keygen --kind device --key-file ./client-noise.key
```

Transfer its public key to the owner through a verified channel. Private keys
stay on their endpoints. Generated identity files are owner-only on Unix;
existing symlinks and insecure files are rejected. Back up keys with the
device's storage controls. Losing the owner signing key requires reprovisioning
host trust; the VPS cannot recover or replace it.

## Sign a grant

On the owner device, authorize two complete sessions for one day:

```sh
tokn-session-hub grant --owner-key-file ./owner.key \
  --host-id HOST_ID --host-public-key HOST_ENCRYPTION_PUBLIC_KEY \
  --recipient-public-key RECIPIENT_PUBLIC_KEY \
  --session-key SESSION_KEY_A --session-key SESSION_KEY_B \
  --expires-in 86400 --out ./share.json
```

Get exact `session_key` values from the host's authenticated local
`POST /api/v1/list_sessions` and `list_session_children` responses, or through
your own full-host encrypted client. They are viewer keys, not provider IDs.
Grants allow up to 128 keys and 32 KiB of serialized grant data. Descendants
and future sessions are not included implicitly. Selected sessions remain
live, including later messages. Session keys encode local
source identity/path; grants and session metadata are not path-redacted.

Replace the `--session-key` arguments with `--all-sessions` for full-host
access, including future sessions. Viewing is the default. Agent input
requires both an all-session grant with `--allow-control` and a connector
started with `--allow-control`. Selected-session grants cannot control an
agent, whose tools may access files beyond the visible conversation.

Transfer `share.json` to the recipient. Copying it alone does not confer
access because it is bound to a device key, but its metadata is sensitive.
The recipient starts the installed client:

```sh
tokn-session-hub client --hub https://hub.example.com \
  --owner-public-key OWNER_PUBLIC_KEY --identity-file ./client-noise.key \
  --grant-file ./share.json --web-root ./apps/viewer/dist
```

Open the local URL it prints. Its generated bearer token stays in browser
memory; the Rust process holds the encryption key and serves the UI
from its local `--web-root`. The server binds only to
numeric loopback and checks Host, Origin, and bearer credentials. One process
opens one host grant; multiple hosts share the same Hub endpoint. Recipients
do not need the owner's Hub passkey. Key/grant management currently uses the CLI.

The host scopes catalog search, pagination, trees, navigation, history, and
details before responding. Guest cache leases are recipient-scoped and read
markers do not modify the owner's state. Global settings, errors, and indexing
counts are suppressed; fixed five-second refresh events keep shared views
live without exposing global event activity. A complete shared conversation
can itself contain other session references, secrets, and native details;
this is not content redaction. Hosts must remain online; there are no offline
encrypted snapshots in this version.

## Revoke

On the host, revoke the ID printed when creating the grant:

```sh
tokn-session-hub revoke --revocations-file ./revoked-grants.json \
  --grant-id GRANT_ID
```

The file is atomically updated. The host checks expiry and revocations before
dispatch and every second during requests and streams. Revocation takes
effect when the host observes it and cannot recall plaintext already received
or undo accepted agent input. Without `--revocations-file`, only grant expiry
and stopping/reconfiguring the connector withdraw access; keep the file
configured when sharing. Issuing a replacement does not revoke the old grant.
Keep endpoint clocks correct.

Hub host removal disconnects the relay and requires reapproval. It is an
availability control; host-local revocation withdraws a recipient's permission.
Hub login/logout does not control independently authorized encrypted clients.

## Protocol boundaries

The standard `Noise_IK_25519_ChaChaPoly_BLAKE2s` protocol is implemented by
`snow`. An independently trusted Ed25519 owner key verifies domain-separated,
typed grants binding host ID, host encryption key, recipient encryption key,
scope, control permission, expiry, and grant ID. The host authenticates the
recipient's Noise static key. No application data is accepted in the replayable
first handshake flight. Traffic keys are derived only on endpoints.

Each request/SSE stream has a fresh channel. Noise records, queues, channels,
and request bodies are bounded; requests up to 1 MiB are fragmented, and
response credits are encrypted. Tampering, reordering, replay, or a bad pin
ends the channel. Disconnection never retries HTTP requests or agent input;
a lost response can mean delivery is uncertain.

Protected connectors reject plaintext frames even from a malicious Hub.
There is no downgrade. The explicit `connect --trusted-hub` option preserves
the original plaintext browser-through-Hub mode; that Hub can read content and
authorize requests. `--owner-public-key` selects the legacy grant mode; without
either compatibility option, `connect` uses host-verified machine pairing.
Upgrade Hub and connectors together for the new encrypted frame types.

Keep HTTPS termination for remote operation. Proxy WebSocket upgrades on
`/hub/v1/tunnel` and `/hub/v1/secure/*`, and apply upgrade rate limits. Secure
channels have host-side cryptographic admission instead of Hub bearer login;
the Hub bounds anonymous channels globally and per host. Browser channels accept
only the configured public UI Origin; native channels omit Origin. The Hub stores
its administrator passkey credentials and host registration identities. Machine
passkey credentials, device authorization, encryption private keys, owner signing
keys, and grant revocations stay on endpoints. `/api/v1/shared` is a host-local
adapter with the existing local API authentication; it is excluded from
remote route allowlists and receives only connector-verified scope.

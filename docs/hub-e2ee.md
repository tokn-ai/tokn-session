# Encrypted Hub access

For normal setup, use [machine pairing](hub-pairing.md). A browser or native app
connects directly through the Hub; shared Rust crypto runs in browser WASM or
the native app. The host verifies an authenticator code or a host-owned passkey
and grants persistent app access or an expiring browser session. Browsers use
TOTP only for passkey enrollment and require sign-in in every new tab or reload.
New-device passkey login requires a trusted
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

Use `tokn-session-hub register` to claim a machine address under a Hub account,
then `connect` to open its tunnel. See
[Hub account registration](hub.md#hub-accounts-and-cli-login).

## Protocol boundaries

The standard `Noise_IK_25519_ChaChaPoly_BLAKE2s` protocol is implemented by
`snow`. The client authenticates the saved host encryption key. The host verifies the
viewing device's key and checks its authorization. Both endpoints derive their
traffic keys after the handshake.

Each request/SSE stream has a fresh channel. Noise records, queues, channels,
and request bodies are bounded; requests up to 1 MiB are fragmented, and
response credits are encrypted. Tampering, reordering, replay, or a bad pin
ends the channel. Disconnection never retries HTTP requests or agent input;
a lost response can mean delivery is uncertain.

Keep HTTPS termination for remote operation. Proxy WebSocket upgrades on
`/hub/v1/tunnel` and `/hub/v1/secure/*`, and apply upgrade rate limits. Secure
channels have host-side cryptographic admission instead of Hub bearer login;
the Hub bounds anonymous channels globally and per host. Browser channels accept
only the configured public UI Origin; native channels omit Origin. The Hub stores
its administrator passkey credentials and host registration identities. Machine
passkey credentials, device authorization, and encryption private keys stay on
endpoints.

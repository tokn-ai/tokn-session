# Session Hub

For the default encrypted workflow, use [authenticator pairing](hub-pairing.md).
Hosts register automatically and verify codes locally; passkeys are optional
Hub administration. The earlier [signed-grant workflow](hub-e2ee.md) remains
available separately.
The setup below describes the explicit `--trusted-hub` compatibility mode and
the passkey administration available alongside encrypted access.

`tokn-session-hub` gives a browser one endpoint for multiple hosts. The Hub
authenticates its owner with passkeys, approves host identities, and forwards
the existing viewer HTTP/SSE API through outbound host connections.

```text
Browser → Hub /hosts/<host_id>/api/v1/… → outbound host tunnel → viewer-api
                                                               ↓
                                                       local viewer-core
                                                       and Relay feed
```

The existing `tokn-session-relay` remains the local provider event feed.
The Hub does not index histories or replicate sessions. Each host owns its
catalog and validates session keys using the existing viewer API.

## Local setup

Build the viewer, then start a local development Hub:

```sh
pnpm --dir apps/viewer install --frozen-lockfile
pnpm --dir apps/viewer build
cargo run -p tokn-session-hub -- serve
```

Open the setup link printed by the Hub. It contains a generated bootstrap
credential in the URL fragment, which the browser removes before rendering.
Create the owner's first passkey. Use `http://localhost:5559` for this setup;
an IP address cannot serve as the WebAuthn relying-party domain.
Subsequent logins use a passkey. An authenticated owner can add another passkey
from the host screen; keep a second authenticator available for recovery.
There are no user-chosen passwords or email/SMS/TOTP fallback logins.

On each session host, run the existing API and a connector:

```sh
cargo run -p tokn-viewer-api -- --api-only
cargo run -p tokn-session-hub -- connect \
  --hub http://localhost:5559 --insecure-loopback --trusted-hub --name "My workstation"
```

The connector prints a pairing code. In the Hub, compare the pending host's
name and code with that terminal before approving it. The code identifies the
enrollment; the authenticated owner's approval grants access. Select the
approved host to open its viewer. Host switching cancels the previous host's
requests and subscriptions.

Connectors allow viewing by default. To permit agent input, start the connector
with `--allow-control` and approve that requested access. A previously approved
view-only identity cannot gain control simply by restarting with this option:
revoke it and explicitly approve the new enrollment. A connector started
without `--allow-control` always enforces viewing-only access, even if the Hub
previously approved control. Viewer bookkeeping, such as read markers and cache
leases, is included in viewing access.

## Remote operation

Give the Hub a stable HTTPS origin and terminate HTTPS/WebSocket connections
with your own reverse proxy:

```sh
cargo run -p tokn-session-hub -- serve \
  --public-url https://hub.example.com
```

The listener defaults to `127.0.0.1:5559`. Forward the public origin's paths to
it, including WebSocket upgrades on `/hub/v1/tunnel`; allow streaming responses
without proxy buffering. The Rust listener speaks HTTP. Keep that backend
private, and expose the HTTPS terminator. Passkeys are tied to the configured
domain: choose it before registering the owner.
Apply request-rate limits at the public proxy to unauthenticated login and
tunnel-handshake endpoints. Internal queue limits bound resource use; they do
not provide protection against sustained denial-of-service traffic.

On a remote host:

```sh
cargo run -p tokn-session-hub -- connect \
  --hub https://hub.example.com --trusted-hub --name "Build server"
```

Only the connector needs outbound connectivity. The local viewer API stays on
loopback. Set `--viewer-url` for a different numeric loopback HTTP address. If
the local API requires a token, supply `TOKN_VIEWER_TOKEN` to the connector as
well as the API; this local token is never sent to the Hub or browser.
Redirects and environment proxies are disabled for local forwarding.

This is a **trusted Hub**: it can read forwarded requests and responses. TLS
protects the client-to-Hub and host-to-Hub connections; this version does not
provide end-to-end encryption in this compatibility mode. Use the
[encrypted client](hub-e2ee.md) for an untrusted Hub. The implementation
creates no hosting resources or deployments.

## Identity and lifecycle

The initial version has one owner per Hub. The owner can access its approved
hosts, approve enrollments, revoke hosts, and add passkeys. Separate users,
team permissions, and unattended client credentials are not implemented.

The administrator can reserve namespaces and assign permanent `username:host`
addresses to registered encrypted machines at `/admin`. These are Hub-local
names, not separate user accounts or content permissions. The directory exposes
only exact address lookup and never supplies Noise keys. Addresses remain reserved
after revocation; their UUID mapping cannot be reassigned. See
[readable machine addresses](hub-pairing.md#readable-machine-addresses).

The Hub persists public passkey credentials and approved hosts in
`~/.tokn/hub/state.sqlite` (`--state-path` overrides it). The connector generates
and persists its Ed25519 private key in `~/.tokn/hub/host.key`
(`--identity-file` overrides it). Use a separate identity file for each Hub.
On Unix, state/key files are restricted to their owner. Protect and back up
these files using the host's normal account/storage controls.

Each tunnel starts with a fresh Hub challenge. The connector signs the
challenge and enrollment attributes; the Hub verifies possession of the
approved key. A host name is a label, not an identity. Revocation closes the
active tunnel and requires fresh approval for subsequent access.

Browser bearer sessions last one hour, live only in memory, and disappear on
reload. Logout or expiry closes active requests and event streams. Restarting
the Hub invalidates browser sessions and outstanding authentication ceremonies;
approved hosts reconnect using their persisted keys. Losing all passkeys has
no remote recovery bypass; preserve an additional passkey before that happens.

## Forwarding contract

- `GET /hub/v1/auth/status` detects a Hub and reports owner setup state.
- `/hub/v1/auth/register/{start,finish}` and `/login/{start,finish}` implement
  server-held, expiring WebAuthn ceremonies. Auth POSTs require the configured
  browser `Origin`; `/hub/v1/auth/logout` revokes the bearer session.
- `GET /hub/v1/hosts` returns approved hosts with connectivity and effective
  `view`/`control` access, durable encrypted-only status, and an optional
  `machine_address`. `DELETE /hub/v1/hosts/<host_id>` revokes one.
- Authenticated `GET /hub/v1/namespaces` lists namespaces; `POST` with
  `{"username":"clouds"}` creates one. Authenticated
  `POST /hub/v1/namespaces/clouds/machines/macbook` with `{"host_id":"UUID"}`
  assigns a registered encrypted machine its permanent address.
- Public `GET /hub/v1/resolve/clouds/macbook` returns `host_id`, `machine_address`,
  `name`, and `online`. No directory key establishes encryption trust.
- `GET /hub/v1/enrollments` lists pending connections. Authenticated
  `POST /hub/v1/enrollments/approve` accepts `{"pairing_code":"…"}`.
- `/hosts/<host_id>/api/v1/<command>` forwards the existing viewer API.
  The browser's Hub bearer credential stays at the Hub.
- `/hub/v1/tunnel` is the native connector's versioned WebSocket transport.

Forwarding uses an exact viewer-route allowlist, bounded bodies, bounded
in-flight requests and streaming queues. The connector independently checks
paths and control permission. It never accepts an arbitrary destination URL
from a client. Host disconnection fails outstanding requests, and reconnecting
does not replay them. In particular, a lost response to agent input can mean
delivery is uncertain; check the session rather than automatically resending.
SSE reconnection retains the viewer's authoritative catalog/timeline refresh
behavior. Host IDs namespace routing; session keys remain local to their owner.

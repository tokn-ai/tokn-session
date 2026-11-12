# Session Hub

`tokn-session-hub` gives the browser and app one endpoint for multiple
machines. Every connected machine has an address owned by a Hub account. Machines verify
authenticator codes locally and browsers unlock sessions with a host-owned
passkey. See [authenticator pairing](hub-pairing.md) for the encrypted workflow.
The Hub administrator uses a separate passkey to manage host routes.

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
Subsequent owner logins use a passkey. An authenticated owner can add another
passkey from the host screen and keep a second authenticator for recovery.

Configure [email delivery](#hub-accounts-and-cli-login) before creating an
account. Sign in on the session host and register its machine name.

```sh
cargo run -p tokn-session-hub -- auth login --hub http://localhost:5559
cargo run -p tokn-session-hub -- connect \
  --hub http://localhost:5559 --insecure-loopback --machine workstation
```

Keep `tokn-viewer-api --api-only` running in a separate terminal. Pair the machine
from the gallery with its authenticator code. A connector defaults to viewing
access. Start it with `--allow-control` to allow agent input for authorized
devices.

## Hub accounts and CLI login

Set `TOKN_HUB_ACCOUNT_FROM` to enable account signup and login by email. The Hub
delivers codes through a sendmail-compatible executable. It defaults to
`/usr/sbin/sendmail`; set `TOKN_HUB_ACCOUNT_SENDMAIL` to an absolute executable
path to use your configured mail relay. The service user needs permission to use
the relay. With email enabled, anyone with a verified address can choose an
available username and create an account on this Hub.

Open **Hub account** in the browser, or start a CLI login:

```sh
tokn-session-hub auth login --hub https://hub.example.com
tokn-session-hub auth status
tokn-session-hub auth logout
```

The browser keeps your Hub account signed in for 90 days and renews that period
when you return to the Hub, at most once per day. Sign out to end that account
session early. Each machine still requires its own passkey unlock in every new
or reloaded tab.

The CLI prints a device code and opens the Hub's account page. Sign in if prompted,
then approve that code. The CLI saves its revocable credential in private local
storage for the selected Hub. For automation, create a named API key in the
account page with machine listing or registration scope. Supply it on stdin to
`auth login --with-token --hub https://hub.example.com`.

Signing in lets you list the account's machines. To add this computer, run the
registration command on it with a machine name. The connector proves it owns
the machine's enrollment key.

```sh
tokn-session-hub connect --hub https://hub.example.com --machine macbook
```

For account `alice` this reserves `alice:macbook` on that Hub. Add
`--register-only` while app hosting is stopped to make the assignment and then
start hosting from the desktop app. Subsequent connector starts use the saved
host key. The browser gallery combines the signed-in account's machines with
saved pairings and discovered machines. Pairing also reveals the account's
discovery group to a viewer who has no account session. Each machine verifies
access to its own sessions. Browser access uses a machine passkey.

Every host needs an account machine assignment before it can connect. An
existing encrypted host can claim an address with `--machine` using its saved
UUID and key. Configure email delivery to create accounts and sign in by email.
During rollout, stop any old unclaimed connectors to close their existing direct
WebRTC connections. The updated Hub rejects their next registration.

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
tokn-session-hub auth login --hub https://hub.example.com
tokn-session-hub connect --hub https://hub.example.com --machine build-server
```

Only the connector needs outbound connectivity. The local viewer API stays on
loopback. Set `--viewer-url` for a different numeric loopback HTTP address. If
the local API requires a token, supply `TOKN_VIEWER_TOKEN` to the connector as
well as the API; this local token is never sent to the Hub or browser.
Redirects and environment proxies are disabled for local forwarding.

## Identity and lifecycle

Each Hub has one administrator, who can revoke hosts and add administration
passkeys. Account holders manage their own machine addresses and automation keys.

The Hub account's username supplies the namespace for every machine address.
Public lookup resolves an exact address to a machine UUID and presence.
Addresses remain reserved to the original UUID after revocation. Clients
verify Noise keys through machine pairing. See
[readable machine addresses](hub-pairing.md#readable-machine-addresses).

The Hub persists public passkey credentials and approved hosts in
`~/.tokn/sessions/hub/state.sqlite` (`--state-path` overrides it). The connector generates
and persists its Ed25519 private key in `~/.tokn/sessions/hub/host-enrollment.key`.
Use a separate state directory for each Hub.
On Unix, state/key files are restricted to their owner. Protect and back up
these files using the host's normal account/storage controls.

Each tunnel starts with a fresh Hub challenge. The connector signs the
challenge and enrollment attributes; the Hub verifies possession of the
approved key. A host name is a label, not an identity. Revocation closes the
active tunnel and removes access through that registration.

Administrator browser sessions last one hour and live in memory until reload.
Restarting the Hub invalidates those sessions and outstanding authentication
ceremonies. Claimed hosts reconnect using their saved keys.

## Forwarding contract

- `GET /hub/v1/auth/status` detects a Hub and reports owner setup state.
- `/hub/v1/auth/register/{start,finish}` and `/login/{start,finish}` implement
  server-held, expiring WebAuthn ceremonies. Auth POSTs require the configured
  browser `Origin`; `/hub/v1/auth/logout` revokes the bearer session.
- `GET /hub/v1/hosts` returns claimed machines with their addresses,
  connectivity, and effective `view` or `control` access.
  `DELETE /hub/v1/hosts/<host_id>` revokes one.
- Public `GET /hub/v1/resolve/alice/macbook` returns `host_id`, `machine_address`,
  `name`, and `online`. No directory key establishes encryption trust.
- A discovery request with `X-Tokn-Discovery-Addresses: 1` also receives
  `machine_address` for entries owned by the account namespace.
- Public `POST /hub/v1/presence` accepts `{"host_ids":["UUID"]}` for up to 128
  unique canonical machine IDs and returns `host_id` and `online` for those IDs.
  A request with `X-Tokn-Presence-Versions: 1` also receives `connector_version`
  when the active connector reports it. Unknown, unclaimed, and revoked hosts are offline.
  Browser origins are checked and responses are not cached.
- `/hub/v1/tunnel` is the native connector's versioned WebSocket transport.
  Its upgrade exchanges package versions through `X-Tokn-Connector-Version`
  on the request and `X-Tokn-Hub-Version` on the response. The Hub retains the
  connector version with its authenticated live connection.
- `/hub/v1/secure/<host_id>` relays encrypted records to a connected machine.

Forwarding uses an exact viewer-route allowlist, bounded bodies, bounded
in-flight requests and streaming queues. The connector independently checks
paths and control permission. It never accepts an arbitrary destination URL
from a client. Host disconnection fails outstanding requests, and reconnecting
does not replay them. In particular, a lost response to agent input can mean
delivery is uncertain; check the session rather than automatically resending.
SSE reconnection retains the viewer's authoritative catalog/timeline refresh
behavior. Host IDs namespace routing; session keys remain local to their owner.

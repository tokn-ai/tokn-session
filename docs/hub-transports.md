# Direct connections and encrypted relay fallback

Browser and native app clients open the selected machine through the Hub, then
try a direct WebRTC connection in the background. The connection panel shows
**Direct** or **Relayed**, with encryption shown separately. A failed attempt
keeps the encrypted Hub relay usable. A later direct failure returns new
requests to that relay. Background negotiation retries after 30 seconds while
the machine remains open; reopening starts a new attempt immediately.

The Hub carries discovery, pairing/passkey ceremonies, and encrypted connection
negotiation. SDP and configured ICE servers travel inside fresh pinned-host
Noise IK channels. The host authorizes the device before allocating a peer.
Established direct peers belong to the connector's lifetime and survive a Hub
tunnel disconnect. The Hub still serves trusted browser code.

WebRTC adds DTLS, but application encryption remains Noise IK on **every**
ordered, reliable DataChannel. Each channel carries one existing HTTP exchange,
including its authenticated body and bounded response credits. A peer is bound
to the device key that authenticated negotiation. Host pins, browser session
expiry, per-request authorization, periodic revocation checks, route and control
restrictions apply on both carriers. Pairing/authentication stays on the Hub
carrier. Legacy signed-grant and trusted-Hub flows retain their previous routes.

Rust `hub-transport::RecordTransport` and the browser `RecordTransport` interface
carry bounded ordered byte records. Relay and WebRTC adapters implement that
contract; encryption, authorization and viewer semantics sit above it. A future
native tunnel can replace the carrier without changing the security protocol.
Peers, channels, record queues, send buffers and negotiation times are bounded.
Peers retire after 512 exchanges to bound upstream channel descriptor retention.
Existing requests drain before the old peer closes; new work uses the relay until
a replacement has authenticated. Closing the machine cancels both active and
retiring peers.
Each encrypted record is at most 65,535 bytes, and response/request chunks remain
32 KiB. Peers reject a negotiated SCTP message limit too small for those records.

Once direct health has passed a fresh Noise/device authorization exchange, new
requests use that peer. In-flight requests keep their original carrier. Live
event streams restart on a path change and use the existing viewer recovery
events. Failed or uncertain commands are reported to the caller and **never
automatically replayed** through another carrier.

Host device authorization controls active direct access. Revoking a Hub
registration removes discovery/relay routing; revoke the device on the host or
stop its connector to end an established direct connection.

## ICE configuration

No third-party STUN or TURN service is enabled by default. Local/LAN candidates
can establish direct connections without one. For NAT traversal, configure a
STUN server you operate or have chosen:

```sh
tokn-session-hub connect --stun-server stun:stun.example.com:3478
```

The host saves this list in `host.json` and provides it only after authenticated
negotiation. Up to eight `stun:` URLs are accepted; credentials, `stuns:` and TURN
URLs are rejected. This WebRTC backend supports UDP STUN discovery. Omit the option to retain the saved list, or pass
`--stun-server` without values to clear it. UDP blocking and some NAT combinations
still require the encrypted Hub relay. The loopback viewer API remains private;
WebRTC manages its own ICE sockets, without a new public viewer HTTP listener.

## Local verification

`hub-transport` tests negotiate real native peers and exchange bounded records.
Host/native integration tests run an isolated Hub, connector, and synthetic API,
cover large encrypted requests, control restrictions, device identity binding,
revocation, remembered access, and continued direct reads after Hub shutdown.
Browser unit tests cover authenticated upgrades, unavailable ICE, cancellation,
event recovery, and no replay of uncertain input.

`cargo run -p tokn-hub-remote --example webrtc_smoke` starts a temporary synthetic
fixture on loopback ports 15578/15579 for browser interoperability checks. Its
`/smoke/authorize` endpoint deliberately grants an ephemeral test key without
pairing; it exists only in this example, uses disposable state, and never reads
real sessions. The example allows the local Vite origin on port 1447.

To run the real browser smoke page, start the fixture, then:

```sh
pnpm --dir apps/viewer build:hub-wasm
pnpm --dir apps/viewer dev --port 1447
```

Open `http://localhost:1447/tests/webrtc-smoke.html`. It verifies a fresh WASM
Noise channel over real WebRTC, a 900 KB request, and another direct request after
stopping Hub tunnels. All displayed content is synthetic. Stop both processes
when finished; the fixture deletes its private temporary state on exit.

# tokn-hub-transport

A bounded, ordered record transport interface with a native WebRTC DataChannel
implementation.

```toml
[dependencies]
tokn-hub-transport = "0.1.1"
```

`RecordTransport` separates encrypted record exchange from the connection
carrier. `WebRtcPeer` supplies the current native direct carrier, while the
interface permits additional native tunnels without changing pairing or
session requests. Negotiation and relay fallback are managed by the Hub
endpoints.

The caller supplies Noise encrypted records and enforces host/device
authorization. This crate does not replace `tokn-hub-client-core` cryptography.

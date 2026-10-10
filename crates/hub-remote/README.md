# tokn-hub-remote

Native Hub client for paired remote session hosts.

```toml
[dependencies]
tokn-hub-remote = "0.1.1"
```

`RemoteManager` manages connection state, machine resolution, pairing,
remembered identities, requests, and live events. `ClientStatus` and
`ConnectionInfo` expose connection and transport status to installed clients.

Connections prefer direct WebRTC and can fall back to the encrypted Hub relay.
Both paths retain the same Noise encryption, pinned host identity, and
authorization. Saved native device state allows trusted clients to reconnect;
browser sign-in and browser secret storage are separate policies.

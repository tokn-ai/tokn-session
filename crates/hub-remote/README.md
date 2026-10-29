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

The installed Android/iOS and desktop viewers use this same native client.
`ClientStore` owns its bounded private identity and pinned-host bundle; the
client has no runtime dependency on the Hub server, SQLite, or its OpenSSL-based
passkey administration. The Hub crate re-exports the store for existing native
callers, preserving the on-disk format and desktop migration behavior.

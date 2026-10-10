# tokn-hub-client-core

Portable pairing and cryptography shared by native hosts, installed apps, and
browser clients.

```toml
[dependencies]
tokn-hub-client-core = "0.1.1"
```

The `address`, `pairing`, `protocol`, and `secure` modules provide machine
references, pairing messages, identities, and Noise encrypted records.
Native and `wasm32` builds share the same cryptographic implementation; WASM
bindings support the browser viewer.

Networking and endpoint authorization are supplied by the caller. A successful
Noise handshake authenticates keys; access to a host still requires its pairing
or saved-device policy. Use `tokn-hub-remote` for the native network client.

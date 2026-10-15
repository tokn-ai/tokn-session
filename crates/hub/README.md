# tokn-session-hub

Host discovery, authenticator/passkey pairing, remembered devices, and
end-to-end encrypted session access through a Hub. Direct WebRTC connections
are preferred, with encrypted Hub relay fallback.

```sh
cargo install tokn-session-hub --version 0.1.1 --locked
tokn-session-hub serve --api-only
```

The package installs `tokn-session-hub` and also exposes its server, connector,
pairing, and authorization modules as a Rust library. `connect` publishes a
host backed by a separately running `tokn-viewer-api`; `client` opens a paired
host through a local browser endpoint.

Native defaults use `~/.tokn/sessions/hub`; desktop client identities remain
separate under `hub/desktop-client`. Explicit state/key paths still take
precedence. For an existing `~/.tokn/hub` or desktop client bundle, first stop
Hub servers, connectors, desktop apps, and their background launchers, then run:

```sh
tokn-session-hub migrate-storage --offline
```

On Unix this moves complete private bundles and leaves compatibility aliases,
so old launchers reach the same identities and connector locks. Conflicting
stores are never merged. Retry the same command after an interrupted migration.

Browser assets are built separately and are not embedded in the crate.
Use `serve --web-root /path/to/viewer/dist` to serve a compiled viewer, or
`serve --api-only` to run without the UI. `client --web-root` selects its local
UI directory. See the project's
[Hub guide](https://github.com/tokn-ai/tokn-session/blob/main/docs/hub.md) and
[pairing guide](https://github.com/tokn-ai/tokn-session/blob/main/docs/hub-pairing.md)
for host setup, stable passkey origins, and HTTPS deployment.

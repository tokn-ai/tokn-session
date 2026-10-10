# tokn-viewer-api

HTTP, WebSocket, and SSE adapter for `tokn-viewer-core`, with optional static
hosting of the shared browser viewer. The package includes a Rust library and
the `tokn-viewer-api` executable.

```sh
cargo install tokn-viewer-api --version 0.1.1 --locked
tokn-viewer-api --api-only
```

The default API binds to `127.0.0.1:5558`. Non-loopback bindings require a
Bearer token through `TOKN_VIEWER_TOKEN` or `--token`; exact browser origins can
be allowed with `--allow-origin`. `--local` reads provider history without a
managed Relay child.

Frontend assets are built separately and are not embedded in the crate. Pass
`--web-root /path/to/viewer/dist` to serve a compiled viewer, or `--api-only`
to expose only the API. See the project's
[viewer guide](https://github.com/tokn-ai/tokn-session/blob/main/apps/viewer/README.md)
for the frontend build and local development commands.

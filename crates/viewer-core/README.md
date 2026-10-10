# tokn-viewer-core

Shared Rust domain for the desktop and browser session viewer.

```toml
[dependencies]
tokn-viewer-core = "0.1.1"
```

`ViewerService` supplies discovery, session windows, event details, source
inspection, and live update state. Runtime modules manage the metadata index,
provider scheduling, snapshot/follow readers, and supervised Relay processes.
The same domain serves Tauri commands and the HTTP adapter.

Frontend assets and HTTP routing are separate. Use `tokn-viewer-api` to expose
the service over HTTP, or embed this crate directly in an installed client.

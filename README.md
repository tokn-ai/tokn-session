# tokn-session

`tokn-session` is a provider-agnostic session layer for agent tools. It can
discover and normalize historical sessions from Pi, Codex, OpenCode, ZCode,
WorkBuddy, and DeepSeek Harness (DSH), while preserving provider-native detail
needed for display and debugging.

The Rust CLI currently supports listing, showing, and browsing sessions, plus
the initial configurable create/append path. A relay provides normalized live
events to the terminal and Discord pet applications.

## Installation

Version **0.1.1 is being prepared**. After it is published, install the CLI with
Rust 1.95 or newer and a C/C++ build toolchain:

```sh
cargo install tokn-session-cli --version 0.1.1 --locked
tokn-session list --source codex --limit 5
```

The package is named `tokn-session-cli`; its executable is `tokn-session`.
Optional services are installed separately:

```sh
cargo install tokn-session-relay --version 0.1.1 --locked
cargo install tokn-session-hub --version 0.1.1 --locked
cargo install tokn-viewer-api --version 0.1.1 --locked
```

The Hub also needs OpenSSL development libraries and `pkg-config` on Linux
(`libssl-dev` and `pkg-config` on Debian/Ubuntu). Browser assets are built
separately. First install their Rust/WASM prerequisites:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
cd apps/viewer
pnpm install --frozen-lockfile
pnpm run build
```

Pass the resulting `dist` directory through `--web-root`,
or use `tokn-session-hub serve --api-only` / `tokn-viewer-api --api-only`.
The desktop viewer is distributed separately from crates.io.

See [release preparation](docs/releasing.md) for package validation and the
publication procedure. From a source checkout, run:

```sh
cargo run -p tokn-session-cli -- list --source codex --limit 5
cargo run -p tokn-session-cli -- show --source pi <session-id>
cargo run -p tokn-session-cli -- browse --source dsh
cargo run -p tokn-session-cli -- list --source zcode --limit 5
cargo run -p tokn-session-cli -- list --source workbuddy --limit 5
```

## Desktop viewer

`apps/viewer` is a Tauri and browser app that presents root sessions from all
six providers in one searchable interface. It reuses the Rust session crates
directly rather than parsing CLI output and safely renders conversational
Markdown without allowing provider content to navigate the WebView. A local,
metadata-only index keeps its sidebar current without writing provider data.
Its message composer can send to a root Codex task in Codex Desktop or a live
Pi session running the input bridge.

```sh
cd apps/viewer
pnpm install
pnpm run check
pnpm tauri dev
```

See [apps/viewer/README.md](apps/viewer/README.md) for build instructions and
architecture, and [docs/handoff.md](docs/handoff.md) for detailed current
implementation status.

## Remote hosts through a Hub

`tokn-session-hub` connects your hosts through one endpoint. Hosts automatically
register their public identities and verify authenticator codes locally to pair
your devices. The installed client remembers trusted keys and reconnects with
end-to-end encryption. See [host onboarding](docs/hub-pairing.md) and
[Hub administration](docs/hub.md). Sharing is deferred from this onboarding flow;
the earlier grant commands remain available. The older browser-through-Hub mode
requires an explicit `--trusted-hub` option.

# Agent Notes

This file is the front door for AI agents working in this repo. Read it before editing. For current implementation status and next likely work, also read `docs/handoff.md` before non-trivial changes.

## Goal

`tokn-session` is a provider-agnostic session layer for agent tools.

It should let users:

- list existing sessions across providers
- show sessions as a normalized event stream
- create new provider sessions through one CLI
- attach to or resume existing sessions
- preserve enough provider-native detail to keep displays useful and debugging possible

The long-term command shape is:

```sh
tokn-session list --source codex
tokn-session show --source opencode <session-id>
tokn-session create --source opencode "create a todo app"
tokn-session attach <session-id>
```

The core abstraction is `AgentEvent`: provider-native historical sessions and live streams should normalize into this IR for display/export.

## Stable Shape

- The CLI binary is `tokn-session`.
- Commands are top-level. Do not reintroduce a `sessions` namespace.
- Historical sources today are `pi`, `codex`, `opencode`, `zcode`, `workbuddy`, and `dsh`.
- Current implemented commands are `list` and `show`.
- Future likely commands are `create` and `attach`.
- Provider-native events are normalized for display first; exact round-trip preservation is not a current goal.
- Unknown events should stay visible enough to discover new provider shapes.

## Workspace

- `crates/core`: shared IR types such as `AgentEvent`, `SessionRef`, and `LoadedSession`.
- `crates/codex-protocol`: tolerant, lossless wire types for persisted Codex rollout JSONL.
- `crates/pi-protocol`: tolerant, lossless wire types for persisted Pi session JSONL.
- `crates/opencode-protocol`: tolerant, lossless wire types for OpenCode V1 payloads and run JSONL.
- `crates/workbuddy-protocol`: tolerant, lossless wire types for persisted WorkBuddy session JSONL.
- `crates/dsh-protocol`: tolerant, lossless wire types for persisted DeepSeek Harness session logs.
- `crates/dsh`: read-only DSH JSONL/Zstandard session discovery and normalization.
- `crates/client`: source dispatch and public client API.
- `crates/cli`: argument parsing and terminal rendering.
- `crates/pi`: Pi JSONL session source and normalization.
- `crates/codex`: Codex JSONL session source and normalization.
- `crates/opencode`: OpenCode SQLite session source and normalization.
- `crates/zcode`: ZCode SQLite session source using the compatible tolerant
  OpenCode V1 wire decoder with ZCode-specific provider semantics.
- `crates/workbuddy`: read-only WorkBuddy SQLite catalog and JSONL session source.
- `crates/relay`: provider live feeds and stdout/ZeroMQ/managed-stdio transports.
- `crates/hub`: host-verified authenticator pairing, saved device trust, encrypted client/host tunnels, and optional passkey administration; legacy signed grants remain available.
- `crates/hub-client-core`: shared native/WASM pairing and Noise encryption without networking.
- `crates/hub-transport`: bounded ordered record interface and native WebRTC carrier.
- `crates/hub-remote`: native direct-to-Hub client, remembered identities, requests, and live events.
- `crates/viewer-core`: shared viewer domain, index scheduler, snapshot/follow readers, and Relay supervision.
- `crates/viewer-api`: HTTP/SSE adapter and optional static web UI host for browser clients.
- `apps/viewer`: shared React UI; local desktop uses viewer-core, remote app/browser clients reach the host through encrypted Hub tunnels.
- `vendor/`: source-of-truth checkouts for upstream projects. Do not edit vendored code unless explicitly asked.

## Before Non-Trivial Work

- Read `docs/handoff.md` for current status, known gaps, and next likely work.
- If `docs/handoff.md` is stale after your change, update it.
- Treat examples as representative; check similar provider code before changing only one source.

## Verification

After code changes, run:

```sh
cargo fmt
cargo check
cargo test
```

Also run at least one relevant CLI smoke. For example:

```sh
cargo run -p tokn-session-cli -- list --source opencode --limit 1
```

When changes are verified, commit them with a conventional commit message such as `feat: ...`, `fix: ...`, `refactor: ...`, or `docs: ...`.

## Style

- Prefer Rust.
- Use 2 spaces for indentation.
- Prefer code quality over minimizing diff size.
- Keep docs current but small. `docs/handoff.md` should describe present state and useful next context, not every historical step.

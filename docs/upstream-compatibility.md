# Upstream compatibility

The source references below were refreshed from upstream default branches on
2026-11-22. They are inspection/build inputs; recorded RPC fixtures retain the
revision or executable that actually produced them.

| Source | Default branch | Previous reference | Current reference |
| --- | --- | --- | --- |
| Codex | `main` | `2351d9e1b6` | `357fe240c9` |
| Pi | `main` | `fdbedcad8a` | `9c46526885` |
| OpenCode | `dev` | `017a5977d2` | `5756d59a68` |
| DeepSeek Harness | `master` | `99f6f02fec` | `2e39e70496` |

## Codex

Historical generated images now accept durable `fileId` references without
inventing a local path. Canonical/legacy fanout deduplicates identical references
while retaining changed files and newly observed paths. `humanCommand` and
unknown future command-approval kinds reject because this client does not
advertise the new required-human-approval guarantee.

Affected crates: `codex`, `codex-adapter`; protocol/source documentation records
the newer inspected schema. Existing app-server sends, questions, turn controls,
rollout identity/ordinals, and local catalog formats remain compatible.
Explicit model-provider catalog filtering, account-scoped discovery, moderation
presentation, and file-backed audio remain separate feature work; native data
stays visible rather than being silently discarded.

## Pi

Prompt/steer/follow-up success can carry `data.disposition` with `started`,
`queued`, or `handled`. Both adapters preserve those facts; legacy acknowledgement
without a disposition proves acceptance only. Shared admission types distinguish
generic queue acceptance from a promise to run after the current turn.
Historical top-level usage, context edits, system messages, and compaction system
text have tolerant views with full native records preserved.

Affected crates: `pi-protocol`, `pi`, `pi-rpc-protocol`, `pi-adapter`,
`interaction-core`, and Viewer admission mapping. The retained 0.82.1 native
recording and its preparation revision are intentionally unchanged.

## OpenCode 2.3

Current history uses `session_v2`, ordered `session_part` rows plus any inline
tail, and owner-fenced `blob_ref`/`blob` media. The reader retains V1 and earlier
inline V2 storage, rejects ambiguous ownership across metadata tables, and
detects part edits without relying on timestamps. Current shell, compaction,
skill, location, idle, tool-streaming, and metadata shapes normalize or remain
inspectable. Image bytes load only on explicit Viewer resource reads, after
session/message/sequence ownership, size, and SHA-256 verification.

The live adapter discovers old and current APIs before dispatch. Current inbox
admission, form answers, permissions, SSE identities, managed credentials, and
readiness use their native contracts. Unsupported form constraints stay visible
and can be declined. Explicit workspace attachment rejects where the current
server cannot attest the requested workspace. A failed mutation never retries
through the other API.

Affected crates: `opencode-protocol`, `opencode`, `opencode-rpc-protocol`,
`opencode-adapter`, `interaction-tests`, and `viewer-core`, plus the shared Viewer.
A separate current native recording covers creation, output, forms, permissions,
steering, queueing, and cancellation; the earlier recording remains intact.

## DeepSeek Harness 0.2

Historical reads support physical log generations 0–4 and native Segment SQLite
storage. Headers, inherited boundaries, compressed frame ranges, and predecessor
cuts are validated without migrating or repairing provider files. SQLite source
identity includes both its physical path and exact native session ID. Client,
Viewer, and Relay track WAL changes; selecting one small trace does not require
buffering the entire database. Versioned JSONL discovery selects current native
artifacts instead of exposing retired generations as separate conversations.

The current live path uses authenticated `/connect`, one Gateway Mux, `$events`
generation readiness, passive Session observations, and scoped event results.
Questions, timed wait claims, one-shot approvals, sends, queueing, and cancellation
retain native scheduling/correlation semantics. Legacy ApiProxy attachment remains
separate. Managed current launches suppress browser opening. Native subagent
origin prevents independent root controls even without a recorded parent ID.
Per-use approval requires a bounded same-session, same-call tool input preview;
unobserved operations remain deny-only. Timed question expiry and uncertain user
responses share one dispatch reservation, so no competing outcome is replayed.

Affected crates: `dsh-protocol`, `dsh`, `dsh-rpc-protocol`, `dsh-adapter`, `client`,
`relay`, `viewer-core`, and `interaction-tests`, plus the shared Viewer. SDK types
are updated separately from the Gateway adapter. The legacy source preparation
script and capture remain tied to their actual old revision; the current native
smoke identifies its published package independently of inspected Git source.

Native verification covers the pinned OpenCode source and the separately
published DSH Gateway package using only a scripted loopback model. DSH JSONL
histories from those runs decode without unknown records in the covered scenarios.
The newer SQLite plugin is absent from that published package: its separate
pinned-source `SqliteStore` writer was bundled outside vendor and used to verify
compressed root/fork histories without model requests. Those evidence sources
are deliberately not described as the same build.

Read-only support does not reconstruct the compacted model context. Unknown
plugin events, fields, and content remain inspectable. Custom SQLite routing
outside the supported local store and new provider-specific optional features
require explicit follow-up support rather than inferred compatibility.

# tokn-session-opencode

OpenCode session discovery and normalization using read-only SQLite access.

```toml
[dependencies]
tokn-session-opencode = "0.1.1"
```

`OpenCodeSessionSource` lists session headers and loads V1 message/part histories,
older inline V2 projections and current `session_v2`/`session_message` histories.
Assistant `session_part` rows retain ordinal order, followed by any old inline
tail, matching native mixed-version storage. Timestamp-free edits and part-type
changes invalidate both incremental cache variants.
Its scan and cache APIs support incremental catalog and viewer reads.
`OpenCodeLiveNormalizer` handles JSONL emitted by `opencode run --format json`.

The adapter decodes payloads with `tokn-opencode-protocol`, correlates tools,
and produces `tokn_session_core::AgentEvent` while preserving unknown native
records. ZCode-specific storage and semantics are exposed through
`tokn-session-zcode`.

`history_format_exact` identifies V1, V2, mixed, and empty sessions from their
actual conversation rows. Shared legacy metadata and the existence of a V2
table do not prove ownership of an empty legacy session; those creations need
the host's durable receipt before control. An exact current `session_v2` row
proves V2 even before its first message. IDs present in both metadata tables and
mixed conversation histories reject rather than silently
continuing or merging incompatible contexts. `session_header_exact` reads one
native ID at the configured database without scanning other histories.

Current text/reasoning blocks need no invented part IDs. Tool streaming,
metadata, shell status/output, compaction outcomes, skills, location changes and
idle markers retain native meaning and unknown fields. User images accept
inline base64 and native `media:<sha256>` references; tools retain media from
their result content. The reference remains opaque during normalization.

`media_reference_hash` validates native references. `read_blob_exact` lazily
joins the exact session/message/sequence to `blob_ref`, bounds declared and
actual size before fetching bytes, and verifies their SHA-256 in one read
transaction. It returns `OpenCodeBlob { mime, data, width, height }`; it does not
fetch remote URLs or infer filesystem paths. Catalogs and history snapshots
never hydrate blob bodies. The caller supplies a byte limit, capped at 64 MiB.

The current schema reference is OpenCode 2.3.0 source
`5756d59a68a3d16ce412280c3ab055031898cce2`. SQL fixtures are representative
scripted data, independent of any older real RPC captures.

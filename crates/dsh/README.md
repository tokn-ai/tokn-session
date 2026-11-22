# tokn-session-dsh

Read-only historical DeepSeek Harness support for `tokn-session`:

```toml
[dependencies]
tokn-session-dsh = "0.1.1"
```

`DshSessionSource` reads provider logs without modifying them. The installed
CLI is provided by the separate `tokn-session-cli` package:

```sh
cargo install tokn-session-cli --version 0.1.1 --locked
tokn-session list --source dsh
tokn-session show --source dsh <session-id> --format pretty
tokn-session show --source dsh <session-id> --format jsonl
tokn-session show --source dsh <session-id> --scope tree
```

Discovery searches `$DSH_HOME/sessions`, falling back to `~/.dsh/sessions`.
`--session-dir` overrides that root. `show` also accepts an explicit file path,
an exact session ID, or an unambiguous ID prefix. `browse --source dsh` uses
the same reader.

The source reads released JSONL generations `0` through `4`, including
`session.jsonl`, `session.jsonl.zstd`, and `session.jsonl.zst`, recursively.
Generations 1–4 use `session.vN.jsonl` with the same compression suffixes. Discovery
selects the highest canonical generation in each directory, retaining older files
untouched. A newer unsupported or corrupt generation never falls back to an older
copy; conflicting encodings are reported.
Zstandard decoding is built in (no external `zstd` program) and supports concatenated
frames. Each file read is bounded to its initial byte length. Invalid JSON,
incomplete/corrupt frames, invalid packed runs, and unsupported format versions
produce path-qualified errors; the reader never repairs or writes session data.
Discovery does not follow nested symbolic links. A missing root is an empty list.

Native SQLite storage at `<root>/.segments/sessions.sqlite` is also supported.
The reader validates the database schema, trace identity, predecessor Segment
cuts, contiguous frame ranges, and UTF-8/Zstandard frame contents in one read
transaction. SQLite claims suppress retained JSONL copies of the same identity;
reading never imports or migrates them. Several sessions share the database path:
use `load_session_path_with_id` or `load_session_records_path_with_id` for an exact
read. A database path alone is accepted only when one trace exists. Exact native
routing-marker paths are accepted only after their hash, identity and colocated
database are verified. `storage_dependency_paths` exposes database/WAL dependencies
for snapshot invalidation. Header checks never decode conversation frames.
SQLite has no per-trace activity time in its immutable catalog; header-only
timestamps use that trace's creation time. A write to another trace never makes
this one appear newly active. Database/WAL modification times only invalidate reads.

The output is a chronological event view, not the reconstructed model context.
Assembled messages take precedence over redundant streamed chunks; unfinished
steps retain text/reasoning deltas. Tool calls and results are correlated by
turn, step, and call ID. Raw JSON arguments are parsed for tool summaries, with
malformed strings retained. Turn/step boundaries become `lifecycle` events;
turn outcomes distinguish completion, cancellation, interruption, blocking,
failure, and token limits. A closed step does not imply success.

Per-call `usage` prefers assembled usage, falling back to the last streamed
snapshot when necessary (even if an assembled message exists without usage).
Normalized input includes uncached input plus cache reads/writes; cache counts
are subsets, not additional tokens to add to the normalized total.

Recognized title, configuration, inbox, context, todo, and auxiliary-request
records become `metadata`, validated against their known shapes. Pretty output
shows compact summaries; JSONL and expanded browser rows retain native detail.
Plugin attribution and surface operations accompany messages/reasoning in
`provenance`, without a duplicate unknown message. Unfamiliar or malformed
records and unsupported content still render visibly as `unknown`, even when
the provider marks them ignorable. See [event IR](https://github.com/tokn-ai/tokn-session/blob/main/docs/event-ir.md).
Message counts include assembled user/assistant records, not deltas or tools.
Timestamps retain native epoch milliseconds as strings.
Current system/developer messages retain their native source and surface metadata.
Developer tool-set changes and attached files remain metadata. Images and unknown
attachment/content representations retain native detail; this crate does not
resolve external attachment bytes. Compact assistant attempts remain stream
metadata with explicit usage when present; they do not fabricate conversation
messages. Interrupted assembled prefixes do not claim a final answer.

Only `origin: "subagent"` headers establish child relationships. Their immutable
`seedLength` excludes inherited parent events in versions 0–1. Versions 2–4 use
`isSeeded` and the last tagged inherited `session/end-seed` cut; untagged resume
markers do not erase own history. SQLite's stored inherited count must agree
with that cut. Ordinary forks remain root sessions.

This crate supplies historical reads; native commands belong to the separate
runtime adapter. Tests use synthetic fixtures derived from the pinned upstream
format and storage contracts; private local logs are never copied into the repository.

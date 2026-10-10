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

The source reads `session.jsonl` and `session.jsonl.zstd` recursively. Zstandard
decoding is built in (no external `zstd` program) and supports concatenated
frames. Each file read is bounded to its initial byte length. Invalid JSON,
incomplete/corrupt frames, invalid packed runs, and unsupported format versions
produce path-qualified errors; the reader never repairs or writes session data.
Discovery does not follow nested symbolic links. A missing root is an empty list.

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

Only `origin: "subagent"` headers establish child relationships. Their immutable
`seedLength` excludes inherited parent events; later `session/end-seed` resume
markers do not erase own history. Ordinary forks remain root sessions.

SQLite storage, create/append, and input bridges are not yet
implemented. The log format currently supported is version `0`, based on the
pinned `vendor/dsh` reference. Tests use synthetic fixtures; private local logs
are never copied into the repository.

# tokn-session-qoder

Read-only Qoder CLI transcript discovery and normalization. The storage contract
is verified against the official `@qoder-ai/qodercli` npm package, version 1.1.94,
and [Qoder's CLI reference](https://docs.qoder.com/cli/cli-reference).

The default root is `$QODER_CONFIG_DIR/projects`, or `~/.qoder/projects`.
`QoderSessionSource::new(Some(root))` overrides the projects directory directly.
Main transcripts are `<project-id>/<session-id>.jsonl`; subagent transcripts are
`<project-id>/<session-id>/subagents/agent-<agent-id>.jsonl`. Subagents retain their
root's native `sessionId`, so their display IDs are `<root>/agent/<agent-id>` and
are not native resumable session IDs.

Discovery reads bounded identity records. Exact host verification also streams
bounded metadata to apply native directory relocation without decoding message
bodies. Full reads follow the latest committed active-leaf marker, preserve
compaction ancestry, and retain unsupported records as unknown events. Raw
snapshot records retain physical line identities, including superseded native
message revisions. Partial final lines wait for a later read. Reads are bounded
to 128 MiB by default and never contact Qoder services or change its files.
Native POSIX, Windows drive-root, and UNC working directories remain readable
across operating systems; live hosts must independently verify their local paths.

The normalizer exposes messages, reasoning, tool invocations/results, response
models, usage, compaction markers, and native metadata. Embedded images remain
explicit resources; remote references are never fetched. Provider content edits
remain visible native metadata rather than guessed replacement messages.

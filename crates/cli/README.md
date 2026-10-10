# tokn-session CLI

List, inspect, and browse agent sessions from Pi, Codex, OpenCode, ZCode,
WorkBuddy, and DeepSeek Harness.

The package is `tokn-session-cli`; the installed executable is `tokn-session`.

```sh
cargo install tokn-session-cli --version 0.1.1 --locked
tokn-session list --source codex --limit 5
tokn-session show --source pi <session-id> --format jsonl
tokn-session browse --source opencode
```

Use `--session-dir` to override provider storage and `--scope tree` when showing
a root session with its children. Unknown provider records remain visible in
normalized output. `create` and `append` use a configurable executor for
supported providers; run `tokn-session --help` for command options.

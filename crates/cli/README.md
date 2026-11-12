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

For ZCode, configure a native CLI executor with `--executor` or
`TOKN_SESSION_ZCODE_EXECUTOR`. Use the same working directory and storage for
both calls. ZCode emits its native JSON event stream for ordinary prompts.

```sh
tokn-session create --source zcode --executor zcode --cwd . "Describe this project"
tokn-session append --source zcode --executor zcode --cwd . --session sess_example "What would you test next?"
```

Replace `sess_example` with the session ID from the creation result, or use
`append --continue` to resume the latest ZCode session in the working directory.
ZCode defaults to `yolo` for headless prompts. An executor such as
`--executor "zcode --mode plan"` selects a different permission mode.

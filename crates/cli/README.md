# tokn-session CLI

List, inspect, and browse agent sessions from Pi, Codex, OpenCode, ZCode, Qoder,
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

Codex uses the local executable without requiring an executor configuration:

```sh
tokn-session create --source codex --cwd . "Describe this project"
tokn-session append --source codex --cwd . --continue "What would you test next?"
```

Use `--session <id>` for a specific saved conversation. `--executor`,
`TOKN_SESSION_CODEX_EXECUTOR`, and `TOKN_SESSION_EXECUTOR` can select a custom
launcher. Otherwise the CLI uses the same local Codex executable discovery as
the viewer, including `TOKN_SESSION_CODEX_EXECUTABLE` as a path or command name.
Standard Codex execution sends the supplied prompt through stdin and streams
native JSONL. A custom executor template with `{prompt}` controls its own arguments
and stdin.

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

Qoder uses its native ACP stdio interface for both creation and continuation:

```sh
tokn-session create --source qoder --executor qoder --cwd . "Describe this project"
tokn-session append --source qoder --executor qoder --cwd . --continue "What would you test next?"
```

Use `TOKN_SESSION_QODER_EXECUTOR` instead of repeating `--executor`, and
`--session <exact-id>` to choose a particular root session. `--continue` selects
the most recently modified persisted root in the exact working directory and
Qoder configuration. Its default historical root is `~/.qoder/projects`, or
`$QODER_CONFIG_DIR/projects`; `--session-dir` overrides that projects directory
for reading. An executor can include `--config-dir <dir>` for ACP storage.
Prompts travel as JSON-RPC content, so flag-shaped prompts require only the
usual `--` CLI separator. ACP errors never fall back to Qoder's print mode.
Qoder uses ordinary native prompt delivery; ACP does not provide an atomic
idle-only start, steering, or queue admission fence.
Output is JSON lines containing session identity, runtime updates, and a final
idle result. Terminal permission prompts allow once or deny; non-terminal
commands deny pending permission requests and exit with an actionable error.

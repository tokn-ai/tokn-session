# tokn-session-client

One Rust interface for Pi, Codex, OpenCode, ZCode, WorkBuddy, and DeepSeek
Harness session providers.

```toml
[dependencies]
tokn-session-client = "0.1.1"
```

Use `AgentClient::list_session_headers` for discovery without conversation
counts, `list_sessions` for counted listings, and `load_session` or
`load_session_tree` for normalized histories. `Source` selects a provider;
an optional directory overrides provider storage discovery.

Provider readers preserve native details through the common event model.
The client also exposes configurable create/append requests for supported
provider executors; historical readers do not modify stored sessions.

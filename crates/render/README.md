# tokn-session-render

Text and JSONL presentation for normalized agent session events.

```toml
[dependencies]
tokn-session-render = "0.1.1"
```

`display_event` returns a compact kind, summary, and inspection detail.
`render_session_list`, `render_session_jsonl`, and related helpers serve CLI
and viewer consumers. Rendering uses the shared event model and correlated
tool operations, while keeping provider-specific native detail inspectable.

This crate formats already-loaded data. Use `tokn-session-client` to discover
and read provider histories.

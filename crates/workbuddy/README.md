# tokn-session-workbuddy

Read-only WorkBuddy discovery through its SQLite catalog and persisted JSONL
session files.

```toml
[dependencies]
tokn-session-workbuddy = "0.1.1"
```

`WorkBuddySessionSource` accepts an optional configuration directory, lists
session headers and relationships, and loads normalized histories. It combines
catalog metadata with messages, reasoning, tool calls/results, and native
records from JSONL without modifying provider data.

The wire decoder is `tokn-workbuddy-protocol`. For a shared interface across
providers, use `tokn-session-client`.

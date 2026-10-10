# tokn-session-index

SQLite metadata index for agent session catalogs.

```toml
[dependencies]
tokn-session-index = "0.1.1"
```

`SessionIndex` stores source checkpoints, session metadata, project grouping,
presentation state, and baseline progress. `SourceKey` and `SessionKey` retain
provider identity so similarly named sessions remain distinct.

The viewer uses the index to schedule discovery, preserve reading state, and
avoid reloading whole histories for sidebar updates. Conversation data remains
in provider-owned files; this crate manages its own local index database.

# tokn-session-pi

Read-only discovery and normalization of persisted Pi session JSONL into the
provider-neutral agent event stream.

```toml
[dependencies]
tokn-session-pi = "0.1.1"
```

`PiSessionSource` lists headers, discovers parent relationships, and loads
session histories from the default provider location or a supplied directory.
The adapter retains reasoning, tools, usage, compaction, extension metadata,
and unfamiliar native records.

`tokn-pi-protocol` supplies tolerant wire decoding. Use `tokn-session-client`
when the caller needs the same interface across multiple providers.

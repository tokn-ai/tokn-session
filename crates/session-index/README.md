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

Launch folders share one stable record whether registered manually or discovered
from provider sessions. `list_project_folders` includes origin and provider
membership; incomplete discovery retains the last confirmed eligibility, and
only a complete provider pass proves absence. Registering a discovered folder
manually pins its existing identity, name, and ordering.

Before resolving catalog cwd paths, capture `project_discovery_checkpoint`.
Pass that checkpoint and host-resolved `ProjectFolderObservation` values to
`reconcile_project_folders`; a source or reconciliation change rejects the stale
write so the caller can retry. Observations are positive evidence even during an
incomplete pass. Omit cached rows that were not confirmed by that pass.
`project_folder_for_cwd` retains previously resolved aliases when their filesystem
targets disappear, without retargeting saved folder identities or admissions.
Startup may use `add_project_folder_observations` for newly discovered cached
cwd values without changing omitted memberships. Unchanged reconciliation does
not write, and `project_folder_revision` advances only with registry metadata so
followers can skip expensive snapshots for ordinary session updates.

Migration SQL is immutable once any build can apply it, including a running
development app. Further schema changes belong in a new migration version.
Compatibility for an observed development schema requires its exact checksum
and verified schema, with the upgrade and ledger change in one transaction.

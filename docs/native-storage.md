# Native storage

The desktop, CLI, and native services share one default root:

```text
~/.tokn/sessions/
  index.sqlite
  hub/
    host.json
    host-access.json
    host-enrollment.key
    host-noise.key
    state.sqlite
    desktop-client/
      client-hosts.json
      client-noise.key
  viewer/
    preferences.json
    machines.json
    relay.json
    rpc-runtimes.json
    rpc-leases/
    import-state.json
```

Files appear only when their owner needs them. `index.sqlite` stays in its
existing location with its SQLite and indexer siblings. Host state, the CLI
client, and the desktop client retain separate identities and trust records;
they are not merged. Display replicas remain in memory and reading markers stay
in WebView/browser storage. Provider session histories remain at provider-owned
locations. Browser preferences and bookmarks stay origin-local browser storage.

`tokn-session-storage::SessionPaths` defines native defaults. Explicit CLI
`--state-dir`, `--state-path`, `--index-path`, and key-file options still take
precedence; deployments using `/var/lib/...` do not move automatically.

## Desktop preferences

Desktop bootstraps native viewer preferences and machine bookmarks before the
workspace starts. Existing native files win over WebView imports. Missing files
import recognized legacy preferences once, or receive versioned defaults.
An import marker prevents a later deleted native file from resurrecting old
WebView choices; after successful import, missing files receive defaults.
Malformed, unsupported, oversized, or unsafe native files produce an error and
are retained. Writes are bounded, locked, private, and atomic. Pending edits stay
visible if saving fails, with an explicit Retry action.
Native preference events synchronize windows in the same app process.
Revision and write IDs distinguish delayed local save responses from later
writes by another window, even when both writes contain identical values.
Independent dev/installed processes read the shared files at startup; this change
does not watch preference files for live updates between those processes.

Reader configuration imports the old Tauri `relay.json` when the native file is
missing. Existing desktop client identities stay usable at the old Tauri
`hub-client` directory until offline migration relocates their complete saved
identity/pins bundle and preserves compatibility paths. Legacy client fallback
opens an existing identity only; it never creates replacement keys.

## Legacy Hub migration

An existing `~/.tokn/hub` directory requires an explicit offline migration:

```sh
# First stop the Hub server, connectors, and desktop/native clients.
tokn-session-hub migrate-storage --offline
```

`--offline` asserts that legacy processes without lifetime locks have stopped.
The command additionally checks known connector locks and existing state locks.
It moves the complete Hub and default desktop-client bundles without
copying/replacing keys, leaves exact
legacy-path compatibility alias on supported platforms, and journals the rename
so an interrupted operation can be retried. Old launcher paths then reach the
same files and ownership locks. Keep this alias while old binaries or launchers
remain installed. Independent stores at both paths are an error; migration never
merges credentials or chooses one by timestamp.

The app can inspect and stop verified legacy background hosting while migration
is pending. Background-service recognition supports verified old/new path forms;
it does not rewrite LaunchAgents or install updated launchers. Stop/remove actions
remain explicit. Running updated desktop or CLI commands never silently migrates
a live Hub server, regenerates identities after migration failure, or deploys a
remote server. On platforms lacking compatibility alias support, retain an
explicit legacy state path until a coordinated migration is available.

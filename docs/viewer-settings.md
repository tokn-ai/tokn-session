# Viewer settings

Settings separates configuration by owner. Opening the dialog preserves the
mounted viewer, current conversation, resource coverage, and live interests.

## Viewing device

`ViewerPreferences` is a versioned, bounded record. Desktop writes
`~/.tokn/sessions/viewer/preferences.json`; browsers use origin-local
`tokn.viewer.preferences.v1`. It contains sidebar grouping, lifecycle visibility,
background session scope, background delivery, and conversation delivery.
The store migrates the four previous preference keys and synchronizes React
consumers in the same window and through browser storage events. Desktop loads
native preferences before mounting the workspace, imports the legacy WebView
record only when no native file exists, and saves through ordered native commands.
Native save failures retain edits with an explicit Retry notice; invalid native
files fail visibly without being overwritten. A reset only restores display defaults.

The settings page and existing quick controls share this store. An unrelated
preference does not restart subscriptions or reload history. Changing delivery
retains the existing per-level cache and baseline rules. Machine selection,
pairing identities, reading markers, and session caches have independent owners.

## Selected machine

`get_machine_settings` returns a versioned public snapshot containing reader
status/settings, edit capabilities, and detected provider source locations.
Discovery uses the same provider-root resolvers as the reader without listing
catalogs, decoding bodies, or opening databases. Missing locations and resolution
errors remain visible. External Relays may use different source locations.
Custom source overrides are not implemented by this page.

Local Tauri snapshots permit the existing validated `configure_relay` command,
which saves `~/.tokn/sessions/viewer/relay.json`, reconfigures the reader, and reconciles the
index. Reader drafts require Apply; live status events do not overwrite unsaved
edits. Hooks register status listeners before reading a baseline and guard late
reads/writes with machine-bound transports and lifecycle generations.

HTTP and encrypted Hub connections expose read-only machine snapshots. Agent
input permission does not grant configuration administration. Session-scoped
shares cannot request machine-wide metadata. Old hosts report unavailable
settings while device preferences remain usable.

**Agent runtimes** has separate selected-machine profile and lifecycle commands
for Codex/Pi/OpenCode/DSH. Paired remote administration additionally requires
the host's explicit runtime-management opt-in; the setting cannot be enabled
remotely. Public profiles redact saved credential and environment values. See
[viewer RPC runtimes](viewer-rpc-runtimes.md) for persistence, ownership, access,
and the current boundary between runtime setup and conversation controls.

## Hosting and access

For the selected local desktop computer, Settings reuses the app-owned host
state and existing hosting dialog for Hub/name/access changes, verified service
actions, and explicit pairing display. It does not load pairing secrets when
Settings opens. Generic remote snapshots have no hosting-management capability;
the page identifies the selected machine and explains this limit.

Saved hosting settings, runtime status, host identity, and device trust remain
separate. “Host this computer” on Machines continues to refer to the viewing
computer, including while another machine is selected.

See [native storage](native-storage.md) for the directory layout, native machine
bookmarks, preserved client/host identities, and offline legacy Hub migration.

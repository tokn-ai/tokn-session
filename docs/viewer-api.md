# Remote session viewer

For one endpoint spanning multiple hosts, use the [Session Hub](hub.md).
Each host retains this API on loopback and connects outward to the Hub.
For an untrusted Hub, the [installed encrypted client](hub-e2ee.md) keeps
decryption on the recipient's device. Its selected-session grants use the
host-local `/api/v1/shared` adapter, behind this API's existing authentication;
remote clients cannot call that adapter or supply their own sharing scope.

The same frontend runs in Tauri or a browser. Desktop calls `viewer-core`
directly through Tauri commands. In browser mode, the Rust `viewer-api` hosts
the compiled frontend and exposes the HTTP/SSE adapter over the same origin.

```text
Desktop UI → Tauri commands/events ─┐
                                  ├→ viewer-core → shared source readers
Browser → viewer-api (HTTP/SSE) ───┘
```

`viewer-core` owns index queries, semantic session updates, history, paging,
trajectories, native Inspector detail, live input, and the indexer. Indexed
Automatic and Local modes share authoritative readers and native file watches;
conversations load on demand. One indexer holds the database lease, while other
API/desktop processes follow its durable index. Polling recovers missed source
notifications. Relay retains standalone normalization/feed transports and the
managed child for unindexed embeddings. See [session update delivery](viewer-session-updates.md)
for levels, frontend caches, revisions, and remaining serialization costs.

## Run locally

From the repository root, build the reused frontend and start the viewer server
on the machine containing sessions:

```sh
pnpm --dir apps/viewer install --frozen-lockfile
pnpm --dir apps/viewer build
cargo run -p tokn-viewer-api
```

Open `http://127.0.0.1:5558`; the browser defaults to the server that delivered
the page. The connection screen remains available for selecting another
machine.
The API defaults to loopback with no token. Set `TOKN_VIEWER_TOKEN` to require
bearer authentication on every data and event endpoint. The browser keeps the
token in memory only; reloads require reconnecting.

Assistant responses offer **Translate → 简体中文** when the browser supports
the local [Translator](https://developer.chrome.com/docs/ai/translator-api)
and [Language Detector](https://developer.chrome.com/docs/ai/language-detection)
APIs. Availability depends on the browser, device, and language pair. Translation
runs on the reader's device; viewer-api only supplies the original response.
The browser may download language models on first use. The UI shows preparation
and download progress, with **Continue translation** if another click is needed
to authorize a model download. Cancel also stops pending preparation.

These APIs require a secure context: HTTPS or a trustworthy loopback URL such
as `http://localhost` or `http://127.0.0.1`. A plain HTTP LAN address is not
eligible. Unsupported browsers show a disabled translation action with the
reason. Code, links, and Markdown formatting are preserved, and **Show original**
switches back without another request. The desktop app continues to use Apple
Translation on supported Macs.

For truncated assistant responses, event page summaries and matching details
include an optional `content_revision` digest of the complete text. A client
can use it to keep a translation across unchanged page refreshes and detect a
response that changed before its detail loaded.

For frontend development, start both servers with one command:

```sh
pnpm --dir apps/viewer dev:web
```

The launcher builds and starts `viewer-api` on a free loopback port, starts
Vite with HMR, and prints `http://127.0.0.1:1437/#token=…`. Open that link to
connect automatically. The random token is passed to the API through its
environment; it is not embedded in frontend code. The browser removes the
fragment before rendering and keeps credentials in memory. Treat the printed
link as a credential. On reload, reopen the link or enter the token manually.
Ctrl-C stops both servers. Set `TOKN_VIEWER_DEV_PORT` to change Vite's port.

To manage the processes separately, use two terminals:

```sh
cargo run -p tokn-viewer-api -- --api-only
pnpm --dir apps/viewer dev
```

Open `http://localhost:1437` and connect using the prefilled address. Vite
serves the React source with HMR and proxies `/api` requests, including SSE,
to `127.0.0.1:5558`. No frontend build or CORS flag is needed. The Rust process
continues running while frontend edits update the page. Rust changes still
require restarting the API process.

For a remote machine, a loopback API plus an SSH tunnel is sufficient:

```sh
ssh -N -L 5558:127.0.0.1:5558 your-machine
```

Open `http://127.0.0.1:5558` after creating the tunnel. Same-origin requests do
not need CORS configuration. `--allow-origin` is only needed when a UI
connects directly across origins instead of using the Vite proxy; it must match that
frontend's exact origin. Repeat the flag for additional origins. There is no
wildcard option. Non-loopback `--bind` requires a token; use HTTPS termination
or a private encrypted tunnel when transmitting sessions across a network. The
server speaks HTTP and does not configure TLS or a hosting provider.

Options:

- `--bind 127.0.0.1:5558`: listening address; port `0` chooses a free port.
- `--web-root apps/viewer/dist`: compiled Vite directory containing
  `index.html`; `TOKN_VIEWER_WEB_ROOT` provides the same setting.
- `--api-only`: disable static serving and skip the frontend build requirement.
- `--allow-origin http://localhost:1437`: allowed frontend origin.
- `--index-path <file>`: defaults to `~/.tokn/sessions/index.sqlite`. Hosts sharing
  this file share one indexer; use a separate path for different provider roots.
- `--native`: include provider-native Inspector records in automatic mode.
- `--local`: read/index history directly without a managed Relay child.
- `TOKN_VIEWER_TOKEN`: access token (also accepted through `--token`).

Provider roots use the existing environment overrides in [relay.md](relay.md).
Choose **Change machine** to close requests/subscriptions and clear the viewer
before connecting elsewhere. Only one machine is selected at a time. The
browser does not change the server's Relay configuration.

The message composer sends through the API host's Codex Desktop or Pi input
bridge. The same API token authorizes both history access and message submission.
See [message input behavior](../apps/viewer/README.md#sending-messages).

## API contract

`GET /api/v1/health` returns `{"version":1}`.
Commands are `POST /api/v1/<command>` with the same snake_case payload as the
Tauri adapter, normally `{"request":{...}}`. Commands without a request take
`{}`. Responses use the shared viewer-core models. Supported commands:

- `list_sessions`, `list_session_children`
- `load_session_updates`, `load_event_page`, `load_trajectory_event_page`, `load_event_detail`
- `acknowledge_session_attention`
- `update_session_view`
- `get_session_index_progress`, `retry_session_index`, `get_relay_status`
- `get_session_input_status`, `submit_session_input`

Session keys are admitted against the server's index before any
history access. A syntactically valid key containing an arbitrary source path
does not grant access. The API allows at most 16 concurrent command requests,
32 SSE clients, and 1 MiB request bodies. Error responses contain `error`;
invalid JSON/body-limit responses may be plain text.

Modern viewers use `load_session_updates` and `session-updated` events for level-based
semantic delivery. The history compatibility API requests `load_event_page` with `window_mode: "retained"` and
`direction: "backward"` to receive the complete resident history window,
initially the latest user turn. `window_mode: "earlier"` with its `previous_cursor`
extends that window by three turns and returns the complete expanded window.
Both window modes require explicit `direction: "backward"` and omit `offset`;
omitting direction selects the legacy default `forward` and is rejected.
`total_events` counts projected rows in this retained window; a non-null
`previous_cursor` means older source history remains. Row pagination without
`window_mode` preserves the full-history API. Window keys/cursors are opaque
and generation-scoped; clients must not construct or modify them.

`update_session_view` takes `view_id`, a monotonically increasing `revision`,
an optional selected `session_key`, and up to 128 `candidate_session_keys` from
the filtered sidebar. Renew every 30 seconds; send null selection and an empty
candidate list to release. Leases expire after 90 seconds. This controls cache
residency/preloading only and never marks messages read. See
[session cache](viewer-session-cache.md) for the eviction and retention policy.

`get_session_input_status` takes `{"request":{"session_key":"…"}}` and returns
`available`, `message`, and `max_length`. `submit_session_input` additionally
takes a UUID `request_id` and the exact `text`. Its receipt contains
`request_id`, `status` (`accepted`, `not_sent`, `unknown`, or `pending`), and
`message`. Acceptance means the owning app admitted the input; history/SSE
remains authoritative for the resulting conversation. The backend resolves
the catalog key and rechecks its source header before contacting the runtime.
It caches up to 1,024 recent receipts in memory and rejects concurrent sends
to the same owner. Repeating an identical request returns its cached receipt;
changing its target or text is rejected. A disconnected caller does not cancel
delivery already in progress. Never automatically retry an unknown outcome.

`GET /api/v1/events` is SSE with a `ready` handshake and 15-second heartbeats.
Named events match Tauri: `session-updated`, `session-notification`,
`relay-changed`, `relay-status`, `session-index-changed`, and
`session-index-progress`. Session updates carry changed semantic objects;
legacy relay/index events retain invalidation/recovery behavior. A lagging subscriber disconnects.
The browser retries, then reloads the catalog and selected timeline so missed
notifications cannot leave stale data indefinitely. Reconnects also refresh index progress and Relay status, establish a fresh progress
revision baseline after restarts, and reject stale in-flight responses. Event pages
include `follow_error` while followers retry, preserving last-good cards with a
visible stale-data warning until recovery. The browser shows its
connection state and preserves last-received data during temporary outages.
Ctrl-C closes event streams and stops background readers.

The legacy local snapshot/follow protocol now lives in viewer-core and can be
started with `tokn-viewer-api snapshot --bind tcp://127.0.0.1:5557 [--native]`.
It remains loopback-only and is used by desktop External mode. The old
`tokn-session-relay serve` command reports migration guidance. This endpoint is
separate from the browser HTTP API and retains its version-1 framed protocol.

Session summaries include `is_running`, `has_running_descendant`,
`unread_final_count`, and `unread_descendant_count`. These are additive to the
legacy unread booleans. Running takes display precedence over unread; counts
represent only this session’s completed final assistant messages, not index
refreshes or subagent replies. Compatibility fields `has_unread_descendant` and
`unread_descendant_count` always return false and zero; clients ignore values
from older servers. Acknowledgement
revisions remain opaque and must come from the displayed newest event page.

Session list requests accept `query.order` (`time`, the default, or `project`).
Project ordering is applied before pagination and groups shared repository
identities by their durable discovery anchor, newest first, then session activity.
Root summaries include nullable `project_key` and `project_order_ms`; `cwd` remains
the original session working directory. Clients should group by `project_key`,
falling back to `cwd` for older servers. Clients keep Recent display positions in
memory; this does not change API activity ordering. Start a fresh cursor when
changing the order or filters.

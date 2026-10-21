# Pi input bridge

`input-bridge.ts` is an opt-in Pi extension that exposes a local Unix socket for
the live Pi process. A client such as terminal-pet can submit input for the
process's active session, and Pi inserts it through `sendUserMessage()` so the
message appears in the current TUI session.

Load it explicitly while experimenting:

```sh
pi --extension /absolute/path/to/tokn-session/extensions/pi/input-bridge.ts
```

For regular use, copy this entire `pi/` directory into a subdirectory of
`~/.pi/agent/extensions/`. Its package manifest makes Pi auto-discover only the
bridge entry point while the shared protocol module remains importable. The
bridge is only enabled for interactive (`tui`) sessions with a persisted
session file. JSON, print, RPC, and ephemeral sessions remain unchanged.

## Discovery

The extension writes a short-lived, session-scoped descriptor in its private
runtime directory. On systems with `XDG_RUNTIME_DIR`, descriptors live under:

```text
$XDG_RUNTIME_DIR/tokn-session/input/pi/sessions/<session-path-hash>.json
```

Otherwise the bridge uses a mode-0700 user directory under the system temporary
directory. The descriptor filename is the SHA-256 digest of the absolute Pi
session path. It points to a process-instance socket and contains the session
identity, process instance id, process id, and a random capability token.
Socket filenames use a shortened instance prefix to remain within Unix socket
path limits; clients always use the complete path from the descriptor.

Only one live bridge may claim a session descriptor. The descriptor and socket
are removed during normal session shutdown. A stale descriptor is harmless:
clients must validate the session and process instance identities and treat a
failed connection as an unavailable bridge.

## Protocol

Requests and responses are one JSON object per line over a fresh socket
connection. Every request includes the `protocol`, `token`, `session_id`,
`session_file`, and `instance_id` from the descriptor.

Status request:

```json
{"protocol":1,"type":"status","request_id":"r1","token":"...","session_id":"...","session_file":"...","instance_id":"..."}
```

Input submission:

```json
{"protocol":1,"type":"submit","request_id":"r2","token":"...","session_id":"...","session_file":"...","instance_id":"...","delivery":"auto","content":[{"type":"text","text":"Continue the task"}]}
```

`delivery` may be `auto`, `follow_up`, or `steer`. Idle input always starts a
turn immediately. While Pi is busy, `auto` and `follow_up` wait until the agent
finishes its current work; `steer` is delivered at Pi's next safe steering
boundary.

Text may contain newlines, tabs, and carriage returns. Whitespace is preserved
so multiline Markdown and indented code reach Pi unchanged; whitespace-only
text and other control characters are rejected.

The `ready` status includes `images_available`, derived from the current model's
`input` capabilities. Older bridges without this field support text only. Image
submissions contain `{ "type": "image", "data": "<base64>", "mimeType": "image/png" }`
content items, optionally alongside one text item. Image-only input is supported.
The bridge rechecks the active model before admitting images; unsupported images
never silently become a text-only message. PNG, JPEG, WebP, and GIF are accepted,
with at most four images, 8 MiB each, and 24 MiB in total. Private request frames
are bounded to 34 MiB after base64 encoding; descriptors and replies remain small.
Admission fingerprints include ordered image content hashes.

Updated bridges advertise `model_controls_version: 1`, the current compact
model identity, and a model revision in `ready`. A `models` request returns at
most 512 configured choices, containing only display identity and image support;
provider URLs, credentials, and native headers are not exposed. Catalog replies
are bounded to 1 MiB by the viewer. Older bridges remain usable for input and do
not advertise model controls.

`select_model` takes `model_provider`, `model_id`, and the latest
`expected_model_revision`, alongside the existing session/instance/token fields.
It requires Pi to be idle without queued messages and validates the choice
against the runtime's registry before calling Pi's native setter. The setter
changes this session **and Pi's saved default model**. The bridge serializes
control/input requests, caches selection receipts, and returns `model_selected`
with the confirmed current model. A lost response or setter exception can mean
the change occurred: refresh model status instead of automatically retrying.

Viewer model choices are drafts until Send. Bridges supporting this flow advertise
`model_input_version: 1`. The viewer submits `delivery: "steer"` and an optional
`model_selection` containing `model_provider`, `model_id`, the original viewer
`expected_revision`, its translated `expected_model_revision`, and
`runtime_generation`. The serialized admission checks current Pi state:

- While busy, including queued messages, it ignores the draft choice and its
  stale revisions, keeps the active model, and steers the current turn.
- While idle, it verifies the instance, revision, and live registry, checks image
  support against the chosen model, applies the choice through Pi's native setter,
  confirms the result, and sends the prompt. Pi also saves the chosen default.

Viewer admissions include `model_result` with the actual compact model, native
revision, and `unchanged`, `applied`, or `ignored_while_running` outcome. The
viewer uses that state to reset the draft picker. Older bridges can still accept
plain messages, but the viewer must not send them model intents. Native setter
and send failures are cached once mutation begins; uncertain responses must not
cause an automatic resend. Legacy `delivery: "auto"` clients retain follow-up
behavior while busy.

The bridge returns `ready`, `admitted`, or `error`. An `admitted` response has a
`started`, `queued_follow_up`, or `queued_steer` disposition. It means the live
Pi runtime accepted responsibility for the input, not that the user message is
already durable. Relay observing the resulting user-message event is the
authoritative confirmation. Repeating an identical `request_id` returns the
cached admission without submitting the message twice; reusing it for different
input is rejected.

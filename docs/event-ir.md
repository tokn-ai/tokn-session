# Event classification

`AgentEvent` is a display-oriented, provider-neutral stream. Native detail is
retained where useful; it is not an exact reconstruction of provider context.

## Images

Messages and tool calls carry optional `images` arrays of `RecordedImage`
references. `MessageImage` and `MessageImageSource` remain compatibility aliases
for the shared image types. Missing arrays decode as empty; empty arrays are
omitted from normalized source JSON. References are base64 data, data URLs,
recorded local files, or explicit unavailable/opaque references. Normalization
never reads image files or fetches URLs.

Assembled tool operations retain the latest output's images, each with its
owning `source_event_index`. A new output replaces the image list, including an
explicit empty output; metadata-only progress preserves it. Output images remain
tool results and do not become assistant replies. Viewer delivery exposes bounded
metadata and lazy resources; normalized JSON export preserves recorded sources.

## Lifecycle

`lifecycle` identifies a turn and optionally a step, with `scope`, `phase`, and
an optional `outcome`. DSH emits started/finished boundaries. A step's finished
phase means closed, not successful: its outcome remains absent. Turn outcomes
are `completed`, `cancelled`, `interrupted`, `blocked`, `failed`, or
`token_limit`. Native reasons remain available, including cancellation causes.
Failed turns also emit the existing `error` event for existing consumers.
New or malformed reasons remain unknown rather than being called successful.

## Usage

`usage.kind` distinguishes `model_call`, `operation_total`, and
`session_snapshot`. Session snapshots replace previous values; never sum them
or add them to per-call/operation accounting. Operations can include multiple
model calls. IDs identify the source record/message and turn/step when known;
absent IDs are not inferred. `input_tokens` includes cached input; cache fields are
subsets. `reasoning_tokens` must not be added to `output_tokens`. Missing
optional counters mean unreported, not necessarily zero. `native` retains the
original usage object and any provider extensions, including costs.
`total_tokens` preserves reported totals, which can include provider estimates
not represented in the input/output counters.

DSH reports uncached input separately; the adapter sums uncached input, cache
reads, and cache writes with overflow checking. It prefers assembled usage,
otherwise the last streamed snapshot for that turn/step, emitting it at that
record's position. An assembled message without usage does not suppress the
streamed usage. This avoids counting the same model call twice.

Pi assistant usage is per-call. Tool-result, compaction, and branch-summary
usage are operation totals, keyed by the session entry ID. Like DSH, Pi adds
uncached input, cache reads, and cache writes with overflow checking.
`cacheWrite1h` is already included in cache writes and is not added again.

Codex `token_usage_record.usage` is per-response `model_call` accounting.
The response ID identifies the usage record, not an assistant message; turn
identity is retained when present. Turn/thread aggregates stay in native detail
and are not emitted as additional calls. Cached input is already part of input.
This shape is based on observed Codex Desktop rollouts; the pinned vendor
checkout predates it. Missing or malformed usage remains unknown.

Codex persisted `token_count.info.total_token_usage` is a session snapshot,
not proof of a new model call. Cached input is already included in input.
Consecutive identical `info` objects are suppressed; changed snapshots,
including decreases and total-only context estimates, are retained unchanged.
`native` retains the full `info` object, including `last_token_usage` and the
context window. Changed rate limits are diagnostic metadata. Unavailable info
is metadata, not zero usage, and resets deduplication; rollback/compaction also
reset it. Malformed counters remain unknown. A live tail started at EOF emits
its first observed snapshot without reconstructing earlier calls.

OpenCode assistant turns emit at most one `model_call` usage event. When valid
`step-finish` parts are present, the last one is authoritative; the assistant
message's tokens are a fallback for incomplete or older rows. Cache reads and
writes are included in `input_tokens` once, while a provider-reported total is
kept unchanged. Invalid accounting remains a visible unknown record without
hiding the turn's message, tool, or reasoning content. Live `step_finish`
envelopes use the same rule when their token data is valid.

## Compaction

`compaction` is a context operation, never a user prompt, final assistant reply,
or turn completion. Its `state` is `requested`, `started`, `summary_generated`,
`completed`, `failed`, `interrupted`, or `skipped`. Only explicit provider
evidence advances the state; a generated summary alone need not be installed.
`provider_phase` retains timing categories such as `pre_request`, independently
of lifecycle state. Missing trigger, reason, or measurements are not inferred.
`summary_opaque` marks unreadable provider summary material.

| Provider | Persisted evidence |
| --- | --- |
| Codex | `compacted` checkpoints, legacy `context_compacted` notices, canonical completed `ContextCompaction` items, and opaque response compaction items. No persisted start signal. |
| Pi | Completed `compaction` entries: summary, first kept entry, and tokens before. Runtime auto-compaction start/end are not in the session JSONL. |
| OpenCode | V1 compaction parts request the operation; a linked assistant `summary: true` message supplies summary and completion/error. It is not a conversation reply. |
| ZCode | `compaction`/`timeline.context_compaction` parts carry explicit status and operation ID; `compact_summary` boundary messages carry summary and retention references. |
| DSH | `compaction/start`, `/summary`, `/end` plus the plugin's replacement `user/message`, correlated by `compactionId`. Only a successful end completes the operation. |

WorkBuddy is intentionally deferred. Tool-output pruning is separate from
summarizing compaction and is not relabeled as a completed compaction.

Measurements distinguish `context_before`, `context_after`, and
`replaced_context`. `estimated: null` means unspecified. DSH's shadowed-token
estimate is only the replaced span. ZCode's `truePostCompactTokenCount` estimates
rebuilt context; its `postCompactTokenCount` is summarizer usage and must not be
used as context-after size. Summary-call usage remains a separate `usage` event:
DSH identifies a model call only with `llmStreamCall: true` and `rawOutput`.

`compaction_operations` projects observations with the same provider/session/
operation ID into one card, keeping the first source index stable and retaining
all contributor indices for inspection. Anonymous observations remain separate.
Terminal state survives later summary enrichment. Codex pairs a checkpoint with
its completion notice across its validated world-state, turn-context, settings,
and token-accounting snapshot records. Session and known turn identities must
match; unrelated or malformed records break correlation. Checkpoints without a
usable window ID receive an adapter key scoped to that checkpoint. Completion
notices from a different turn remain separate when no explicit operation link exists.
This projection never deletes earlier transcript rows
or reconstructs the compacted model context. Relay native data stays an optional
sibling; no `native` field is added to this event.

Provider evidence: pinned `vendor/codex` rollout policy and compaction code,
`vendor/pi` session-manager types, `vendor/opencode` V1 compaction handling,
and `vendor/dsh/packages/compaction/compaction/src/types.ts`/checkpoint code.
ZCode fixtures are based on the installed 3.7.3 bundle, not a captured real
compaction session; new shapes must retain the unknown fallback.

## Agent communication

Incoming agent deliveries use `agent_activity` with optional `communication`:
readable `text`, `has_encrypted_content`, and optional `trigger_turn`. They retain
provider-supplied sender and recipient identities separately from ordinary
assistant replies. Ciphertext stays in native detail; an exact routing-only
header accompanying encrypted content is not presented as a readable body.
Unsupported or malformed content remains unknown instead of partial prose.

Codex `response_item.agent_message` is preserved in both legacy and paginated
history, because incoming deliveries have no canonical completed-item duplicate.
Only immediately preceding `inter_agent_communication_metadata` supplies its
turn-trigger flag. The metadata record remains independently preserved, and
legacy `inter_agent_communication` supplies its own flag.

Viewer summaries carry content-presence flags; expansion and the inspector load
readable Markdown through event detail. Sender navigation requires a unique
identity within the selected session's canonical relation tree. Repeated agent
paths in unrelated root sessions do not establish a relationship.

## Questions

`question_request` is a recorded request for user input. It contains the
provider/session, request and turn IDs when available, optional blocking flag,
recorded phase, optional fallback text, structured questions/options, native payload,
and timestamp. Questions preserve IDs/headers where supplied, option labels and
descriptions, free-text allowance, and secret-answer intent. A finished record
means the request was recorded; it does not mean the user answered it.

Codex paginated history stores `item_completed.AgentMessage` with
`delivery: "async"` and `questions: [{ title, options? }]`. Its
`phase: "final_answer"` is not an ordinary final assistant reply. The async tool
call is a duplicate in this mode. Legacy history instead keeps
`request_user_input_async` function calls; unpaired canonical question items in
exports are also readable. Structured `request_user_input` invocations have no
canonical request item, so their raw calls remain readable in both history
modes. Explicit `event_msg.request_user_input` records in exports are supported,
although upstream currently excludes them from persisted rollouts. Missing or
null `isBlocking` on explicit request events defaults to true. Raw structured
tool calls lack the effective collaboration mode, so their blocking flag stays
unknown; the handler supplies their free-form Other choice. Deprecated
`autoResolutionMs` remains native detail. Unknown fields remain inspectable;
malformed canonical items stay unknown,
while malformed legacy tool arguments retain their ordinary tool representation.

`question_reply` contains a recorded structured answer: request/turn IDs,
question IDs, original prompts/headers when known, answer strings, native payload,
and timestamp. Request IDs are optional when only an opaque question identity
is recorded. Codex `request_user_input` tool results are correlated by call ID
in both legacy and paginated history, including incremental reads. Canonical
outputs explicitly named `request_user_input` can stand alone without invented
prompts. Duplicate raw/canonical results produce one reply; correlation retains
the latest 256 requests. Unknown question IDs and empty answers stay visible,
and malformed results become unknown events. Async acceptance is not an answer.
Desktop/TUI user messages containing complete
`<send_user_message_question_reply>` envelopes normalize as replies in both
history modes, including single objects, arrays, and IDE context prefixes.
Their `questionItemId` is preserved; `["request_user_input_async", item_id, index]`
provides the request ID and matches the normalized async question ID. The
envelope's recorded prompt and answer remain readable without the request.
An object-replacement marker at the JSON boundary is tolerated. Quoted,
malformed, incomplete, and attachment-bearing messages remain ordinary text;
ordinary messages are not paired by proximity.

The viewer derives outstanding question attention from these historical events.
Only an explicit true blocking flag is labeled **Input required**; false or
unknown blocking is **Question available**. A reply resolves only its exact
question IDs with recorded answers, including explicit empty strings; empty
answer arrays do not resolve questions. Reading a session does not resolve them.
Turn completion/interruption, a superseding turn, a final assistant reply, or a
provider error or validated response cancellation retires remaining questions.
This projection retains at most 256 request identities. Historical cards remain
after their attention retires; approval handling and live answering remain separate work.

The viewer keeps requests outside work trajectories and shows Markdown prompts,
static choice lists, and free-text/secret markers. It does not infer current
pending state, selected answers, or submission controls from history. Replies
retain their original user-message or tool-result representation.

Source of truth: `vendor/codex` at `2351d9e1b6`, particularly
`protocol/src/items.rs`, `protocol/src/request_user_input.rs`,
`core/src/tools/handlers/request_user_input_async.rs`, and `rollout/src/policy.rs`
under `codex-rs/`.

## Metadata and provenance

`metadata` means a recognized non-conversation record whose required envelope
and payload fields were validated. It carries a category, native type, compact
summary, and full native record. Known DSH title/configuration snapshots,
inbox edits, context/todo snapshots, and auxiliary model requests use this
category. Queue inserts are not delivered user messages. Incomplete known
stream structure is metadata, not a completed tool call.

Pretty output uses compact summaries. Browser expansion and JSONL retain
native diagnostic bodies. No general rule hides `unknown`: unfamiliar tags,
malformed shapes, and unsupported content retain the existing visible fallback,
regardless of the provider's `ignorable` flag.

Messages and reasoning may carry optional `provenance`: the native `source`,
`surface_op`, and `source_event_seqs`. It preserves plugin attribution and
context edits without duplicating the conversation as an unknown event.
Provenance does not apply surface edits to the chronological history.

Reasoning may also explicitly state `redacted: true`. This means the provider
withheld the readable text; it is distinct from a hidden event. Redacted
reasoning remains visible as a safe event marker, while its content is not
retained for human rendering.

Pi `custom` records are opaque extension state, not messages. `custom_message`
records are system-role context messages with extension attribution, original
native content/details, and the provider's `display` flag in provenance.
Hidden content remains in JSONL; pretty output and browser rows show only a
placeholder, including for unsupported hidden custom-message shapes. Discord
does not publish extension messages. Hidden messages do not count toward the
relay's recent-message replay window. Recognized branch summaries,
labels, session names, leaf changes, and active tools are metadata.

Pi assistant `stopReason: error` or a nonempty `errorMessage` produces an error
event even when the response has no content. Partial content and usage remain
visible, with a diagnostic metadata record preserving the native message and
its IDs. An aborted response produces cancellation metadata rather than a
provider error; these persisted records do not establish a native turn boundary.

Request failures use `error`; warnings, reconnect attempts, and cancellations
use diagnostic metadata or native lifecycle outcomes. Failed tools keep their
own `is_error` flag without manufacturing a request failure. Partial replies,
reasoning, and usage survive terminal errors, and terminal outcomes follow the
content from the same native record. Repeated native failures are not deduplicated
by message text. Diagnostic metadata and lifecycle/tool native payloads preserve
provider codes, retry flags, and native correlation fields. Errors also carry
their own optional `native` payload, so inspecting an error does not depend on
neighboring diagnostic records or enabling full Relay native snapshots. Older
serialized errors without `native` still decode; Rust struct literals must now
provide `native: None` or the originating provider record. The Inspector separates
this payload from normalized JSON and applies its existing size bounds. Pi native
error/diagnostic envelopes omit provider-redacted thinking; explicitly hidden
ZCode error records retain machine data but remain hidden in human views.
Malformed or unsupported records remain visible as unknown.

| Provider | Error and cancellation handling |
| --- | --- |
| Pi | Assistant failures and legacy session errors stay visible with native diagnostics. Empty recognized failure text blocks do not add spurious unknown events. |
| Codex | Terminal errors and nested completion errors remain visible; failed completions have failed lifecycle outcomes. Warnings, stream retries, and the native nonterminal rollback/steering rejection codes are diagnostics. Aborts retain interruption diagnostics and lifecycle outcomes when an ID exists. Explicit image/tool-search failure statuses remain tool errors. |
| OpenCode / ZCode | Historical and live errors share classification. Only the native `MessageAbortedError` tag means cancellation; cancelled summaries are interrupted compactions. Blank structured errors use a useful fallback. |
| WorkBuddy | Meaningful `providerData.error` values produce errors with exact native diagnostics. False, blank, and recursively empty values do not. Assistant content and usage remain intact. |
| DSH | Current and precisely supported legacy turn endings preserve failed/cancelled lifecycle outcomes. Blank failure messages fall back to their code or a generic label. Invalid turn identities and unsupported shapes stay unknown. |

Pi and OpenCode/ZCode cancellation diagnostics lack authoritative turn IDs;
Codex aborts may also omit their optional ID.
`AgentEvent::is_response_cancelled` recognizes only their native response
discriminators so activity and question consumers can retire pending work
without inventing lifecycle identities. Tool cancellations and diagnostic prose
do not end the containing response.

The additive Codex completion-error, nonterminal error-code, and optional abort-ID
shapes were checked against upstream checkout `cf7e9cfe6a`, newer than the pinned
`2351d9e1b6` source. The tolerant decoder accepts them without a vendor update.

Codex turn context, world state, inter-agent communication metadata,
and rollbacks are metadata. Compaction summaries are
not final assistant replies. Historical subagent ownership filtering runs
before accounting and metadata normalization. No provenance is inferred from
a session's source for otherwise unattributed messages.

## Adoption

DSH produces lifecycle, usage, metadata, and provenance. Codex preserves
task/turn start, completion, and abort as turn-scoped lifecycle with native
turn IDs; required missing IDs remain unknown rather than fabricated. Valid
aborts without their optional ID retain diagnostic metadata only. Aborts retain
native diagnostics without a provider error. Pi, Codex, and
OpenCode all normalize usage; Pi and Codex also adopt metadata, and Pi supplies
extension provenance.
Terminal Pet ignores compaction, usage, passive metadata, and hidden content for
activity/focus and lease handling. Validated response cancellations and turn
interruption/cancellation lifecycle records clear pending work as neutral
outcomes. Broader authoritative Pet runtime state remains follow-up work;
Pi historical logs do not contain the live agent/turn boundaries, so
those require a separate bridge feature. DSH relay/input remain separate work.

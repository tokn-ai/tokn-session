# Session models

The viewer keeps model context separate from timeline richness, expanded work,
and raw source records. `EventPage.current_model` and `SessionUpdate.state.current_model`
carry a compact `SessionModelState`: model provider, ID, optional display name,
source (`live`, `recorded`, `catalog`, or `unknown`), and opaque revision.
An older host omitting this additive field does not clear cached context.
Recorded context is explicitly labelled and does not assert a currently running
runtime. Live status falls back to that context when unavailable.

Provider observations distinguish configured selections, request models,
response models, and present-day catalog snapshots. The first known model
establishes context; unchanged settings, effort-only updates, hidden records,
and catalog seeds are not model changes. A genuine change decorates the existing
source event key with `model_changed` and a previous/current identity. It remains
outside trajectories and inner activity groups, including at Final delivery.
Native inspection still returns the original source record. Codex retained
history carries model context immediately before the loaded range without
manufacturing source rows or shifting keys. Its cached checkpoint probe reads at
most 1 MiB before falling back to unknown preceding context.

## Live commands

New-session drafts use `list_session_creation_models`, scoped to the local
project, provider and resolved runtime plan. Availability, catalog reads and
first Send use the same host resolver. The additive `runtime_plan_id` pins
configuration identity/revision, strategy and working folder without exposing
launch configuration; it is distinct from a live instance or an authorization
token. Opening the picker lazily queries a verified catalog, reusing a connected
runtime or closing a temporary metadata
process afterwards. Pi uses `--no-session`; its normal CLI startup still loads
configuration/extensions and can run native migrations. No saved conversation,
prompt or model-setting call is made during discovery.

**Use runtime default** preserves existing creation behavior. Explicit Pi,
Codex and OpenCode choices stay in the draft until Send, reset when the
project/provider/runtime changes, and determine image compatibility. A refreshed
runtime plan also clears the model choice while preserving text/images. DSH
remains default-only because its creation API does not accept a model. The host verifies
the complete plan before and after catalog discovery, again before admission,
and checks the actual creation runtime before applying the model. Stale
project/profile/default or executable metadata rejects the old selection. Pi
uses the common plan contract with its existing worker and catalog. Its driver
passes creation-only launch arguments and verifies native state;
Codex/OpenCode pass native creation parameters. The selected model
identity and runtime-plan proof belong to the same durable creation fingerprint
as the first prompt. RPC receipts also retain the exact resolved plan;
uncertain results never trigger a separate setter or prompt replay. Native
catalog details and credentials do not enter receipts. This flow remains local
desktop only and does not change existing-session RPC model capabilities.

`get_session_model_status`, `list_session_models`, and `submit_session_input`
share snake_case request/response contracts in `viewer-core::models`. Tauri
commands are async. HTTP invokes the same service; encrypted Hub routing enforces
control permission independently. Session shares and view-only grants receive
unavailable status and empty catalogs, while recorded context remains visible.

Status does not load full histories, and catalogs load only when opening the
picker. Choices contain only model identity, display name, and image capability;
provider endpoints and authentication headers never leave the host. Pi catalogs
are bounded to 512 distinct identities. Unsupported runtimes and older bridge
versions return a reason instead of guessed choices.

Supported Pi controls keep their model choice in the local per-session draft. Picking a
model never mutates the runtime or global defaults, and the picker remains usable
while a turn is running. The header always shows confirmed runtime context.
Hosts advertise `supports_model_input`; older hosts keep the picker read-only.

For Pi, Send captures text, image IDs, and an optional model intent (provider/ID, expected
revision, runtime generation) under one UUID. The host uses fresh native state:
an idle runtime validates ownership, generation, revision, and catalog membership,
sets and reads back the chosen model, then sends. A busy runtime ignores the model
intent and always steers with its active model. Pi uses `prompt` with native
`streamingBehavior: "steer"`, so a turn settling during admission still receives
the message. It never queues a follow-up for viewer sends. Older bridge clients'
explicit `auto` delivery retains their legacy behavior.

Model mutation and input share admission and bounded UUID receipts. Managed Pi
also coordinates startup, Stop, and idle retirement; inspection never resumes a
retired worker. Native bridges advertise `model_input_version: 1`. Ambiguous
set/send results are retained and never automatically replayed. A confirmed busy
result resets the model override even if an attachment prevents delivery; other
failed or uncertain sends retain drafts. Accepted responses return actual model
state and clear the captured override without clearing later draft edits.

Image uploads are bounded staging, independent of the active model. Delivery
checks the effective model after the idle/busy decision, allowing an idle draft
to choose a vision model without an early model change. Pi's native setter may
also persist its default provider/model for new sessions, but this happens only
when sending an idle prompt with that choice. New Pi sessions inherit defaults
unless their draft explicitly chooses a creation model.

Codex defaults to RPC. Model reads use an already bound app-server and native
catalog; an unbound thread retains recorded context without starting or resuming
it. The common RPC adapters do not guarantee atomic model-on-send, so these
catalogs remain inspectable metadata with read-only selection and an explicit
reason. Opening an existing session's picker does not activate a runtime. See
[shared session supervision](shared-session-supervisor.md).

When Codex Desktop is explicitly selected, status uses local `thread-owner-discovery`, then binds
`thread-follower-read-model-settings` to that exact Desktop client and verifies
its response owner. Both reads share one bounded metadata budget. Missing owners
and pending settings preserve recorded context with an unavailable reason; they
do not resume a thread or become message-delivery errors. The follower connection currently has no verified catalog tied to the
same provider/account, so selection is unavailable. A standalone app-server
catalog, cached model list, or hardcoded IDs are not treated as Desktop authority.

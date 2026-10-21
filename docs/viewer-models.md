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

`get_session_model_status`, `list_session_models`, and `select_session_model`
share snake_case request/response contracts in `viewer-core::models`. Tauri
commands are async. HTTP invokes the same service; encrypted Hub routing enforces
control permission independently. Session shares and view-only grants receive
unavailable status and empty catalogs, while recorded context remains visible.

Status does not load full histories, and catalogs load only when opening the
picker. Choices contain only model identity, display name, and image capability;
provider endpoints and authentication headers never leave the host. Pi catalogs
are bounded to 512 distinct identities. Unsupported runtimes and older bridge
versions return a reason instead of guessed choices.

Selection carries a UUID, expected model revision, and runtime generation. The
host rechecks idle state, ownership, catalog membership, and readback. Input and
model mutation share per-session admission; managed Pi also coordinates startup,
Stop, and idle retirement. Opening the picker never resumes a retired worker.
Repeated UUIDs return bounded receipts; an ambiguous mutation is not automatically
retried. The frontend retains its draft and requires a status refresh after an
uncertain result. Confirmed selection refreshes input/image capabilities.

Pi uses native `setModel`/`set_model`, including Pi's normal persistence of the
default provider and model for new sessions. This behavior is disclosed in the
picker. New Pi Session continues to inherit its runtime defaults.

Codex status reads `thread-follower-read-model-settings` from the owning Desktop
client. The follower connection currently has no verified catalog tied to the
same provider/account, so selection is unavailable. A standalone app-server
catalog, cached model list, or hardcoded IDs are not treated as Desktop authority.

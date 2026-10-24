# tokn-interaction-core

Rust contracts and orchestration for native agent runtimes. Adapters depend on
this crate and their dedicated RPC protocol crate; this crate never depends on
provider implementations, historical readers, or a frontend.

Runtime identity exists before a saved session. Attached endpoints and managed
children have distinct ownership. Metadata is runtime/session scoped. Commands
and live pending interactions use instance-bound identities and bounded receipts;
uncertain mutations are never automatically replayed.


The shared host orchestration modules are:

- `creation`: bounded durable admission for native RPC session creation, exact
  identity publication and first-input progress. UUID retries observe receipts;
  restart and uncertain acknowledgements never replay creation or first input.
  Cancellation fences later initial delivery and cancels only the native turn.
- `managed_sessions`: durable exclusive-worker creation receipts, coalesced resume, process leases,
  worker admission, restart and idle retirement, behind native driver/host traits.
- `admission`: bounded message/control receipts, uncertain-delivery retention and
  atomic transport-transition fences.
- `session_ownership` and `session_supervisor`: canonical cross-process native
  leases, per-session gates and cancellation-safe, deduplicated route changes.
- `runtime`: configured lifecycle, startup quarantine, native creation/adoption without
  implicit resume, native binding recovery,
  event pumps and awaited shutdown, behind host configuration/factory traits.

Provider-specific parsing and workflows belong to protocol/adapter crates. Hosts
supply executable/profile configuration, authorization, verified native identities,
opaque session keys and presentation; opening history never activates a runtime.

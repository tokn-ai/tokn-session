# tokn-interaction-core

Rust contracts and orchestration for native agent runtimes. Adapters depend on
this crate and their dedicated RPC protocol crate; this crate never depends on
provider implementations, historical readers, or a frontend.

Runtime identity exists before a saved session. Attached endpoints and managed
children have distinct ownership. Metadata is runtime/session scoped. Commands
and live pending interactions use instance-bound identities and bounded receipts;
uncertain mutations are never automatically replayed.

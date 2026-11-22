# tokn-dsh-adapter

Rust `InteractionAdapter` for current DeepSeek Harness Gateway hosts and legacy
ApiProxy hosts. Current source reference:
`2e39e70496aa02986ca6a820c5d497e816b69d7e` (0.2.2-rc.5).

`DshAdapter::attach(ApiProxyConfig::new(endpoint)).await` discovers the protocol
before any session mutation. Current hosts exchange their launch token through
`/connect?interactions=answer`; the returned API Cookie stays in memory. Supply
the token in `bearer_token`, or use the native tokenized launch URL. Neither
appears in runtime descriptors or errors. The authenticated `/api/remote.mux`
socket owns that credential's lifetime. Attachment waits for `$events` readiness
and verifies passive `session/list`. Only a missing current route selects legacy
`events.mux`/`events.host`; authentication and transport failures never select
another protocol. Closing an attachment never stops its external host.

`DshAdapter::launch(ManagedOptions::new(cwd)).await` owns a current native
`dsh --profile web --host 127.0.0.1 --port 0 --no-open` child. Configure `program`,
launcher-prefix arguments, and environment overrides for the installed runtime.
Only that child's bounded native loopback readiness line is accepted, including
its launch token. No shell, browser, existing listener adoption, or port probing
is involved. Startup failure kills/reaps the child; close requests graceful Unix
termination, then escalates after its deadline. Drop requests bounded cleanup.
Older installations without `--no-open` can be attached to an explicitly started
legacy host.

Creation sends a reserved identity and project cwd without a prompt, verifying
the returned identity/preset and passive directory entry. Inspection never
resumes a cold Agent. Current event and state subscriptions must both be ready
before sending input. Session models remain distinct from deployment defaults;
catalog image support is unknown and conservatively false.

`StartOrSteer` sends native `mode: steer`; `Queue` sends `mode: queue`.
Receipts confirm acceptance without guessing which scheduling path ran.
Strict Start, strict Steer, Guide, and atomic model-on-send reject; a separate
model setter never substitutes for atomic intent. Cancellation preserves the
native inbox rather than clearing queued messages or stopping the host.

Gateway questions and one-shot approvals retain native event/Agent IDs and the
connection-generation ID. Replies use `$events/result`. Timed questions first
acquire `userQuestions/attachWait`; a card is published only after native claim
readiness, and completion/cancellation retires it. Batches preserve labels,
custom text, selection limits, and supporting details. Timed claims keep the
native remaining duration: expiry sends one `ASK_TIMED_OUT` outcome and releases
the claim. A dispatched or uncertain user response is never raced by a second
timeout outcome. Approval cards offer Allow once only after a bounded native
tool call with matching session, call ID, and tool name supplies the exact input.
Missing, mismatched, or oversized previews remain deny-only; late observations
can populate a pending card without changing its wire identity or response fence.
Approval intents disallow free text. Unknown waterfalls delegate to native answerers. Permanent grants
are unavailable. Pending views retain complete wire frames separately from
display details and offered decisions. Legacy RPC/audit identities are retained
on the legacy response path.

Bodies, frames, subscriptions, pending requests, and live state are bounded;
connect, unary, and heartbeat operations have deadlines. Mutation failures after
dispatch remain uncertain. No mutation is retried, replayed, or sent through
another protocol. Event loss requires explicit reconnection and a fresh history
baseline. Runtime instance IDs and publication sequences fence stale callers.

The native stdio SDK is described separately by `tokn-dsh-rpc-protocol::sdk`.
The adapter uses Gateway for its richer interactive contract. Scripted tests
exercise both generations without accounts or hosted model requests.

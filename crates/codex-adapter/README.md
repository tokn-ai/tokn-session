# tokn-codex-adapter

Rust adapter for Codex's public `app-server` RPC. Depends on the standalone
`tokn-codex-rpc-protocol` wire types and the provider-independent
`tokn-interaction-core` contract. Historical rollouts and the viewer's existing
Codex Desktop IPC input path remain separate.

`CodexAdapter::spawn` owns an explicitly configured `codex app-server --listen
stdio://` child. `attach` accepts stdio handles supplied by their owner;
`connect` attaches to an explicit WS(S) endpoint with optional bearer
authentication. Executables, arguments, environment, endpoints, and credentials
are host configuration. There is no process discovery, credential extraction,
or automatic attachment to Desktop. Native WebSocket transport is experimental;
Unix socket transport is not implemented here.

Initialization happens once per transport, with experimental API opt-in for
native question/approval metadata. Create a thread with `create_thread`, or
explicitly load and subscribe to saved work with `resume_thread`. Metadata and
state use non-activating reads. Inspection alone never authorizes a mutation;
control requires this connection's successful create/resume binding. Resume
omits old turn bodies, and active-turn recovery reads only one metadata row.
`create_thread_with_descriptor` additionally returns native cwd, history path,
and ephemeral status for host verification; it checks explicit creation settings.
The returned path can precede native file persistence. Creation never sends an
initial prompt, discovers a history file by guessing, or retries an uncertain write.

Register the adapter with `InteractionService` to reuse instance-bound dispatch,
live pending requests, receipts, and shutdown. The adapter also protects direct
mutation calls with bounded receipts whose admission survives caller cancellation.
Disconnect invalidates the connection; reattachment is explicit and uncertain
writes are never replayed automatically.

Supported commands:

- Metadata, current thread state, and paginated native model catalog reads.
  Catalog entries belong to the server's startup provider, so choices do not
  invent a provider identity from the selected thread.
- `StartOrSteer` uses native `turn/start` and returns `Accepted`; Codex does not
  say which disposition occurred. `Steer` uses `turn/steer` with the active turn
  ID precondition. Its optional `expected_revision` is that native turn ID.
- Turn interruption uses the exact active turn ID. Inline images use validated,
  bounded data URLs. Strict idle-only Start, automatic Queue, and atomic
  idle-only model-on-send reject with explicit unsupported errors.
- Native callback questions, including `isBlocking: false`, retain exact server
  request and question IDs. Selected options and optional notes stay separate in
  the common contract; notes encode as native `user_note: ...` answers. Secret
  prompts retain `is_secret` for an eventual UI to mask. The distinct
  `request_user_input_async` tool emits assistant-item metadata without a callback
  and remains native-only through this adapter.
- Command/file approval supports approve-once and deny, respecting native
  `availableDecisions`. A generic permanent grant cannot be expressed faithfully
  and rejects. Unknown server requests receive a method-not-supported RPC error.

Responses and notifications update one ordered pump. Epoch guards keep delayed
query snapshots from replacing newer live state. Identical pending-request
replays are idempotent; terminal turn/thread events retire the appropriate
requests. A successful question response confirms a complete transport write,
because native callback responses have no separate acknowledgement.

JSONL/WebSocket frames, pending calls, event queues, session state, requests,
stderr, and receipts are bounded. A damaged partial write or lost/overflowed
stream is terminal. Managed close stops and reaps only the owned child and
continues cleanup if its caller disconnects; attached close only detaches its
transport. Always await the host interaction service's shutdown.

Unit tests exercise failure and ordering cases with synthetic peers. The private
`interaction-tests` crate additionally records a real Codex process against a
loopback Responses server, then replays that traffic in ordinary workspace tests.
No provider account is required. See its README for executable provenance and
capture instructions.

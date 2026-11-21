# tokn-qoder-adapter

Managed Qoder CLI interaction over the official `qoder --acp` interface.
The adapter appends `--acp`; executable arguments may contain a local runner's
script path. Project, storage, protocol, remote, worktree, print and session
startup overrides are rejected. The host supplies and fences `QODER_CONFIG_DIR`
and verifies native persisted session identity and canonical project directory.
ACP itself does not attest the storage root or echo the resolved directory.

ACP version 1 is negotiated before use. Supported operations are native
session creation, exact resume (or load when resume is unavailable), cached
metadata/model inspection, normal native prompts, streaming updates, cancellation
and per-use tool permission allow/deny. A requested creation model is validated
against the actual new session's catalog and applied before returning the
session. Model intent on individual prompts, revision fences, steering, Guide,
Queue and strict Start are unsupported. No sessions are created or resumed for
inspection;
global model inspection reuses an already-bound session catalog, or reports
unsupported when none is resident. Catalogs do not assert individual model
vision support.

`tokn-acp-protocol` contains only standard ACP wire types. Model discovery and
selection prefer standard `configOptions` whose category is `model`, preserve
the opaque configuration ID, flatten standard grouped choices, and use
`session/set_config_option`. Its complete returned configuration must confirm
the requested selected value. An incomplete or failed standard response never
triggers a second mutation through a different API.

Qoder compatibility is isolated in `src/qoder.rs`: the retired `models` catalog
and `session/set_model` are available only when no standard model selector was
provided. The optional nonstandard root `userMessageId` response field and
`_meta.qoder.toolName` interpretation also stay in that module. Local wrappers
flatten the standard ACP response types; these fields do not extend the shared
protocol crate. Compatibility support does not claim full ACP conformance.

Image content is enabled only when the agent negotiates prompt image support.
PNG, JPEG, WebP and GIF inputs use ACP inline base64, at most four images,
8 MiB decoded each and 24 MiB total. This negotiation does not promise that
all selectable models understand images. Text is bounded to 16,384 characters.

DeliveryMode::Prompt returns Accepted: ACP has no atomic idle/steer/queue
fence, and Qoder may perform autonomous work. The adapter permits at most one
managed pending prompt per session. Admission waits for the exact native
session/prompt RPC result. Agent chunks, tool activity, permission callbacks and
pipe writes cannot distinguish managed input from autonomous background work
and do not prove admission. Streaming, Stop and permission responses remain
available while the prompt request waits. State becomes Running before dispatch
and returns to Idle after the native prompt result; cancellation remains
Stopping until that result. Prompt and cancellation writes are ordered without
waiting for the entire prompt. Unknown outcomes are never replayed.
Host-only Native events carry `protocol: "acp"`, `prompt_id` and native `update`;
terminal events carry `type: "qoder.prompt_result"` with `stop_reason` or
`error_kind`. Public consumers must bound and sanitize their live projection.

Permission options preserve their opaque native IDs. Only offered allow-once
and reject-once choices are used; denial otherwise uses ACP cancellation.
Always-allow is unsupported. Replies have no native ACK and remain pending,
with further responses disabled, until a terminal tool/turn update. Identical
requests with a different RPC envelope are cancelled while the original remains
the approval target. Pending requests and retired-ID tombstones are bounded.
No filesystem, terminal or elicitation client capability is advertised.
Unsupported reverse methods receive an immediate protocol error. Qoder
AskUserQuestion/plan-mode callbacks are cancelled rather than represented as
ordinary tool approvals: native plan approval can alter the permission mode.

The transport bounds incoming frames to 16 MiB, outgoing frames to 36 MiB,
requests to 32 in flight, writes to 3 seconds and ordinary RPC replies to
20 seconds. Prompt completion and admission have a one-hour bound. A partial or
cancelled write closes the pipe. Owned processes receive
EOF on close and are killed/reaped after a bounded grace period. There is no
attached-process transport or automatic mutation retry. At most 128 sessions
are resident per adapter.

Verification uses a standalone Rust scripted ACP peer, with standard-only and
separate Qoder compatibility scenarios, including permission,
resume, image, Stop ordering and framing failure scenarios. An ignored test
accepts explicit `TOKN_QODER_ACP_EXECUTABLE` and JSON-array
`TOKN_QODER_ACP_ARGUMENTS` for a disposable anonymous handshake/auth-failure
check. Published Qoder CLI 1.1.94 was checked without provider credentials.
An authenticated native prompt and account-specific model/image behavior have
not been verified in this environment.

References: [Qoder ACP](https://docs.qoder.com/cli/acp),
[ACP initialization](https://agentclientprotocol.com/protocol/v1/initialization),
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup),
[prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn),
[tool permissions](https://agentclientprotocol.com/protocol/v1/tool-calls).

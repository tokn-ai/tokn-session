# tokn-dsh-rpc-protocol

Native DeepSeek Harness wire types, based on pinned upstream commit
`99f6f02fecdb7dff40c3fbc9470f5907c29f74ca`.

`api` models ApiProxy's HTTP/SSE request, response, interaction and model shapes.
Unknown fields and event payloads remain available. `sse` is a bounded incremental
decoder supporting comments, multiline data, CRLF and split UTF-8 sequences.

`sdk` separately describes the smaller newline-delimited stdio SDK protocol:
`initialize`, `session/prompt`, `shutdown`, and event/status/subagent notifications.
That SDK has no metadata/model queries, explicit steer, human-question responses,
turn cancellation, load/resume, or reconnect replay. These types do not claim
those operations exist and do not launch a process.

# tokn-dsh-rpc-protocol

Native DeepSeek Harness wire types, inspected at upstream
`2e39e70496aa02986ca6a820c5d497e816b69d7e` (0.2.2-rc.5).

- `gateway` models multiplexed Remote streams, scoped forwarded events,
  generation readiness, and passive Session state.
- `api` retains shared unary envelopes and legacy ApiProxy interaction/model
  shapes, originally verified at `99f6f02fecdb7dff40c3fbc9470f5907c29f74ca`.
- `sdk` separately models newline-delimited JSON-RPC, current input capabilities,
  prompt/query, resume/wait, working-directory methods, and request/notification
  names. It does not provide Gateway's model catalog, passive state directory,
  explicit steer/queue admission, or turn cancellation.
- `sse` retains the bounded incremental legacy codec, including comments,
  multiline data, CRLF, and split UTF-8 sequences.

Unknown payload fields remain inspectable. These modules describe wire data;
they neither launch processes nor claim that different protocols share the same
capabilities. Recorded fixtures retain their actual capture provenance.

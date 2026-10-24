# Native Pi RPC fixture

`session.json` records actual stdin/stdout of Pi **0.82.1**, compiled from the
vendored revision `fdbedcad8aeb7c1e2ebd3b85f16e60e5fab0b2c3`. The agent uses a
fresh temporary home, an explicit local model definition, and a test extension
whose tool asks a native `ctx.ui.select` question. No hosted provider is used.

The same adapter workflow runs during capture and ordinary replay:

- query metadata, current state, and available models;
- send a prompt and observe its assistant response;
- invoke the extension tool, discover its pending question, and select Beta;
- hold a model stream open, send steering and queued input, then complete both;
- start another model stream, cancel the turn, and verify native idle state.

An additional replay runs the same complete transcript through
`PiAdapter::attach_supervised`, used by the viewer's owned-session supervisor.
It verifies every native command and dialog answer, all correlated responses,
ordered compact lifecycle/assistant outcomes, token/body suppression, and final
idle state. This exercises captured real-agent wire behavior without generating
a separate synthetic conversation.

Only volatile values are sanitized: temporary paths, the loopback model port,
UUID identities, and timestamps/durations. Native records, nested payloads,
request order, and selected answer remain as emitted. Generated model data is
restored from the matching Pi AI release artifact, pinned by SHA-256 in the
preparation script and fixture metadata. Its upstream manifest is validated
before building the pinned source.

Build with Node >=22.19 and npm, outside this checkout:

```sh
scripts/rpc/prepare-pi.sh /path/to/vendor/pi /absolute/fresh/pi-build
```

Run/capture (the node path should be absolute because the child environment is
cleared):

```sh
TOKN_PI_RPC_ROOT=/absolute/fresh/pi-build \
TOKN_RPC_NODE=/absolute/path/to/node \
TOKN_CAPTURE_RPC_FIXTURES=1 \
cargo test -p tokn-interaction-tests --test pi capture_real_pi_rpc_conversation -- --ignored --nocapture
```

Without `TOKN_CAPTURE_RPC_FIXTURES=1`, the ignored test still exercises the real process
and asserts behavior but does not rewrite the fixture. Normal replay needs no
agent, model server, keys, or network:

```sh
cargo test -p tokn-interaction-tests --test pi replay_real_pi_rpc_conversation
```

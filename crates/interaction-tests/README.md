# Real-agent RPC contracts

This private workspace crate runs real Pi, OpenCode, DSH, and Codex processes with a
scripted OpenAI-compatible server bound to loopback. It captures native RPC, not
handwritten substitutes for an agent. The default tests replay checked-in traffic
through the actual Rust adapters and need neither Node/Bun nor provider accounts.

```sh
cargo test -p tokn-interaction-tests
```

## Run the actual agents

Dependency setup can access package registries. Runtime tests use cleared
environments, temporary homes/projects, explicit model routes, and dummy keys.
They do not read personal sessions or use hosted model services. These captures
were verified with Node 24.10.0, Bun 1.3.13, and pnpm 10.30.3. Pi uses npm. No vendor source is modified:
preparation archives exact pinned revisions into new disposable directories.

```sh
git submodule update --init vendor/pi vendor/opencode vendor/dsh
scripts/rpc/prepare-pi.sh vendor/pi /tmp/tokn-rpc-pi
scripts/rpc/prepare-opencode.sh vendor/opencode /tmp/tokn-rpc-opencode
scripts/rpc/prepare-dsh.sh vendor/dsh /tmp/tokn-rpc-dsh

export TOKN_RPC_NODE="$(command -v node)"
export TOKN_PI_RPC_ROOT=/tmp/tokn-rpc-pi
export TOKN_OPENCODE_RPC_ROOT=/tmp/tokn-rpc-opencode
export TOKN_DSH_RPC_ROOT=/tmp/tokn-rpc-dsh
export TOKN_CODEX_RPC_EXECUTABLE=/absolute/path/to/codex
scripts/test-real-rpc.sh all
```

Each test verifies its source-revision marker. Pi additionally needs generated
model metadata omitted from upstream Git; its setup script verifies the matching
release artifact's SHA-256 before building. Setup fails on existing output
directories rather than altering another checkout.

Codex uses an explicitly supplied native `codex-cli 0.163.0-alpha.11` executable
and the Responses streaming API. Its fixture records the actual executable
SHA-256 separately from the upstream protocol source inspected for development;
that source reference does not claim to identify the Desktop binary's build.
The test invokes `app-server` with an isolated `CODEX_HOME`, a local model
provider, disabled analytics, and no provider authentication. Run it independently
with `scripts/test-real-rpc.sh codex`; no Node/Bun installation is needed.

The scenarios cover metadata/models, normal assistant output, tool-driven native
questions and answers, steering while a stream is held open, queued follow-up
input, and cancellation. OpenCode also exercises native permission approval.
Codex covers choice/free-text answers, command approval, duplicate admission,
explicit and atomic steering, interruption, and explicit resume. Its strict
idle-only Start and Queue operations reject honestly because the supported
native RPC contracts do not guarantee those delivery modes.
OpenCode's native V1 and V2 configuration readers differ. The scenario supplies
configuration accepted by both, which V2 migrates internally. The adapter keeps
the legacy-history check and reports an explicit unsupported reason if native
V2-only configuration prevents that check; a captured rejection covers this case.

The local model server implements Chat Completions and Responses streaming,
with explicit gates for running-state tests. Exhausting
its response script is an error; no request falls back to an external provider.

## Capture and replay

```sh
scripts/test-real-rpc.sh all --capture
cargo test -p tokn-interaction-tests
```

Capture is opt-in (`TOKN_CAPTURE_RPC_FIXTURES=1`). Review fixture changes before
committing. Paths, local endpoint addresses and Pi's entropy/timing fields are
normalized while preserving reference equality. Codex also replaces its
temporary home aliases and machine hostname. Native fields are retained;
authorization headers are never stored by the HTTP/WebSocket recording proxy.
String credential values exposed by native metadata are redacted as well.
Fixtures identify the native revision and scenario, with provider-specific
provenance beside each capture.

Network recordings reserve HTTP exchanges at request ingress and fill responses
in place so events cannot move before the mutation that caused them. Replay
matches native requests and gates events behind preceding mutations; every
captured mutation must be consumed. Observational polling may repeat. Pi and Codex use
an ordered bidirectional JSONL transcript and verifies request shape/order.

Real-process tests are explicitly ignored in ordinary Cargo runs: they require
prepared upstream builds or the documented Codex executable, and selecting them without their prerequisites fails
clearly. Replay runs in the normal workspace CI. The private test crate is not
published or included in crate-archive verification.

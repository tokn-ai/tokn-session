#!/usr/bin/env bash
# Execute actual pinned agents against the test-only loopback model server.
# Dependency preparation is separate; this command never needs provider keys.
set -euo pipefail
provider="${1:-all}"
case "$provider" in pi|opencode|dsh|all) ;; *) echo 'usage: test-real-rpc.sh [pi|opencode|dsh|all] [--capture]' >&2; exit 1;; esac
if [[ "${2:-}" == --capture ]]; then
  export TOKN_CAPTURE_RPC_FIXTURES=1
elif [[ -n "${2:-}" ]]; then
  echo 'second argument must be --capture when intentionally regenerating fixtures' >&2
  exit 1
fi
repository_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repository_root"
# Keep native RPC loopback traffic out of any inherited corporate proxy.
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1"
export no_proxy="${no_proxy:+$no_proxy,}127.0.0.1,localhost,::1"
if [[ "$provider" == pi || "$provider" == all ]]; then
  : "${TOKN_PI_RPC_ROOT:?Prepare Pi with scripts/rpc/prepare-pi.sh and set TOKN_PI_RPC_ROOT}"
  : "${TOKN_RPC_NODE:?Set TOKN_RPC_NODE to an absolute supported Node executable}"
  cargo test -p tokn-interaction-tests --test pi capture_real_pi_rpc_conversation -- --ignored --nocapture --test-threads=1
fi
if [[ "$provider" == opencode || "$provider" == all ]]; then
  : "${TOKN_OPENCODE_RPC_ROOT:?Prepare OpenCode with scripts/rpc/prepare-opencode.sh and set TOKN_OPENCODE_RPC_ROOT}"
  cargo test -p tokn-interaction-tests --test opencode real_opencode_rpc_local_model_capture -- --ignored --nocapture --test-threads=1
fi
if [[ "$provider" == dsh || "$provider" == all ]]; then
  : "${TOKN_DSH_RPC_ROOT:?Prepare DSH with scripts/rpc/prepare-dsh.sh and set TOKN_DSH_RPC_ROOT}"
  : "${TOKN_RPC_NODE:?Set TOKN_RPC_NODE to an absolute supported Node executable}"
  cargo test -p tokn-interaction-tests --test dsh capture_real_dsh_rpc -- --ignored --nocapture --test-threads=1
fi

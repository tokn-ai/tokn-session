#!/usr/bin/env bash
# Materialize the exact native OpenCode source outside the repository.
set -euo pipefail
source_root="${1:?usage: prepare-opencode.sh /path/to/vendor/opencode /absolute/output}"
output_root="${2:?usage: prepare-opencode.sh /path/to/vendor/opencode /absolute/output}"
revision=5756d59a68a3d16ce412280c3ab055031898cce2
case "$output_root" in /*) ;; *) echo 'output must be absolute' >&2; exit 1;; esac
[[ ! -e "$output_root" ]] || { echo "output already exists: $output_root" >&2; exit 1; }
bun_executable="$(command -v bun)"
git -C "$source_root" cat-file -e "$revision^{commit}"
mkdir -p "$output_root"
git -C "$source_root" archive "$revision" | tar -x -C "$output_root"
cd "$output_root"
output_root="$(pwd -P)"
# Materialize the current CLI and its workspace dependencies. Upstream scripts
# stay disabled; neither this preparation nor startup uses provider credentials.
bun install --frozen-lockfile --ignore-scripts --omit optional --filter ./packages/cli
# Resolve Bun here so the test process can start with an empty environment.
# The native sources and their RPC handlers are unchanged.
printf '#!/bin/sh\nexec %q run --preload %q %q "$@"\n' "$bun_executable" "$output_root/packages/cli/node_modules/@opentui/solid/scripts/preload.js" "$output_root/packages/cli/src/index.ts" > .tokn-rpc-opencode
chmod 700 .tokn-rpc-opencode
printf '%s\n' "$revision" > .tokn-rpc-source-revision
printf 'TOKN_OPENCODE_RPC_ROOT=%s\n' "$output_root"

#!/usr/bin/env bash
# Materialize the exact native OpenCode source outside the repository.
set -euo pipefail
source_root="${1:?usage: prepare-opencode.sh /path/to/vendor/opencode /absolute/output}"
output_root="${2:?usage: prepare-opencode.sh /path/to/vendor/opencode /absolute/output}"
revision=017a5977d2107092007623e507fc5c6eb337d3b2
case "$output_root" in /*) ;; *) echo 'output must be absolute' >&2; exit 1;; esac
[[ ! -e "$output_root" ]] || { echo "output already exists: $output_root" >&2; exit 1; }
bun_executable="$(command -v bun)"
git -C "$source_root" cat-file -e "$revision^{commit}"
mkdir -p "$output_root"
git -C "$source_root" archive "$revision" | tar -x -C "$output_root"
cd "$output_root"
# Source execution needs @opencode-ai/core, declared upstream as a development
# dependency. A path filter avoids the root workspace with the same package name.
bun install --frozen-lockfile --ignore-scripts --filter ./packages/opencode
# Resolve Bun here so the test process can start with an empty environment.
# The native sources and their RPC handlers are unchanged.
printf '#!/bin/sh\nexec %q run --conditions=browser %q "$@"\n' "$bun_executable" "$output_root/packages/opencode/src/index.ts" > .tokn-rpc-opencode
chmod 700 .tokn-rpc-opencode
printf '%s\n' "$revision" > .tokn-rpc-source-revision
printf 'TOKN_OPENCODE_RPC_ROOT=%s\n' "$output_root"

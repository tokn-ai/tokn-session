#!/usr/bin/env bash
# Build the exact DSH revision outside the checkout. Native tests only use a
# locally scripted model and isolated home; dependency installation is separate.
set -euo pipefail
source_root="${1:?usage: prepare-dsh.sh /path/to/vendor/dsh /absolute/output}"
output_root="${2:?usage: prepare-dsh.sh /path/to/vendor/dsh /absolute/output}"
revision=99f6f02fecdb7dff40c3fbc9470f5907c29f74ca
case "$output_root" in /*) ;; *) echo 'output must be absolute' >&2; exit 1;; esac
if [[ -e "$output_root" ]]; then
  echo "output already exists: $output_root" >&2
  exit 1
fi
node -e 'const [major,minor] = process.versions.node.split(".").map(Number); if (!(major >= 24 || major === 22 && minor >= 19)) process.exit(1)' || { echo 'DSH requires Node >=24 or >=22.19' >&2; exit 1; }
git -C "$source_root" cat-file -e "$revision^{commit}"
mkdir -p "$output_root"
git -C "$source_root" archive "$revision" | tar -x -C "$output_root"
cd "$output_root"
pnpm install --frozen-lockfile --ignore-scripts --store-dir "$output_root/.pnpm-store"
# The shipped web profile loads both faces and verifies built frontend assets
# before publishing its native readiness URL, even for RPC-only consumers.
pnpm run build:lib
pnpm run build:web
printf '%s\n' "$revision" > .tokn-rpc-source-revision
printf 'TOKN_DSH_RPC_ROOT=%s\n' "$output_root"

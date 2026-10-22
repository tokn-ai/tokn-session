#!/usr/bin/env bash
# Build the exact vendored Pi revision outside the checkout, without fetching
# live provider catalogs or loading a developer's Pi configuration.
set -euo pipefail
source_root="${1:?usage: prepare-pi.sh /path/to/vendor/pi /absolute/output}"
output_root="${2:?usage: prepare-pi.sh /path/to/vendor/pi /absolute/output}"
revision=fdbedcad8aeb7c1e2ebd3b85f16e60e5fab0b2c3
case "$output_root" in /*) ;; *) echo 'output must be absolute' >&2; exit 1;; esac
if [[ -e "$output_root" ]]; then
  echo "output already exists: $output_root" >&2
  exit 1
fi
git -C "$source_root" cat-file -e "$revision^{commit}"
mkdir -p "$output_root"
git -C "$source_root" archive "$revision" | tar -x -C "$output_root"
cd "$output_root"
npm ci --ignore-scripts --no-audit --no-fund --cache "$output_root/.npm-cache"
# Upstream excludes generated catalog JSON from Git. Recover only those data
# files from the matching release artifact; its digest pins the build input.
mkdir -p .model-artifact packages/ai/src/providers/data
npm pack @earendil-works/pi-ai@0.82.1 --ignore-scripts --silent --pack-destination .model-artifact --cache "$output_root/.npm-cache"
artifact=.model-artifact/earendil-works-pi-ai-0.82.1.tgz
expected=2f9df9522808b621cd3449876537f03d8a8df8b8d7ec2d5b18c6a910aa85b490
actual="$(node --input-type=module -e 'import { createHash } from "node:crypto"; import { readFileSync } from "node:fs"; process.stdout.write(createHash("sha256").update(readFileSync(process.argv[1])).digest("hex"))' "$artifact")"
[[ "$actual" == "$expected" ]] || { echo 'Pi generated-data artifact digest changed' >&2; exit 1; }
tar -xf "$artifact" -C packages/ai/src/providers/data --strip-components=4 package/dist/providers/data
printf '%s\n' "$actual" > .tokn-rpc-model-data-sha256
npm --prefix packages/tui run build
npm --prefix packages/ai run build:offline
npm --prefix packages/agent run build
npm --prefix packages/coding-agent run build
printf '%s\n' "$revision" > .tokn-rpc-source-revision
printf 'TOKN_PI_RPC_ROOT=%s\n' "$output_root"

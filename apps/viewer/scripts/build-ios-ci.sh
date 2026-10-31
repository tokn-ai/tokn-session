#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin || "$(uname -m)" != arm64 ]]; then
  echo 'The iOS CI build requires an Apple Silicon Mac with Xcode.' >&2
  exit 1
fi

viewer_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$viewer_dir"

# Generate from the locked Tauri CLI instead of storing machine-specific Xcode files.
# Rust targets are installed by CI, so init only prepares the native project.
pnpm tauri ios init --ci --skip-targets-install
pnpm tauri ios build --ci --debug --target aarch64-sim --no-sign -- --locked

shopt -s nullglob
apps=(src-tauri/gen/apple/build/*/*.app)
if [[ "${#apps[@]}" -ne 1 ]]; then
  echo "Expected one simulator app bundle, found ${#apps[@]}." >&2
  exit 1
fi
app_path="${apps[0]}"
executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app_path/Info.plist")"
test -x "$app_path/$executable"
xcrun lipo "$app_path/$executable" -verify_arch arm64
test "$(/usr/libexec/PlistBuddy -c 'Print :DTPlatformName' "$app_path/Info.plist")" = iphonesimulator

# Preserve the executable mode and any bundle symlinks in the uploaded artifact.
artifact_path="$viewer_dir/src-tauri/gen/apple/build/tokn-sessions-ios-simulator-arm64.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$artifact_path" -C "$(dirname "$app_path")" "$(basename "$app_path")"
echo "Simulator archive: $artifact_path"

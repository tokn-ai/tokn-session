# Releasing crates

## Version 0.1.1

This release is prepared but has not been uploaded. The 23 publishable workspace
crates share version `0.1.1`; the desktop viewer has `publish = false`.
Only the Codex, Pi, and OpenCode protocol crates previously shipped as `0.1.0`.
The Codex protocol retains those published enum and struct shapes; newer token
usage and history metadata are exposed through additive accessors.

Every crate includes its README, MIT license, and any test fixtures it needs.
Internal dependencies have both a local path and a `0.1.1` registry requirement.
The binaries are `tokn-session` (package `tokn-session-cli`),
`tokn-session-relay`, `tokn-session-hub`, and `tokn-viewer-api`.
Browser assets and desktop bundles are separate distributions.

## Validate

Builds require Rust 1.95 or newer and a C/C++ toolchain. Hub passkey support
needs OpenSSL development libraries and `pkg-config` on Linux. Packaging the
entire unpublished dependency graph uses Cargo 1.99 or newer, which stages
workspace dependencies locally while verifying the archives.

Run these from a clean checkout of the intended release commit:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo run -p tokn-session-cli --locked -- list --source codex --limit 1
cargo +1.95.0 check --workspace --exclude tokn-session-viewer --all-targets --locked
cargo publish --workspace --exclude tokn-session-viewer --dry-run --locked
```

The dry run packages and verifies all 23 crates without uploading. Cargo orders
the dependency graph and resolves the unpublished workspace versions from a
temporary local registry. Inspect `target/package/*.crate` before publication.
The release-check workflow repeats archive verification and the Rust 1.95 check
for pull requests and main. It also extracts the archives into a temporary
workspace and tests their sources, using local path patches solely to resolve
unpublished sibling crates. That supplemental check generates a separate lock;
the dry run verifies the original registry lockfiles. Existing CI checks the
full workspace and viewer.

For installed-service smoke tests, build from the extracted packages and use
`--help`, `tokn-session list`, and API-only service startup. Browser assets must
be supplied with `--web-root`; they are not embedded in the Rust packages.
Pi live input uses Unix sockets and is only supported on Unix.

## Publish

Publishing is a separate, explicit action after the release PR is merged and
CI passes. Run it from a clean checkout of that reviewed commit with a crates.io
owner credential configured through Cargo; never commit the credential:

```sh
cargo publish --workspace --exclude tokn-session-viewer --locked
```

Cargo uploads dependency crates before their consumers and waits for registry
availability. If a run stops after some uploads, confirm their versions on
crates.io and publish only the remaining packages with repeated `-p` selections.
Published versions are immutable: fix any shipped mistake with a new version.
After confirming all packages are available, tag the same commit `v0.1.1` and
record the release on GitHub. Install the four executable packages into a fresh
directory with `cargo install --version 0.1.1 --locked` and repeat the smoke tests.

# Releasing crates and mobile builds

## Version 0.1.1

The 33 publishable workspace crates share version `0.1.1`; the desktop viewer
and the native RPC test harness have `publish = false`. Before this release,
only the Codex, Pi, and OpenCode
protocol crates shipped as `0.1.0`. Use the publish script's `--list` mode below
to check which current versions are already available on crates.io.
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
cargo run -p tokn-session-cli --locked -- list --source codex --session-dir crates/codex/fixtures --limit 1
cargo +1.95.0 check --workspace --exclude tokn-session-viewer --all-targets --locked
cargo package --workspace --exclude tokn-session-viewer --exclude tokn-interaction-tests --locked
```

Packaging verifies all 33 crates without uploading. Cargo orders
the dependency graph and resolves the unpublished workspace versions from a
temporary local registry. Inspect `target/package/*.crate` before publication.
The release-check workflow repeats archive verification and the Rust 1.95 check
for pull requests and main. It also extracts the archives into a temporary
workspace and tests their sources, using local path patches solely to resolve
unpublished sibling crates. That supplemental check generates a separate lock;
packaging verifies the original registry lockfiles. Existing CI checks the
full workspace and viewer.

For installed-service smoke tests, build from the extracted packages and use
`--help`, `tokn-session list`, and API-only service startup. Browser assets must
be supplied with `--web-root`; they are not embedded in the Rust packages.
Pi live input uses Unix sockets and is only supported on Unix.

## Publish

Publishing is a separate, explicit action after the release PR is merged and
CI passes. Run it from a clean checkout of that reviewed commit with a crates.io
owner credential configured through Cargo; never commit the credential. The
script requires Python 3 and Cargo 1.99 or newer:

```sh
./scripts/publish-crates.py --list
./scripts/publish-crates.py --dry-run
./scripts/publish-crates.py
```

The script discovers publishable workspace crates from Cargo metadata and
checks each current version on crates.io. It skips versions already uploaded,
including yanked versions, and excludes private or other-registry-only crates.
An older published version does not skip the current release. Registry errors
stop the script before any upload. `--list` only displays the remaining versions;
`--dry-run` verifies them; running without either flag publishes them.

Publication builds use a separate `release-publish` directory inside Cargo's
configured target directory. The publish dry run repeats verification and
aborts every upload. Its temporary archives remain under
`target/release-publish/package/tmp-crate` with the default target directory;
use `cargo package` for the reviewable `target/package/*.crate` files.

Cargo uploads dependency crates before their consumers and waits for registry
availability. If a run stops after some uploads, rerun the script: it checks
crates.io again and passes only the remaining versions to Cargo.
Published versions are immutable: fix any shipped mistake with a new version.
After confirming all packages are available, tag the same commit `v0.1.1` and
record the release on GitHub. Install the four executable packages into a fresh
directory with `cargo install --version 0.1.1 --locked` and repeat the smoke tests.

## Mobile downloads

The **Mobile builds** workflow keeps CI artifacts for seven days and publishes
verified Android ARM64 debug APKs and unsigned iOS ARM64 simulator archives:

| Trigger | Publication |
| --- | --- |
| Every push to `main` | Update the rolling `nightly` prerelease. |
| GitHub Release published | Attach successful builds of the event's exact commit to that release. |
| Manual dispatch with `release_tag` | Retry an existing published release, building its current tag. |
| PR or manual dispatch with an empty `release_tag` | Build and retain CI artifacts only. |

Every main commit finishes its builds; only superseded PR runs are canceled.
Each platform publishes independently, so a failed build does not block a
successful platform's download. Any build or publication failure still leaves
the workflow failed. Nightly keeps the last successful download per platform;
release notes identify its commit, UTC build date, and SHA-256 checksum. Late
older runs cannot replace newer downloads or roll back the latest attempt's status.

Versioned-release retries add missing assets and retain existing successful
ones. Publication checks that the tag still points to the built commit.
Set `release_tag` in the workflow's **Run workflow** form to retry that release;
`nightly` is reserved for pushes to `main`. Keep the release editable until all
desired assets are attached. GitHub immutable releases lock assets on publication
and cannot support these later uploads; supporting them would require uploading
to a draft before publishing. See
[GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).

Only the publisher job grants `GITHUB_TOKEN` release write permission; PRs
cannot publish, and no extra secrets are required. These remain development
downloads: the Android APK uses debug signing, and the iOS archive runs only in
the simulator. Android CI does not configure a persistent signing key for
installed-app upgrades. Device signing and App Store/Play uploads remain separate work;
see [mobile build setup](viewer-mobile.md). This workflow does not invoke
`scripts/publish-crates.py`; crates.io publication stays explicit as described above.

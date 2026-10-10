# Session Hub on vultr-2

Public origin: `https://ahub.clouds56.top`. The DNS A record points to
`149.28.148.11`. Operate the saved `vultr-2` host through `ctl`.

This deploys the Hub and compiled browser UI. Session indexing and provider
access remain on the machines that connect outward to the Hub.

The first active release is `70bec8f` (2026-10-10), merging main `4d0c9be`
with the session delivery split. Release metadata records the full source commit
and asset hashes. Verification passed HTTPS health, every served asset hash,
WASM content type, anonymous access rejection, and the WebSocket host challenge.
The existing site remained reachable; both the Hub and certificate renewal are
enabled at boot.

## Layout

- `/opt/tokn-hub/releases/<release>/`: Linux binary, `web/`, and `release.json`.
- `/opt/tokn-hub/current`: active release symlink.
- `/var/lib/tokn-hub/state.sqlite`: persistent Hub identities and administration.
- `/etc/systemd/system/tokn-hub.service`: unprivileged loopback service on 5559.
- `/etc/nginx/conf.d/ahub.conf`: HTTPS, streaming, and WebSocket forwarding.
- `/etc/letsencrypt/live/ahub.clouds56.top/`: certificate and private key.

Release files are root-owned and read-only to the `tokn-hub` service account.
Systemd creates its private state directory. The existing Certbot renewal timer
and nginx deploy hook maintain HTTPS. nginx limits authentication and tunnel
handshakes separately and logs paths without query strings.

## Initial setup

Stage the supplied configurations with `ctl scp`. Install `ahub.http.conf` as
`/etc/nginx/conf.d/ahub.conf`, run `nginx -t`, and reload nginx. Once DNS resolves:

```sh
ctl -H vultr-2 exec -- certbot certonly --non-interactive --webroot \
  --webroot-path /var/lib/letsencrypt \
  --cert-name ahub.clouds56.top -d ahub.clouds56.top
```

This uses the server's existing ACME account. After staging a release, create
the system account `tokn-hub`, install `tokn-hub.service`, validate it with
`systemd-analyze verify`, and enable/start it. Replace the HTTP-only nginx
configuration with `ahub.conf`, validate, and reload.

## Updates and rollback

Build the frontend with `pnpm --dir apps/viewer build` and the Linux Hub with
`cargo build --locked --release -p tokn-session-hub` on Linux. Cross-compilation
with cargo-zigbuild must use Linux OpenSSL headers/libraries matching the target
server and a compatible glibc target; the first release used
`x86_64-unknown-linux-gnu.2.38`. Upload only the binary, compiled frontend, and
release metadata. Keep credentials and state out of release archives.

Extract into a new release directory, check its hashes and `ldd` output, and
run the staged binary's `serve --help` before activation. Preserve the previous
symlink target. Replace `current` with an atomic symlink rename, then restart
`tokn-hub`. Check health and nginx before declaring success. A code rollback
repoints `current` to the previous compatible release and restarts the service;
it does not revert database migrations. Back up persistent state separately
before updates that change storage schemas.

```sh
ctl -H vultr-2 exec -- systemctl status tokn-hub --no-pager
ctl -H vultr-2 exec -- curl --fail http://127.0.0.1:5559/hub/v1/health
curl --fail https://ahub.clouds56.top/hub/v1/health
```

Also verify the frontend asset paths, unauthenticated host access rejection,
the WebSocket upgrade/challenge on `/hub/v1/tunnel`, and renewal timer status.
Hub startup logs contain an optional bootstrap credential for first passkey
administration. Read those logs only in a trusted terminal; do not publish them.

## Connect hosts

Open `https://ahub.clouds56.top` and follow
[authenticator pairing](../../docs/hub-pairing.md): the host connector prints a
machine reference, and the browser pairs directly over an encrypted channel.
The browser build includes the shared Rust encryption core compiled to WASM;
`pnpm build` requires the pinned wasm-bindgen CLI from `Cargo.lock`.
Optional passkey and namespace administration lives at `/admin`, as described in
[Hub administration](../../docs/hub.md). Authenticator seeds stay on the host;
pairing uses the current code and the pinned machine reference.

The existing Hub HTTP tunnels retain SSE session delivery. Deploying this Hub
does not add WebSocket forwarding for the viewer's `/api/v1/live` endpoint.

# Linux Hub deployment examples

These host-independent examples run the Session Hub and compiled browser UI
behind nginx with a loopback systemd service. Session indexing and provider
access stay on the machines connecting outward to the Hub.

Keep actual host names, domains, public addresses, rendered configurations,
credentials, backups and deployment records in operator-owned storage outside
this repository. These files describe a reusable layout, not a live deployment.

## Configuration

Copy the examples outside the checkout and replace every `__HUB_DOMAIN__` with
your public DNS name. Review the service account, paths and loopback port for
your target system before installing the copies.

- `nginx-http.conf.example`: HTTP-only ACME challenge configuration for initial
  certificate issuance.
- `nginx.conf.example`: HTTPS, WebSocket and streaming forwarding. Certificate
  paths assume a certificate named after the chosen domain.
- `tokn-hub.service.example`: unprivileged Hub service, including its explicit
  public URL and persistent state path.

The example layout uses `/opt/tokn-hub/releases/<release>/` for the binary and
`web/`, `/opt/tokn-hub/current` for the active release symlink, and
`/var/lib/tokn-hub/state.sqlite` for persistent state. Release files should be
read-only to the `tokn-hub` service account; systemd creates its private state
directory. Create the service account and configure DNS, certificates and renewal
through your own infrastructure workflow.

For initial certificate issuance, install the rendered HTTP-only configuration
and issue a certificate using the `/var/lib/letsencrypt` webroot. Install the
rendered HTTPS configuration once its certificate files exist. Validate nginx
with `nginx -t` and the service with `systemd-analyze verify` before activation.
nginx preserves streaming/WebSocket connections and logs paths without query
strings.

## Build and update

Build the browser UI with `pnpm --dir apps/viewer build` and the Hub binary with
`cargo build --locked --release -p tokn-session-hub` on a compatible Linux system.
The browser build requires the pinned wasm-bindgen CLI from `Cargo.lock`.
Stage the binary and compiled frontend in a new release directory; keep all
private state outside release archives.

Check staged asset hashes, native library compatibility and `serve --help` before
activation. Preserve the previous release, replace `current` atomically, restart
the service, and verify:

- service status and `/hub/v1/health` through both loopback and HTTPS;
- frontend assets and WASM content type;
- unauthenticated access rejection and the `/hub/v1/tunnel` WebSocket handshake;
- certificate renewal.

Back up persistent state before storage upgrades. A binary rollback does not
reverse database migrations. Keep release inventories, rollout results and
backup locations in your private operational records.

See [Hub administration](../../docs/hub.md) and
[authenticator pairing](../../docs/hub-pairing.md) for client setup. Bootstrap
credentials printed in startup logs belong in a trusted operator terminal.

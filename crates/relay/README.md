# tokn-session-relay

Live provider feeds with normalized events and optional native records. The
library exposes `SessionRelay`, `SessionTailer`, and transport helpers; the
package also installs the `tokn-session-relay` executable.

```sh
cargo install tokn-session-relay --version 0.1.1 --locked
tokn-session-relay stdout
tokn-session-relay zeromq
```

Feeds retain provider identity, session context, and correlated tool updates.
Managed stdio supports viewer-owned supervision and request/response reads.
Filesystem watching and replay settings are configurable; use `--help` for
available source and transport options.

Applications embedding the relay can depend on `tokn-session-relay = "0.1.1"`.

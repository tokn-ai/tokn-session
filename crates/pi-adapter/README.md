# Pi runtime adapter

`PiAdapter` implements the provider-independent interaction contract for native
headless Pi JSONL. `spawn` owns and terminates its child; `attach` accepts existing
reader/writer handles supplied by the owner of an RPC transport and only detaches
those handles. It cannot recover another process's stdin/stdout after startup.

Metadata, current state, model discovery, commands, messages, queued follow-ups,
abort, and bounded concurrent extension dialogs use the real RPC protocol.
Start-or-steer uses Pi's native `prompt` admission. Strict active-only steering
is unavailable because Pi's standalone `steer` cannot atomically reject a settled
runtime. Model intent is unavailable: Pi offers separate setter/prompt requests,
not an atomic model-on-send operation. Callers must consult capabilities.

Incoming native notifications are preserved, including streaming messages and
tool progress. Pending dialogs are scoped to a runtime instance and native
session, expire conservatively, and never survive a disconnected transport.
Pi has no UI-response acknowledgement or pending-dialog query; a dialog reply
acknowledgement confirms its write, not native consumption. Host services should
keep command receipts across client reconnects and must not replay uncertain writes.

The `framing` module is also consumed by the viewer's existing managed Pi owner.

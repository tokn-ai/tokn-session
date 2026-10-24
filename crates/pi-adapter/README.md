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

`spawn_supervised` and `attach_supervised` provide an exclusive native transport
for a host that already owns a persisted session workflow. The adapter still
owns the child, pipes, correlation validation, bounded writes, stderr, and
awaited cleanup. Typed commands retain host-supplied correlation IDs; validated
responses and compact lifecycle metadata share one ordered queue installed
before the reader starts. Tokens and transcript bodies are filtered before
that bounded queue. A disconnect drains already queued metadata before reporting
one terminal error. Attached close never terminates an external process.
Owned-process exit is observed independently of stdout EOF, so descendants
retaining inherited output pipes cannot keep a stopped session alive. Hosts can
distinguish a confirmed successful process exit from a transport/protocol failure
with `exited_successfully`; later cleanup never changes that classification.

`managed::PiSessionDriver` plugs these native workflows into interaction-core's
shared `ManagedSessions` manager. The adapter owns fresh state/catalog admission,
model setter/readback, busy draft selection handling, extension dialogs,
dialog-aware deadlines, turn failure/retry/compaction mapping, and native idle
eligibility. The core owns durable creation receipts, bounded message receipts,
admission gates, process and native-session leases, restart/shutdown ordering,
and idle deadlines.
History identity is verified by the Pi source crate before the host maps it to
an opaque session key. The driver imports no viewer types and never decodes
presentation keys.

The native `bridge` module also supports explicit attachment to the Pi input
extension using a private descriptor and authenticated Unix socket. Its status,
models, images and admission envelopes use the same common managed DTOs.

Typed dialog replies perform no new state preflight. Generic adapter dialog
expiry and receipts do not compete with exclusive supervised workflows. A
successful write confirms transport delivery, not native consumption, and
uncertain writes must not be replayed. The common interaction API keeps its
existing capability limits; supervised workflows do not claim an atomic model
mutation that native Pi cannot guarantee.

All managed launches preserve explicit `NODE_EXTRA_CA_CERTS`, including an empty
value. When it is absent, a configured `SSL_CERT_FILE` supplies Node's CA bundle;
explicit launch environment values take precedence over inherited values.

# tokn-zcode-adapter

Managed ZCode interaction through `zcode app-server --stdio`. The adapter
verifies the expected history database at startup and checks the native session
ID and working directory before binding. It supports creating a session,
resuming saved history, native text guidance and follow-up queues, stopping a
turn, and reading live state and model choices.

`Guide` starts a turn when idle and offers guidance at native current-turn
boundaries when busy. Native delivery may instead become a later follow-up;
the receipt reports generic acceptance when the ACK cannot distinguish these
outcomes. `Queue` starts when idle or admits a native follow-up while busy.
Successful turns automatically advance eligible queued inputs. Stop cancels
the current native execution and holds following inputs. While those inputs
remain held, sending again requires keeping or clearing the queue through ZCode. Queue contents belong
to the native runtime; completion, persistence across restarts and recovery of
a held queue are not guaranteed by this adapter. Strict idle-only `Start`,
active-only `Steer`, `StartOrSteer`, revision/turn fences, model-on-send and
image upload are unsupported. Commands use caller identities and verified native ACKs without
mutation retries.

New sessions use native deferred persistence so the first V4 input admission
materializes history before its command ledger. Live requests expose
per-use tool approval or denial, AskUserQuestion choices, multiple selections,
written answers and option notes, and ExitPlanMode approval or plan feedback.
Only a retained native callback authorizes a response. Reannouncements keep one
business identity and refresh their native correlation. Responses stay marked
as dispatched until native resolution or pending-state reconciliation, and
uncertain writes are never replayed. Unsupported host callbacks are declined.
The managed process closes through its input pipe. Saved history remains available
through `tokn-session-zcode`.

The native transport was verified against ZCode CLI 0.17.1 bundled with ZCode
Desktop 3.17.2. See the [upstream app-server contract](https://github.com/zai-org/ZCode/blob/main/apps/zcode-cli/docs/design/v2/zcode-protocol-agent-server.md).

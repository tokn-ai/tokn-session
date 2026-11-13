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
remain held, sending again requires keeping or clearing the queue through ZCode.
The adapter does not replay native queues after restarting. Strict idle-only
`Start`, active-only `Steer`, `StartOrSteer` and revision/turn fences are unsupported.
Commands use caller identities and verified native ACKs without mutation retries.

Model intent belongs to each input. Admission updates the session continuation
default, queued work retains its selected model, and guidance can switch models
at the next native model step. A request already in flight keeps its model.
The adapter preserves the current reasoning setting when retaining a model and
uses the advertised reasoning default when selecting another. It advertises
`per_input_model`, independently of the stricter `atomic_model_on_send` contract.
The full catalog is captured from an authorized native session creation and
kept only in that runtime instance, then merged with fresh current-model facts.
Capability changes, native configuration changes, source-session closure and
disconnect discard it. Native app-server has no independent catalog revision or
sessionless model endpoint. A cold resumed runtime can expose only the current
model until an actual new conversation supplies a full catalog. Resume keeps
its bounded history read because full native resume would inline every image.
A runtime with no bound session asks the host to connect a session first;
inspection never creates or resumes one.

Image-only inputs and PNG, JPEG, WebP and GIF attachments are supported: up to
four images, 8 MiB each and 24 MiB total. Before uploading, the adapter verifies
and pins an image-capable model from the native catalog. Images use native
begin/chunk/commit uploads with 512 KiB chunks, checksums, exact upload ACKs and
session-owned artifact references. Busy guidance with images becomes a queued
follow-up. A failed upload never submits the prompt; an uncertain commit may
leave a native artifact. Incomplete staging expires under native policy, while
committed artifacts use native session retention. The upload schema is verified
for CLI 0.17.1; incompatible schemas fail closed without a fallback or replay.

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

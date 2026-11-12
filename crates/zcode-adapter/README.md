# tokn-zcode-adapter

Managed ZCode interaction through `zcode app-server --stdio`. The adapter
verifies the expected history database at startup and checks the native session
ID and working directory before binding. It supports creating a session,
resuming saved history, sending text while idle, stopping a turn, and reading
live state and model choices.

ZCode persists a new session when the first turn starts. Live requests expose
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

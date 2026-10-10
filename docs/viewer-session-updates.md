# Session update delivery

The indexed viewer reads provider changes through its shared source reader.
Automatic and Local modes both use that reader and native index watches;
Automatic no longer starts a second feed child that normalizes the same append.
Standalone Relay stdout/ZeroMQ and unindexed embeddings retain their feed paths.
Polling remains the recovery path for missed file notifications.

Viewer-core owns normalized history, semantic projection, question resolution,
and subscription revisions. The frontend owns bounded display replicas and
reading/disclosure state. A semantic subscription has cumulative levels:

- `final`: user messages, final assistant messages, questions and errors.
- `steps`: intermediate assistant messages, tool summaries, and other activity.
- `details`: the above plus payloads for explicitly requested `detail_keys`.
- `all`: every normalized source event and all display details in the retained
  history window, without requiring `detail_keys`. Older turns remain paginated.

`load_session_updates` accepts `subscription_id`, `session_key`, `level`, an
optional `cursor`, up to 16 `detail_keys`, and optional `unsubscribe`. It returns
an initial/catch-up snapshot or changes relative to the cursor. Subsequent
`session-updated` events carry the same envelope through Tauri or HTTP/SSE.
Snapshots and registration share a lock so updates cannot slip between them.
Each level/subscription has independent revision coverage. A missing cursor,
revision gap, expiration, or eviction recovers with a snapshot; no unbounded
replay log is retained. Source replacement publishes a new generation.

`items` contain user/assistant messages, notifications, tool summaries, and
details and individual source events with stable `item_id` values.
At `all`, `event:<event_key>` items contain an `EventDetail` in `event`, with
`event_order` preserving source order independently of tool correlation and
work folding. Invocation, progress/result fragments, lifecycle, and unknown
records remain separate. Redaction, native opt-in, and per-payload size bounds
are the same as Inspector; `all` does not bypass them. `removed_items` retires objects;
`semantic_order` changes only when the semantic sequence changes. Intermediate
messages remain individually available even when displayed inside a work group.
`groups` and `item_order` are an adapter for the existing folded conversation
UI, separate from semantic objects. Work summaries include ordered `child_keys`
pointing to semantic items; membership is independent of bounded Inspector
source records. Unchanged items and orders are omitted.
Control `state` includes history cursors, outstanding questions, attention
revision, and running status. Errors preserve the last usable display and emit
a source-error notification; recovery removes it. `session-notification` carries
compact indexed unread/running/question state directly to sidebar rows.
`session-index-changed.catalog_refresh_required` is false for body completions
whose effective title/preview stay unchanged and whose compact notifications
were delivered. Catalog changes, warning changes, stale/shared-index commits,
and missing notifications still require a catalog read. Older backends omit
the flag and retain that read. A selected semantic subscription consumes pushes
instead of also reloading its timeline on index invalidations.

Selected conversations subscribe at `all`. Expanded tools and Inspector use
the delivered display details from that replica, without separate detail
subscriptions. Work groups read their complete semantic membership from the replica. Each
inner activity group between messages mounts all rows on first expansion and
keeps them mounted on collapse; opening the outer Worked/Working disclosure
does not mount every inner group. Live updates append to loaded groups.
Legacy groups without membership assemble trajectory transport pages before
publishing any rows, rejecting incomplete or changed responses. Transport
chunks never create a partial display group or a within-group load-more control.
Recently opened conversations retain their all-level display
while receiving final-level updates, which do not advance their all cursor. Reopening renders cached
content immediately, then catches up. The frontend buffers pushes arriving
before the initial response, rejects revision gaps and stale responses, and
keeps unchanged object references. It preserves disclosure/Inspector selection
and stored reading positions. Only affected details and group contents are
invalidated. A 30-second heartbeat renews active subscriptions and recovers
missed events. Browser session-update frames allow up to 64 MiB; other notification frames
remain limited to 2 MiB. Subscriptions expire after 90 seconds; backend state is bounded
to 24 subscriptions and a 64 MiB estimate. Frontend replicas retain eight recent
sessions with a 64 MiB target, protecting the selected session. Activity does
not promote sessions in the frontend LRU.

Older servers use the retained-page fallback. Read-only shared-session SSE
continues its scoped fixed-cadence invalidations; guests obtain catch-up changes
through the authorized command rather than the host-wide event stream.

Remaining costs: the source snapshot subscription still clones retained IR and
uses JSON framing even in embedded mode. Display projection (including full payloads when an all-level subscriber exists)
is rebuilt before comparing objects; this change removes unchanged frontend payloads,
not every backend scan or journal serialization. The sidebar activity index has
its own bounded source readers. None of these costs has been benchmarked here.

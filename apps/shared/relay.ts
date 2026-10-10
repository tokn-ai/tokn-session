export type JsonObject = Record<string, unknown>;

export interface RelaySession {
  provider?: string;
  session_id: string;
  parent_session_id?: string | null;
  agent_path?: string | null;
  agent_nickname?: string | null;
  agent_role?: string | null;
  title?: string | null;
  cwd?: string | null;
  started_at?: string | null;
  project?: {
    id?: string | null;
    name?: string | null;
    project_name?: string | null;
    folder?: string | null;
    folder_name?: string | null;
    repository_name?: string | null;
    repository_url?: string | null;
    branch?: string | null;
    commit_hash?: string | null;
  } | null;
}

export interface AgentEvent extends JsonObject {
  type: string;
  provider?: string;
  session_id?: string | null;
  timestamp?: string | null;
}

/** Internal worker input, not the Relay wire format. */
export interface RelayEvent {
  path?: string;
  topic: string;
  session: RelaySession;
  event: AgentEvent;
}

export interface RelayRecord {
  path?: string;
  topic: string;
  session: RelaySession;
  record_id: string;
  operation: "upsert" | "remove";
  native?: unknown;
  events: AgentEvent[];
}

/** Validate the entire batch before allowing any of its events through. */
export function parseRelayRecord(value: unknown): RelayRecord | null {
  const record = asObject(value);
  const session = asObject(record?.session);
  const topic = asString(record?.topic);
  const sessionId = asString(session?.session_id);
  const recordId = asString(record?.record_id);
  const operation = record?.operation;
  if (!record || !session || !topic || !sessionId || !recordId
    || (operation !== "upsert" && operation !== "remove")
    || !Array.isArray(record.events)) {
    return null;
  }
  const events: AgentEvent[] = [];
  for (const value of record.events) {
    const event = asObject(value);
    const type = asString(event?.type);
    if (!event || !type) {
      return null;
    }
    events.push({ ...event, type });
  }
  if (operation === "remove" && events.length !== 0) {
    return null;
  }
  const parsed: RelayRecord = {
    topic,
    session: { session_id: sessionId },
    record_id: recordId,
    operation,
    events
  };
  const path = asString(record.path);
  if (path) parsed.path = path;
  const provider = asString(session.provider);
  if (provider) parsed.session.provider = provider;
  for (const key of [
    "parent_session_id", "agent_path", "agent_nickname", "agent_role",
    "title", "cwd", "started_at"
  ] as const) {
    const value = session[key];
    if (value === null || typeof value === "string") {
      parsed.session[key] = value;
    }
  }
  if (session.project === null) {
    parsed.session.project = null;
  } else {
    const project = asObject(session.project);
    if (project) {
      parsed.session.project = {};
      for (const key of [
        "id", "name", "project_name", "folder", "folder_name", "repository_name",
        "repository_url", "branch", "commit_hash"
      ] as const) {
        const value = project[key];
        if (value === null || typeof value === "string") {
          parsed.session.project[key] = value;
        }
      }
    }
  }
  if (Object.hasOwn(record, "native")) parsed.native = record.native;
  return parsed;
}

/** Translate mutable record snapshots to activity without replaying unchanged
 * event occurrences. The bounded cache is not a durable deduplication guarantee.
 * JSONL records are immutable append observations and need no retained cache.
 */
export class RelayActivityDispatcher {
  #snapshots = new Map<string, Map<string, number>>();
  #capacity: number;

  constructor(capacity = 4096) {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new Error("Relay activity cache capacity must be a positive integer");
    }
    this.#capacity = capacity;
  }

  async dispatch(
    record: RelayRecord,
    onEvent: (event: RelayEvent) => void | Promise<void>,
    signal?: AbortSignal
  ): Promise<void> {
    if (!["opencode.", "zcode.", "workbuddy.", "dsh."].some((prefix) => record.topic.startsWith(prefix))) {
      await dispatchRelayRecord(record, onEvent, signal);
      return;
    }
    const key = JSON.stringify([record.path, record.topic, record.record_id]);
    const previous = this.#snapshots.get(key);
    this.#snapshots.delete(key);
    if (record.operation === "remove") return;
    const occurrences = new Map<string, number>();
    // Context events can shift positions; counts retain additional identical events.
    const events = record.events.filter((event) => {
      const fingerprint = JSON.stringify(event);
      const count = (occurrences.get(fingerprint) ?? 0) + 1;
      occurrences.set(fingerprint, count);
      return count > (previous?.get(fingerprint) ?? 0);
    });
    await dispatchRelayRecord({
      ...record,
      events
    }, onEvent, signal);
    if (signal?.aborted) return;
    this.#snapshots.set(key, occurrences);
    if (this.#snapshots.size > this.#capacity) {
      this.#snapshots.delete(this.#snapshots.keys().next().value!);
    }
  }
}

/** Preserve ordering and backpressure; empty batches are no-ops. */
export async function dispatchRelayRecord(
  record: RelayRecord,
  onEvent: (event: RelayEvent) => void | Promise<void>,
  signal?: AbortSignal
): Promise<void> {
  for (const event of record.events) {
    if (signal?.aborted) return;
    const input: RelayEvent = { topic: record.topic, session: record.session, event };
    if (record.path !== undefined) input.path = record.path;
    await onEvent(input);
  }
}

export function asObject(value: unknown): JsonObject | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as JsonObject
    : null;
}

export function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

export function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

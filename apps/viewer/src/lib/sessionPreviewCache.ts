import type { BackgroundSessionUpdate, BackgroundUpdatesSubscription, SessionNotification } from "./types";

interface PreviewEntry {
  preview?: string | null;
  activity?: SessionNotification;
  message_version: number;
  removed?: boolean;
}
export interface PreviewReadToken { epoch: number; message_version: number }

export function assistantPreviewLine(value: string | null | undefined): string | null {
  const line = value?.split(/\r?\n/).find((line) => line.trim())?.replace(/\s+/g, " ").trim();
  return line ? [...line].slice(0, 240).join("") : null;
}

/** Compact live state has its own budget; it never creates a timeline replica. */
export class SessionPreviewCache {
  private entries = new Map<string, PreviewEntry>();
  private subscription_id: string | null = null;
  private generation: string | null = null;
  private revision = 0n;
  private epoch = 0;
  private clock = 0;

  constructor(private limit = 2048) {}

  begin(subscription_id: string) {
    this.subscription_id = subscription_id;
    this.generation = null;
    this.revision = 0n;
    this.resetAuthority();
  }

  private resetAuthority() {
    // A new live interest has no replay baseline. Keep bounded known text,
    // while relinquishing the previous stream's activity authority.
    for (const [key, entry] of this.entries) this.entries.set(key, { preview: entry.preview, message_version: entry.message_version, removed: entry.removed });
    this.epoch += 1;
  }

  acknowledge(ack: BackgroundUpdatesSubscription) {
    if (ack.subscription_id !== this.subscription_id || ack.subscribed === false) return;
    // Events can overtake the command response; their cursor must survive it.
    if (this.generation === null) this.generation = ack.generation;
  }

  apply(update: BackgroundSessionUpdate): boolean {
    if (update.subscription_id !== this.subscription_id || !/^\d+$/.test(update.revision)) return false;
    const next = BigInt(update.revision);
    if (this.generation !== null && update.generation !== this.generation) {
      // A subscription identity is tied to one server generation. Reconnects
      // get a new identity; an old server must not regain authority.
      return false;
    }
    if (next <= this.revision) return false;
    this.generation ??= update.generation;
    if (next !== this.revision + 1n) {
      // Every record is a complete compact projection for its session. A gap
      // invalidates other cached projections without asking for their history.
      this.resetAuthority();
    }
    this.revision = next;
    if (update.removed) {
      this.write(update.session_key, { preview: null, removed: true, message_version: ++this.clock });
      return true;
    }
    const previous = this.entries.get(update.session_key);
    const { session_key, has_unread, unread_final_count, is_running, has_running_descendant, question_attention } = update;
    const entry: PreviewEntry = { ...previous, removed: false, message_version: previous?.message_version ?? 0,
      activity: { session_key, has_unread, unread_final_count, is_running, has_running_descendant, question_attention } };
    // Null message_revision means no body projection was available, rather
    // than that the source authoritatively removed its last assistant message.
    if (update.message_revision !== null) {
      entry.preview = assistantPreviewLine(update.latest_assistant_message?.preview);
      entry.message_version = ++this.clock;
    }
    this.write(session_key, entry);
    return true;
  }

  capture(session_key: string): PreviewReadToken {
    return { epoch: this.epoch, message_version: this.entries.get(session_key)?.message_version ?? 0 };
  }

  rememberPage(session_key: string, preview: string | null, token: PreviewReadToken): boolean {
    const previous = this.entries.get(session_key);
    if (previous?.removed || token.epoch !== this.epoch || token.message_version !== (previous?.message_version ?? 0)) return false;
    const line = assistantPreviewLine(preview);
    if (previous?.preview === line || (line === null && previous?.preview === undefined)) return false;
    this.write(session_key, { ...previous, preview: line, message_version: ++this.clock });
    return true;
  }

  preview(session_key: string): string | null | undefined { return this.entries.get(session_key)?.preview; }
  activity(session_key: string): SessionNotification | undefined { return this.entries.get(session_key)?.activity; }
  isRemoved(session_key: string): boolean { return this.entries.get(session_key)?.removed === true; }

  private write(session_key: string, entry: PreviewEntry) {
    this.entries.delete(session_key);
    this.entries.set(session_key, entry);
    while (this.entries.size > this.limit) {
      this.entries.delete(this.entries.keys().next().value!);
      this.epoch += 1;
    }
  }
}

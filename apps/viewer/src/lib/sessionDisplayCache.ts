import { createUuid } from "./id";
import type { EventPageResponse, SessionUpdate, SessionUpdateItem, SessionUpdatesRequest, UpdateLevel } from "./types";

interface Replica {
  subscription_id: string;
  generation: string;
  revision: string;
  items: Map<string, SessionUpdateItem>;
  order: string[];
  event_order: string[];
  page: EventPageResponse;
  bytes: number;
}

function equal(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (!left || !right || typeof left !== "object" || typeof right !== "object") return false;
  const a = Object.entries(left), b = Object.entries(right);
  return a.length === b.length && a.every(([key, value]) => Object.prototype.hasOwnProperty.call(right, key)
    && equal(value, (right as Record<string, unknown>)[key]));
}

function weight(value: unknown): number {
  if (typeof value === "string") return value.length * 2;
  if (Array.isArray(value)) return value.reduce((size, item) => size + weight(item), 24);
  if (value && typeof value === "object") return Object.values(value).reduce<number>((size, item) => size + weight(item), 32);
  return 8;
}

/** Each level has independent coverage. A final-only update never advances the
 * steps cursor. Unchanged objects retain their references across updates. */
export class SessionDisplayCache {
  private replicas = new Map<string, Replica>();
  private subscriptions = new Map<string, string>();
  private prefix = createUuid();
  private access = new Map<string, number>();
  private clock = 0;
  private selected: string | null = null;
  private pending = new Map<string, SessionUpdate[]>();
  private pageItems = new WeakMap<EventPageResponse, Map<string, SessionUpdateItem>>();
  private acceptedItems = new Map<string, Map<string, SessionUpdateItem>>();
  private key(session_key: string, level: UpdateLevel) { return JSON.stringify([session_key, level]); }

  request(session_key: string, level: UpdateLevel = "all", detail_keys: string[] = []): SessionUpdatesRequest {
    const key = this.key(session_key, level);
    if (!this.access.has(session_key)) this.access.set(session_key, ++this.clock);
    let subscription_id = this.subscriptions.get(key);
    if (!subscription_id) {
      subscription_id = `${this.prefix}:${createUuid()}`;
      this.subscriptions.set(key, subscription_id);
    }
    return { session_key, level, subscription_id, cursor: this.replicas.get(key)?.revision ?? null, detail_keys };
  }

  select(session_key: string | null) {
    this.selected = session_key;
    if (session_key) this.access.set(session_key, ++this.clock);
  }

  release(session_key: string, level: UpdateLevel): SessionUpdatesRequest | null {
    const key = this.key(session_key, level);
    if (!this.subscriptions.has(key)) return null;
    const request = { ...this.request(session_key, level), unsubscribe: true };
    this.subscriptions.delete(key);
    return request;
  }

  get(session_key: string, level: UpdateLevel = "all"): EventPageResponse | null {
    const key = this.key(session_key, level);
    const replica = this.replicas.get(key);
    if (!replica) return null;
    this.replicas.delete(key); this.replicas.set(key, replica);
    return replica.page;
  }

  detail(session_key: string, event_key: string) {
    return this.replicas.get(this.key(session_key, "all"))?.items.get(`detail:${event_key}`)?.detail
      ?? this.replicas.get(this.key(session_key, "details"))?.items.get(`detail:${event_key}`)?.detail ?? null;
  }

  sourceEvents(session_key: string) {
    const replica = this.replicas.get(this.key(session_key, "all"));
    return replica?.event_order.map((id) => replica.items.get(id)?.event).filter((event) => event !== undefined) ?? [];
  }

  invalidate(session_key?: string) {
    for (const [key] of this.replicas) {
      if (!session_key || JSON.parse(key)[0] === session_key) { this.replicas.delete(key); this.pending.delete(key); }
    }
  }

  backgroundRequests(): SessionUpdatesRequest[] {
    return [...this.subscriptions.keys()].map((key) => JSON.parse(key) as [string, UpdateLevel])
      .filter(([session_key, level]) => level === "final" && session_key !== this.selected)
      .slice(-8).map(([session_key, level]) => this.request(session_key, level));
  }

  commit(session_key: string, page: EventPageResponse): ReadonlySet<string> | null {
    const items = this.pageItems.get(page);
    if (!items) return null;
    const previous = this.acceptedItems.get(session_key);
    const changed = new Set<string>();
    const changedKey = (key: string, item: SessionUpdateItem) => item.kind === "event" ? null : item.event_key ?? key;
    for (const [key, item] of items) {
      const eventKey = changedKey(key, item);
      if (eventKey && previous?.get(key) !== item) changed.add(eventKey);
    }
    for (const [key, item] of previous ?? []) {
      const eventKey = changedKey(key, item);
      if (eventKey && !items.has(key)) changed.add(eventKey);
    }
    this.acceptedItems.set(session_key, items);
    return changed;
  }

  stale(update: SessionUpdate): boolean {
    const previous = this.replicas.get(this.key(update.session_key, update.level));
    return previous?.generation === update.generation && (previous.revision.length > update.revision.length
      || (previous.revision.length === update.revision.length && previous.revision >= update.revision));
  }

  accepts(update: SessionUpdate): boolean {
    return this.subscriptions.get(this.key(update.session_key, update.level)) === update.subscription_id;
  }

  apply(update: SessionUpdate): EventPageResponse | null {
    const key = this.key(update.session_key, update.level);
    if (!this.accepts(update)) return null;
    const previous = this.replicas.get(key);
    if (previous?.generation === update.generation
      && (previous.revision.length > update.revision.length
        || (previous.revision.length === update.revision.length && previous.revision > update.revision))) return previous.page;
    if (!update.snapshot && previous?.generation === update.generation && previous.revision === update.revision) return previous.page;
    if (!update.snapshot && !previous) {
      const queued = this.pending.get(key) ?? [];
      queued.push(update);
      this.pending.set(key, queued.slice(-32));
      return null;
    }
    if (!update.snapshot && (!previous || previous.generation !== update.generation || previous.revision !== update.base_revision)) {
      this.replicas.delete(key);
      return null;
    }
    const items = new Map(update.snapshot ? [] : previous?.items);
    let bytes = update.snapshot ? 0 : previous?.bytes ?? 0;
    for (const id of update.removed_items) {
      if (items.has(id)) bytes -= weight(items.get(id));
      items.delete(id);
    }
    for (const item of [...update.items, ...(update.groups ?? [])]) {
      if (items.has(item.item_id)) bytes -= weight(items.get(item.item_id));
      const old = previous?.items.get(item.item_id);
      const retained = old && equal(old, item) ? old : item;
      items.set(item.item_id, retained); bytes += weight(retained);
    }
    const order = update.item_order ?? previous?.order ?? [];
    const event_order = update.event_order ?? previous?.event_order ?? [];
    if (event_order.some((id) => !items.get(id)?.event)) { this.replicas.delete(key); return null; }
    const events = order.map((id) => items.get(id)?.summary).filter((event) => event !== undefined);
    if (events.length !== order.length) { this.replicas.delete(key); return null; }
    const page: EventPageResponse = { ...update.state, events };
    this.pageItems.set(page, items);
    this.replicas.delete(key);
    this.replicas.set(key, { subscription_id: update.subscription_id, generation: update.generation, revision: update.revision, items, order, event_order, page, bytes });
    if (update.snapshot && this.pending.has(key)) {
      const queued = this.pending.get(key)!;
      this.pending.delete(key);
      for (const pending of queued) this.apply(pending);
      return this.replicas.get(key)?.page ?? null;
    }
    const sessions = () => new Set([...this.replicas.keys()].map((key) => JSON.parse(key)[0] as string));
    while (sessions().size > 8 || [...this.replicas.values()].reduce((size, replica) => size + replica.bytes, 0) > 64 * 1024 * 1024) {
      const oldest = [...sessions()].filter((session) => session !== this.selected)
        .sort((left, right) => (this.access.get(left) ?? 0) - (this.access.get(right) ?? 0))[0];
      if (!oldest) break;
      for (const key of this.replicas.keys()) if (JSON.parse(key)[0] === oldest) {
        this.replicas.delete(key); this.subscriptions.delete(key);
      }
      this.access.delete(oldest);
      this.acceptedItems.delete(oldest);
    }
    return page;
  }
}

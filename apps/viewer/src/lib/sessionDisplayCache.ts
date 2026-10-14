import { SessionResources } from "./sessionResources";
import { createUuid } from "./id";
import type { EventPageResponse, EventSummary, TrajectoryEventPageResponse, SessionUpdate, SessionUpdateItem, SessionUpdatesRequest, SessionUpdateScope, UpdateLevel } from "./types";

interface Replica {
  session_key: string;
  subscription_id: string;
  generation: string;
  revision: string;
  items: Map<string, SessionUpdateItem>;
  order: string[];
  semantic_order: string[];
  event_order: string[];
  page: EventPageResponse & { error?: string };
  bytes: number;
}

interface LiveLease {
  expires_at: number;
  suspended: boolean;
}

const LIVE_LEASE_MS = 90_000;

function sourceGenerationFrom(revision?: string): string | undefined {
  const index = revision?.lastIndexOf(":") ?? -1;
  return index < 0 ? undefined : revision!.slice(0, index);
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

function isAssistantFragment(event: EventSummary): boolean {
  return event.phase === "delta" && (event.provider === "codex" || event.provider === "dsh");
}

interface AssistantPreviewProjection {
  preview: string | null;
  latest_is_fragment: boolean;
}

/** Each level has independent coverage. A final-only update never advances the
 * steps cursor. Unchanged objects retain their references across updates. */
export class SessionDisplayCache {
  readonly resources = new SessionResources();
  runningRevision = 0;
  previewRevision = 0;
  private loadedPreviews = new Map<string, AssistantPreviewProjection>();
  coverageVersion(session_key: string, level: UpdateLevel = "steps") {
    const replica = this.replicas.get(this.key(session_key, level));
    return replica ? { generation: replica.generation, revision: replica.revision } : {};
  }
  subscriptionIds() { return [...this.subscriptions.values()]; }
  private replicas = new Map<string, Replica>();
  private subscriptions = new Map<string, string>();
  private prefix = createUuid();
  private access = new Map<string, number>();
  private clock = 0;
  private selected: string | null = null;
  private scopes = new Map<string, SessionUpdateScope>();
  private pending = new Map<string, SessionUpdate[]>();
  private pageItems = new WeakMap<EventPageResponse, Map<string, SessionUpdateItem>>();
  private acceptedSources = new Map<string, string>();
  private running = new Map<string, boolean>();
  private liveLeases = new Map<string, LiveLease>();
  private sourceRevisions = new Map<string, string>();
  private retiredGenerations = new Map<string, Set<string>>();
  private evictedSubscriptions: SessionUpdatesRequest[] = [];
  private acceptedItems = new Map<string, Map<string, SessionUpdateItem>>();
  private pageLevels = new WeakMap<EventPageResponse, UpdateLevel>();
  private key(session_key: string, level: UpdateLevel) { return JSON.stringify([session_key, level]); }

  request(session_key: string, level: UpdateLevel = "all", detail_keys: string[] = []): SessionUpdatesRequest {
    const key = this.key(session_key, level);
    if (!this.access.has(session_key)) this.access.set(session_key, ++this.clock);
    let subscription_id = this.subscriptions.get(key);
    if (!subscription_id) {
      subscription_id = `${this.prefix}:${createUuid()}`;
      this.subscriptions.set(key, subscription_id);
      this.runningRevision += 1;
    }
    const scope = this.scopes.get(session_key) ?? { history: "recent_turns" as const, group_keys: [] };
    return { session_key, level, subscription_id, cursor: this.replicas.get(key)?.revision ?? null, detail_keys,
      ...(scope ? { scope: level === "details" ? { ...scope, group_keys: [] } : scope } : {}) };
  }

  includeGroup(session_key: string, group_key: string) {
    const scope = this.scopes.get(session_key) ?? { history: "recent_turns" as const, group_keys: [] };
    this.scopes.set(session_key, { ...scope, group_keys: [...new Set([...scope.group_keys, group_key])] });
  }

  includeHistory(session_key: string) {
    const scope = this.scopes.get(session_key) ?? { history: "recent_turns" as const, group_keys: [] };
    this.scopes.set(session_key, { ...scope, history: "retained" });
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
    this.liveLeases.delete(request.subscription_id);
    this.clearUnownedRunning(session_key);
    this.runningRevision += 1;
    return request;
  }

  get(session_key: string, level: UpdateLevel = "all"): EventPageResponse | null {
    const key = this.key(session_key, level);
    const replica = this.replicas.get(key);
    if (!replica) return null;
    this.replicas.delete(key); this.replicas.set(key, replica);
    return replica.page;
  }

  /** Indexed attention can lag the source stream. Only a live replica owns
   * running state; retained display content alone must not mask the catalog. */
  liveRunning(session_key: string): boolean | undefined {
    const subscribed = (["messages", "steps", "all", "final"] as const).some((level) => {
      const key = this.key(session_key, level);
      const replica = this.replicas.get(key);
      return replica && this.subscriptions.get(key) === replica.subscription_id && this.hasLiveLease(replica);
    });
    return subscribed ? this.running.get(session_key) : undefined;
  }

  /** Retained cards remain useful while their source ownership is suspended. */
  suspendLive(ids: readonly string[] = this.subscriptionIds()) {
    const active = new Set(this.subscriptionIds());
    for (const id of ids) {
      if (!active.has(id)) continue;
      const lease = this.liveLeases.get(id);
      if (!lease?.suspended) {
        this.liveLeases.set(id, { expires_at: lease?.expires_at ?? 0, suspended: true });
        this.runningRevision += 1;
      }
    }
  }

  /** A renewal proves an existing interest is alive; it cannot recover a
   * suspended or expired baseline whose missing updates still need a read. */
  acknowledgeRenewal(ids: readonly string[]) {
    for (const replica of this.replicas.values()) {
      if (!ids.includes(replica.subscription_id) || !this.hasLiveLease(replica)) continue;
      this.liveLeases.set(replica.subscription_id, { expires_at: Date.now() + LIVE_LEASE_MS, suspended: false });
    }
  }

  requests(ids?: readonly string[]): SessionUpdatesRequest[] {
    return [...this.subscriptions].filter(([, id]) => !ids || ids.includes(id)).map(([key]) => {
      const [session_key, level] = JSON.parse(key) as [string, UpdateLevel];
      return this.request(session_key, level);
    });
  }

  /** Return exact release requests before the transport forgets their IDs. */
  takeEvictedSubscriptions(): SessionUpdatesRequest[] {
    const requests = this.evictedSubscriptions;
    this.evictedSubscriptions = [];
    return requests;
  }

  liveRecoveryRequests(): SessionUpdatesRequest[] {
    return this.requests().filter((request) => request.level !== "details" && this.needsBaseline([request.subscription_id]));
  }

  needsBaseline(ids?: readonly string[]): boolean {
    return this.requests(ids).some((request) => {
      if (request.level === "details") return false;
      const replica = this.replicas.get(this.key(request.session_key, request.level));
      return !replica || replica.subscription_id !== request.subscription_id || !this.hasLiveLease(replica);
    });
  }

  sourceGeneration(session_key: string): string | undefined {
    return sourceGenerationFrom(this.sourceRevisions.get(session_key));
  }

  /** Source generations are opaque. A request begun before a replacement may
   * publish only if its response belongs to the source now accepted locally. */
  acceptsSnapshot(update: SessionUpdate, source_generation_at_request: string | undefined): boolean {
    if (update.level === "details") return true;
    if (this.isRetiredSource(update) || this.isOlderSource(update)) return false;
    const current = this.sourceGeneration(update.session_key);
    return current === source_generation_at_request || sourceGenerationFrom(update.state.source_revision) === current;
  }

  private hasLiveLease(replica: Replica): boolean {
    const lease = this.liveLeases.get(replica.subscription_id);
    const current = this.sourceGeneration(replica.session_key);
    return !!lease && !lease.suspended && Date.now() < lease.expires_at
      && !replica.page.error && !replica.page.follow_error
      && (current === undefined || sourceGenerationFrom(replica.page.source_revision) === current);
  }

  private clearUnownedRunning(session_key: string) {
    if (![...this.replicas].some(([key, replica]) => JSON.parse(key)[0] === session_key
      && this.subscriptions.get(key) === replica.subscription_id && this.hasLiveLease(replica))) {
      this.running.delete(session_key);
    }
  }

  private isRetiredSource(update: SessionUpdate): boolean {
    const generation = sourceGenerationFrom(update.state.source_revision);
    return update.level !== "details" && generation !== undefined
      && this.retiredGenerations.get(update.session_key)?.has(generation) === true;
  }

  private isOlderSource(update: SessionUpdate): boolean {
    if (update.level === "details") return false;
    const before = this.sourceRevisions.get(update.session_key), after = update.state.source_revision;
    if (!before || !after || sourceGenerationFrom(before) !== sourceGenerationFrom(after)) return false;
    const before_revision = before.slice(before.lastIndexOf(":") + 1), after_revision = after.slice(after.lastIndexOf(":") + 1);
    return /^\d+$/.test(before_revision) && /^\d+$/.test(after_revision) && BigInt(after_revision) < BigInt(before_revision);
  }

  private observeSource(session_key: string, page: EventPageResponse, subscription_id: string) {
    const generation = sourceGenerationFrom(page.source_revision);
    if (generation === undefined) return;
    const previous = this.sourceGeneration(session_key);
    this.sourceRevisions.set(session_key, page.source_revision!);
    if (previous === generation) return;
    if (previous !== undefined) {
      const retired = this.retiredGenerations.get(session_key) ?? new Set<string>();
      retired.add(previous);
      this.retiredGenerations.set(session_key, retired);
      this.suspendLive(this.requests().filter((request) => request.session_key === session_key
        && request.subscription_id !== subscription_id).map((request) => request.subscription_id));
    }
    this.running.delete(session_key);
  }

  /** An active semantic replica is fresher than the independently indexed
   * compact stream. Retained/released timelines do not keep this authority. */
  liveAssistantPreview(session_key: string): string | null | undefined {
    const active = (["messages", "steps", "all"] as const).some((level) => {
      const key = this.key(session_key, level);
      const replica = this.replicas.get(key);
      return replica && this.subscriptions.get(key) === replica.subscription_id && this.hasLiveLease(replica);
    });
    if (!active) return undefined;
    const projection = this.assistantPreviewProjection(session_key);
    // Codex and DSH deltas are fragments, unlike OpenCode/ZCode snapshots.
    // The compact producer accumulates those fragments with provider context.
    return projection.latest_is_fragment ? undefined : projection.preview;
  }

  /** Best available assistant line from retained semantic summaries. Reading
   * this projection never subscribes, loads history, or fetches tool details. */
  assistantPreview(session_key: string): string | null {
    return this.assistantPreviewProjection(session_key).preview;
  }

  hasFragmentedAssistantPreview(session_key: string): boolean {
    return this.assistantPreviewProjection(session_key).latest_is_fragment;
  }

  private assistantPreviewProjection(session_key: string): AssistantPreviewProjection {
    const currentGeneration = this.sourceGeneration(session_key);
    const revision = (replica: Replica) => {
      const value = replica.page.source_revision?.split(":").pop();
      return value && /^\d+$/.test(value) ? BigInt(value) : 0n;
    };
    const eligible = (["messages", "steps", "all", "final"] as const)
      .map((level) => ({ level, replica: this.replicas.get(this.key(session_key, level)) }))
      .filter((entry): entry is { level: "messages" | "steps" | "all" | "final"; replica: Replica } => !!entry.replica
        && (currentGeneration === undefined || sourceGenerationFrom(entry.replica.page.source_revision) === currentGeneration));
    const complete = eligible.filter(({ level }) => level !== "final")
      .sort((left, right) => revision(left.replica) < revision(right.replica) ? 1 : revision(left.replica) > revision(right.replica) ? -1 : 0)[0];
    // A newer full semantic page can remove a message. Older, filtered final
    // replicas must not resurrect it; newer final updates can still contribute.
    const replicas = eligible.filter((entry) => entry === complete || (entry.level === "final"
      && (!complete || revision(entry.replica) > revision(complete.replica))));
    if (eligible.length === 0) return this.loadedPreviews.get(session_key) ?? { preview: null, latest_is_fragment: false };
    let preview: string | null = null;
    let position = -1;
    let latestPosition = -1;
    let latest_is_fragment = false;
    for (const { replica } of replicas) {
      // Semantic messages belong to this session even when its display rows
      // fold intermediate replies into work groups. Child sessions have their
      // own replicas and never enter this preview.
      const summaries = [...replica.items.values()].flatMap((item) => item.summary ? [item.summary] : []);
      for (const [index, event] of summaries.entries()) {
        if (event.type !== "message" || event.role !== "assistant" || event.is_hidden) continue;
        // Absolute source slots preserve chronology across delivery levels and
        // older-history expansion. Legacy pages retain their local ordering.
        const slot = (event.slot_key ?? event.event_key).match(/(?:^|\.)event\.v1\.([0-9a-f]+)$/);
        const sourcePosition = slot ? Number.parseInt(slot[1], 16) : index;
        if (sourcePosition >= latestPosition) {
          latestPosition = sourcePosition;
          latest_is_fragment = isAssistantFragment(event);
        }
        if (isAssistantFragment(event)) continue;
        const line = event.summary.split(/\r?\n/).find((line) => line.trim())?.replace(/\s+/g, " ").trim();
        if (!line) continue;
        if (sourcePosition >= position) {
          position = sourcePosition;
          preview = [...line].slice(0, 240).join("");
        }
      }
    }
    return { preview, latest_is_fragment };
  }

  /** Record already accepted legacy pages; semantic pages are owned by their
   * replicas. Retain only the bounded preview, never another history copy. */
  rememberLoadedPage(session_key: string, page: EventPageResponse) {
    if (this.pageItems.has(page)) return;
    const assistantEvents = [...page.events].reverse().filter((event) => event.type === "message" && event.role === "assistant" && !event.is_hidden);
    const latest_is_fragment = !!assistantEvents[0] && isAssistantFragment(assistantEvents[0]);
    const event = assistantEvents.find((event) => !isAssistantFragment(event) && event.summary.trim());
    const line = event?.summary.split(/\r?\n/).find((line) => line.trim())?.replace(/\s+/g, " ").trim();
    const preview = line ? [...line].slice(0, 240).join("") : null;
    const previous = this.loadedPreviews.get(session_key);
    this.loadedPreviews.delete(session_key);
    this.loadedPreviews.set(session_key, { preview, latest_is_fragment });
    if (previous?.preview !== preview || previous?.latest_is_fragment !== latest_is_fragment) this.previewRevision += 1;
    while (this.loadedPreviews.size > 8) {
      const oldest = [...this.loadedPreviews.keys()].find((key) => key !== this.selected);
      if (!oldest) break;
      this.loadedPreviews.delete(oldest);
      this.previewRevision += 1;
    }
  }

  private observeRunning(session_key: string, page: EventPageResponse) {
    if (page.is_running === undefined) return;
    this.runningRevision += 1;
    this.running.set(session_key, page.is_running);
  }

  private currentResourceReplicas(session_key: string, levels: UpdateLevel[]): Replica[] {
    const source = this.sourceRevisions.get(session_key);
    return levels.flatMap((level) => {
      const key = this.key(session_key, level);
      const replica = this.replicas.get(key);
      if (!replica || (source && replica.page.source_revision && source !== replica.page.source_revision)) return [];
      return [{ replica, active: this.subscriptions.get(key) === replica.subscription_id }];
    }).sort((left, right) => Number(right.active) - Number(left.active)).map(({ replica }) => replica);
  }

  detail(session_key: string, event_key: string) {
    return this.currentResourceReplicas(session_key, ["all", "steps", "details"])
      .map((replica) => replica.items.get(`detail:${event_key}`)?.detail).find((detail) => !!detail)
      ?? this.resources.detail(session_key, "tool", event_key);
  }

  /** Complete semantic membership; activity disclosures decide which rows to mount. */
  trajectoryGroup(session_key: string, trajectory_key: string): TrajectoryEventPageResponse | null {
    const replicas = this.currentResourceReplicas(session_key, ["all", "steps"]);
    const replica = replicas.find((entry) => {
      const keys = entry?.items.get(trajectory_key)?.summary?.child_keys;
      return keys && keys.every((key) => entry?.items.get(key)?.summary);
    });
    const keys = replica?.items.get(trajectory_key)?.summary?.child_keys;
    if (!replica || !keys) return null;
    const events = keys.map((key) => replica.items.get(key)?.summary);
    return {
      events: events as NonNullable<typeof events[number]>[],
      previous_cursor: null,
      next_cursor: null,
      total_events: keys.length,
    };
  }

  activityGroupForEvent(session_key: string, event_key: string): string | null {
    for (const replica of this.currentResourceReplicas(session_key, ["all", "steps"])) {
      for (const item of replica.items.values()) {
        if (item.summary?.type === "activity_group" && item.summary.child_keys?.includes(event_key)) return item.item_id;
      }
    }
    return null;
  }

  sourceEvents(session_key: string) {
    const replica = this.replicas.get(this.key(session_key, "all"));
    return replica?.event_order.map((id) => replica.items.get(id)?.event).filter((event) => event !== undefined) ?? [];
  }

  invalidate(session_key?: string) {
    this.runningRevision += 1;
    this.previewRevision += 1;
    const owners = session_key ? [session_key] : [...new Set([
      ...this.running.keys(), ...this.loadedPreviews.keys(), ...this.acceptedSources.keys(),
      ...this.access.keys(), ...[...this.subscriptions.keys(), ...this.replicas.keys(), ...this.pending.keys()].map((key) => JSON.parse(key)[0] as string),
    ])];
    this.suspendLive(this.requests().filter((request) => owners.includes(request.session_key)).map((request) => request.subscription_id));
    for (const owner of owners) {
      this.running.delete(owner);
      this.loadedPreviews.delete(owner);
      for (const key of this.acceptedItems.keys()) if (JSON.parse(key)[0] === owner) this.acceptedItems.delete(key);
      this.acceptedSources.delete(owner);
      this.resources.remove(owner);
    }
    for (const map of [this.replicas, this.pending]) {
      for (const key of map.keys()) if (owners.includes(JSON.parse(key)[0])) map.delete(key);
    }
  }

  commit(session_key: string, page: EventPageResponse): ReadonlySet<string> | null {
    const items = this.pageItems.get(page);
    if (!items) return null;
    const acceptedKey = this.key(session_key, this.pageLevels.get(page) ?? "steps");
    const previous = this.acceptedItems.get(acceptedKey);
    const changed = new Set<string>();
    const previousSource = this.acceptedSources.get(session_key);
    if (page.source_revision) {
      if (previousSource && previousSource !== page.source_revision) {
        for (const key of this.resources.payloadKeys(session_key)) changed.add(key);
      }
      this.acceptedSources.set(session_key, page.source_revision);
    }
    const changedKey = (key: string, item: SessionUpdateItem) => item.kind === "event" ? null : item.event_key ?? key;
    for (const [key, item] of items) {
      const eventKey = changedKey(key, item);
      if (eventKey && previous && !equal(previous.get(key), item)) changed.add(eventKey);
    }
    for (const [key, item] of previous ?? []) {
      const eventKey = changedKey(key, item);
      if (eventKey && !items.has(key)) changed.add(eventKey);
    }
    const details = this.replicas.get(this.key(session_key, "details"));
    for (const key of changed) { details?.items.delete(`detail:${key}`); this.resources.invalidate(session_key, key); }
    this.acceptedItems.set(acceptedKey, items);
    return changed;
  }

  stale(update: SessionUpdate): boolean {
    if (this.isRetiredSource(update) || this.isOlderSource(update)) return true;
    const previous = this.replicas.get(this.key(update.session_key, update.level));
    return previous?.generation === update.generation && (previous.revision.length > update.revision.length
      || (previous.revision.length === update.revision.length && previous.revision >= update.revision));
  }

  accepts(update: SessionUpdate): boolean {
    return this.subscriptions.get(this.key(update.session_key, update.level)) === update.subscription_id;
  }

  apply(update: SessionUpdate, leaseAcknowledged = false): EventPageResponse | null {
    const key = this.key(update.session_key, update.level);
    if (!this.accepts(update) || this.isRetiredSource(update) || this.isOlderSource(update)) return null;
    const current_source = this.sourceGeneration(update.session_key);
    const next_source = sourceGenerationFrom(update.state.source_revision);
    if (!leaseAcknowledged && update.level !== "details" && current_source !== undefined
      && next_source !== undefined && next_source !== current_source) {
      // Opaque generations have no ordering. A queued frame can describe an
      // unseen older source, so only an acknowledged backward baseline may
      // replace the accepted source; keep the current cards until it arrives.
      this.suspendLive([update.subscription_id]);
      return null;
    }
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
      this.dropReplica(key, update.session_key);
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
    const semantic_order = update.semantic_order ?? (update.snapshot || update.item_order != null ? order : previous?.semantic_order ?? order);
    const display_order = update.level === "messages" || update.level === "final" ? semantic_order : order;
    const event_order = update.event_order ?? previous?.event_order ?? [];
    if (event_order.some((id) => !items.get(id)?.event)) { this.dropReplica(key, update.session_key); return null; }
    const events = display_order.map((id) => items.get(id)?.summary).filter((event) => event !== undefined);
    if (update.level === "all" && [...items.values()].some((item) =>
      item.summary?.child_keys?.some((child) => !items.get(child)?.summary))) {
      this.dropReplica(key, update.session_key); return null;
    }
    if (events.length !== display_order.length) { this.dropReplica(key, update.session_key); return null; }
    const page: Replica["page"] = { ...update.state, events,
      ...(update.level === "messages" || update.level === "final" ? { total_events: events.length } : {}) };
    if (update.level !== "details" && update.state.scope) {
      const pendingScope = this.scopes.get(update.session_key);
      this.scopes.set(update.session_key, { ...update.state.scope,
        history: pendingScope?.history === "retained" ? "retained" : update.state.scope.history,
        group_keys: update.snapshot && previous && previous.generation !== update.generation
          ? update.state.scope.group_keys : [...new Set([...update.state.scope.group_keys, ...(pendingScope?.group_keys ?? [])])],
      });
    }
    if (update.level !== "details") {
      if (page.error || page.follow_error) {
        this.suspendLive(this.requests().filter((request) => request.session_key === update.session_key).map((request) => request.subscription_id));
      } else {
        const lease = this.liveLeases.get(update.subscription_id);
        const baseline = update.snapshot && (leaseAcknowledged || !lease);
        if (baseline || lease && !lease.suspended && Date.now() < lease.expires_at) {
          this.observeSource(update.session_key, page, update.subscription_id);
          this.observeRunning(update.session_key, page);
        }
        if (baseline) {
          this.liveLeases.set(update.subscription_id, { expires_at: Date.now() + LIVE_LEASE_MS, suspended: false });
        }
      }
    }
    if (update.level !== "details") this.previewRevision += 1;
    this.pageItems.set(page, items);
    this.pageLevels.set(page, update.level);
    this.replicas.delete(key);
    this.replicas.set(key, { session_key: update.session_key, subscription_id: update.subscription_id, generation: update.generation, revision: update.revision, items, order, semantic_order, event_order, page, bytes });
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
      for (const key of new Set([...this.replicas.keys(), ...this.subscriptions.keys(), ...this.pending.keys()])) if (JSON.parse(key)[0] === oldest) {
        const [, level] = JSON.parse(key) as [string, UpdateLevel];
        const release = this.release(oldest, level);
        if (release) this.evictedSubscriptions.push(release);
        this.replicas.delete(key); this.pending.delete(key);
      }
      this.access.delete(oldest);
      this.scopes.delete(oldest);
      this.resources.remove(oldest);
      for (const key of this.acceptedItems.keys()) if (JSON.parse(key)[0] === oldest) this.acceptedItems.delete(key);
      this.acceptedSources.delete(oldest);
      this.running.delete(oldest);
      this.sourceRevisions.delete(oldest);
      this.retiredGenerations.delete(oldest);
      this.loadedPreviews.delete(oldest);
    }
    return page;
  }

  private dropReplica(key: string, session_key: string) {
    const id = this.subscriptions.get(key);
    if (id) this.suspendLive([id]);
    this.replicas.delete(key);
    this.pending.delete(key);
    this.resources.invalidate(session_key);
    this.clearUnownedRunning(session_key);
    this.runningRevision += 1;
  }
}

import { createUuid } from "./id";
import { listSessionChildren, listSessions, openSessionList } from "./tauri";
import { SESSION_PAGE_SIZE } from "./state";
import { compareActivity, compareProjects, compareSessionChildren } from "./sidebarOrder";
import type { CommandInvoker } from "./transport";
import type { ListSessionsResponse, OpenSessionListRequest, SessionListQuery, SessionListEntry, SessionListUpdate, SessionSummary } from "./types";

class InvalidListUpdate extends Error {}
class ListCursorGap extends InvalidListUpdate {}

interface Watch {
  request: OpenSessionListRequest;
  entries: Map<string, SessionListEntry>;
  page?: ListSessionsResponse;
  cursor?: string;
  opening?: Promise<ListSessionsResponse>;
  queued: SessionListUpdate[];
  buffering: boolean;
  overflow: boolean;
  queuedRecovery?: Promise<void>;
  retry: boolean;
  waitingForLease: boolean;
}

function apply(watch: Watch, update: SessionListUpdate): void {
  if (update?.subscription_id !== watch.request.subscription_id || typeof update.synced_through !== "string" || !update.synced_through
    || !Array.isArray(update.sessions) || !Array.isArray(update.removed_keys) || !Array.isArray(update.pending_providers)
    || !Array.isArray(update.source_errors) || (update.next_cursor !== null && typeof update.next_cursor !== "string")) {
    throw new InvalidListUpdate("Invalid session list update");
  }
  const snapshot = update.base_cursor === null;
  if (snapshot && update.removed_keys.length) throw new InvalidListUpdate("Invalid session list snapshot");
  if (!snapshot && (!watch.page || !watch.cursor || update.base_cursor !== watch.cursor)) throw new ListCursorGap();

  // Reuse unchanged summaries so their visible overlays survive a snapshot.
  const entries = snapshot ? new Map<string, SessionListEntry>() : new Map(watch.entries);
  const touched = new Set<string>();
  for (const key of update.removed_keys) {
    if (typeof key !== "string" || !key || touched.has(key)) throw new InvalidListUpdate("Invalid removed session key");
    touched.add(key);
    entries.delete(key);
  }
  let changed = (snapshot ? update.sessions.length : entries.size) !== watch.entries.size;
  for (const entry of update.sessions) {
    const key = entry?.session?.session_key;
    if (typeof key !== "string" || !key || touched.has(key) || typeof entry.revision !== "string" || !/^[a-f0-9]{64}$/.test(entry.revision)) {
      throw new InvalidListUpdate("Invalid session list entry");
    }
    touched.add(key);
    const old = watch.entries.get(key);
    if (old?.revision === entry.revision) entries.set(key, old);
    else {
      entries.set(key, entry);
      changed = true;
    }
  }
  const sessions = !changed && watch.page && !watch.waitingForLease
    ? watch.page.sessions : [...entries.values()].map((entry) => entry.session);
  const previous = watch.page;
  if (!previous || sessions !== previous.sessions || update.next_cursor !== previous.next_cursor
    || JSON.stringify(update.pending_providers) !== JSON.stringify(previous.pending_providers)
    || JSON.stringify(update.source_errors) !== JSON.stringify(previous.source_errors)) {
    if (!snapshot && update.synced_through === watch.cursor) throw new InvalidListUpdate("Changed session list reused its cutoff");
    watch.page = { sessions, next_cursor: update.next_cursor, pending_providers: update.pending_providers, source_errors: update.source_errors };
  }
  watch.entries = entries;
  watch.cursor = update.synced_through;
}

function applyPush(watch: Watch, update: SessionListUpdate): void {
  if (watch.cursor && update.synced_through === watch.cursor) return;
  // A pushed replacement has no base to check. An open from our latest
  // cutoff confirms its current contents even if a late frame is delivered.
  if (update.base_cursor === null) throw new ListCursorGap();
  apply(watch, update);
}

const MAX_PENDING = 32;
// Start another independent live window on a page boundary before the host
// limit. Previously loaded sessions keep their subscriptions as browsing continues.
const SESSIONS_PER_WINDOW = Math.floor(4096 / SESSION_PAGE_SIZE) * SESSION_PAGE_SIZE;
type Parent = string | null;

export class SessionLists {
  private windows = new Map<Parent, Watch[]>();
  private rootQuery?: SessionListQuery;
  private byId = new Map<string, Watch>();
  private recovering?: Promise<void>;
  private recoveryQueue = Promise.resolve();
  private reconnectPending = false;
  private nextAdmission = 0;

  constructor(
    private send: CommandInvoker,
    private release: (command: string, payload: Record<string, unknown>) => Promise<void>,
    private prefix?: string,
  ) {}
  onPage: (parent: Parent, page: ListSessionsResponse) => void = () => {};
  onError: (parent: Parent, error: unknown) => void = () => {};
  limit(parent: Parent) {
    return this.windows.get(parent)?.reduce((n, watch) => n + watch.request.limit, 0) ?? 0;
  }

  drop(parent: Parent) {
    for (const watch of this.windows.get(parent) ?? []) {
      this.byId.delete(watch.request.subscription_id);
      this.retire(watch);
    }
    this.windows.delete(parent);
  }
  private retire(watch: Watch) {
    void this.release("release_session_list", { request: { subscription_id: watch.request.subscription_id } }).catch(() => {});
  }
  clear(childrenOnly = false) {
    for (const parent of this.windows.keys()) if (!childrenOnly || parent !== null) this.drop(parent);
  }

  private combined(parent: Parent): ListSessionsResponse {
    const windows = this.windows.get(parent) ?? [];
    const first = windows[0]?.page;
    if (!first) throw new Error("Session list was closed");
    const compare = parent !== null ? compareSessionChildren : this.rootQuery?.order === "project" ? compareProjects : compareActivity;
    const sessions = new Map<string, SessionSummary>();
    let cursor = first.next_cursor;
    for (const watch of windows) {
      if (!watch.page) break;
      for (const session of watch.page.sessions) {
        if (!sessions.has(session.session_key)) sessions.set(session.session_key, session);
      }
      cursor = watch.page.next_cursor;
    }
    return { ...first, sessions: [...sessions.values()].sort(compare), next_cursor: cursor };
  }

  async open(parent: Parent, query: SessionListQuery | undefined, limit: number): Promise<ListSessionsResponse> {
    let windows = this.windows.get(parent);
    let reusable: Map<string, SessionListEntry> | undefined;
    const current = windows?.[0]?.request.query;
    const orderChanged = parent === null && (this.rootQuery?.order ?? "time") !== (query?.order ?? "time");
    if (parent === null) this.rootQuery = query;
    const sameFilters = current?.search === query?.search && (current?.activity ?? "all") === (query?.activity ?? "all")
      && JSON.stringify(current?.providers) === JSON.stringify(query?.providers);
    const sameRanking = (current?.order ?? "time") === (query?.order ?? "time");
    if (windows && (!sameFilters || !sameRanking)) {
      const complete = windows.every((watch) => watch.page && !watch.opening) && windows[windows.length - 1]?.page?.next_cursor === null;
      if (sameFilters && complete) {
        // Both rankings select exactly the same complete set. Retain its lease.
        if (orderChanged) return this.combined(parent);
      } else {
        // This query needs a new membership snapshot. Retain unchanged summaries
        // through ranking/filter switches without carrying any old membership.
        reusable = new Map();
        for (const window of windows) for (const [key, entry] of window.entries) reusable.set(key, entry);
        this.drop(parent);
        windows = undefined;
      }
    } else if (orderChanged && windows?.every((watch) => watch.page && !watch.opening)) {
      // Switching back to the ranking that already owns this membership.
      return this.combined(parent);
    }
    if (!windows) {
      windows = [];
      this.windows.set(parent, windows);
    }
    const previousLimit = this.limit(parent);
    for (let offset = 0; offset < limit; offset += SESSIONS_PER_WINDOW) {
      const count = Math.min(SESSIONS_PER_WINDOW, limit - offset);
      const index = offset / SESSIONS_PER_WINDOW;
      let watch = windows[index];
      if (!watch) {
        const id = createUuid();
        const subscription_id = this.prefix ? `${this.prefix}:${id}` : id;
        watch = {
          request: { subscription_id, ...(parent === null ? { query } : { parent_session_key: parent }),
            ...(offset ? { offset } : {}), limit: count },
          entries: reusable ?? new Map(), queued: [], buffering: false, overflow: false, retry: false, waitingForLease: false,
        };
        windows.push(watch);
        this.byId.set(subscription_id, watch);
      } else if (limit > previousLimit && watch.request.limit === count && watch.page && !watch.opening) {
        continue;
      }
      while (watch.opening && count !== watch.request.limit) await watch.opening;
      if (this.windows.get(parent) !== windows) throw new Error("Session list was closed");
      watch.request.limit = count;
      await this.read(watch, false, true);
    }
    if (this.windows.get(parent) !== windows) throw new Error("Session list was closed");
    const page = this.combined(parent);
    return this.needsNewMembership(parent, page) ? this.open(null, this.rootQuery, this.limit(null)) : page;
  }

  private needsNewMembership(parent: Parent, page: ListSessionsResponse): boolean {
    return parent === null && page.next_cursor !== null
      && (this.windows.get(null)?.[0]?.request.query?.order ?? "time") !== (this.rootQuery?.order ?? "time");
  }

  private publish(parent: Parent): void {
    const page = this.combined(parent);
    if (!this.needsNewMembership(parent, page)) { this.onPage(parent, page); return; }
    // A complete set can become a partial page after a new session arrives.
    // Re-select membership using the currently visible ranking before paging.
    const query = this.rootQuery;
    void this.open(null, query, this.limit(null)).then((selected) => {
      if (this.windows.has(null) && this.rootQuery === query) this.onPage(null, selected);
    }, (error) => { if (this.windows.has(null) && this.rootQuery === query) this.onError(null, error); });
  }

  // A full host can still serve ordinary pages while this window waits for a
  // lease. Only explicit browsing reads another page; admission retries reuse
  // what is on screen until the host accepts an authoritative live snapshot.
  private async staticPage(watch: Watch): Promise<ListSessionsResponse> {
    const { parent_session_key, query, offset = 0, limit } = watch.request;
    const sessions = (watch.page?.sessions ?? []).slice();
    let page = watch.page;
    while (!page || (sessions.length < limit && page.next_cursor)) {
      if (this.byId.get(watch.request.subscription_id) !== watch) throw new Error("Session list was closed");
      const window = { offset: offset + sessions.length, limit: Math.min(SESSION_PAGE_SIZE, limit - sessions.length) };
      const batch = parent_session_key === undefined
        ? await listSessions({ ...window, query: query! }, this.send)
        : { ...await listSessionChildren({ ...window, parent_session_key }, this.send), pending_providers: [], source_errors: [] };
      sessions.push(...batch.sessions);
      page = { ...batch, sessions };
      if (!batch.sessions.length) break;
    }
    if (this.byId.get(watch.request.subscription_id) !== watch) throw new Error("Session list was closed");
    watch.waitingForLease = true;
    page = { ...page, sessions: [...new Map(page.sessions.map((session) => [session.session_key, session])).values()] };
    watch.page = page;
    watch.cursor = undefined;
    return page;
  }

  private read(watch: Watch, cold = false, allowStatic = false): Promise<ListSessionsResponse> {
    if (watch.opening) return watch.opening;
    const current = () => this.byId.get(watch.request.subscription_id) === watch;
    const fetch = async (reset: boolean) => {
      watch.buffering = true;
      watch.queued = [];
      watch.overflow = false;
      const since = reset ? undefined : watch.cursor;
      const update = await openSessionList({ ...watch.request, ...(since === undefined ? {} : { since }) }, this.send);
      if (!current()) {
        this.retire(watch);
        throw new Error("Session list was closed");
      }
      if (!update || (update.base_cursor !== null && (!since || update.base_cursor !== since))) {
        throw new InvalidListUpdate("Unexpected session list base");
      }
      apply(watch, update);
      watch.waitingForLease = false;
      if (watch.overflow) throw new ListCursorGap("Too many pending session list updates");
      for (const pushed of watch.queued) applyPush(watch, pushed);
      watch.buffering = false;
      watch.queued = [];
      return watch.page!;
    };
    const run = async () => {
      let reset = cold;
      let replayed = false;
      while (true) {
        try {
          return await fetch(reset);
        } catch (error) {
          if (!current()) throw error;
          if (error instanceof ListCursorGap && watch.cursor && !replayed) {
            // The stream can race a command response. Ask for changes after
            // the last applied cutoff before falling back to a full list.
            replayed = true;
            reset = false;
          } else if (error instanceof InvalidListUpdate && !reset) {
            reset = true;
          } else {
            if (/live session list capacity exceeded/i.test(String(error))) {
              if (allowStatic) return this.staticPage(watch);
              if (watch.waitingForLease && watch.page) return watch.page;
            }
            throw error;
          }
        }
      }
    };
    watch.opening = run().then((page) => {
      watch.retry = false;
      return page;
    }, (error: unknown) => {
      watch.retry = true;
      throw error;
    }).finally(() => {
      watch.opening = undefined;
      watch.buffering = false;
      watch.queued = [];
    });
    return watch.opening;
  }

  push(update: SessionListUpdate) {
    const watch = this.byId.get(update.subscription_id);
    if (!watch || (watch.cursor && update.synced_through === watch.cursor)) return;
    // Capacity can be reached while growing a smaller live window. Its old
    // pushes belong to the shorter order until this client gets a new baseline.
    if (watch.waitingForLease && !watch.buffering) return;
    if (watch.buffering) {
      if (watch.queued.length < MAX_PENDING) watch.queued.push(update);
      else watch.overflow = true;
      return;
    }
    const parent = watch.request.parent_session_key ?? null;
    const previous = watch.page;
    try {
      applyPush(watch, update);
    } catch (error) {
      void this.recover(watch, !(error instanceof ListCursorGap));
      return;
    }
    if (watch.page !== previous) this.publish(parent);
  }

  private recover(watch: Watch, cold = false): Promise<void> {
    if (watch.queuedRecovery) return watch.queuedRecovery;
    const current = () => this.byId.get(watch.request.subscription_id) === watch;
    const parent = watch.request.parent_session_key ?? null;
    const restore = async () => {
      // An in-flight response may predate a disconnect. Validate again once it
      // settles even when the original snapshot happened to arrive successfully.
      if (watch.opening) await watch.opening.catch(() => {});
      // Frames during read() are buffered. Once that read publishes, a new gap
      // must be able to queue another catch-up, even within onPage itself.
      watch.queuedRecovery = undefined;
      if (!current()) return;
      try {
        await this.read(watch, cold);
        if (current()) this.publish(parent);
      } catch (error) {
        if (current()) this.onError(parent, error);
      }
    };
    // A burst of gapped views shares the same bounded machine command channel.
    watch.queuedRecovery = this.recoveryQueue.then(restore);
    this.recoveryQueue = watch.queuedRecovery.catch(() => {});
    return watch.queuedRecovery;
  }

  reconnect(): void {
    this.reconnectPending = true;
    if (this.recovering) return;
    const restore = async () => {
      while (this.reconnectPending) {
        this.reconnectPending = false;
        for (const watch of [...this.byId.values()]) await this.recover(watch);
      }
    };
    this.recovering = restore().finally(() => { this.recovering = undefined; });
  }

  async renew(): Promise<void> {
    if (this.recovering) return;
    const ready = [...this.byId.values()].filter((watch) => !watch.opening);
    const waiting = ready.filter((watch) => watch.waitingForLease);
    if (waiting.length) await this.recover(waiting[this.nextAdmission++ % waiting.length]);
    const active = ready.filter((watch) => !watch.waitingForLease);
    for (let start = 0; start < active.length; start += 64) {
      const batch = active.slice(start, start + 64);
      const ids = batch.map((watch) => watch.request.subscription_id);
      let missing: Set<string>;
      try {
        const result = await this.send<{ missing_ids: string[] }>("renew_session_lists", { ids });
        missing = new Set(result.missing_ids);
      } catch {
        // A temporary network failure does not establish that the lease expired.
        return;
      }
      for (const watch of batch) {
        if (watch.retry || missing.has(watch.request.subscription_id)) await this.recover(watch);
      }
    }
  }
}

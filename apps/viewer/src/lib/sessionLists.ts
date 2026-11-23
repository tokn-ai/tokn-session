import { createUuid } from "./id";
import { listSessionChildren, listSessions, openSessionList } from "./tauri";
import { SESSION_PAGE_SIZE } from "./state";
import { compareActivity, compareProjects, compareSessionChildren } from "./sidebarOrder";
import type { CommandInvoker } from "./transport";
import type { ListSessionsResponse, OpenSessionListRequest, SessionListQuery, SessionListRow, SessionListUpdate, SessionSummary } from "./types";

class InvalidListUpdate extends Error {}

interface Watch {
  request: OpenSessionListRequest;
  rows: Map<string, SessionListRow>;
  page?: ListSessionsResponse;
  generation?: string;
  revision?: string;
  opening?: Promise<ListSessionsResponse>;
  queued: SessionListUpdate[];
  buffering: boolean;
  overflow: boolean;
  recovery?: Promise<void>;
  retry: boolean;
  waitingForLease: boolean;
}

function apply(watch: Watch, update: SessionListUpdate): void {
  if (update.subscription_id !== watch.request.subscription_id || !update.generation || !/^\d+$/.test(update.revision)) {
    throw new InvalidListUpdate("Invalid session list revision");
  }
  const snapshot = update.base_revision === null;
  if (snapshot && (update.removed_keys.length || (watch.revision && watch.generation === update.generation
    && BigInt(update.revision) < BigInt(watch.revision)))) {
    throw new InvalidListUpdate("Invalid session list snapshot");
  }
  if (!snapshot && (!watch.page || watch.generation !== update.generation || watch.revision !== update.base_revision
    || BigInt(update.revision) < BigInt(watch.revision))) {
    throw new InvalidListUpdate("Session list revision gap");
  }

  // Content revisions identify scoped row bodies independently of a window's
  // generation. Resyncing an unchanged row preserves its visible overlays.
  const rows = snapshot ? new Map<string, SessionListRow>() : new Map(watch.rows);
  const touched = new Set(update.removed_keys);
  for (const key of touched) rows.delete(key);
  let changed = (snapshot ? update.rows.length : rows.size) !== watch.rows.size;
  for (const entry of update.rows) {
    const key = entry.session?.session_key;
    if (!key || touched.has(key) || !/^[a-f0-9]{64}$/.test(entry.revision)) {
      throw new InvalidListUpdate("Invalid session list row");
    }
    touched.add(key);
    const old = watch.rows.get(key);
    if (old?.revision === entry.revision) rows.set(key, old);
    else {
      rows.set(key, entry);
      changed = true;
    }
  }
  if (!snapshot && update.revision === watch.revision && changed) throw new InvalidListUpdate("Invalid session list revision");

  const sessions = !changed && watch.page && !watch.waitingForLease
    ? watch.page.sessions : [...rows.values()].map((row) => row.session);
  watch.rows = rows;
  const previous = watch.page;
  if (!previous || sessions !== previous.sessions || update.next_cursor !== previous.next_cursor
    || JSON.stringify(update.pending_providers) !== JSON.stringify(previous.pending_providers)
    || JSON.stringify(update.source_errors) !== JSON.stringify(previous.source_errors)) {
    watch.page = { sessions, next_cursor: update.next_cursor, pending_providers: update.pending_providers, source_errors: update.source_errors };
  }
  watch.generation = update.generation;
  watch.revision = update.revision;
}

const MAX_PENDING = 32;
// Start another independent live window on a page boundary before the host
// limit. Previously loaded rows keep their subscriptions as browsing continues.
const WINDOW_ROWS = Math.floor(4096 / SESSION_PAGE_SIZE) * SESSION_PAGE_SIZE;
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
    const rows = new Map<string, SessionSummary>();
    let cursor = first.next_cursor;
    for (const watch of windows) {
      if (!watch.page) break;
      for (const row of watch.page.sessions) if (!rows.has(row.session_key)) rows.set(row.session_key, row);
      cursor = watch.page.next_cursor;
    }
    return { ...first, sessions: [...rows.values()].sort(compare), next_cursor: cursor };
  }

  async open(parent: Parent, query: SessionListQuery | undefined, limit: number): Promise<ListSessionsResponse> {
    let windows = this.windows.get(parent);
    let reusable: Map<string, SessionListRow> | undefined;
    const current = windows?.[0]?.request.query;
    const orderChanged = parent === null && (this.rootQuery?.order ?? "time") !== (query?.order ?? "time");
    if (parent === null) this.rootQuery = query;
    const sameFilters = current?.search === query?.search && JSON.stringify(current?.providers) === JSON.stringify(query?.providers);
    const sameRanking = (current?.order ?? "time") === (query?.order ?? "time");
    if (windows && (!sameFilters || !sameRanking)) {
      const complete = windows.every((watch) => watch.page && !watch.opening) && windows[windows.length - 1]?.page?.next_cursor === null;
      if (sameFilters && complete) {
        // Both rankings select exactly the same complete set. Retain its lease.
        if (orderChanged) return this.combined(parent);
      } else {
        // This query needs a new membership snapshot. Retain equal row bodies
        // through ranking/filter switches without carrying any old membership.
        reusable = new Map();
        for (const window of windows) for (const [key, row] of window.rows) reusable.set(key, row);
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
    for (let offset = 0; offset < limit; offset += WINDOW_ROWS) {
      const count = Math.min(WINDOW_ROWS, limit - offset);
      const index = offset / WINDOW_ROWS;
      let watch = windows[index];
      if (!watch) {
        const id = createUuid();
        const subscription_id = this.prefix ? `${this.prefix}:${id}` : id;
        watch = {
          request: { subscription_id, ...(parent === null ? { query } : { parent_session_key: parent }),
            ...(offset ? { offset } : {}), limit: count },
          rows: reusable ?? new Map(), queued: [], buffering: false, overflow: false, retry: false, waitingForLease: false,
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
    page = { ...page, sessions: [...new Map(page.sessions.map((row) => [row.session_key, row])).values()] };
    watch.page = page;
    watch.generation = undefined;
    watch.revision = undefined;
    return page;
  }

  private read(watch: Watch, cold = false, allowStatic = false): Promise<ListSessionsResponse> {
    if (watch.opening) return watch.opening;
    const current = () => this.byId.get(watch.request.subscription_id) === watch;
    const fetch = async (reset: boolean) => {
      watch.buffering = true;
      watch.queued = [];
      watch.overflow = false;
      const resume = !reset && watch.revision ? { generation: watch.generation, revision: watch.revision } : {};
      const update = await openSessionList({ ...watch.request, ...resume }, this.send);
      if (!current()) {
        this.retire(watch);
        throw new Error("Session list was closed");
      }
      apply(watch, update);
      watch.waitingForLease = false;
      if (watch.overflow) throw new InvalidListUpdate("Too many pending session list updates");
      for (const pushed of watch.queued) if (!this.stale(watch, pushed)) apply(watch, pushed);
      watch.buffering = false;
      watch.queued = [];
      return watch.page!;
    };
    // Only malformed/gapped pushes warrant another baseline. Connection and
    // permission failures surface to the caller without a repeated command.
    const run = async () => {
      try {
        return await fetch(cold);
      } catch (error) {
        if (!current()) throw error;
        if (/live session list capacity exceeded/i.test(String(error))) {
          if (allowStatic) return this.staticPage(watch);
          if (watch.waitingForLease && watch.page) return watch.page;
        }
        if (cold || !(error instanceof InvalidListUpdate)) throw error;
        return fetch(true);
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

  private stale(watch: Watch, update: SessionListUpdate) {
    return update.generation === watch.generation && watch.revision !== undefined && /^\d+$/.test(update.revision)
      && BigInt(update.revision) <= BigInt(watch.revision);
  }

  push(update: SessionListUpdate) {
    const watch = this.byId.get(update.subscription_id);
    if (!watch || this.stale(watch, update)) return;
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
      if (update.base_revision === null) throw new InvalidListUpdate("Unexpected session list snapshot");
      apply(watch, update);
    } catch {
      void this.recover(watch, true);
      return;
    }
    if (watch.page !== previous) this.publish(parent);
  }

  private recover(watch: Watch, cold = false): Promise<void> {
    if (watch.recovery) return watch.recovery;
    const current = () => this.byId.get(watch.request.subscription_id) === watch;
    const parent = watch.request.parent_session_key ?? null;
    const restore = async () => {
      // An in-flight response may predate a disconnect. Validate again once it
      // settles even when the original snapshot happened to arrive successfully.
      if (watch.opening) await watch.opening.catch(() => {});
      if (!current()) return;
      try {
        await this.read(watch, cold);
        if (current()) this.publish(parent);
      } catch (error) {
        if (current()) this.onError(parent, error);
      }
    };
    // A burst of gapped views shares the same bounded machine command channel.
    watch.recovery = this.recoveryQueue.then(restore).finally(() => { watch.recovery = undefined; });
    this.recoveryQueue = watch.recovery.catch(() => {});
    return watch.recovery;
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

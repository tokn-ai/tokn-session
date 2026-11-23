import { describe, expect, it, vi } from "vitest";
import { revisionedSession } from "../../tests/sessionListFixtures";
import { SessionLists } from "./sessionLists";
import type { OpenSessionListRequest, SessionListUpdate, SessionSummary } from "./types";
import type { CommandInvoker } from "./transport";

const row = (session_key: string, title = session_key, updated_at_ms = -Number(session_key) || 0): SessionSummary => ({
  session_key, session_id: session_key, parent_session_id: null, is_subagent: false, provider: "codex",
  title, preview: null, cwd: null, project: null, updated_at_ms, timestamp: null, agent_path: null,
  agent_nickname: null, agent_role: null, child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
});
function snapshot(request: OpenSessionListRequest, sessions: SessionSummary[], revision = "1"): SessionListUpdate {
  const rows = sessions.map(revisionedSession);
  return { subscription_id: request.subscription_id, generation: "g", base_revision: null, revision, rows,
    removed_keys: [], next_cursor: null, pending_providers: [], source_errors: [] };
}

function setup(open: (request: OpenSessionListRequest) => Promise<SessionListUpdate>, renewed = true) {
  const invoke = vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "open_session_list") return open(payload!.request as OpenSessionListRequest);
    if (command === "renew_session_lists") return { missing_ids: renewed ? [] : payload!.ids };
    throw new Error(command);
  });
  const release = vi.fn(async () => {});
  const lists = new SessionLists(invoke as CommandInvoker, release, "viewer");
  lists.onPage = vi.fn();
  return { invoke, release, lists };
}

describe("live session list windows", () => {
  it("orders changed rows from metadata and retains unchanged bodies across deltas and fresh baselines", async () => {
    const oldA = row("a", "A", 1), oldB = row("b", "B", 3), oldC = row("c", "C", 2);
    let request!: OpenSessionListRequest;
    let source = [oldA, oldB, oldC];
    const { lists, invoke } = setup(async (r) => {
      request = r;
      return { ...snapshot(r, source.map((s) => ({ ...s }))), generation: r.revision ? "restarted" : "g" };
    });
    const initial = await lists.open(null, {}, 60);
    expect(initial.sessions.map((s) => s.session_key)).toEqual(["b", "c", "a"]);
    const retainedB = initial.sessions[0];
    const renamedA = row("a", "renamed", 4), addedD = row("d", "D", 2);
    const delta = { ...snapshot(request, [addedD, renamedA, { ...oldB }], "2"), base_revision: "1", removed_keys: ["c"] };
    lists.push(delta);
    const last = vi.mocked(lists.onPage).mock.lastCall![1].sessions;
    expect(last).toEqual([renamedA, retainedB, addedD]);
    expect(last[1]).toBe(retainedB);
    lists.push(delta);
    expect(lists.onPage).toHaveBeenCalledOnce();
    source = [addedD, oldB, renamedA];
    const resynced = await lists.open(null, {}, 60);
    expect(resynced.sessions.every((s, i) => s === last[i])).toBe(true);
    await lists.renew();
    expect(invoke.mock.calls.map(([name]) => name)).toEqual(["open_session_list", "open_session_list", "renew_session_lists"]);
  });

  it("ignores an unchanged row revision and replaces only overlapping rows that changed across a ranked page switch", async () => {
    const a = row("a"), b = row("b"), c = row("c");
    let request!: OpenSessionListRequest;
    const { lists } = setup(async (r) => {
      request = r;
      const sessions = (r.query?.order === "time" ? [a, b] : [c, b]).map((s) => ({ ...s }));
      return { ...snapshot(r, sessions), next_cursor: "more" };
    });
    const first = await lists.open(null, { order: "time" }, 2);
    const unchanged = { ...snapshot(request, [{ ...b }], "2"), base_revision: "1", next_cursor: "more" };
    lists.push(unchanged);
    expect(lists.onPage).not.toHaveBeenCalled();
    const next = await lists.open(null, { order: "project" }, 2);
    expect(next.sessions.find((s) => s.session_key === "b")).toBe(first.sessions[1]);
    expect(next.sessions.some((s) => s.session_key === "a")).toBe(false);
  });

  it("sorts a complete set locally, then selects the right membership when it grows past the boundary", async () => {
    const requests: OpenSessionListRequest[] = [];
    const a = { ...row("a", "A", 5), project_key: "first", project_order_ms: 1 };
    const b = { ...row("b", "B", 1), project_key: "second", project_order_ms: 9 };
    const c = { ...row("c", "C", 2), project_key: "third", project_order_ms: 8 };
    const { lists } = setup(async (r) => {
      requests.push(r);
      return r.query?.order === "project" ? { ...snapshot(r, [c, b]), next_cursor: "more" } : snapshot(r, [b, a]);
    });
    expect((await lists.open(null, { order: "time" }, 2)).sessions).toEqual([a, b]);
    expect((await lists.open(null, { order: "project" }, 2)).sessions).toEqual([b, a]);
    expect(requests).toHaveLength(1);
    lists.push({ ...snapshot(requests[0], [c], "2"), removed_keys: ["b"], base_revision: "1", next_cursor: "more" });
    await vi.waitFor(() => expect(lists.onPage).toHaveBeenCalledOnce());
    expect(requests.map((r) => r.query?.order)).toEqual(["time", "project"]);
    expect(vi.mocked(lists.onPage).mock.lastCall![1].sessions).toEqual([b, c]);
  });

  it("sorts siblings by recency and stable local ties regardless of the root grouping", async () => {
    const a = { ...row("key:z", "A", 2), session_id: "alpha" };
    const b = { ...row("key:a", "B", 2), session_id: "beta" };
    const early = { ...row("key:b", "early", 1), project_order_ms: 100 };
    const { lists } = setup(async (r) => snapshot(r, [early, b, a]));
    expect((await lists.open("parent", undefined, 60)).sessions).toEqual([a, b, early]);
  });

  it("queues a live rename before its snapshot response and discards pushes to released child views", async () => {
    let resolve!: () => void;
    let request!: OpenSessionListRequest;
    const { lists, release } = setup((r) => {
      request = r;
      return new Promise((done) => { resolve = () => done(snapshot(r, [row("child")])); });
    });
    const opening = lists.open("parent", undefined, 60);
    const delta = { ...snapshot(request, [row("child", "new title")], "2"), base_revision: "1" };
    lists.push(delta);
    resolve();
    expect((await opening).sessions[0].title).toBe("new title");
    lists.drop("parent");
    lists.push({ ...delta, revision: "3", base_revision: "2" });
    expect(lists.onPage).not.toHaveBeenCalled();
    expect(release).toHaveBeenCalledWith("release_session_list", { request: { subscription_id: request.subscription_id } });
  });

  it("recovers only the mismatched window and accepts an unchanged renewed baseline", async () => {
    const requests: OpenSessionListRequest[] = [];
    const { lists } = setup(async (r) => {
      requests.push(r);
      if (r.revision) return { ...snapshot(r, [], r.revision), base_revision: r.revision };
      return snapshot(r, [row(r.parent_session_key ? "child" : "root")], String(requests.length));
    });
    await lists.open(null, {}, 60);
    await lists.open("root", undefined, 60);
    lists.push({ ...snapshot(requests[1], [], "10"), base_revision: "9" });
    await vi.waitFor(() => expect(lists.onPage).toHaveBeenCalledOnce());
    expect(requests.map((r) => r.parent_session_key)).toEqual([undefined, "root", "root"]);
    expect(requests[2].revision).toBeUndefined();
    lists.reconnect();
    await vi.waitFor(() => expect(lists.onPage).toHaveBeenCalledTimes(3));
    expect(requests.slice(3).map((r) => r.revision)).toEqual(["1", "3"]);
  });

  it("batches lease renewal, restores only missing interests, and keeps rows through a network failure", async () => {
    const requests: OpenSessionListRequest[] = [];
    const renewals: string[][] = [];
    let offline = true;
    const invoke: CommandInvoker = async <T,>(name: string, payload?: Record<string, unknown>) => {
      if (name === "open_session_list") {
        const request = payload!.request as OpenSessionListRequest;
        requests.push(request);
        return snapshot(request, [row(request.parent_session_key ?? "root")]) as T;
      }
      renewals.push(payload!.ids as string[]);
      if (offline) throw new Error("Offline");
      return { missing_ids: [requests[1].subscription_id] } as T;
    };
    const lists = new SessionLists(invoke, async () => {});
    await lists.open(null, {}, 60);
    await lists.open("root", undefined, 60);
    await lists.renew();
    expect(requests).toHaveLength(2);
    offline = false;
    await lists.renew();
    expect(renewals).toEqual(Array(2).fill(requests.slice(0, 2).map((r) => r.subscription_id)));
    expect(requests.map((r) => r.parent_session_key)).toEqual([undefined, "root", "root"]);
  });

  it("browses past a full host without rereading pages on idle admission retries", async () => {
    let full = true;
    const commands: string[] = [];
    const invoke: CommandInvoker = async <T,>(name: string, payload?: Record<string, unknown>) => {
      commands.push(name);
      const r = payload!.request as OpenSessionListRequest;
      if (name === "open_session_list") {
        if (full) throw new Error("Live session list capacity exceeded");
        return snapshot(r, [row("live")]) as T;
      }
      if (name === "renew_session_lists") return { missing_ids: [] } as T;
      if (name !== "list_sessions") throw new Error(name);
      return { sessions: Array.from({ length: r.limit }, (_, i) => row(String((r.offset ?? 0) + i))),
        next_cursor: "more", source_errors: [], pending_providers: [] } as T;
    };
    const lists = new SessionLists(invoke, async () => {});
    const initial = await lists.open(null, {}, 60);
    expect(initial.sessions).toHaveLength(60);
    await lists.renew();
    expect(commands.filter((c) => c === "list_sessions")).toHaveLength(1);
    const extended = await lists.open(null, {}, 120);
    expect(extended.sessions.map((r) => r.session_key)).toEqual(Array.from({ length: 120 }, (_, i) => String(i)));
    expect(commands.filter((c) => c === "list_sessions")).toHaveLength(2);
    full = false;
    lists.onPage = vi.fn();
    await lists.renew();
    expect(vi.mocked(lists.onPage).mock.lastCall![1].sessions).toEqual([row("live")]);
  });

  it("keeps previous pages live when the host window bound is reached", async () => {
    const requests: OpenSessionListRequest[] = [];
    const { lists } = setup(async (r) => {
      requests.push(r);
      return { ...snapshot(r, Array.from({ length: r.limit }, (_, i) => row(String((r.offset ?? 0) + i)))),
        next_cursor: String((r.offset ?? 0) + r.limit) };
    });
    await lists.open(null, {}, 4080);
    const extended = await lists.open(null, {}, 4140);
    expect(requests.map((r) => [r.offset, r.limit])).toEqual([[undefined, 4080], [4080, 60]]);
    expect(extended.sessions).toHaveLength(4140);
    lists.push({ ...snapshot(requests[0], [row("0", "renamed")], "2"), base_revision: "1", next_cursor: "4080" });
    const visible = vi.mocked(lists.onPage).mock.lastCall![1];
    expect(visible.sessions).toHaveLength(4140);
    expect(visible.next_cursor).toBe("4140");
    expect(visible.sessions[0].title).toBe("renamed");
  });
});

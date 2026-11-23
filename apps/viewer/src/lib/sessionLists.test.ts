import { describe, expect, it, vi } from "vitest";
import { SessionLists } from "./sessionLists";
import type { OpenSessionListRequest, SessionListUpdate, SessionSummary } from "./types";
import type { CommandInvoker } from "./transport";

const row = (session_key: string, title = session_key) => ({ session_key, title }) as SessionSummary;
function snapshot(request: OpenSessionListRequest, rows: SessionSummary[], revision = "1"): SessionListUpdate {
  return { subscription_id: request.subscription_id, generation: "g", base_revision: null, revision, rows,
    removed_keys: [], order_edits: [], next_cursor: null, pending_providers: [], source_errors: [] };
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
  it("applies renames, moves, insertions and removals locally and retains untouched row objects", async () => {
    const oldA = row("a"), oldB = row("b");
    let request!: OpenSessionListRequest;
    const { lists, invoke } = setup(async (r) => { request = r; return snapshot(r, [oldA, oldB, row("c")]); });
    const initial = await lists.open(null, {}, 60);
    const update: SessionListUpdate = { ...snapshot(request, [row("a", "renamed")], "2"), base_revision: "1" };
    lists.push(update);
    expect(lists.onPage).toHaveBeenLastCalledWith(null, { ...initial, sessions: [row("a", "renamed"), oldB, row("c")] });
    const newA = update.rows[0];
    lists.push({ ...update, base_revision: "2", revision: "3", rows: [row("d")], removed_keys: ["c"], order_edits: [
      { start: 2, delete_count: 1, insert: [] }, { start: 1, delete_count: 1, insert: [] },
      { start: 0, delete_count: 0, insert: ["b", "d"] },
    ] });
    const last = vi.mocked(lists.onPage).mock.lastCall![1].sessions;
    expect(last.map((r) => r.session_key)).toEqual(["b", "d", "a"]);
    expect(last[0]).toBe(oldB);
    expect(last[2]).toBe(newA);
    lists.push(update); // Replayed event after revision 3.
    await lists.renew();
    expect(lists.onPage).toHaveBeenCalledTimes(2);
    expect(invoke.mock.calls.map(([name]) => name)).toEqual(["open_session_list", "renew_session_lists"]);
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

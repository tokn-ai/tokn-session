import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { BackgroundSessionUpdate, BackgroundUpdatesRequest, BackgroundUpdatesSubscription } from "./types";
import { useBackgroundUpdates } from "./useBackgroundUpdates";

type Handler = (event: { payload: unknown }) => void;

function transport() {
  const handlers = new Map<string, Set<Handler>>();
  const send = vi.fn(async (command: string, payload?: Record<string, unknown>): Promise<BackgroundUpdatesSubscription | undefined> => {
    if (command === "subscribe_background_updates") {
      const request = (payload as { request: BackgroundUpdatesRequest }).request;
      return { subscription_id: request.subscription_id, generation: "server", revision: "0", scope: request.scope,
        level: request.level ?? "messages", subscribed: true };
    }
    if (command === "renew_background_subscriptions") return;
    throw new Error(`Unexpected command: ${command}`);
  });
  const subscribe = vi.fn(async (event: string, handler: Handler) => {
    const listeners = handlers.get(event) ?? new Set<Handler>();
    listeners.add(handler); handlers.set(event, listeners);
    return () => { listeners.delete(handler); };
  });
  const release = vi.fn(async (_command: string, _payload: Record<string, unknown>) => {});
  return {
    send, subscribe, release,
    invoke: send as CommandInvoker, listen: subscribe as EventSubscriber,
    emit: (event: string, payload: unknown) => {
      for (const handler of handlers.get(event) ?? []) handler({ payload });
    },
    requests: () => send.mock.calls.filter(([command]) => command === "subscribe_background_updates")
      .map(([, payload]) => (payload as { request: BackgroundUpdatesRequest }).request),
  };
}

function update(request: BackgroundUpdatesRequest, session_key: string, revision = "1", preview = "Main assistant reply."): BackgroundSessionUpdate {
  return { subscription_id: request.subscription_id, generation: "server", revision, session_key,
    level: request.level ?? "messages",
    has_unread: false, unread_final_count: 0, is_running: false, has_running_descendant: false,
    question_attention: { required_count: 0, available_count: 0 },
    latest_assistant_message: { message_id: `${session_key}:message`, delivery: "final", phase: "finished", preview },
    message_revision: `${session_key}:${revision}` };
}

beforeEach(() => { vi.useFakeTimers(); localStorage.clear(); });
afterEach(() => { cleanup(); localStorage.clear(); vi.useRealTimers(); });

describe("background update interests", () => {
  it("does not restore activity from a subscription acknowledgement delayed across lease expiry", async () => {
    const connection = transport();
    const original = connection.send.getMockImplementation()!;
    let accept!: () => void;
    const pending = new Promise<void>((resolve) => { accept = resolve; });
    connection.send.mockImplementationOnce(async (command, payload) => {
      await pending;
      return original(command, payload);
    });
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => connection.emit("background-session-updated", { ...update(first, "root"), is_running: true }));
    expect(result.current.cache.activity("root")).toBeUndefined();
    vi.setSystemTime(Date.now() + 90_001);
    await act(async () => { accept(); });
    expect(connection.requests()).toHaveLength(2);
    expect(result.current.cache.preview("root")).toBe("Main assistant reply.");
    expect(result.current.cache.activity("root")).toBeUndefined();
  });

  it.each([false, true])("releases activity=%s immediately after renewal failure while preserving previews", async (is_running) => {
    const connection = transport();
    const original = connection.send.getMockImplementation()!;
    connection.send.mockImplementation(async (command, payload) => {
      if (command === "renew_background_subscriptions") throw new Error("Connection interrupted");
      return original(command, payload);
    });
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const request = connection.requests()[0];
    act(() => connection.emit("background-session-updated", { ...update(request, "root"), is_running }));
    expect(result.current.cache.activity("root")?.is_running).toBe(is_running);
    expect(result.current.authority_revision).toBe(0);
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(result.current.cache.activity("root")).toBeUndefined();
    expect(result.current.authority_revision).toBe(1);
    expect(result.current.cache.preview("root")).toBe("Main assistant reply.");
    act(() => connection.emit("background-session-updated", { ...update(request, "root", "2"), is_running: !is_running }));
    expect(result.current.cache.activity("root")).toBeUndefined();
    await act(async () => { window.dispatchEvent(new Event("focus")); });
    expect(connection.requests()).toHaveLength(2);
    expect(result.current.cache.activity("root")).toBeUndefined();
  });

  it("recovers compact socket loss and local expiry without resetting on rich reconnects", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => connection.emit("background-session-updated", { ...update(first, "root"), is_running: true }));
    await act(async () => { connection.emit("transport-reconnected", { subscription_ids: ["rich-only"] }); });
    expect(connection.requests()).toHaveLength(1);
    expect(result.current.cache.activity("root")?.is_running).toBe(true);
    await act(async () => { connection.emit("background-subscriptions-lost", { subscription_ids: [first.subscription_id] }); });
    expect(connection.requests()).toHaveLength(2);
    expect(result.current.cache.activity("root")).toBeUndefined();
    expect(result.current.cache.preview("root")).toBe("Main assistant reply.");
    const second = connection.requests()[1];
    act(() => connection.emit("background-session-updated", update(second, "root")));
    vi.setSystemTime(Date.now() + 90_001);
    expect(result.current.cache.activity("root")).toBeUndefined();
    await act(async () => { window.dispatchEvent(new Event("focus")); });
    expect(connection.requests()).toHaveLength(3);
    expect(connection.send.mock.calls.every(([command]) => command === "subscribe_background_updates")).toBe(true);
  });

  it("defaults to all sessions, without deriving interests or reads from visible rows", async () => {
    const connection = transport();
    const { result, rerender, unmount } = renderHook(({ keys }) => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, keys), {
      initialProps: { keys: ["root", "child"] },
    });
    await act(async () => {});
    const request = connection.requests()[0];
    expect(request).toMatchObject({ scope: "all", session_keys: [] });
    expect(request).not.toHaveProperty("level");
    expect(result.current.scope).toBe("all");
    expect(result.current.level).toBe("messages");
    expect(result.current.status).toBeNull();
    await act(async () => { rerender({ keys: ["different"] }); });
    expect(connection.requests()).toHaveLength(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(connection.send.mock.calls.map(([command]) => command)).toEqual(["subscribe_background_updates", "renew_background_subscriptions"]);
    expect(connection.send).toHaveBeenLastCalledWith("renew_background_subscriptions", { ids: [request.subscription_id] });
    unmount();
    expect(connection.release).toHaveBeenCalledExactlyOnceWith("subscribe_background_updates", { request: { ...request, unsubscribe: true } });
  });

  it("persists independent levels, retires the previous interest, and preserves known previews without reads", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, ["visible"]));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => connection.emit("background-session-updated", update(first, "visible", "1", "Known reply")));
    await act(async () => { result.current.changeLevel("final"); });
    const second = connection.requests()[1];
    expect(second).toMatchObject({ level: "final", scope: "all", session_keys: [] });
    expect(second.subscription_id).not.toBe(first.subscription_id);
    expect(localStorage.getItem("tokn.viewer.background-update-level")).toBe("final");
    expect(result.current.cache.preview("visible")).toBe("Known reply");
    expect(result.current.cache.activity("visible")).toBeUndefined();
    expect(connection.release).toHaveBeenCalledWith("subscribe_background_updates", { request: { ...first, unsubscribe: true } });
    act(() => {
      connection.emit("background-session-updated", update(first, "visible", "2", "Retired commentary"));
      connection.emit("background-session-updated", { ...update(second, "visible"), latest_assistant_message: null, message_revision: null, is_running: true });
    });
    expect(result.current.cache.preview("visible")).toBe("Known reply");
    expect(result.current.cache.activity("visible")?.is_running).toBe(true);
    await act(async () => { result.current.changeScope("sidebar"); });
    expect(connection.requests()[2]).toMatchObject({ level: "final", scope: "sidebar", session_keys: ["visible"] });
    await act(async () => { result.current.changeLevel("messages"); });
    expect(connection.requests()[3]).not.toHaveProperty("level");
    expect(connection.requests()[3]).toMatchObject({ scope: "sidebar", session_keys: ["visible"] });
    expect(connection.send.mock.calls.every(([command]) => command === "subscribe_background_updates")).toBe(true);
  });

  it("restores final filtering and rejects unacknowledged legacy delivery without a fallback", async () => {
    localStorage.setItem("tokn.viewer.background-update-level", "final");
    const connection = transport();
    connection.send.mockImplementation(async (_command, payload) => {
      const request = (payload as { request: BackgroundUpdatesRequest }).request;
      connection.emit("background-session-updated", { ...update(request, "visible", "1", "Unfiltered reply"), level: undefined });
      return { subscription_id: request.subscription_id, generation: "server", revision: "0", scope: request.scope, subscribed: true };
    });
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    expect(result.current.level).toBe("final");
    expect(result.current.cache.preview("visible")).toBeUndefined();
    expect(result.current.status).toContain("Final-only background updates are unavailable");
    await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
    expect(connection.requests()).toHaveLength(1);
    expect(connection.requests()[0].level).toBe("final");
    expect(connection.release).toHaveBeenCalled();
  });

  it("reports old-host validation errors for final filtering without retrying as messages", async () => {
    localStorage.setItem("tokn.viewer.background-update-level", "final");
    const connection = transport();
    connection.send.mockRejectedValue(new Error("Invalid background subscription"));
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    expect(result.current.status).toContain("Select All assistant messages or update the host");
    expect(connection.requests()).toHaveLength(1);
  });

  it("restores sidebar scope and treats sorted unique keys as one interest set", async () => {
    localStorage.setItem("tokn.viewer.background-updates", "sidebar");
    const connection = transport();
    const { result, rerender } = renderHook(({ keys }) => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, keys), {
      initialProps: { keys: ["z", "a", "a"] },
    });
    await act(async () => {});
    expect(result.current.scope).toBe("sidebar");
    const first = connection.requests()[0];
    expect(first).toMatchObject({ scope: "sidebar", session_keys: ["a", "z"] });
    act(() => connection.emit("background-session-updated", { ...update(first, "z", "1", "Known sidebar reply"), is_running: true }));
    const token = result.current.cache.capture("z");
    await act(async () => { rerender({ keys: ["a", "z"] }); });
    expect(connection.requests()).toHaveLength(1);
    expect(result.current.cache.activity("z")?.is_running).toBe(true);
    await act(async () => { rerender({ keys: ["new", "z"] }); });
    const second = connection.requests()[1];
    expect(second).toMatchObject({ scope: "sidebar", session_keys: ["new", "z"] });
    expect(second.subscription_id).not.toBe(first.subscription_id);
    expect(connection.release).toHaveBeenCalledWith("subscribe_background_updates", { request: { ...first, unsubscribe: true } });
    expect(result.current.cache.preview("z")).toBe("Known sidebar reply");
    expect(result.current.cache.activity("z")).toBeUndefined();
    act(() => {
      connection.emit("background-session-updated", update(first, "z", "2", "Retired stream reply"));
      result.current.rememberPage("z", "Delayed old page", token);
    });
    expect(result.current.cache.preview("z")).toBe("Known sidebar reply");
    expect(connection.send.mock.calls.every(([command]) => command === "subscribe_background_updates")).toBe(true);
  });

  it("persists scope changes, keeps known text, and retires old activity authority", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, ["visible"]));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => connection.emit("background-session-updated", update(first, "unopened")));
    expect(result.current.cache.preview("unopened")).toBe("Main assistant reply.");
    await act(async () => { result.current.changeScope("sidebar"); });
    const second = connection.requests()[1];
    expect(second).toMatchObject({ scope: "sidebar", session_keys: ["visible"] });
    expect(second.subscription_id).not.toBe(first.subscription_id);
    expect(localStorage.getItem("tokn.viewer.background-updates")).toBe("sidebar");
    expect(result.current.cache.preview("unopened")).toBe("Main assistant reply.");
    expect(result.current.cache.activity("unopened")).toBeUndefined();
    expect(connection.release).toHaveBeenCalledWith("subscribe_background_updates", { request: { ...first, unsubscribe: true } });
    act(() => connection.emit("background-session-updated", update(first, "unopened", "2", "Retired message")));
    expect(result.current.cache.preview("unopened")).toBe("Main assistant reply.");
    expect(result.current.cache.activity("unopened")).toBeUndefined();
    expect(connection.send.mock.calls.map(([command]) => command)).toEqual(["subscribe_background_updates", "subscribe_background_updates"]);
  });

  it("keeps unopened root and child previews independent, without timeline reads", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const request = connection.requests()[0];
    act(() => {
      connection.emit("background-session-updated", update(request, "root", "1", "\nRoot reply.\nHidden second line"));
      connection.emit("background-session-updated", { ...update(request, "child", "2", "Child reply."), is_running: true });
    });
    expect(result.current.cache.preview("root")).toBe("Root reply.");
    expect(result.current.cache.preview("child")).toBe("Child reply.");
    expect(result.current.cache.activity("root")?.is_running).toBe(false);
    expect(result.current.cache.activity("child")?.is_running).toBe(true);
    expect(connection.requests()[0].session_keys).toEqual([]);
    expect(connection.send.mock.calls.map(([command]) => command)).toEqual(["subscribe_background_updates"]);
  });

  it("rejects stale records, resets authority after gaps/reconnects, and retains known previews", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => {
      connection.emit("background-session-updated", update(first, "root", "1", "Root reply."));
      connection.emit("background-session-updated", update(first, "child", "2", "Child reply."));
      connection.emit("background-session-updated", update(first, "root", "1", "Older reply."));
      connection.emit("background-session-updated", { ...update(first, "root", "3", "Other server"), generation: "retired-server" });
    });
    expect(result.current.cache.preview("root")).toBe("Root reply.");
    expect(result.current.cache.preview("child")).toBe("Child reply.");
    const token = result.current.cache.capture("root");
    const childToken = result.current.cache.capture("child");
    act(() => connection.emit("background-session-updated", update(first, "root", "4", "After the gap.")));
    expect(result.current.cache.preview("child")).toBe("Child reply.");
    expect(result.current.cache.activity("child")).toBeUndefined();
    act(() => {
      result.current.rememberPage("root", "Delayed page", token);
      result.current.rememberPage("child", "Delayed child page", childToken);
    });
    expect(result.current.cache.preview("root")).toBe("After the gap.");
    expect(result.current.cache.preview("child")).toBe("Child reply.");
    const reconnectToken = result.current.cache.capture("root");
    await act(async () => { connection.emit("transport-reconnected", {}); });
    const second = connection.requests()[1];
    expect(second.subscription_id).not.toBe(first.subscription_id);
    expect(result.current.cache.preview("root")).toBe("After the gap.");
    expect(result.current.cache.preview("child")).toBe("Child reply.");
    expect(result.current.cache.activity("root")).toBeUndefined();
    expect(result.current.cache.activity("child")).toBeUndefined();
    expect(connection.release).toHaveBeenCalledWith("subscribe_background_updates", { request: { ...first, unsubscribe: true } });
    act(() => {
      connection.emit("background-session-updated", update(first, "root", "5", "Late old socket"));
      connection.emit("background-session-updated", update(second, "child", "1", "Fresh reply."));
      result.current.rememberPage("root", "Late pre-reconnect read", reconnectToken);
    });
    expect(result.current.cache.preview("root")).toBe("After the gap.");
    expect(result.current.cache.activity("root")).toBeUndefined();
    expect(result.current.cache.preview("child")).toBe("Fresh reply.");
    expect(result.current.cache.activity("child")?.is_running).toBe(false);
    expect(connection.send.mock.calls.map(([command]) => command)).toEqual(["subscribe_background_updates", "subscribe_background_updates"]);
  });

  it("lets live message state win over a delayed open-page projection", async () => {
    const connection = transport();
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const token = result.current.cache.capture("root");
    act(() => result.current.rememberPage("root", "Opened page", token));
    const loaded = result.current.cache.capture("root");
    const request = connection.requests()[0];
    act(() => connection.emit("background-session-updated", { ...update(request, "root"), message_revision: null, latest_assistant_message: null }));
    expect(result.current.cache.preview("root")).toBe("Opened page");
    act(() => connection.emit("background-session-updated", update(request, "root", "2", "Live reply")));
    act(() => result.current.rememberPage("root", "Delayed old page", loaded));
    expect(result.current.cache.preview("root")).toBe("Live reply");
  });

  it("keeps compact events that arrive before the subscription acknowledgement", async () => {
    const connection = transport();
    let accept!: () => void;
    const accepted = new Promise<void>((resolve) => { accept = resolve; });
    connection.send.mockImplementationOnce(async (_command, payload) => {
      await accepted;
      const request = (payload as { request: BackgroundUpdatesRequest }).request;
      return { subscription_id: request.subscription_id, generation: "server", revision: "0", scope: request.scope, subscribed: true };
    });
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const request = connection.requests()[0];
    expect(result.current.status).toBe("Connecting background updates…");
    act(() => connection.emit("background-session-updated", update(request, "root", "1", "Arrived before ack")));
    await act(async () => { accept(); });
    expect(result.current.status).toBeNull();
    expect(result.current.cache.preview("root")).toBe("Arrived before ack");
    act(() => connection.emit("background-session-updated", update(request, "root", "1", "Duplicate")));
    expect(result.current.cache.preview("root")).toBe("Arrived before ack");
    act(() => connection.emit("background-session-updated", update(request, "root", "2", "Later reply")));
    expect(result.current.cache.preview("root")).toBe("Later reply");
    expect(connection.send).toHaveBeenCalledOnce();
  });

  it("replaces an expired lease with a fresh live-only identity and retains known text", async () => {
    const connection = transport();
    let expired = false;
    connection.send.mockImplementation(async (command, payload) => {
      if (command === "renew_background_subscriptions") {
        expired = true;
        throw new Error("Background subscription expired; subscribe again");
      }
      const request = (payload as { request: BackgroundUpdatesRequest }).request;
      return { subscription_id: request.subscription_id, generation: "server", revision: "0", scope: request.scope, subscribed: true };
    });
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    const first = connection.requests()[0];
    act(() => connection.emit("background-session-updated", { ...update(first, "root"), is_running: true }));
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(expired).toBe(true);
    const second = connection.requests()[1];
    expect(second.subscription_id).not.toBe(first.subscription_id);
    expect(second).toMatchObject({ scope: "all", session_keys: [] });
    expect(connection.release).toHaveBeenCalledWith("subscribe_background_updates", { request: { ...first, unsubscribe: true } });
    expect(result.current.status).toBeNull();
    expect(result.current.cache.preview("root")).toBe("Main assistant reply.");
    expect(result.current.cache.activity("root")).toBeUndefined();
    expect(connection.send.mock.calls.map(([command]) => command)).toEqual(["subscribe_background_updates", "renew_background_subscriptions", "subscribe_background_updates"]);
    act(() => connection.emit("background-session-updated", update(first, "root", "2", "Expired stream")));
    expect(result.current.cache.preview("root")).toBe("Main assistant reply.");
  });

  it("does not retry disabled External reader subscriptions", async () => {
    const connection = transport();
    connection.send.mockRejectedValue(new Error("Background updates unavailable in External reader mode"));
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    expect(result.current.status).toBe("Background updates unavailable in External reader mode. Open sessions still update.");
    await act(async () => { await vi.advanceTimersByTimeAsync(120_000); });
    expect(connection.send).toHaveBeenCalledOnce();
  });

  it("stays quiet after an unsupported legacy command instead of renewing or rereading", async () => {
    const connection = transport();
    connection.send.mockRejectedValue(new Error("Unknown viewer API route: subscribe_background_updates"));
    const { result } = renderHook(() => useBackgroundUpdates(connection.invoke, connection.listen, connection.release, []));
    await act(async () => {});
    expect(result.current.status).toBe("Background updates unavailable on this host. Open sessions still update.");
    expect(result.current.authority_revision).toBe(0);
    await act(async () => { await vi.advanceTimersByTimeAsync(120_000); });
    expect(connection.send).toHaveBeenCalledOnce();
    expect(result.current.cache.preview("unopened")).toBeUndefined();
  });
});

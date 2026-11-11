import { afterEach, expect, it, vi } from "vitest";
import { loadBrowserPresence, MAX_PRESENCE_HOSTS, validatePresenceIds, validatePresenceResult } from "./hubPresence";

const uuid = (index: number) => `550e8400-e29b-41d4-a716-${index.toString(16).padStart(12, "0")}`;
const first = uuid(1);
const second = uuid(2);
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json; charset=utf-8" } });
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });

it("bounds requests to unique canonical UUIDs and accepts the full batch", () => {
  const maximum = Array.from({ length: MAX_PRESENCE_HOSTS }, (_, index) => uuid(index));
  expect(() => validatePresenceIds(maximum)).not.toThrow();
  expect(() => validatePresenceIds([])).not.toThrow();
  for (const ids of [[first, first], ["invalid"], [first.toUpperCase()], [...maximum, uuid(MAX_PRESENCE_HOSTS)], [null]]) {
    expect(() => validatePresenceIds(ids as string[])).toThrow("Invalid machine presence request");
  }
  expect(() => validatePresenceIds(null as unknown as string[])).toThrow("Invalid machine presence request");
});

it("requires exactly the requested identities and strips directory authority fields", () => {
  expect(validatePresenceResult({ machines: [
    { host_id: second, online: false, host_public_key: "untrusted", name: "Untrusted name" },
    { host_id: first, online: true, connector_version: "0.1.1-beta.2+build" },
  ] }, [first, second])).toEqual({
    machines: [
      { host_id: second, online: false },
      { host_id: first, online: true, connector_version: "0.1.1-beta.2+build" },
    ],
  });
  for (const value of [null, [], {}, { machines: [] },
    { machines: [{ host_id: first, online: true }] },
    { machines: [{ host_id: first, online: true }, { host_id: first, online: false }] },
    { machines: [{ host_id: first, online: true }, { host_id: uuid(3), online: false }] },
    { machines: [{ host_id: first, online: true }, { host_id: second, online: "false" }] },
    { machines: [{ host_id: first, online: true }, null] },
  ]) expect(() => validatePresenceResult(value, [first, second])).toThrow();
  expect(validatePresenceResult({ machines: [] }, [])).toEqual({ machines: [] });
  for (const connector_version of ["1".repeat(65), "0.1.1\n", "v0.1.1", 1]) {
    expect(() => validatePresenceResult({ machines: [{ host_id: first, online: true, connector_version }] }, [first])).toThrow("connector version");
  }
  expect(() => validatePresenceResult({
    machines: [{ host_id: first, online: false, connector_version: "0.1.1" }],
  }, [first])).toThrow("connector version");
});

it("sends bounded presence IDs without credentials and treats a successful false result as offline", async () => {
  const fetcher = vi.fn().mockResolvedValue(json({ machines: [{ host_id: first, online: false }] }));
  vi.stubGlobal("fetch", fetcher);
  expect(await loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).toEqual({ machines: [{ host_id: first, online: false }] });
  expect(fetcher).toHaveBeenCalledWith("https://hub.example/hub/v1/presence", expect.objectContaining({
    method: "POST", body: JSON.stringify({ host_ids: [first] }), credentials: "omit", redirect: "error", cache: "no-store",
    headers: { Accept: "application/json", "Content-Type": "application/json", "X-Tokn-Presence-Versions": "1" },
  }));
  expect(await loadBrowserPresence("https://hub.example", [], new AbortController().signal)).toEqual({ machines: [] });
  expect(fetcher).toHaveBeenCalledOnce();
});

it("does not translate server or transport failures into offline presence", async () => {
  const fetcher = vi.fn().mockResolvedValue(new Response("Unavailable", { status: 503 }));
  vi.stubGlobal("fetch", fetcher);
  await expect(loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).rejects.toThrow("503");
  fetcher.mockRejectedValue(new TypeError("Network unavailable"));
  await expect(loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).rejects.toThrow("Network unavailable");
});

it("rejects oversized, non-JSON and incomplete responses", async () => {
  const fetcher = vi.fn();
  vi.stubGlobal("fetch", fetcher);
  fetcher.mockResolvedValue(new Response(" ".repeat(24 * 1024 + 1), { headers: { "Content-Type": "application/json" } }));
  await expect(loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).rejects.toThrow("too large");
  fetcher.mockResolvedValue(new Response("<html>Not presence</html>", { headers: { "Content-Type": "text/html" } }));
  await expect(loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).rejects.toThrow("invalid machine presence");
  fetcher.mockResolvedValue(json({ machines: [] }));
  await expect(loadBrowserPresence("https://hub.example", [first], new AbortController().signal)).rejects.toThrow("incomplete");
});

it("does not start canceled requests or accept a response canceled during transport", async () => {
  const fetcher = vi.fn();
  vi.stubGlobal("fetch", fetcher);
  const canceled = new AbortController();
  canceled.abort();
  await expect(loadBrowserPresence("https://hub.example", [first], canceled.signal)).rejects.toThrow("disconnected");
  expect(fetcher).not.toHaveBeenCalled();
  const active = new AbortController();
  fetcher.mockImplementation(async (_url, init) => {
    active.abort();
    expect(init.signal.aborted).toBe(true);
    return json({ machines: [{ host_id: first, online: true }] });
  });
  await expect(loadBrowserPresence("https://hub.example", [first], active.signal)).rejects.toThrow("disconnected");
});

it("aborts a transport that exceeds the presence deadline", async () => {
  vi.useFakeTimers();
  let signal!: AbortSignal;
  vi.stubGlobal("fetch", vi.fn().mockImplementation((_url, init) => new Promise((_resolve, reject) => {
    signal = init.signal;
    signal.addEventListener("abort", () => reject(new DOMException("Request aborted", "AbortError")), { once: true });
  })));
  const pending = loadBrowserPresence("https://hub.example", [first], new AbortController().signal);
  const rejected = expect(pending).rejects.toThrow("Request aborted");
  await vi.advanceTimersByTimeAsync(10_000);
  await rejected;
  expect(signal.aborted).toBe(true);
});

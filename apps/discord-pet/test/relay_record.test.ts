import { describe, expect, test } from "bun:test";

import { dispatchRelayRecord, parseRelayRecord, RelayActivityDispatcher, type RelayEvent } from "../src/protocol";

const envelope = {
  path: "/sessions/pi.jsonl",
  topic: "pi.session-1",
  session: {
    session_id: "session-1", agent_path: "/root/child", agent_nickname: "Scout",
    project: { project_name: "project", folder: "/work/project", repository_name: "repo" }
  },
  record_id: "jsonl:12",
  operation: "upsert",
  events: [{ type: "reasoning" }, { type: "message", text: "hello" }, { type: "tool_call" }]
};

describe("shared Relay record protocol", () => {
  test.each(["opencode", "zcode", "workbuddy", "dsh"])("does not replay unchanged %s events on native or record updates", async (provider) => {
    const activity = new RelayActivityDispatcher(2);
    const record = parseRelayRecord({ ...envelope, topic: `${provider}.session-1`, record_id: "message:1" })!;
    const seen: string[] = [];
    const observe = (input: RelayEvent): void => { seen.push(input.event.type); };
    await activity.dispatch(record, observe);
    await activity.dispatch({ ...record, native: { changed: true } }, observe);
    expect(seen).toEqual(["reasoning", "message", "tool_call"]);
    const changed = { ...record, events: [...record.events, { type: "usage" }] };
    await activity.dispatch(changed, observe);
    expect(seen).toEqual(["reasoning", "message", "tool_call", "usage"]);
    await activity.dispatch({ ...record, events: [], operation: "remove" }, observe);
    await activity.dispatch(record, observe);
    expect(seen).toHaveLength(7);
    await activity.dispatch({ ...record, record_id: "message:2" }, observe);
    await activity.dispatch({ ...record, record_id: "message:3" }, observe);
    await activity.dispatch(record, observe);
    expect(seen).toHaveLength(16);
  });

  test.each(["opencode", "zcode", "workbuddy", "dsh"])("does not replay %s messages when model changes shift their position", async (provider) => {
    const activity = new RelayActivityDispatcher();
    const record = parseRelayRecord({ ...envelope, topic: `${provider}.session-1`, record_id: "message:1" })!;
    const model = { type: "provider_changed", model_id: "model-a" };
    const message = { type: "message", message_id: "1", text: "reply" };
    const seen: RelayEvent["event"][] = [];
    const observe = (input: RelayEvent): void => { seen.push(input.event); };
    await activity.dispatch({ ...record, events: [model, message] }, observe);
    await activity.dispatch({ ...record, events: [message] }, observe);
    expect(seen).toEqual([model, message]);
    const next_model = { ...model, model_id: "model-b" };
    await activity.dispatch({ ...record, events: [next_model, message] }, observe);
    expect(seen).toEqual([model, message, next_model]);
    const changed_message = { ...message, text: "edited reply" };
    await activity.dispatch({ ...record, events: [changed_message, next_model] }, observe);
    expect(seen).toEqual([model, message, next_model, changed_message]);
  });

  test("dispatches added identical occurrences and changed events in batch order", async () => {
    const activity = new RelayActivityDispatcher();
    const record = parseRelayRecord({ ...envelope, topic: "opencode.session-1", record_id: "message:1" })!;
    const usage = { type: "usage", input_tokens: 1 };
    const reasoning = { type: "reasoning", text: "thinking" };
    const message = { type: "message", text: "reply" };
    const seen: RelayEvent["event"][] = [];
    const observe = (input: RelayEvent): void => { seen.push(input.event); };
    await activity.dispatch({ ...record, events: [usage, usage, reasoning] }, observe);
    expect(seen).toEqual([usage, usage, reasoning]);
    seen.length = 0;
    await activity.dispatch({ ...record, events: [usage, message, reasoning, usage, usage] }, observe);
    expect(seen).toEqual([message, usage]);
    seen.length = 0;
    await activity.dispatch({ ...record, events: [usage, usage, usage, reasoning, message] }, observe);
    expect(seen).toEqual([]);
    await activity.dispatch({ ...record, events: [usage] }, observe);
    await activity.dispatch({ ...record, events: [usage, usage] }, observe);
    expect(seen).toEqual([usage]);
  });

  test("keeps JSONL append observations outside the mutable-record cache", async () => {
    const activity = new RelayActivityDispatcher();
    const record = parseRelayRecord(envelope)!;
    const seen: string[] = [];
    const observe = (input: RelayEvent): void => { seen.push(input.event.type); };
    await activity.dispatch(record, observe);
    await activity.dispatch(record, observe);
    expect(seen).toEqual(["reasoning", "message", "tool_call", "reasoning", "message", "tool_call"]);
  });

  test("does not cache a mutable batch interrupted by cancellation", async () => {
    const activity = new RelayActivityDispatcher();
    const record = parseRelayRecord({ ...envelope, topic: "opencode.session-1", record_id: "message:1" })!;
    const abort = new AbortController();
    const seen: string[] = [];
    await activity.dispatch(record, async (input) => {
      seen.push(input.event.type);
      await Promise.resolve();
      abort.abort();
    }, abort.signal);
    expect(seen).toEqual(["reasoning"]);
    await activity.dispatch(record, (input) => { seen.push(input.event.type); });
    expect(seen).toEqual(["reasoning", "reasoning", "message", "tool_call"]);
    await activity.dispatch(record, (input) => { seen.push(input.event.type); });
    expect(seen).toHaveLength(4);
  });

  test("preserves ordered batches, session metadata and optional native data", async () => {
    const native = { arbitrary: [1, 2], provider_extension: true };
    const record = parseRelayRecord({ ...envelope, native });
    expect(record?.native).toEqual(native);
    const seen: RelayEvent[] = [];
    await dispatchRelayRecord(record!, async (event) => { seen.push(event); });
    expect(seen.map((input) => input.event.type)).toEqual(["reasoning", "message", "tool_call"]);
    for (const input of seen) {
      expect(input.path).toBe(envelope.path);
      expect(input.topic).toBe(envelope.topic);
      expect(input.session).toEqual(envelope.session);
      expect("native" in input).toBe(false);
    }
    expect(Object.hasOwn(parseRelayRecord(envelope)!, "native")).toBe(false);
  });

  test("accepts empty native-only and removal records without waking workers", async () => {
    const seen: RelayEvent[] = [];
    for (const operation of ["upsert", "remove"]) {
      const record = parseRelayRecord({ ...envelope, operation, events: [], native: {} });
      expect(record).not.toBeNull();
      await dispatchRelayRecord(record!, (event) => { seen.push(event); });
    }
    expect(seen).toEqual([]);
  });

  test("rejects legacy envelopes and malformed batches atomically", () => {
    const { events, ...rest } = envelope;
    expect(parseRelayRecord({ ...rest, event: events[0] })).toBeNull();
    for (const invalid of [null, [], {}, { type: 3 }]) {
      expect(parseRelayRecord({ ...envelope, events: [events[0], invalid] })).toBeNull();
    }
    expect(parseRelayRecord({ ...envelope, operation: "remove" })).toBeNull();
    expect(parseRelayRecord({ ...envelope, operation: "future" })).toBeNull();
  });

  test("waits for workers and respects cancellation between events", async () => {
    const record = parseRelayRecord(envelope)!;
    const abort = new AbortController();
    const seen: string[] = [];
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const dispatch = dispatchRelayRecord(record, async (input) => {
      seen.push(input.event.type);
      await gate;
      abort.abort();
    }, abort.signal);
    await Promise.resolve();
    expect(seen).toEqual(["reasoning"]);
    release();
    await dispatch;
    expect(seen).toEqual(["reasoning"]);
  });
});

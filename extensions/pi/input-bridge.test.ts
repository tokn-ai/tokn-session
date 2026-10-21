import { createConnection } from "node:net";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  descriptorPathForSession,
  PiInputBridge,
  socketPathForProcess,
  type PiInputBridgeDescriptor,
  type PiInputBridgeResponse,
  type PiInputContent,
  type PiExtensionContext,
  type PiNativeModel
} from "./input-bridge";

interface TestHarness {
  context: PiExtensionContext;
  descriptor: PiInputBridgeDescriptor;
  bridge: PiInputBridge;
  getIdle(): boolean;
  setIdle(value: boolean): void;
  messages: Array<{
    message: string | PiInputContent[];
    options?: { deliverAs?: "steer" | "followUp" };
  }>;
  model_changes: PiNativeModel[];
}

async function makeHarness(
  root: string,
  models?: PiNativeModel[],
  behavior?: {
    set_model?(model: PiNativeModel, context: PiExtensionContext): Promise<boolean>;
    send?(): void;
  }
): Promise<TestHarness> {
  const sessionFile = join(root, "session.jsonl");
  const descriptorPath = join(root, "descriptor.json");
  const socketPath = join(root, "bridge.sock");
  await writeFile(sessionFile, "{\"type\":\"session\",\"id\":\"session-1\"}\n");

  let idle = true;
  const messages: TestHarness["messages"] = [];
  const model_changes: PiNativeModel[] = [];
  const context: PiExtensionContext = {
    mode: "tui",
    isIdle: () => idle,
    sessionManager: {
      getSessionFile: () => sessionFile,
      getSessionId: () => "session-1"
    },
    ui: {
      notify: () => {},
      setStatus: () => {}
    }
  };
  if (models) { context.model = models[0]; context.modelRegistry = { getAvailable: () => models }; }
  const bridge = new PiInputBridge({
    api: {
      ...(models ? { async setModel(model: PiNativeModel) {
        model_changes.push(model);
        if (behavior?.set_model) return behavior.set_model(model, context);
        context.model = model;
        return true;
      } } : {}),
      sendUserMessage(message, options) {
        messages.push({ message, options });
        idle = false;
        behavior?.send?.();
      }
    },
    context,
    descriptor_path: descriptorPath,
    socket_path: socketPath,
    token: "test-token",
    instance_id: "instance-1",
    pid: 1234
  });
  const descriptor = await bridge.start();
  return {
    context,
    descriptor,
    bridge,
    getIdle: () => idle,
    setIdle: (value) => {
      idle = value;
    },
    messages,
    model_changes
  };
}

function request(socketPath: string, value: unknown): Promise<PiInputBridgeResponse> {
  return new Promise((resolveRequest, rejectRequest) => {
    const socket = createConnection(socketPath);
    let output = "";
    socket.setEncoding("utf8");
    socket.once("connect", () => {
      socket.write(`${JSON.stringify(value)}\n`);
    });
    socket.on("data", (chunk: string) => {
      output += chunk;
    });
    socket.once("end", () => {
      try {
        resolveRequest(JSON.parse(output) as PiInputBridgeResponse);
      } catch (error) {
        rejectRequest(error);
      }
    });
    socket.once("error", rejectRequest);
  });
}

describe("Pi input bridge", () => {
  test("applies an idle prompt's draft model before checking image support and deduplicates the entire operation", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-model-input-"));
    const models = [
      { id: "text", provider: "test", input: ["text"] },
      { id: "vision", provider: "test", input: ["text", "image"] }
    ];
    const harness = await makeHarness(root, models);
    const base = { protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1" };
    try {
      const status = await request(harness.descriptor.socket_path, { ...base, type: "status" });
      if (status.type !== "ready") throw new Error("expected ready");
      expect(status.model_input_version).toBe(1);
      // Discovery and composing a selection never call the native setter.
      await request(harness.descriptor.socket_path, { ...base, type: "models", request_id: "catalog" });
      expect(harness.model_changes).toHaveLength(0);
      const submit = {
        ...base, type: "submit", request_id: "vision-prompt", delivery: "steer",
        content: [{ type: "image", data: "YQ==", mimeType: "image/png" }],
        model_selection: {
          model_provider: "test", model_id: "vision", runtime_generation: "instance-1",
          expected_revision: "viewer-revision", expected_model_revision: status.model_revision
        }
      };
      const result = await request(harness.descriptor.socket_path, submit);
      expect(result).toMatchObject({ type: "admitted", disposition: "started", model_result: { outcome: "applied", model: { model_id: "vision" } } });
      // The native client translates a stale viewer revision differently on a
      // later retry; the original intent still identifies this exact receipt.
      await expect(request(harness.descriptor.socket_path, {
        ...submit, model_selection: { ...submit.model_selection, expected_model_revision: "stale:viewer-revision" }
      })).resolves.toEqual(result);
      expect(harness.model_changes).toEqual([models[1]]);
      expect(harness.messages).toHaveLength(1);
      await expect(request(harness.descriptor.socket_path, {
        ...submit, model_selection: { ...submit.model_selection, model_id: "text" }
      })).resolves.toMatchObject({ type: "error", code: "request_conflict" });
    } finally { await harness.bridge.stop(); await rm(root, { recursive: true, force: true }); }
  });

  test("busy viewer input ignores stale draft model and process revisions and always steers the actual model", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-busy-model-"));
    const harness = await makeHarness(root, [{ id: "active", provider: "actual", input: ["text"] }]);
    harness.setIdle(false);
    const submit = {
      protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file,
      instance_id: "instance-1", type: "submit", request_id: "busy", delivery: "steer",
      content: [{ type: "text", text: "Do this now" }],
      model_selection: {
        model_provider: "other", model_id: "no-longer-listed", expected_revision: "stale",
        expected_model_revision: "stale", runtime_generation: "old-instance"
      }
    };
    try {
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "admitted", disposition: "queued_steer",
        model_result: { outcome: "ignored_while_running", model: { model_provider: "actual", model_id: "active" } }
      });
      expect(harness.model_changes).toHaveLength(0);
      expect(harness.messages).toEqual([{ message: "Do this now", options: { deliverAs: "steer" } }]);
      for (const [index, model_selection] of [
        { model_provider: "", model_id: "", expected_revision: "", expected_model_revision: "", runtime_generation: "" },
        null
      ].entries()) {
        const malformed = { ...submit, request_id: `ignored-${index}`, model_selection };
        const result = await request(harness.descriptor.socket_path, malformed);
        expect(result).toMatchObject({ type: "admitted", disposition: "queued_steer", model_result: { outcome: "ignored_while_running" } });
        await expect(request(harness.descriptor.socket_path, malformed)).resolves.toEqual(result);
      }
      expect(harness.model_changes).toHaveLength(0);
      expect(harness.messages).toHaveLength(3);
    } finally { await harness.bridge.stop(); await rm(root, { recursive: true, force: true }); }
  });

  test("busy image validation uses the active model and pending messages prevent an idle model switch", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-model-images-"));
    const harness = await makeHarness(root, [
      { id: "text", provider: "test", input: ["text"] },
      { id: "vision", provider: "test", input: ["text", "image"] }
    ]);
    const base = { protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1" };
    try {
      const status = await request(harness.descriptor.socket_path, { ...base, type: "status" });
      if (status.type !== "ready") throw new Error("expected ready");
      harness.context.hasPendingMessages = () => true;
      const submit = {
        ...base, type: "submit", request_id: "pending-image", delivery: "steer",
        content: [{ type: "image", data: "YQ==", mimeType: "image/png" }],
        model_selection: {
          model_provider: "test", model_id: "vision", runtime_generation: "instance-1",
          expected_revision: "viewer", expected_model_revision: status.model_revision
        }
      };
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "error", code: "unsupported", model_result: { outcome: "ignored_while_running", model: { model_id: "text" } }
      });
      expect(harness.model_changes).toHaveLength(0);
      expect(harness.messages).toHaveLength(0);
      await expect(request(harness.descriptor.socket_path, {
        ...submit, request_id: "pending-text", content: [{ type: "text", text: "steer pending turn" }]
      })).resolves.toMatchObject({ type: "admitted", disposition: "queued_steer", model_result: { outcome: "ignored_while_running" } });
      expect(harness.messages[0]?.options).toEqual({ deliverAs: "steer" });
    } finally { await harness.bridge.stop(); await rm(root, { recursive: true, force: true }); }
  });

  test("idle selections require current revisions, registry membership, and the effective image capability", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-model-guards-"));
    const harness = await makeHarness(root, [{ id: "text", provider: "test", input: ["text"] }]);
    const base = { protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1" };
    try {
      const status = await request(harness.descriptor.socket_path, { ...base, type: "status" });
      if (status.type !== "ready") throw new Error("expected ready");
      const submit = {
        ...base, type: "submit", request_id: "guarded", delivery: "steer", content: [{ type: "text", text: "next" }],
        model_selection: { model_provider: "test", model_id: "text", runtime_generation: "instance-1", expected_revision: "viewer", expected_model_revision: status.model_revision }
      };
      for (const model_selection of [
        { ...submit.model_selection, expected_model_revision: "stale" },
        { ...submit.model_selection, runtime_generation: "old-process" },
        { ...submit.model_selection, model_id: "missing" }
      ]) {
        await expect(request(harness.descriptor.socket_path, { ...submit, model_selection })).resolves.toMatchObject({ type: "error" });
      }
      await expect(request(harness.descriptor.socket_path, {
        ...submit, content: [{ type: "image", data: "YQ==", mimeType: "image/png" }]
      })).resolves.toMatchObject({ type: "error", code: "unsupported" });
      expect(harness.messages).toHaveLength(0);
      expect(harness.model_changes).toHaveLength(0);
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "admitted", disposition: "started", model_result: { outcome: "unchanged", model: { model_id: "text" } }
      });
      expect(harness.model_changes).toHaveLength(0);
    } finally { await harness.bridge.stop(); await rm(root, { recursive: true, force: true }); }
  });

  test("caches uncertain setter, busy readback, and input failures without reapplying models or resending prompts", async () => {
    for (const failure of ["setter", "send", "busy"] as const) {
      const root = await mkdtemp(join(tmpdir(), "tokn-pi-model-uncertain-"));
      const models = [{ id: "old", provider: "test", input: ["text"] }, { id: "next", provider: "test", input: ["text"] }];
      const harness = await makeHarness(root, models, {
        async set_model(model, context) {
          context.model = model;
          if (failure === "setter") throw new Error("hook failed after setting model");
          if (failure === "busy") context.isIdle = () => false;
          return true;
        },
        send() { throw new Error("send confirmation failed"); }
      });
      const base = { protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1" };
      try {
        const status = await request(harness.descriptor.socket_path, { ...base, type: "status" });
        if (status.type !== "ready") throw new Error("expected ready");
        const submit = {
          ...base, type: "submit", request_id: "uncertain", delivery: "steer", content: [{ type: "text", text: "next" }],
          model_selection: { model_provider: "test", model_id: "next", runtime_generation: "instance-1", expected_revision: "viewer", expected_model_revision: status.model_revision }
        };
        const result = await request(harness.descriptor.socket_path, submit);
        expect(result).toMatchObject({ type: "error", code: "bridge_unavailable", may_have_been_sent: true, model_result: { outcome: "applied", model: { model_id: "next" } } });
        await expect(request(harness.descriptor.socket_path, submit)).resolves.toEqual(result);
        expect(harness.model_changes).toHaveLength(1);
        expect(harness.messages).toHaveLength(failure === "send" ? 1 : 0);
      } finally { await harness.bridge.stop(); await rm(root, { recursive: true, force: true }); }
    }
  });

  test("lists compact models and selects once only when idle with a current model revision", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-models-"));
    const models = [{ id:"text", provider:"test", name:"Text", input:["text"], headers:{ authorization:"secret" } },
      { id:"vision", provider:"test", name:"Vision", input:["text","image"] }];
    const harness = await makeHarness(root, models);
    const base = { protocol:1, token:"test-token", session_id:"session-1", session_file:harness.descriptor.session_file, instance_id:"instance-1" };
    try {
      const status = await request(harness.descriptor.socket_path, { ...base, type:"status", request_id:"status" });
      if (status.type !== "ready") throw new Error("expected ready status");
      const catalog = await request(harness.descriptor.socket_path, { ...base, type:"models", request_id:"models" });
      expect(catalog).toMatchObject({ type:"models", models:[{ model:{ model_id:"text" }, supports_images:false }, { model:{ model_id:"vision" }, supports_images:true }] });
      expect(JSON.stringify(catalog)).not.toContain("secret");
      const selection = { ...base, type:"select_model", request_id:"select", expected_model_revision:status.model_revision, model_provider:"test", model_id:"vision" };
      harness.setIdle(false);
      await expect(request(harness.descriptor.socket_path, selection)).resolves.toMatchObject({ type:"error", code:"unsupported" });
      harness.setIdle(true);
      const selected = await request(harness.descriptor.socket_path, selection);
      expect(selected).toMatchObject({ type:"model_selected", model:{ model_id:"vision" } });
      await expect(request(harness.descriptor.socket_path, selection)).resolves.toEqual(selected);
      expect(harness.model_changes).toHaveLength(1);
      await expect(request(harness.descriptor.socket_path, { ...base, type:"status", request_id:"fresh" })).resolves.toMatchObject({ images_available:true });
      await expect(request(harness.descriptor.socket_path, { ...selection, request_id:"stale", model_id:"text" })).resolves.toMatchObject({ type:"error", code:"request_conflict" });
    } finally { await harness.bridge.stop(); await rm(root, { recursive:true, force:true }); }
  });
  test("advertises current model images, sends image-only content, and binds ordered images to request IDs", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-images-"));
    const harness = await makeHarness(root);
    const base = { protocol: 1, token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1" };
    try {
      await expect(request(harness.descriptor.socket_path, { ...base, type: "status" })).resolves.toMatchObject({ type: "ready", images_available: false });
      harness.context.model = { input: ["text", "image"] };
      await expect(request(harness.descriptor.socket_path, { ...base, type: "status" })).resolves.toMatchObject({ type: "ready", images_available: true });
      const content = [{ type: "image", data: Buffer.from("a".repeat(40 * 1024)).toString("base64"), mimeType: "image/png" }];
      const message = { ...base, type: "submit", request_id: "image-message", delivery: "auto", content };
      await expect(request(harness.descriptor.socket_path, message)).resolves.toMatchObject({ type: "admitted", disposition: "started" });
      await expect(request(harness.descriptor.socket_path, message)).resolves.toMatchObject({ type: "admitted" });
      expect(harness.messages).toEqual([{ message: content, options: undefined }]);
      await expect(request(harness.descriptor.socket_path, { ...message, content: [{ ...content[0], data: Buffer.from("different").toString("base64") }] })).resolves.toMatchObject({ type: "error", code: "request_conflict" });
      harness.context.model = { input: ["text"] };
      await expect(request(harness.descriptor.socket_path, { ...message, request_id: "unsupported" })).resolves.toMatchObject({ type: "error", code: "unsupported" });
      expect(harness.messages).toHaveLength(1);
    } finally {
      await harness.bridge.stop();
      await rm(root, { recursive: true, force: true });
    }
  });

  test("rejects malformed, unsupported, duplicate text, and excessive image content before delivery", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-invalid-images-"));
    const harness = await makeHarness(root);
    harness.context.model = { input: ["image"] };
    const base = { protocol: 1, type: "submit", token: "test-token", session_id: "session-1", session_file: harness.descriptor.session_file, instance_id: "instance-1", request_id: "invalid-image", delivery: "auto" };
    try {
      for (const content of [
        [{ type: "image", data: "not-base64", mimeType: "image/png" }],
        [{ type: "image", data: "YQ==", mimeType: "image/svg+xml" }],
        [{ type: "image", data: "", mimeType: "image/png" }],
        Array.from({ length: 5 }, () => ({ type: "image", data: "YQ==", mimeType: "image/png" })),
        [{ type: "text", text: "one" }, { type: "text", text: "two" }]
      ]) {
        await expect(request(harness.descriptor.socket_path, { ...base, content })).resolves.toMatchObject({ type: "error", code: "message_invalid" });
      }
      expect(harness.messages).toHaveLength(0);
    } finally {
      await harness.bridge.stop();
      await rm(root, { recursive: true, force: true });
    }
  });

  test("uses session descriptors and process-instance sockets", () => {
    const firstSession = descriptorPathForSession("/sessions/first.jsonl");
    const secondSession = descriptorPathForSession("/sessions/second.jsonl");
    const firstProcess = socketPathForProcess(1234, "aaaaaaaaaaaaaaaa1111111111111111");
    const secondProcess = socketPathForProcess(1234, "bbbbbbbbbbbbbbbb2222222222222222");

    expect(firstSession).not.toBe(secondSession);
    expect(firstProcess).not.toBe(secondProcess);
    expect(Buffer.byteLength(firstProcess)).toBeLessThan(104);
  });

  test("publishes a process endpoint and starts input while Pi is idle", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-test-"));
    const harness = await makeHarness(root);
    try {
      const descriptor = JSON.parse(await readFile(join(root, "descriptor.json"), "utf8")) as PiInputBridgeDescriptor;
      expect(descriptor).toEqual(harness.descriptor);
      expect(descriptor).toMatchObject({
        protocol: 1,
        provider: "pi",
        transport: "unix",
        session_id: "session-1",
        instance_id: "instance-1",
        pid: 1234,
        token: "test-token"
      });

      await expect(
        request(harness.descriptor.socket_path, {
          protocol: 1,
          type: "status",
          request_id: "status-1",
          token: "test-token",
          session_id: "session-1",
          session_file: harness.descriptor.session_file,
          instance_id: "instance-1"
        })
      ).resolves.toMatchObject({
        protocol: 1,
        type: "ready",
        request_id: "status-1",
        instance_id: "instance-1",
        state: "idle"
      });

      await expect(
        request(harness.descriptor.socket_path, {
          protocol: 1,
          type: "submit",
          request_id: "submit-1",
          token: "test-token",
          session_id: "session-1",
          session_file: harness.descriptor.session_file,
          instance_id: "instance-1",
          delivery: "auto",
          content: [{ type: "text", text: "  Continue the task  " }]
        })
      ).resolves.toMatchObject({
        protocol: 1,
        type: "admitted",
        request_id: "submit-1",
        session_id: "session-1",
        instance_id: "instance-1",
        disposition: "started"
      });
      expect(harness.messages).toEqual([{
        message: "  Continue the task  ",
        options: undefined
      }]);
      expect(harness.getIdle()).toBe(false);
    } finally {
      await harness.bridge.stop();
      try {
        await expect(readFile(join(root, "descriptor.json"))).rejects.toMatchObject({ code: "ENOENT" });
      } finally {
        await rm(root, { recursive: true, force: true });
      }
    }
  });

  test("queues busy input and deduplicates retries", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-test-"));
    const harness = await makeHarness(root);
    harness.setIdle(false);
    const submit = {
      protocol: 1,
      type: "submit",
      request_id: "submit-queued",
      token: "test-token",
      session_id: "session-1",
      session_file: harness.descriptor.session_file,
      instance_id: "instance-1",
      delivery: "auto",
      content: [{ type: "text", text: "One more" }]
    };
    try {
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "admitted",
        disposition: "queued_follow_up"
      });
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "admitted",
        disposition: "queued_follow_up"
      });
      expect(harness.messages).toEqual([{
        message: "One more",
        options: { deliverAs: "followUp" }
      }]);

      await expect(request(harness.descriptor.socket_path, {
        ...submit,
        delivery: "steer"
      })).resolves.toMatchObject({
        type: "error",
        code: "request_conflict"
      });

      await expect(request(harness.descriptor.socket_path, {
        ...submit,
        request_id: "submit-steer",
        delivery: "steer"
      })).resolves.toMatchObject({
        type: "admitted",
        disposition: "queued_steer"
      });
      expect(harness.messages.at(-1)).toEqual({
        message: "One more",
        options: { deliverAs: "steer" }
      });
    } finally {
      await harness.bridge.stop();
      await rm(root, { recursive: true, force: true });
    }
  });

  test("rejects unauthorized and mismatched session requests", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-test-"));
    const harness = await makeHarness(root);
    try {
      await expect(
        request(harness.descriptor.socket_path, {
          protocol: 1,
          type: "status",
          token: "wrong-token",
          session_id: "session-1",
          session_file: harness.descriptor.session_file,
          instance_id: "instance-1"
        })
      ).resolves.toMatchObject({ type: "error", code: "unauthorized" });

      await expect(
        request(harness.descriptor.socket_path, {
          protocol: 1,
          type: "status",
          token: "test-token",
          session_id: "other-session",
          session_file: harness.descriptor.session_file,
          instance_id: "instance-1"
        })
      ).resolves.toMatchObject({ type: "error", code: "session_mismatch" });

      await expect(
        request(harness.descriptor.socket_path, {
          protocol: 1,
          type: "status",
          token: "test-token",
          session_id: "session-1",
          session_file: harness.descriptor.session_file,
          instance_id: "old-instance"
        })
      ).resolves.toMatchObject({ type: "error", code: "instance_mismatch" });
    } finally {
      await harness.bridge.stop();
      await rm(root, { recursive: true, force: true });
    }
  });

  test("preserves multiline messages and rejects other control characters", async () => {
    const root = await mkdtemp(join(tmpdir(), "tokn-pi-input-test-"));
    const harness = await makeHarness(root);
    const submit = {
      protocol: 1,
      type: "submit",
      request_id: "multiline",
      token: "test-token",
      session_id: "session-1",
      session_file: harness.descriptor.session_file,
      instance_id: "instance-1",
      delivery: "auto",
      content: [{ type: "text", text: "    Explain this:\n\tfirst line\r\n第二行\n" }]
    };
    try {
      await expect(request(harness.descriptor.socket_path, submit)).resolves.toMatchObject({
        type: "admitted",
        disposition: "started"
      });
      expect(harness.messages).toEqual([{
        message: submit.content[0]!.text,
        options: undefined
      }]);
      for (const character of ["\u0000", "\u0008", "\u000b", "\u000c", "\u000e", "\u001b", "\u001f", "\u007f"]) {
        await expect(request(harness.descriptor.socket_path, {
          ...submit,
          request_id: `invalid-${character.charCodeAt(0)}`,
          content: [{ type: "text", text: `before${character}after` }]
        })).resolves.toMatchObject({ type: "error", code: "message_invalid" });
      }
      await expect(request(harness.descriptor.socket_path, {
        ...submit,
        request_id: "whitespace-only",
        content: [{ type: "text", text: " \t\r\n" }]
      })).resolves.toMatchObject({ type: "error", code: "message_invalid" });
      expect(harness.messages).toHaveLength(1);
    } finally {
      await harness.bridge.stop();
      await rm(root, { recursive: true, force: true });
    }
  });
});

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
  type PiExtensionContext
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
}

async function makeHarness(root: string): Promise<TestHarness> {
  const sessionFile = join(root, "session.jsonl");
  const descriptorPath = join(root, "descriptor.json");
  const socketPath = join(root, "bridge.sock");
  await writeFile(sessionFile, "{\"type\":\"session\",\"id\":\"session-1\"}\n");

  let idle = true;
  const messages: TestHarness["messages"] = [];
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
  const bridge = new PiInputBridge({
    api: {
      sendUserMessage(message, options) {
        messages.push({ message, options });
        idle = false;
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
    messages
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

import { createHash, randomBytes } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  unlink,
} from "node:fs/promises";
import { createServer, createConnection, type Server, type Socket } from "node:net";
import { dirname, resolve } from "node:path";
import {
  descriptorPathForSession,
  PI_INPUT_PROTOCOL_VERSION,
  PI_INPUT_PROVIDER,
  socketPathForProcess,
  type PiInputBridgeDescriptor,
  type PiInputBridgeRequest,
  type PiInputBridgeResponse,
  type PiInputDelivery,
  type PiInputContent,
  type PiModelIdentity,
  type PiModelChoice,
} from "./lib/input-protocol";

export {
  descriptorPathForSession,
  socketPathForProcess,
} from "./lib/input-protocol";
export type {
  PiInputBridgeDescriptor,
  PiInputBridgeRequest,
  PiInputBridgeResponse,
  PiInputDelivery,
  PiInputTextContent,
  PiInputImageContent,
  PiInputContent,
} from "./lib/input-protocol";

const PROTOCOL_VERSION = PI_INPUT_PROTOCOL_VERSION;
const PROVIDER = PI_INPUT_PROVIDER;
const MAX_FRAME_BYTES = 34 * 1024 * 1024;
const MAX_IMAGES = 4;
const MAX_IMAGE_BYTES = 8 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES = 24 * 1024 * 1024;
const IMAGE_MIME_TYPES = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);
const MAX_MESSAGE_LENGTH = 16 * 1024;
const SOCKET_REQUEST_TIMEOUT_MS = 5_000;
const MAX_ADMISSION_CACHE_SIZE = 256;

type SessionStartReason = "startup" | "reload" | "new" | "resume" | "fork";
type SessionShutdownReason = "quit" | "reload" | "new" | "resume" | "fork";

export interface PiSessionStartEvent {
  reason: SessionStartReason;
  previousSessionFile?: string;
}

export interface PiSessionShutdownEvent {
  reason: SessionShutdownReason;
  targetSessionFile?: string;
}

export interface PiSessionManager {
  getSessionFile(): string | undefined;
  getSessionId(): string;
}

export interface PiExtensionUi {
  notify(message: string, type?: "info" | "warning" | "error"): void;
  setStatus(key: string, text: string | undefined): void;
}

export interface PiExtensionContext {
  mode: string;
  model?: PiNativeModel;
  modelRegistry?: { getAvailable(): PiNativeModel[] };
  isIdle(): boolean;
  hasPendingMessages?(): boolean;
  sessionManager: PiSessionManager;
  ui: PiExtensionUi;
}

export interface PiNativeModel { id?: string; provider?: string; name?: string; input?: readonly string[] }

export interface PiExtensionApi {
  on(
    event: "session_start",
    handler: (event: PiSessionStartEvent, context: PiExtensionContext) => void | Promise<void>
  ): void;
  on(
    event: "session_shutdown",
    handler: (event: PiSessionShutdownEvent, context: PiExtensionContext) => void | Promise<void>
  ): void;
  sendUserMessage(
    content: string | PiInputContent[],
    options?: { deliverAs?: "steer" | "followUp" }
  ): void;
  setModel?(model: PiNativeModel): Promise<boolean>;
}

export interface PiInputBridgeOptions {
  api: Pick<PiExtensionApi, "sendUserMessage" | "setModel">;
  context: PiExtensionContext;
  descriptor_path?: string;
  socket_path?: string;
  token?: string;
  instance_id?: string;
  pid?: number;
}

interface CachedAdmission {
  fingerprint: string;
  response: Extract<PiInputBridgeResponse, { type: "admitted" }>;
}

export class PiInputBridge {
  readonly #api: Pick<PiExtensionApi, "sendUserMessage" | "setModel">;
  readonly #context: PiExtensionContext;
  readonly #descriptorPathOverride: string | undefined;
  readonly #socketPathOverride: string | undefined;
  readonly #tokenOverride: string | undefined;
  readonly #instanceIdOverride: string | undefined;
  readonly #pid: number;
  #server: Server | undefined;
  #descriptor: PiInputBridgeDescriptor | undefined;
  #descriptorPath: string | undefined;
  readonly #connections = new Set<Socket>();
  readonly #admissions = new Map<string, CachedAdmission>();
  readonly #modelAdmissions = new Map<string, { fingerprint: string; response: PiInputBridgeResponse }>();
  #requests: Promise<void> = Promise.resolve();

  constructor(options: PiInputBridgeOptions) {
    this.#api = options.api;
    this.#context = options.context;
    this.#descriptorPathOverride = options.descriptor_path;
    this.#socketPathOverride = options.socket_path;
    this.#tokenOverride = options.token;
    this.#instanceIdOverride = options.instance_id;
    this.#pid = options.pid ?? process.pid;
  }

  get descriptor(): PiInputBridgeDescriptor | undefined {
    return this.#descriptor;
  }

  async start(): Promise<PiInputBridgeDescriptor> {
    if (this.#descriptor) {
      return this.#descriptor;
    }
    if (process.platform === "win32") {
      throw new Error("the Pi input bridge currently requires Unix domain sockets");
    }
    if (this.#context.mode !== "tui") {
      throw new Error("the Pi input bridge is only available in interactive mode");
    }

    const sessionFile = this.#context.sessionManager.getSessionFile();
    if (!sessionFile) {
      throw new Error("the Pi input bridge requires a persisted session");
    }

    const sessionPath = resolve(sessionFile);
    const instanceId = this.#instanceIdOverride ?? randomBytes(16).toString("hex");
    const descriptorPath = this.#descriptorPathOverride ?? descriptorPathForSession(sessionPath);
    const socketPath = this.#socketPathOverride ?? socketPathForProcess(this.#pid, instanceId);
    const descriptor: PiInputBridgeDescriptor = {
      protocol: PROTOCOL_VERSION,
      provider: PROVIDER,
      transport: "unix",
      session_id: this.#context.sessionManager.getSessionId(),
      session_file: sessionPath,
      instance_id: instanceId,
      socket_path: socketPath,
      pid: this.#pid,
      token: this.#tokenOverride ?? randomBytes(32).toString("hex")
    };

    await prepareSocketPath(socketPath);
    await ensurePrivateDirectory(dirname(descriptorPath));
    await prepareDescriptorPath(descriptorPath);

    const server = createServer((socket) => {
      this.#connections.add(socket);
      socket.once("close", () => this.#connections.delete(socket));
      this.#handleSocket(socket);
    });

    try {
      await listen(server, socketPath);
      server.unref();
      await chmod(socketPath, 0o600);
      await claimDescriptor(descriptorPath, descriptor);
    } catch (error) {
      for (const socket of this.#connections) {
        socket.destroy();
      }
      await closeServer(server, socketPath);
      throw error;
    }

    this.#server = server;
    this.#descriptor = descriptor;
    this.#descriptorPath = descriptorPath;
    return descriptor;
  }

  async stop(): Promise<void> {
    const descriptor = this.#descriptor;
    const server = this.#server;
    const descriptorPath = this.#descriptorPath;
    this.#descriptor = undefined;
    this.#server = undefined;
    this.#descriptorPath = undefined;
    this.#admissions.clear();
    this.#modelAdmissions.clear();

    if (descriptor) {
      try {
        if (server) {
          for (const socket of this.#connections) {
            socket.destroy();
          }
          await closeServer(server, descriptor.socket_path);
        }
      } finally {
        await removeDescriptor(descriptorPath ?? descriptorPathForSession(descriptor.session_file), descriptor);
      }
    }
  }

  #handleSocket(socket: Socket): void {
    socket.setTimeout(SOCKET_REQUEST_TIMEOUT_MS, () => socket.destroy());

    const chunks: Buffer[] = [];
    let received_bytes = 0;
    const onData = (chunk: Buffer): void => {
      const newline = chunk.indexOf(0x0a);
      const payload = newline < 0 ? chunk : chunk.subarray(0, newline);
      received_bytes += payload.length;
      if (received_bytes > MAX_FRAME_BYTES) {
        socket.off("data", onData);
        socket.end(encodeResponse({
          protocol: PROTOCOL_VERSION,
          type: "error",
          code: "invalid_request",
          message: "request frame is too large"
        }));
        return;
      }
      chunks.push(payload);
      if (newline < 0) {
        return;
      }
      socket.off("data", onData);
      // Concatenate once so image-sized frames do not repeatedly copy all
      // preceding chunks or rescan their UTF-8 byte lengths.
      const line = Buffer.concat(chunks, received_bytes).toString("utf8").replace(/\r$/u, "");
      this.#respond(socket, line);
    };
    socket.on("data", onData);
    socket.on("error", () => {
      socket.destroy();
    });
  }

  #respond(socket: Socket, line: string): void {
    this.#requests = this.#requests.then(async () => {
      const response = await this.#handleRequest(line);
      socket.end(encodeResponse(response));
    }).catch(() => socket.destroy());
  }

  async #handleRequest(line: string): Promise<PiInputBridgeResponse> {
    let value: unknown;
    try {
      value = JSON.parse(line);
    } catch {
      return errorResponse("invalid_request", "request is not valid JSON");
    }

    if (!isRecord(value) || value.protocol !== PROTOCOL_VERSION || typeof value.type !== "string") {
      return errorResponse("invalid_request", "request has an unsupported protocol");
    }

    const requestId = asOptionalString(value.request_id);
    const token = asOptionalString(value.token);
    const sessionId = asOptionalString(value.session_id);
    const sessionFile = asOptionalString(value.session_file);
    const instanceId = asOptionalString(value.instance_id);
    const descriptor = this.#descriptor;
    if (!descriptor || !token || token !== descriptor.token) {
      return errorResponse("unauthorized", "bridge capability token is invalid", requestId);
    }
    if (!sessionId || !sessionFile || sessionId !== descriptor.session_id || resolve(sessionFile) !== descriptor.session_file) {
      return errorResponse("session_mismatch", "request does not target the active Pi session", requestId);
    }
    if (!instanceId || instanceId !== descriptor.instance_id) {
      return errorResponse("instance_mismatch", "request targets a stale Pi process instance", requestId);
    }
    const currentFile = this.#context.sessionManager.getSessionFile();
    if (this.#context.sessionManager.getSessionId() !== descriptor.session_id || !currentFile || resolve(currentFile) !== descriptor.session_file) {
      return errorResponse("session_mismatch", "the Pi process has switched to another session", requestId);
    }

    if (value.type === "status") {
      return {
        protocol: PROTOCOL_VERSION,
        type: "ready",
        ...(requestId ? { request_id: requestId } : {}),
        session_id: descriptor.session_id,
        session_file: descriptor.session_file,
        instance_id: descriptor.instance_id,
        state: this.#context.isIdle() ? "idle" : "busy",
        images_available: this.#context.model?.input?.includes("image") === true,
        model_controls_version: 1,
        model: this.#modelIdentity(),
        model_revision: this.#modelRevision(),
        can_list_models: typeof this.#context.modelRegistry?.getAvailable === "function",
        can_select_model: typeof this.#api.setModel === "function" && this.#idle()
      };
    }

    if ((value.type === "models" || value.type === "select_model") && requestId) {
      const fingerprint = JSON.stringify([value.type, value.model_provider, value.model_id, value.expected_model_revision]);
      const previous = this.#modelAdmissions.get(requestId);
      if (previous) return previous.fingerprint === fingerprint ? previous.response
        : errorResponse("request_conflict", "request_id was already used for another model change", requestId);
      const registry = this.#context.modelRegistry;
      if (!registry) return errorResponse("unsupported", "this Pi extension does not expose model discovery", requestId);
      const nativeModels = registry.getAvailable();
      if (nativeModels.length > 512) return errorResponse("unsupported", "model catalog exceeds 512 entries", requestId);
      const models = nativeModels.map((model): PiModelChoice | null => {
        const identity = modelIdentity(model);
        return identity ? { model: identity, supports_images: model.input?.includes("image") === true } : null;
      });
      if (models.some((model) => !model)) return errorResponse("unsupported", "model catalog contains invalid identities", requestId);
      if (value.type === "select_model") {
        if (!this.#api.setModel) return errorResponse("unsupported", "this Pi extension cannot select models", requestId);
        if (!this.#idle()) return errorResponse("unsupported", "wait for Pi and queued messages to finish before changing models", requestId);
        if (value.expected_model_revision !== this.#modelRevision()) return errorResponse("request_conflict", "the active model changed; refresh and retry", requestId);
        const selected = nativeModels.find((model) => model.provider === value.model_provider && model.id === value.model_id);
        if (!selected) return errorResponse("unsupported", "the selected model is no longer available", requestId);
        // Pi's native setter changes the session and Pi's saved default. It
        // mutates before awaiting model_select hooks; do not retry on a throw.
        try {
          if (!await this.#api.setModel(selected)) return this.#rememberModelAdmission(requestId, fingerprint, errorResponse("unsupported", "Pi could not authenticate the selected model", requestId));
        } catch (error) { return this.#rememberModelAdmission(requestId, fingerprint, errorResponse("bridge_unavailable", errorMessage(error), requestId)); }
        const currentFile = this.#context.sessionManager.getSessionFile();
        if (this.#descriptor !== descriptor || this.#context.sessionManager.getSessionId() !== descriptor.session_id
          || !currentFile || resolve(currentFile) !== descriptor.session_file) {
          return this.#rememberModelAdmission(requestId, fingerprint, errorResponse("bridge_unavailable", "Pi changed sessions before the model selection was confirmed", requestId));
        }
      }
      const response: PiInputBridgeResponse = {
        protocol: PROTOCOL_VERSION, type: value.type === "models" ? "models" : "model_selected",
        request_id: requestId, session_id: descriptor.session_id, instance_id: descriptor.instance_id,
        model: this.#modelIdentity(), model_revision: this.#modelRevision(), state: this.#idle() ? "idle" : "busy",
        model_controls_version: 1, can_list_models: true, can_select_model: typeof this.#api.setModel === "function" && this.#idle(),
        ...(value.type === "models" ? { models: models as PiModelChoice[] } : {})
      };
      return value.type === "select_model" ? this.#rememberModelAdmission(requestId, fingerprint, response) : response;
    }

    if (value.type !== "submit" || !requestId) {
      return errorResponse("invalid_request", "request type is unsupported", requestId);
    }

    const delivery = normalizeDelivery(value.delivery);
    const message = normalizeContent(value.content);
    if (!delivery) {
      return errorResponse("invalid_request", "delivery must be auto, follow_up, or steer", requestId);
    }
    if (!message) {
      return errorResponse("message_invalid", "content must contain valid text or supported image attachments", requestId);
    }

    const fingerprint = JSON.stringify({
      session_id: descriptor.session_id,
      instance_id: descriptor.instance_id,
      delivery,
      message: typeof message === "string" ? message : message.map((item) => item.type === "image" ? { type: item.type, mime_type: item.mimeType, sha256: createHash("sha256").update(Buffer.from(item.data, "base64")).digest("hex") } : item)
    });
    const previous = this.#admissions.get(requestId);
    if (previous) {
      if (previous.fingerprint !== fingerprint) {
        return errorResponse("request_conflict", "request_id was already used for different input", requestId);
      }
      return previous.response;
    }

    if (typeof message !== "string" && message.some((item) => item.type === "image") && this.#context.model?.input?.includes("image") !== true) {
      return errorResponse("unsupported", "the active Pi model does not support image input", requestId);
    }

    const idle = this.#context.isIdle();
    const deliverAs = idle
      ? undefined
      : delivery === "steer"
        ? "steer"
        : "followUp";
    try {
      this.#api.sendUserMessage(message, deliverAs ? { deliverAs } : undefined);
    } catch (error) {
      return errorResponse("bridge_unavailable", errorMessage(error), requestId);
    }

    const response: Extract<PiInputBridgeResponse, { type: "admitted" }> = {
      protocol: PROTOCOL_VERSION,
      type: "admitted",
      request_id: requestId,
      session_id: descriptor.session_id,
      instance_id: descriptor.instance_id,
      disposition: idle
        ? "started"
        : deliverAs === "steer"
          ? "queued_steer"
          : "queued_follow_up"
    };
    this.#rememberAdmission(requestId, fingerprint, response);
    return response;
  }

  #idle(): boolean { return this.#context.isIdle() && !this.#context.hasPendingMessages?.(); }

  #modelIdentity(): PiModelIdentity | null { return modelIdentity(this.#context.model); }

  #modelRevision(): string {
    return createHash("sha256").update(JSON.stringify(this.#modelIdentity())).digest("hex");
  }

  #rememberModelAdmission(request_id: string, fingerprint: string, response: PiInputBridgeResponse): PiInputBridgeResponse {
    this.#modelAdmissions.set(request_id, { fingerprint, response });
    if (this.#modelAdmissions.size > MAX_ADMISSION_CACHE_SIZE) {
      const oldest = this.#modelAdmissions.keys().next().value;
      if (oldest) this.#modelAdmissions.delete(oldest);
    }
    return response;
  }

  #rememberAdmission(
    requestId: string,
    fingerprint: string,
    response: Extract<PiInputBridgeResponse, { type: "admitted" }>
  ): void {
    this.#admissions.set(requestId, { fingerprint, response });
    if (this.#admissions.size <= MAX_ADMISSION_CACHE_SIZE) {
      return;
    }
    const oldest = this.#admissions.keys().next().value as string | undefined;
    if (oldest) {
      this.#admissions.delete(oldest);
    }
  }
}

export default function installPiInputBridge(pi: PiExtensionApi): void {
  let bridge: PiInputBridge | undefined;

  pi.on("session_start", async (_event, context) => {
    await bridge?.stop();
    bridge = undefined;
    if (context.mode !== "tui" || !context.sessionManager.getSessionFile()) {
      return;
    }

    try {
      bridge = new PiInputBridge({ api: pi, context });
      await bridge.start();
      context.ui.setStatus("tokn-input", "input bridge ready");
    } catch (error) {
      context.ui.setStatus("tokn-input", "input bridge unavailable");
      context.ui.notify(`Pi input bridge unavailable: ${errorMessage(error)}`, "warning");
    }
  });

  pi.on("session_shutdown", async (_event, context) => {
    await bridge?.stop();
    bridge = undefined;
    context.ui.setStatus("tokn-input", undefined);
  });
}

function modelIdentity(model: PiNativeModel | undefined): PiModelIdentity | null {
  const valid = (value: string | undefined): value is string => typeof value === "string" && value.length > 0
    && Buffer.byteLength(value) <= 512 && value.trim() === value && !/[\u0000-\u001f\u007f-\u009f]/u.test(value);
  return model && valid(model.id) && valid(model.provider)
    ? { model_provider: model.provider, model_id: model.id, display_name: valid(model.name) ? model.name : null }
    : null;
}

async function prepareSocketPath(socketPath: string): Promise<void> {
  await ensurePrivateDirectory(dirname(socketPath));
  try {
    const info = await lstat(socketPath);
    if (!info.isSocket()) {
      throw new Error(`bridge socket path is occupied: ${socketPath}`);
    }
  } catch (error) {
    if (isNodeError(error, "ENOENT")) {
      return;
    }
    throw error;
  }

  const live = await probeSocket(socketPath);
  if (live) {
    throw new Error("another Pi input bridge is already active for this session");
  }
  await unlink(socketPath).catch((error: unknown) => {
    if (!isNodeError(error, "ENOENT")) {
      throw error;
    }
  });
}

async function ensurePrivateDirectory(path: string): Promise<void> {
  await mkdir(path, { recursive: true, mode: 0o700 });
  const info = await lstat(path);
  if (!info.isDirectory()) {
    throw new Error(`bridge runtime path is not a directory: ${path}`);
  }
  await chmod(path, 0o700);
}

async function prepareDescriptorPath(descriptorPath: string): Promise<void> {
  let existing: Partial<PiInputBridgeDescriptor>;
  try {
    existing = JSON.parse(await readFile(descriptorPath, "utf8")) as Partial<PiInputBridgeDescriptor>;
  } catch (error) {
    if (isNodeError(error, "ENOENT")) {
      return;
    }
    await unlink(descriptorPath);
    return;
  }

  if (typeof existing.socket_path === "string" && await probeSocket(existing.socket_path)) {
    throw new Error("another Pi input bridge is already active for this session");
  }
  await unlink(descriptorPath).catch((error: unknown) => {
    if (!isNodeError(error, "ENOENT")) {
      throw error;
    }
  });
}

function probeSocket(socketPath: string): Promise<boolean> {
  return new Promise((resolveProbe, rejectProbe) => {
    const socket = createConnection(socketPath);
    let settled = false;
    const finish = (result: boolean): void => {
      if (settled) {
        return;
      }
      settled = true;
      socket.removeAllListeners();
      socket.destroy();
      resolveProbe(result);
    };
    socket.once("connect", () => finish(true));
    socket.once("error", (error: unknown) => {
      if (isNodeError(error, "ENOENT") || isNodeError(error, "ECONNREFUSED")) {
        finish(false);
        return;
      }
      if (settled) {
        return;
      }
      settled = true;
      socket.removeAllListeners();
      socket.destroy();
      rejectProbe(error);
    });
  });
}

function listen(server: Server, socketPath: string): Promise<void> {
  return new Promise((resolveListen, rejectListen) => {
    const onError = (error: Error): void => {
      server.off("listening", onListening);
      rejectListen(error);
    };
    const onListening = (): void => {
      server.off("error", onError);
      resolveListen();
    };
    server.once("error", onError);
    server.once("listening", onListening);
    server.listen(socketPath);
  });
}

async function closeServer(server: Server, socketPath: string): Promise<void> {
  await new Promise<void>((resolveClose, rejectClose) => {
    server.close((error) => {
      if (error) {
        rejectClose(error);
        return;
      }
      resolveClose();
    });
  }).catch((error: unknown) => {
    if (!isNodeError(error, "ERR_SERVER_NOT_RUNNING")) {
      throw error;
    }
  });
  await unlink(socketPath).catch((error: unknown) => {
    if (!isNodeError(error, "ENOENT")) {
      throw error;
    }
  });
}

async function claimDescriptor(path: string, descriptor: PiInputBridgeDescriptor): Promise<void> {
  const handle = await open(path, "wx", 0o600);
  try {
    await handle.writeFile(`${JSON.stringify(descriptor)}\n`);
    await handle.chmod(0o600);
  } catch (error) {
    await unlink(path).catch(() => {});
    throw error;
  } finally {
    await handle.close();
  }
}

async function removeDescriptor(path: string, descriptor: PiInputBridgeDescriptor): Promise<void> {
  try {
    const current = JSON.parse(await readFile(path, "utf8")) as Partial<PiInputBridgeDescriptor>;
    if (current.token !== descriptor.token || current.socket_path !== descriptor.socket_path) {
      return;
    }
  } catch (error) {
    if (isNodeError(error, "ENOENT")) {
      return;
    }
    return;
  }
  await unlink(path).catch((error: unknown) => {
    if (!isNodeError(error, "ENOENT")) {
      throw error;
    }
  });
}

function normalizeDelivery(value: unknown): PiInputDelivery | undefined {
  return value === "auto" || value === "follow_up" || value === "steer"
    ? value
    : undefined;
}

function normalizeContent(value: unknown): string | PiInputContent[] | undefined {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_IMAGES + 1) {
    return undefined;
  }
  const content: PiInputContent[] = [];
  let text_count = 0;
  let image_count = 0;
  let total_image_bytes = 0;
  for (const item of value) {
    if (!isRecord(item)) {
      return undefined;
    }
    if (item.type === "text") {
      if (typeof item.text !== "string" || ++text_count > 1
        || item.text.trim().length === 0 || Array.from(item.text).length > MAX_MESSAGE_LENGTH
        || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(item.text)) {
        return undefined;
      }
      content.push({ type: "text", text: item.text });
    } else if (item.type === "image") {
      if (++image_count > MAX_IMAGES || typeof item.data !== "string" || typeof item.mimeType !== "string"
        || !IMAGE_MIME_TYPES.has(item.mimeType) || item.data.length > Math.ceil(MAX_IMAGE_BYTES / 3) * 4
        || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/u.test(item.data)) {
        return undefined;
      }
      const bytes = Buffer.from(item.data, "base64");
      total_image_bytes += bytes.length;
      if (bytes.length === 0 || bytes.length > MAX_IMAGE_BYTES || total_image_bytes > MAX_TOTAL_IMAGE_BYTES
        || bytes.toString("base64") !== item.data) {
        return undefined;
      }
      content.push({ type: "image", data: item.data, mimeType: item.mimeType });
    } else {
      return undefined;
    }
  }
  // Preserve the established text-only extension API and wire behavior.
  return image_count === 0 ? (content[0] as { type: "text"; text: string }).text : content;
}

function errorResponse(
  code: Extract<PiInputBridgeResponse, { type: "error" }>["code"],
  message: string,
  requestId?: string
): PiInputBridgeResponse {
  return {
    protocol: PROTOCOL_VERSION,
    type: "error",
    ...(requestId ? { request_id: requestId } : {}),
    code,
    message
  };
}

function encodeResponse(response: PiInputBridgeResponse): string {
  return `${JSON.stringify(response)}\n`;
}

function asOptionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNodeError(value: unknown, code: string): value is NodeJS.ErrnoException {
  return typeof value === "object" && value !== null && "code" in value && value.code === code;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

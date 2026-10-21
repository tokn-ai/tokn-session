import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";

/** Wire contract shared by the Pi extension and local input clients. */
export const PI_INPUT_PROTOCOL_VERSION = 1;
export const PI_INPUT_PROVIDER = "pi";

const RUNTIME_DIRECTORY = `tokn-session-input-${process.getuid?.() ?? "user"}`;

export interface PiInputBridgeDescriptor {
  protocol: typeof PI_INPUT_PROTOCOL_VERSION;
  provider: typeof PI_INPUT_PROVIDER;
  transport: "unix";
  session_id: string;
  session_file: string;
  instance_id: string;
  socket_path: string;
  pid: number;
  token: string;
}

export type PiInputDelivery = "auto" | "follow_up" | "steer";

export interface PiInputTextContent {
  type: "text";
  text: string;
}

export interface PiInputImageContent {
  type: "image";
  data: string;
  mimeType: string;
}

export type PiInputContent = PiInputTextContent | PiInputImageContent;

export interface PiModelIdentity {
  model_provider: string | null;
  model_id: string;
  display_name: string | null;
}

export interface PiModelChoice { model: PiModelIdentity; supports_images: boolean }

export type PiInputBridgeRequest =
  | { protocol: typeof PI_INPUT_PROTOCOL_VERSION; type: "models" | "select_model"; request_id: string;
      token: string; session_id: string; session_file: string; instance_id: string;
      model_provider?: string; model_id?: string; expected_model_revision?: string }
  | {
      protocol: typeof PI_INPUT_PROTOCOL_VERSION;
      type: "status";
      request_id?: string;
      token: string;
      session_id: string;
      session_file: string;
      instance_id: string;
    }
  | {
      protocol: typeof PI_INPUT_PROTOCOL_VERSION;
      type: "submit";
      request_id: string;
      token: string;
      session_id: string;
      session_file: string;
      instance_id: string;
      delivery: PiInputDelivery;
      content: PiInputContent[];
    };

export type PiInputBridgeResponse =
  | {
      protocol: typeof PI_INPUT_PROTOCOL_VERSION;
      type: "ready";
      request_id?: string;
      session_id: string;
      session_file: string;
      instance_id: string;
      state: "idle" | "busy";
      images_available?: boolean;
      model_controls_version?: 1;
      model?: PiModelIdentity | null;
      model_revision?: string;
      can_list_models?: boolean;
      can_select_model?: boolean;
    }
  | { protocol: typeof PI_INPUT_PROTOCOL_VERSION; type: "models" | "model_selected"; request_id: string;
      session_id: string; instance_id: string; model: PiModelIdentity | null; model_revision: string;
      state: "idle" | "busy"; models?: PiModelChoice[];
      model_controls_version: 1; can_list_models: boolean; can_select_model: boolean }
  | {
      protocol: typeof PI_INPUT_PROTOCOL_VERSION;
      type: "admitted";
      request_id: string;
      session_id: string;
      instance_id: string;
      disposition: "started" | "queued_follow_up" | "queued_steer";
    }
  | {
      protocol: typeof PI_INPUT_PROTOCOL_VERSION;
      type: "error";
      request_id?: string;
      code:
        | "bridge_unavailable"
        | "instance_mismatch"
        | "invalid_request"
        | "message_invalid"
        | "request_conflict"
        | "session_mismatch"
        | "unauthorized"
        | "unsupported";
      message: string;
    };

export function descriptorPathForSession(sessionFile: string): string {
  const digest = createHash("sha256").update(resolve(sessionFile)).digest("hex");
  return join(runtimeDirectory(), PI_INPUT_PROVIDER, "sessions", `${digest}.json`);
}

export function socketPathForProcess(pid: number, instanceId: string): string {
  const user = process.getuid?.() ?? "user";
  const filename = `${pid}-${instanceId.slice(0, 16)}.sock`;
  const preferred = join(tmpdir(), `tsi-${user}`, "p", filename);
  return Buffer.byteLength(preferred) < 104
    ? preferred
    : join("/tmp", `tsi-${user}`, "p", filename);
}

function runtimeDirectory(): string {
  const configured = process.env.XDG_RUNTIME_DIR;
  if (configured && isAbsolute(configured)) {
    return join(configured, "tokn-session", "input");
  }
  return join(tmpdir(), RUNTIME_DIRECTORY);
}

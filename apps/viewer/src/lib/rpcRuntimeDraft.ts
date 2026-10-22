import type { RpcProvider, RpcRuntimeMode, RpcRuntimeProfile, RpcRuntimeProfileInput } from "./types";

export interface RpcRuntimeDraft {
  profile_id: string | null;
  name: string;
  provider: RpcProvider;
  mode: RpcRuntimeMode;
  executable: string;
  arguments: string;
  working_directory: string;
  endpoint: string;
  environment_action: "keep" | "replace" | "clear";
  environment: string;
  credential_action: "keep" | "none" | "bearer" | "basic";
  token: string;
  username: string;
  password: string;
  is_default: boolean;
}
export const RPC_PROVIDER_LABELS: Record<RpcProvider, string> = { codex: "Codex", pi: "Pi", open_code: "OpenCode", dsh: "DSH" };
export function rpcRuntimeDraft(profile?: RpcRuntimeProfile): RpcRuntimeDraft {
  return {
    profile_id: profile?.profile_id ?? null, name: profile?.name ?? "", provider: profile?.provider ?? "codex",
    mode: profile?.mode ?? "managed", executable: profile?.executable ?? "", arguments: JSON.stringify(profile?.arguments ?? [], null, 2),
    working_directory: profile?.working_directory ?? "", endpoint: profile?.endpoint ?? "",
    environment_action: "keep", environment: "", credential_action: "keep", token: "", username: "", password: "", is_default: profile?.is_default ?? false,
  };
}
export function rpcRuntimeInput(draft: RpcRuntimeDraft): RpcRuntimeProfileInput {
  let args: string[] = [];
  if (draft.mode === "managed" && (draft.provider === "codex" || draft.provider === "pi")) {
    let parsed: unknown;
    try { parsed = JSON.parse(draft.arguments); } catch { throw new Error("Arguments must be a JSON array of strings."); }
    if (!Array.isArray(parsed) || parsed.some((arg) => typeof arg !== "string")) throw new Error("Arguments must be a JSON array of strings.");
    args = parsed;
  }
  let environment: Record<string, string> | null = null;
  if (draft.mode === "managed" && draft.environment_action === "clear") environment = {};
  if (draft.mode === "managed" && draft.environment_action === "replace") {
    let parsed: unknown;
    try { parsed = JSON.parse(draft.environment); } catch { throw new Error("Environment must be a JSON object with string values."); }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed) || Object.values(parsed).some((value) => typeof value !== "string")) {
      throw new Error("Environment must be a JSON object with string values.");
    }
    environment = parsed as Record<string, string>;
  }
  return {
    profile_id: draft.profile_id, name: draft.name.trim(), provider: draft.provider, mode: draft.mode,
    executable: draft.mode === "managed" ? draft.executable.trim() || null : null,
    arguments: draft.mode === "managed" ? args : [],
    working_directory: draft.mode === "managed" || draft.provider === "open_code" ? draft.working_directory.trim() || null : null,
    endpoint: draft.mode === "attached" ? draft.endpoint.trim() || null : null,
    environment: draft.mode === "managed" ? environment : {},
    credentials: draft.mode === "managed" ? { type: "none" } : draft.credential_action === "keep" ? null
      : draft.credential_action === "none" ? { type: "none" }
      : draft.credential_action === "bearer" ? { type: "bearer", token: draft.token }
      : { type: "basic", username: draft.username, password: draft.password },
    is_default: draft.is_default,
  };
}

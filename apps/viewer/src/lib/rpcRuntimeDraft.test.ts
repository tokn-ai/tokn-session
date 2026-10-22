import { expect, it } from "vitest";
import { rpcRuntimeDraft, rpcRuntimeInput } from "./rpcRuntimeDraft";
it("clears incompatible secret configuration when changing runtime modes", () => {
  const draft = rpcRuntimeDraft();
  expect(rpcRuntimeInput({ ...draft, mode: "attached", endpoint: "ws://localhost:9876", arguments: "invalid unused JSON", environment_action: "replace", environment: "invalid unused JSON" }))
    .toMatchObject({ executable: null, arguments: [], environment: {}, credentials: null });
  expect(rpcRuntimeInput({ ...draft, credential_action: "bearer", token: "old endpoint secret" }))
    .toMatchObject({ endpoint: null, credentials: { type: "none" }, environment: null });
});
it("rejects malformed fields and non-string values before a host command", () => {
  const draft = rpcRuntimeDraft();
  expect(() => rpcRuntimeInput({ ...draft, arguments: "{}" })).toThrow("Arguments must be a JSON array of strings.");
  expect(() => rpcRuntimeInput({ ...draft, arguments: "[1]" })).toThrow("Arguments must be a JSON array of strings.");
  expect(() => rpcRuntimeInput({ ...draft, environment_action: "replace", environment: '{"SECRET":42}' }))
    .toThrow("Environment must be a JSON object with string values.");
});
it("retains the required project directory only for runtimes that use it", () => {
  const draft = { ...rpcRuntimeDraft(), working_directory: " /project/path " };
  expect(rpcRuntimeInput(draft).working_directory).toBe("/project/path");
  expect(rpcRuntimeInput({ ...draft, mode: "attached", provider: "open_code" }).working_directory).toBe("/project/path");
  expect(rpcRuntimeInput({ ...draft, mode: "attached", provider: "codex" }).working_directory).toBeNull();
  expect(rpcRuntimeInput({ ...draft, mode: "attached", provider: "dsh" }).working_directory).toBeNull();
});

import { useCallback, useEffect, useState, type ReactNode, type RefObject } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SessionDraftsProvider } from "../components/SessionDraftsProvider";
import { ViewerPage } from "./ViewerPage";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../lib/transport", async (original) => {
  const core = await import("@tauri-apps/api/core");
  const events = await import("@tauri-apps/api/event");
  return { ...await original<typeof import("../lib/transport")>(), isDesktop: () => true,
    captureTransport: () => ({ invoke: core.invoke, listen: events.listen, on_close: () => () => {} }) };
});
vi.mock("../components/TranslationProvider", () => ({ TranslationProvider: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/QuestionAnswersProvider", () => ({ QuestionAnswersProvider: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/SessionDrawer", () => ({ SessionDrawer: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/Inspector", () => ({ Inspector: () => null }));
vi.mock("../components/StatusBar", () => ({ StatusBar: ({ settings_trigger_ref }: { settings_trigger_ref: RefObject<HTMLButtonElement | null> }) =>
  <button ref={settings_trigger_ref}>Settings</button> }));
vi.mock("../components/RelayConnection", () => ({ RelayConnection: () => null }));
vi.mock("../components/Conversation", () => ({ Conversation: () => null }));
vi.mock("../components/ProjectWorkspace", () => ({ RegisterProjectDialog: () => null, ManagedSessionControls: () => null, ManagedPiDialog: () => null }));
vi.mock("../components/Sidebar", () => ({ Sidebar: ({ workspace_available, on_new_session }: { workspace_available: boolean; on_new_session: () => void }) =>
  <button disabled={!workspace_available} onClick={on_new_session}>New session</button> }));
vi.mock("../components/SettingsDialog", () => ({ SettingsDialog: ({ initial_section, remote, machine_key, on_close, return_focus }: {
  initial_section: string; remote: boolean; machine_key: string; on_close: () => void; return_focus: RefObject<HTMLButtonElement | null>;
}) => {
  useEffect(() => () => { return_focus.current?.focus(); }, [return_focus]);
  return <div role="dialog" aria-label="Runtime setup" data-section={initial_section} data-remote={remote} data-machine={machine_key}>
  <button onClick={async () => { await invoke("save_rpc_runtime_profile", { request: {} }); on_close(); }}>Save profile and return</button>
</div>; } }));
vi.mock("../lib/useViewerState", () => ({ useViewerState: () => {
  const [selectedSessionKey, setSession] = useState<string | null>(null);
  return { selectedSessionKey, clearSessionSelection: useCallback(() => setSession(null), []), openCreatedSession: useCallback(() => {}, []) };
} }));

let configured = false;
beforeEach(() => {
  configured = false;
  vi.mocked(listen).mockResolvedValue(() => {});
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "list_registered_projects") return { available: true, unavailable_reason: null, projects: [
      { project_id: "project", group_key: "/repo", folder_path: "/repo", name: "Repo", created_at_ms: 1, available: true },
    ] };
    if (command === "get_managed_session") return null;
    if (command === "get_session_creation_options") return { project_id: "project", providers: [
      { provider: "pi", available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
      { provider: "open_code", available: configured, reason: configured ? null : "Configure an OpenCode runtime for this project folder.", supports_images: true,
        default_profile_id: configured ? "local" : null, runtimes: configured ? [{ profile_id: "local", revision: 1, name: "Local OpenCode", available: true, reason: null }] : [] },
    ] };
    if (command === "save_rpc_runtime_profile") { configured = true; return {}; }
    throw new Error(`Unexpected runtime setup command: ${command}`);
  });
});
afterEach(() => { cleanup(); vi.clearAllMocks(); });

it("opens local Agent runtimes from an unavailable draft and reloads options after profile save without losing text", async () => {
  render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
  await waitFor(() => expect(screen.getByRole("button", { name: "New session" })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: "New session" }));
  fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Keep this task" } });
  fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "open_code" } });
  fireEvent.click(await screen.findByRole("button", { name: "Configure agent runtime" }));
  const settings = screen.getByRole("dialog", { name: "Runtime setup" });
  expect(settings).toHaveAttribute("data-section", "agent_runtimes");
  expect(settings).toHaveAttribute("data-remote", "false");
  expect(settings).toHaveAttribute("data-machine", "local");
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  fireEvent.click(screen.getByRole("button", { name: "Save profile and return" }));
  await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
  expect(screen.queryByRole("dialog", { name: "Runtime setup" })).not.toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep this task");
  expect(screen.getByRole("combobox", { name: "Agent" })).toHaveValue("open_code");
  expect(screen.getByRole("combobox", { name: "Runtime" })).toHaveTextContent("Local OpenCode");
  expect(screen.getByRole("button", { name: "Settings" })).toHaveFocus();
  expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "get_session_creation_options")).toHaveLength(2);
  expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "create_project_session" || command === "list_session_creation_models")).toBe(false);
});

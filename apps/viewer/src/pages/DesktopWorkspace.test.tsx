import { StrictMode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { DesktopWorkspace } from "./DesktopWorkspace";
import { initializeLocalViewer } from "../lib/tauri";
import { readMachinePreferences, rememberMachine } from "../lib/machinePreferences";
import type { HubMachineSelection } from "../lib/types";
import { selectMachine } from "../lib/transport";
import { useLocalSessionDrafts } from "../components/SessionDraftsProvider";
import { listen } from "@tauri-apps/api/event";

const selection: HubMachineSelection = { kind: "hub", hub_url: "https://hub.example", host_id: "550e8400-e29b-41d4-a716-446655440000" };
vi.mock("../components/LocalHostProvider", () => ({ LocalHostProvider: ({ children }: { children: import("react").ReactNode }) => children }));

vi.mock("../lib/tauri", async (original) => ({ ...await original<typeof import("../lib/tauri")>(), initializeLocalViewer: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn().mockResolvedValue(vi.fn()) }));
vi.mock("../components/HubAccess", () => ({ HubAccess: ({ initial_hub_url, startup_host_id, on_local, on_machine_open, on_hub_ready, local_error }: {
  initial_hub_url: string; startup_host_id: string | null; on_local: () => void; local_error?: string;
  on_machine_open: (selection: HubMachineSelection) => void; on_hub_ready: (hub_url: string) => void;
}) => <main><h1>Machines</h1><p>Hub: {initial_hub_url}</p><p>Reopen: {startup_host_id ?? "none"}</p>
  {local_error && <p role="alert">{local_error}</p>}<button onClick={on_local}>Open This machine</button>
  <button onClick={() => on_machine_open(selection)}>Open remote machine</button>
  <button onClick={() => on_hub_ready("https://other.example")}>Load another Hub</button>
</main> }));
vi.mock("./ViewerPage", () => ({ ViewerPage: ({ on_open_machines }: { on_open_machines: () => void }) => {
  const drafts = useLocalSessionDrafts();
  return <><p>Local sessions opened</p><button onClick={on_open_machines}>Machines</button>
    <button onClick={() => drafts?.createDraft("project")}>New draft</button>
    {drafts?.drafts.map((draft) => <label key={draft.draft_id}>Draft message<input value={draft.prompt}
      onChange={(event) => drafts.updateDraft(draft.draft_id, { prompt: event.target.value })} /></label>)}
  </>;
} }));

beforeEach(() => { vi.mocked(initializeLocalViewer).mockResolvedValue(undefined); });
afterEach(() => { cleanup(); selectMachine(); localStorage.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); });

it("starts in Machines without initializing local sessions on a new installation", () => {
  render(<DesktopWorkspace />);
  expect(screen.getByRole("heading", { name: "Machines" })).toBeInTheDocument();
  expect(initializeLocalViewer).not.toHaveBeenCalled();
  expect(listen).not.toHaveBeenCalled();
  expect(screen.getByText("Reopen: none")).toBeInTheDocument();
});

it("reopens the exact remembered remote without initializing local sessions", () => {
  rememberMachine(readMachinePreferences(), selection);
  render(<StrictMode><DesktopWorkspace /></StrictMode>);
  expect(screen.getByText(`Hub: ${selection.hub_url}`)).toBeInTheDocument();
  expect(screen.getByText(`Reopen: ${selection.host_id}`)).toBeInTheDocument();
  expect(initializeLocalViewer).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Load another Hub" }));
  expect(readMachinePreferences().selected_machine).toEqual(selection);
});

it("opens local sessions only after initialization succeeds and remembers Local for restart", async () => {
  let finish!: () => void;
  vi.mocked(initializeLocalViewer).mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  const view = render(<DesktopWorkspace />);
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(screen.getByRole("status")).toHaveTextContent("Opening this machine");
  expect(screen.queryByText("Local sessions opened")).not.toBeInTheDocument();
  expect(listen).not.toHaveBeenCalled();
  expect(readMachinePreferences().selected_machine).toBeNull();
  finish();
  expect(await screen.findByText("Local sessions opened")).toBeInTheDocument();
  expect(readMachinePreferences().selected_machine).toEqual({ kind: "local" });
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  expect(screen.getByText("Reopen: none")).toBeInTheDocument();
  view.unmount();
  vi.mocked(initializeLocalViewer).mockResolvedValue(undefined);
  render(<DesktopWorkspace />);
  expect(await screen.findByText("Local sessions opened")).toBeInTheDocument();
  expect(initializeLocalViewer).toHaveBeenCalledTimes(2);
});

it("keeps remote access and the previous selection available after local initialization fails", async () => {
  rememberMachine(readMachinePreferences(), selection);
  vi.mocked(initializeLocalViewer).mockRejectedValueOnce(new Error("Local database cannot be opened"));
  render(<DesktopWorkspace />);
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Local database cannot be opened");
  expect(listen).not.toHaveBeenCalled();
  expect(readMachinePreferences().selected_machine).toEqual(selection);
  expect(screen.getByText("Reopen: none")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Open remote machine" }));
  expect(readMachinePreferences().selected_machine).toEqual(selection);
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(await screen.findByText("Local sessions opened")).toBeInTheDocument();
});

it("does not open or remember a canceled local initialization that finishes late", async () => {
  let finish!: () => void;
  vi.mocked(initializeLocalViewer).mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  render(<DesktopWorkspace />);
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  fireEvent.click(screen.getByRole("button", { name: "Open remote machine" }));
  finish();
  await waitFor(() => expect(readMachinePreferences().selected_machine).toEqual(selection));
  expect(screen.queryByText("Local sessions opened")).not.toBeInTheDocument();
  expect(listen).not.toHaveBeenCalled();
});

it("ignores an abandoned StrictMode local attempt even if it rejects after its successor", async () => {
  rememberMachine(readMachinePreferences(), { kind: "local" });
  let reject_stale!: (error: Error) => void;
  vi.mocked(initializeLocalViewer).mockImplementationOnce(() => new Promise((_resolve, reject) => { reject_stale = reject; }))
    .mockResolvedValueOnce(undefined);
  render(<StrictMode><DesktopWorkspace /></StrictMode>);
  expect(await screen.findByText("Local sessions opened")).toBeInTheDocument();
  reject_stale(new Error("Abandoned initialization"));
  await waitFor(() => expect(screen.queryByRole("alert")).not.toBeInTheDocument());
  expect(readMachinePreferences().selected_machine).toEqual({ kind: "local" });
});

it("keeps local drafts while visiting Machines and reconnecting to the local viewer", async () => {
  render(<DesktopWorkspace />);
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(await screen.findByText("Local sessions opened")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "New draft" }));
  fireEvent.change(screen.getByLabelText("Draft message"), { target: { value: "Continue this later" } });
  await waitFor(() => expect(listen).toHaveBeenCalledTimes(1));
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  expect(screen.queryByLabelText("Draft message")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Open remote machine" }));
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(await screen.findByLabelText("Draft message")).toHaveValue("Continue this later");
  expect(listen).toHaveBeenCalledTimes(1);
});

import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RuntimeSettings } from "./RuntimeSettings";
import { SettingsPage, type SettingsPageProps } from "../pages/SettingsPage";
import { useViewerRuntimes } from "../lib/useViewerRuntimes";
import type { RuntimeInfo } from "../lib/types";
vi.mock("../lib/useViewerRuntimes", () => ({ useViewerRuntimes: vi.fn() }));
const row: RuntimeInfo = { runtime_id: "pi:owned", instance_id: "owned:1", name: "Pi · Hello", kind: "pi", execution_kind: "subprocess", ownership: "viewer",
  status: "running", pid: 4321, started_at_ms: null, session_key: "saved", session_id: "session", project_id: "project", working_directory: "/project/aa",
  endpoint: null, detail: "Working on the selected project", last_error: null, can_restart: true, restart_disabled_reason: null };
function state(overrides: Partial<ReturnType<typeof useViewerRuntimes>> = {}): ReturnType<typeof useViewerRuntimes> {
  return { desktop: true, inventory: { version: 1, runtimes: [row] }, loading: false, error: null, live_error: null,
    actions: new Map(), refresh: vi.fn(async () => {}), restart: vi.fn(async () => {}), ...overrides };
}
beforeEach(() => { vi.resetAllMocks(); vi.mocked(useViewerRuntimes).mockReturnValue(state()); });
afterEach(cleanup);

it("keeps metadata in Details and offers only supported viewer restart actions", () => {
  const current = state({ inventory: { version: 1, runtimes: [row,
    { ...row, runtime_id: "reader", name: "Session reader", kind: "reader", execution_kind: "embedded", pid: null, session_key: null, detail: null, can_restart: false, restart_disabled_reason: "Part of the viewer runtime" },
    { ...row, runtime_id: "external", name: "External Relay", ownership: "external", execution_kind: "external", can_restart: true, restart_disabled_reason: "Manage this service outside the viewer" },
  ] } });
  vi.mocked(useViewerRuntimes).mockReturnValue(current);
  render(<RuntimeSettings active />);
  expect(screen.getByText("Viewer-managed · Subprocess")).not.toBeVisible();
  expect(screen.getByText("Viewer-managed · In-process")).not.toBeVisible();
  expect(screen.getByText("Externally managed · External connection")).not.toBeVisible();
  expect(screen.getByText("Restart interrupts the current turn.")).toBeVisible();
  expect(screen.getByText("External", { exact: true })).toBeVisible();
  const pi = within(screen.getByRole("article", { name: "Pi · Hello" }));
  expect(pi.getByText("Working on the selected project")).not.toBeVisible();
  expect(pi.getByText("/project/aa")).not.toBeVisible();
  expect(pi.getByText("4321")).not.toBeVisible();
  fireEvent.click(pi.getByText("Details"));
  expect(pi.getByText("/project/aa")).toBeVisible();
  expect(pi.getByText("4321")).toBeVisible();
  expect(pi.getByText("Viewer-managed · Subprocess")).toBeVisible();
  expect(screen.queryByRole("button", { name: "Restart Session reader" })).not.toBeInTheDocument();
  expect(screen.getByText("Part of the viewer runtime")).not.toBeVisible();
  expect(screen.queryByRole("button", { name: "Restart External Relay" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Restart Pi · Hello" }));
  expect(current.restart).toHaveBeenCalledExactlyOnceWith(row);
});

it("keeps saved Pi sessions distinct from active processes and scopes progress/errors to one row", () => {
  vi.mocked(useViewerRuntimes).mockReturnValue(state({ inventory: { version: 1, runtimes: [
    { ...row, status: "stopped", pid: null, last_error: "Last provider request failed" },
    { ...row, runtime_id: "pi:other", name: "Pi · Other" },
  ] }, actions: new Map([[row.runtime_id, { busy: true, error: null, message: null }]]) }));
  const view = render(<RuntimeSettings active />);
  expect(screen.getByText("Saved")).toBeVisible();
  expect(screen.getByRole("button", { name: "Restarting Pi · Hello" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Restart Pi · Other" })).toBeEnabled();
  expect(screen.getByText(/Last provider request failed/)).toBeInTheDocument();
  vi.mocked(useViewerRuntimes).mockReturnValue(state({ actions: new Map([[row.runtime_id, { busy: false, error: "Runtime instance changed", message: null }]]) }));
  view.rerender(<RuntimeSettings active />);
  expect(screen.getByRole("alert")).toHaveTextContent("Runtime instance changed");
});

it("retains stale rows during refresh failures and offers an explicit retry", () => {
  const current = state({ error: "Connection to runtime manager failed" });
  vi.mocked(useViewerRuntimes).mockReturnValue(current);
  render(<RuntimeSettings active />);
  expect(screen.getByText("Pi · Hello")).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("Status may be outdated");
  fireEvent.click(screen.getByRole("button", { name: "Retry runtime inventory" }));
  expect(current.refresh).toHaveBeenCalledOnce();
});

it("renders loading, empty, and browser-unavailable states", () => {
  vi.mocked(useViewerRuntimes).mockReturnValue(state({ inventory: null, loading: true }));
  const view = render(<RuntimeSettings active />);
  expect(screen.getByRole("status")).toHaveTextContent("Loading runtimes…");
  expect(screen.getByRole("button", { name: "Refresh runtimes" })).toBeDisabled();
  vi.mocked(useViewerRuntimes).mockReturnValue(state({ inventory: { version: 1, runtimes: [] } }));
  view.rerender(<RuntimeSettings active />);
  expect(screen.getByText("No runtimes are known to this viewer.")).toBeInTheDocument();
  vi.mocked(useViewerRuntimes).mockReturnValue(state({ desktop: false, inventory: null }));
  view.rerender(<RuntimeSettings active />);
  expect(screen.getByText("Runtime management is available in this computer’s desktop app.")).toBeInTheDocument();
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
});

it("scopes Runtimes to this viewer while a remote machine or no machine is selected", () => {
  const props: SettingsPageProps = { on_close: vi.fn(), machine_label: "Remote machine", machine_key: "remote", machine_connected: false,
    preferences: { version: 1, sidebar_order: "time", show_lifecycle: false, background_updates_scope: "all", background_update_level: "messages", conversation_update_level: "steps" },
    on_preferences_change: vi.fn(), on_preferences_reset: vi.fn(), settings: null, settings_loading: false, settings_error: null,
    on_settings_retry: vi.fn(), on_reader_apply: vi.fn(async () => {}), hosting: null };
  render(<SettingsPage {...props} />);
  expect(useViewerRuntimes).toHaveBeenLastCalledWith(false);
  fireEvent.click(screen.getByRole("button", { name: /^Runtimes/ }));
  expect(useViewerRuntimes).toHaveBeenLastCalledWith(true);
  const section = screen.getByRole("region", { name: "Runtimes" });
  expect(within(section).getByText("This viewer")).toBeVisible();
  expect(within(section).getByText("Pi · Hello")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /^Viewer preferences/ }));
  expect(useViewerRuntimes).toHaveBeenLastCalledWith(false);
});

import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MachineSettingsSnapshot, ViewerPreferences } from "../lib/types";
import { SettingsPage, type SettingsPageProps } from "./SettingsPage";

afterEach(cleanup);

const preferences: ViewerPreferences = {
  version: 1,
  sidebar_order: "time",
  show_lifecycle: false,
  background_updates_scope: "all",
  background_update_level: "messages",
  conversation_update_level: "steps",
};

const snapshot: MachineSettingsSnapshot = {
  version: 1,
  reader: {
    status: { settings: { mode: "automatic", endpoint: "tcp://127.0.0.1:5557", include_native: false }, active_endpoint: null, phase: "live", native: false, error: null },
    editable: true,
    edit_reason: null,
  },
  sources: [
    { provider: "codex", locations: [{ path: "/home/host/.codex/sessions", kind: "directory", exists: true }, { path: "/home/host/.codex/archived_sessions", kind: "directory", exists: false }], error: null },
    { provider: "opencode", locations: [{ path: "/home/host/.local/share/opencode/opencode.db", kind: "database", exists: true }], error: null },
  ],
  sources_editable: false,
  sources_edit_reason: "Locations are detected from this machine’s environment.",
  hosting: { available: false, editable: false, reason: "Remote hosting configuration is unavailable." },
};

function props(overrides: Partial<SettingsPageProps> = {}): SettingsPageProps {
  return {
    on_close: vi.fn(), machine_label: "Dev machine", machine_key: "local", preferences,
    on_preferences_change: vi.fn(), on_preferences_reset: vi.fn(), settings: snapshot,
    settings_loading: false, settings_error: null, on_settings_retry: vi.fn(),
    on_reader_apply: vi.fn().mockResolvedValue(undefined), hosting: null, ...overrides,
  };
}

function openSources() { fireEvent.click(screen.getByRole("button", { name: /^Sources & reader/ })); }
function openHosting() { fireEvent.click(screen.getByRole("button", { name: /^Hosting & access/ })); }
function openPreferences() { fireEvent.click(screen.getByRole("button", { name: /^Viewer preferences/ })); }

it("opens directly at Agent runtimes for draft configuration recovery", () => {
  render(<SettingsPage {...props({ initial_section: "agent_runtimes", machine_connected: false })} />);
  expect(screen.getByRole("button", { name: /^Agent runtimes/ })).toHaveAttribute("aria-current", "page");
  expect(screen.getByRole("heading", { name: "Agent runtimes" })).toBeVisible();
  expect(screen.getByText("Connect to a machine to manage its agent runtimes.")).toBeVisible();
});

describe("SettingsPage", () => {
  it("applies only device preferences immediately and keeps close and reset explicit", () => {
    const change = vi.fn(); const reset = vi.fn(); const close = vi.fn(); const apply = vi.fn();
    function Harness() {
      const [current, setCurrent] = useState(preferences);
      return <SettingsPage {...props({ preferences: current, on_close: close, on_preferences_reset: reset, on_reader_apply: apply, on_preferences_change: (partial) => { change(partial); setCurrent((before) => ({ ...before, ...partial })); } })} />;
    }
    render(<Harness />);
    expect(screen.getByRole("heading", { name: "Viewer preferences" })).toBeInTheDocument();
    expect(screen.getByText("This device", { selector: ".settings-page__scope" })).toBeInTheDocument();
    fireEvent.change(screen.getByRole("combobox", { name: "Sidebar grouping" }), { target: { value: "project" } });
    fireEvent.click(screen.getByRole("checkbox", { name: "Lifecycle events" }));
    fireEvent.change(screen.getByRole("combobox", { name: "Background sessions" }), { target: { value: "sidebar" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Background delivery" }), { target: { value: "final" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Conversation delivery" }), { target: { value: "all" } });
    expect(change.mock.calls.map(([partial]) => partial)).toEqual([{ sidebar_order: "project" }, { show_lifecycle: true }, { background_updates_scope: "sidebar" }, { background_update_level: "final" }, { conversation_update_level: "all" }]);
    expect(screen.getByRole("combobox", { name: "Background delivery" })).toHaveAccessibleDescription("Final assistant replies and attention updates.");
    expect(apply).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Reset viewer preferences" }));
    expect(reset).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Close settings" }));
    expect(close).toHaveBeenCalledOnce();
  });

  it("identifies the selected machine and exposes source paths only as detected records", () => {
    render(<SettingsPage {...props({ settings: { ...snapshot, reader: { ...snapshot.reader, editable: false, edit_reason: "Remote reader configuration is read-only." } } })} />);
    openSources();
    expect(screen.getByRole("heading", { name: "Sources & reader" })).toBeInTheDocument();
    expect(within(screen.getByRole("region", { name: "Sources & reader" })).getByText("Dev machine", { selector: ".settings-page__scope" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toBeDisabled();
    expect(screen.getByRole("checkbox", { name: "Native inspection records" })).toBeDisabled();
    expect(screen.getByText("Remote reader configuration is read-only.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply reader settings" })).not.toBeInTheDocument();
    expect(screen.getByText("/home/host/.codex/sessions").tagName).toBe("CODE");
    expect(screen.getByText("Not found")).toBeInTheDocument();
    expect(screen.getByText("Database")).toBeInTheDocument();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    openHosting();
    expect(screen.getByText("Remote hosting configuration is unavailable.")).toBeInTheDocument();
  });

  it("retains dirty reader drafts across live snapshots and section changes, then discards explicitly", () => {
    const current = props();
    const view = render(<SettingsPage {...current} />);
    openSources();
    fireEvent.change(screen.getByRole("combobox", { name: "Reader mode" }), { target: { value: "external" } });
    fireEvent.change(screen.getByRole("textbox", { name: "Relay endpoint" }), { target: { value: "tcp://127.0.0.1:9000" } });
    expect(current.on_reader_apply).not.toHaveBeenCalled();
    const updated: MachineSettingsSnapshot = { ...snapshot, reader: { ...snapshot.reader, status: { ...snapshot.reader.status, settings: { ...snapshot.reader.status.settings, mode: "local" }, phase: "reconnecting" } } };
    view.rerender(<SettingsPage {...current} settings={updated} />);
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toHaveValue("external");
    expect(screen.getByRole("textbox", { name: "Relay endpoint" })).toHaveValue("tcp://127.0.0.1:9000");
    openPreferences(); openSources();
    expect(screen.getByRole("textbox", { name: "Relay endpoint" })).toHaveValue("tcp://127.0.0.1:9000");
    view.rerender(<SettingsPage {...current} settings={updated} settings_error="Status refresh failed." />);
    expect(screen.getByRole("textbox", { name: "Relay endpoint" })).toHaveValue("tcp://127.0.0.1:9000");
    fireEvent.click(screen.getByRole("button", { name: "Discard changes" }));
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toHaveValue("local");
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeDisabled();
  });

  it("validates an external endpoint and retains the draft after a rejected Apply", async () => {
    const apply = vi.fn().mockRejectedValue(new Error("The Relay endpoint is not supported."));
    render(<SettingsPage {...props({ on_reader_apply: apply })} />);
    openSources();
    fireEvent.change(screen.getByRole("combobox", { name: "Reader mode" }), { target: { value: "external" } });
    const endpoint = screen.getByRole("textbox", { name: "Relay endpoint" });
    fireEvent.change(endpoint, { target: { value: "   " } });
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeDisabled();
    fireEvent.change(endpoint, { target: { value: "  ipc:///tmp/relay.ipc  " } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Apply reader settings" })));
    expect(apply).toHaveBeenCalledExactlyOnceWith({ mode: "external", endpoint: "ipc:///tmp/relay.ipc", include_native: false });
    expect(screen.getByRole("alert")).toHaveTextContent("The Relay endpoint is not supported.");
    expect(endpoint).toHaveValue("  ipc:///tmp/relay.ipc  ");
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeEnabled();
  });

  it("follows new saved settings after edits are undone or confirmed by a live snapshot", () => {
    const current = props();
    const view = render(<SettingsPage {...current} />);
    openSources();
    fireEvent.change(screen.getByRole("combobox", { name: "Reader mode" }), { target: { value: "external" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Reader mode" }), { target: { value: "automatic" } });
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeDisabled();
    const external: MachineSettingsSnapshot = { ...snapshot, reader: { ...snapshot.reader, status: { ...snapshot.reader.status, settings: { mode: "external", endpoint: "tcp://127.0.0.1:9000", include_native: false } } } };
    view.rerender(<SettingsPage {...current} settings={external} />);
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toHaveValue("external");
    expect(screen.getByRole("textbox", { name: "Relay endpoint" })).toHaveValue("tcp://127.0.0.1:9000");
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeDisabled();
    fireEvent.change(screen.getByRole("textbox", { name: "Relay endpoint" }), { target: { value: "tcp://127.0.0.1:9001" } });
    const confirmed: MachineSettingsSnapshot = { ...external, reader: { ...external.reader, status: { ...external.reader.status, settings: { ...external.reader.status.settings, endpoint: "tcp://127.0.0.1:9001" } } } };
    view.rerender(<SettingsPage {...current} settings={confirmed} />);
    expect(screen.getByRole("button", { name: "Apply reader settings" })).toBeDisabled();
    view.rerender(<SettingsPage {...current} settings={snapshot} />);
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toHaveValue("automatic");
    expect(screen.queryByRole("textbox", { name: "Relay endpoint" })).not.toBeInTheDocument();
    expect(current.on_reader_apply).not.toHaveBeenCalled();
  });

  it("disables pending changes and resets reader drafts when the selected machine changes", async () => {
    let resolve!: () => void;
    const apply = vi.fn(() => new Promise<void>((done) => { resolve = done; }));
    const current = props({ on_reader_apply: apply });
    const view = render(<SettingsPage {...current} />);
    openSources();
    fireEvent.click(screen.getByRole("checkbox", { name: "Native inspection records" }));
    fireEvent.click(screen.getByRole("button", { name: "Apply reader settings" }));
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Applying…" })).toBeDisabled();
    await act(async () => resolve());
    expect(screen.getByText("Reader settings applied.")).toBeInTheDocument();
    fireEvent.change(screen.getByRole("combobox", { name: "Reader mode" }), { target: { value: "external" } });
    view.rerender(<SettingsPage {...current} machine_key="hub:other" machine_label="Other machine" settings={{ ...snapshot, reader: { ...snapshot.reader, editable: false } }} />);
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toHaveValue("automatic");
    expect(screen.getByRole("combobox", { name: "Reader mode" })).toBeDisabled();
    expect(screen.queryByRole("textbox", { name: "Relay endpoint" })).not.toBeInTheDocument();
  });

  it("keeps device preferences usable while machine settings load or fail and offers retry", () => {
    const current = props({ settings: null, settings_loading: true });
    const view = render(<SettingsPage {...current} />);
    expect(screen.getByRole("combobox", { name: "Sidebar grouping" })).toBeEnabled();
    openSources();
    expect(screen.getByRole("status")).toHaveTextContent("Loading machine settings…");
    view.rerender(<SettingsPage {...current} settings_loading={false} settings_error="Machine settings require owner access." />);
    expect(screen.getByRole("alert")).toHaveTextContent("Machine settings require owner access.");
    expect(screen.queryByRole("combobox", { name: "Reader mode" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry machine settings" }));
    expect(current.on_settings_retry).toHaveBeenCalledOnce();
    openPreferences();
    expect(screen.getByRole("combobox", { name: "Background sessions" })).toBeEnabled();
  });

  it("renders supplied hosting controls without loading secrets or conflating viewer reset", () => {
    const open_host = vi.fn(); const reset = vi.fn();
    render(<SettingsPage {...props({ on_preferences_reset: reset, hosting: <div><p>This computer is available while the app is open.</p><button onClick={open_host} type="button">Host this computer</button></div> })} />);
    openHosting();
    expect(screen.getByRole("heading", { name: "Hosting & access" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
    expect(open_host).toHaveBeenCalledOnce();
    expect(reset).not.toHaveBeenCalled();
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
  });
});

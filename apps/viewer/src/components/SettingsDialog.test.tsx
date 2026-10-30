import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { SettingsDialog } from "./SettingsDialog";

vi.mock("../lib/transport", () => ({ isDesktop: () => true }));
vi.mock("../lib/useMachineSettings", () => ({ useMachineSettings: () => ({ settings: null, loading: false, error: null, retry: vi.fn(), applyReader: vi.fn() }) }));
vi.mock("../lib/viewerPreferences", () => ({ useViewerPreferences: () => ({ preferences: {}, updatePreferences: vi.fn(), resetPreferences: vi.fn() }) }));
vi.mock("../pages/SettingsPage", () => ({ SettingsPage: () => <button>Settings content</button> }));
vi.mock("./MachineHostingSettings", () => ({ MachineHostingSettings: () => null }));
vi.mock("./NativePreferenceSaveNotice", () => ({ NativePreferenceSaveNotice: () => null }));
vi.mock("./ActionNotifications", () => ({ ActionNotificationHost: () => null }));

afterEach(cleanup);

it("returns focus to the replacement control when profile setup removes the original opener", () => {
  const opener = document.createElement("button");
  const fallback = document.createElement("button");
  document.body.append(opener, fallback);
  const return_focus = { current: opener };
  const view = render(<SettingsDialog remote={false} machine_label="This computer" machine_key="local"
    on_close={() => {}} return_focus={return_focus} initial_section="agent_runtimes" />);
  opener.remove();
  return_focus.current = fallback;
  view.unmount();
  expect(fallback).toHaveFocus();
  fallback.remove();
});

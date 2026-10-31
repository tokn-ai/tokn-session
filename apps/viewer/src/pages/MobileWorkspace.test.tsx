import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { MobileWorkspace } from "./MobileWorkspace";
import { readMachinePreferences, rememberMachine } from "../lib/machinePreferences";
import type { HubAccessProps } from "../components/HubAccess";

vi.mock("../components/NativeDevicePreferencesProvider", () => ({
  NativeDevicePreferencesProvider: ({ children }: { children: import("react").ReactNode }) => children,
}));
const remote = { kind: "hub" as const, hub_url: "https://hub.example", host_id: "550e8400-e29b-41d4-a716-446655440000" };
vi.mock("../components/HubAccess", () => ({ HubAccess: (props: HubAccessProps) => <>
  <p>Hub: {props.initial_hub_url}</p><p>Reopen: {props.startup_host_id ?? "none"}</p>
  {props.on_local && <button>Local</button>}
  <button onClick={() => props.on_machine_open?.(remote)}>Open remote</button>
</> }));
afterEach(() => { cleanup(); localStorage.clear(); });

it("ignores a local selection and remembers the remote machine without remounting the picker", () => {
  rememberMachine(readMachinePreferences(), { kind: "local" });
  render(<MobileWorkspace />);
  expect(screen.queryByRole("button", { name: "Local" })).not.toBeInTheDocument();
  expect(screen.getByText("Reopen: none")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Open remote" }));
  expect(readMachinePreferences().selected_machine).toEqual(remote);
  expect(screen.getByText("Reopen: none")).toBeInTheDocument();
});

it("restores the saved Hub and machine after device preferences hydrate", () => {
  rememberMachine(readMachinePreferences(), remote);
  render(<MobileWorkspace />);
  expect(screen.getByText(`Hub: ${remote.hub_url}`)).toBeInTheDocument();
  expect(screen.getByText(`Reopen: ${remote.host_id}`)).toBeInTheDocument();
});

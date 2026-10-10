import { StrictMode, type ReactNode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubAccess } from "./HubAccess";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { machineReference } from "../lib/hubDeviceStore";
import { RemoteClient, selectMachine } from "../lib/transport";
vi.mock("../lib/hubAccess", () => ({ openHubAccess: vi.fn() }));
vi.mock("../pages/ViewerPage", () => ({ ViewerPage: ({ connection }: { connection: ReactNode }) => <><div>Encrypted sessions</div>{connection}</> }));
const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43), machine_address: "alice:workstation", name: "Workstation" };
let service: HubAccessService;
let client: RemoteClient;
beforeEach(() => {
  client = new RemoteClient("https://hub.example/encrypted/machine", "");
  service = {
    hub_url: "https://hub.example", status: vi.fn().mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) }),
    pair: vi.fn().mockResolvedValue(host), authenticate: vi.fn().mockResolvedValue(undefined),
    pairMachine: vi.fn().mockResolvedValue(host), loginMachine: vi.fn().mockResolvedValue(host), resolve: vi.fn(), rememberMetadata: vi.fn(),
    connect: vi.fn().mockResolvedValue(client), forget: vi.fn().mockResolvedValue(undefined),
  };
  vi.mocked(openHubAccess).mockResolvedValue(service);
});
afterEach(() => { cleanup(); selectMachine(); Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); localStorage.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); });
async function ready() { await screen.findByLabelText("Machine address or reference"); }

it("pairs in browser without a local bearer token and closes the selected machine on switching", async () => {
  const close = vi.spyOn(client, "close");
  render(<StrictMode><HubAccess initial_hub_url="https://hub.example" /></StrictMode>);
  await ready();
  expect(screen.getByRole("link", { name: "Hub administration" })).toHaveAttribute("href", "/admin");
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and connect" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pairMachine).toHaveBeenCalledWith(host.machine_address, "123456", expect.any(AbortSignal));
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  expect(screen.getByRole("dialog")).toHaveTextContent("alice:workstation");
  expect(screen.getByRole("dialog")).toHaveTextContent("https://hub.example");
  expect(screen.getByRole("dialog")).toHaveTextContent("End-to-end encrypted");
  fireEvent.click(screen.getByRole("button", { name: "Manage connections" }));
  expect(close).toHaveBeenCalledOnce();
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
});

it("requires the complete machine reference before passkey first use", async () => {
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  vi.mocked(service.loginMachine).mockRejectedValueOnce(new Error("Paste the complete machine reference."));
  fireEvent.click(screen.getByRole("radio", { name: "Use machine passkey" }));
  expect(screen.queryByLabelText("Authenticator code")).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("complete machine reference");
  expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: machineReference(host) } });
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.loginMachine).toHaveBeenLastCalledWith(machineReference(host), expect.any(AbortSignal));
});

it("reopens the remembered machine without another code and offers host-owned passkey enrollment", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pair).not.toHaveBeenCalled(); expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Add a machine passkey" }));
  await waitFor(() => expect(service.authenticate).toHaveBeenCalledWith(host, true, expect.any(AbortSignal)));
});

it("cancels abandoned pairing and rejects a late connection after unmount", async () => {
  let resolve_connection!: (client: RemoteClient) => void;
  vi.mocked(service.connect).mockImplementation(() => new Promise((resolve) => { resolve_connection = resolve; }));
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  const view = render(<HubAccess initial_hub_url="https://hub.example" />);
  await waitFor(() => expect(service.connect).toHaveBeenCalled());
  const signal = vi.mocked(service.connect).mock.calls[0][1];
  view.unmount(); expect(signal.aborted).toBe(true);
  resolve_connection(client);
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
});

it("cancels a remembered reconnect and ignores its late connection", async () => {
  let resolve_connection!: (client: RemoteClient) => void;
  vi.mocked(service.connect).mockImplementation(() => new Promise((resolve) => { resolve_connection = resolve; }));
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByText("Connecting to machine…")).toBeInTheDocument();
  const signal = vi.mocked(service.connect).mock.calls[0][1];
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(signal.aborted).toBe(true);
  expect(screen.getByRole("button", { name: "Open alice:workstation" })).toBeEnabled();
  resolve_connection(client);
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("keeps a failed remembered machine available for an explicit retry without pairing again", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  vi.mocked(service.connect).mockRejectedValueOnce(new Error("Machine is offline"));
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByRole("alert")).toHaveTextContent("Machine is offline");
  expect(screen.getByRole("button", { name: "Open alice:workstation" })).toBeEnabled();
  expect(service.connect).toHaveBeenCalledOnce();
  expect(service.pairMachine).not.toHaveBeenCalled(); expect(service.loginMachine).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Open alice:workstation" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledTimes(2);
});

it("keeps a long machine address visible beside a concise accessible open action", async () => {
  const machine_address = `${"a".repeat(63)}:${"b".repeat(63)}`;
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, machine_address }], selected_host_id: null, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(screen.getByRole("heading", { name: machine_address })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: `Open ${machine_address}` })).toHaveTextContent(/^Open$/);
});

it("clears the code and does not open a machine after canceled pairing", async () => {
  let finish_pairing!: (result: typeof host) => void;
  vi.mocked(service.pairMachine).mockImplementation(() => new Promise((resolve) => { finish_pairing = resolve; }));
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(screen.getByText("No saved machines")).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and connect" }));
  const signal = vi.mocked(service.pairMachine).mock.calls[0][2];
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(signal.aborted).toBe(true);
  finish_pairing(host);
  await waitFor(() => expect(screen.getByRole("button", { name: "Pair and connect" })).toBeDisabled());
  expect(service.connect).not.toHaveBeenCalled();
});

it("separates a draft Hub address and removes the previous service after a failed switch", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(openHubAccess).mockResolvedValueOnce(service).mockRejectedValueOnce(new Error("New Hub unavailable"));
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Change Hub" }));
  fireEvent.change(screen.getByLabelText("Hub address"), { target: { value: "https://other.example" } });
  expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent("https://hub.example");
  fireEvent.click(screen.getByRole("button", { name: "Connect to Hub" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("New Hub unavailable");
  expect(screen.queryByRole("button", { name: "Open alice:workstation" })).not.toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Current Hub" })).not.toBeInTheDocument();
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://other.example");
});

import { installViewerHost } from "../lib/platform";
import { createApellHost } from "../lib/apellHost";
import { StrictMode, type ReactNode } from "react";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubAccess } from "./HubAccess";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { machineReference } from "../lib/hubDeviceStore";
import type { DiscoveryResult, SavedHubHost } from "../lib/types";
import { RemoteClient, selectMachine, viewerStorageScope } from "../lib/transport";
vi.mock("./HostSetup", async (importOriginal) => ({ ...await importOriginal<typeof import("./HostSetup")>(), HostSetup: () => <div>Host setup</div> }));
vi.mock("../lib/hubAccess", () => ({ openHubAccess: vi.fn() }));
vi.mock("../pages/ViewerPage", () => ({ ViewerPage: ({ connection }: { connection: ReactNode }) => <><div>Encrypted sessions</div>{connection}</> }));
const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43), machine_address: "alice:workstation", name: "Workstation" };
let service: HubAccessService;
let client: RemoteClient;
let restore_host: (() => void) | undefined;
beforeEach(() => {
  client = new RemoteClient("https://hub.example/encrypted/machine", "");
  service = {
    hub_url: "https://hub.example", status: vi.fn().mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) }),
    pair: vi.fn().mockResolvedValue(host), authenticate: vi.fn().mockResolvedValue(undefined),
    pairMachine: vi.fn().mockResolvedValue(host), loginMachine: vi.fn().mockResolvedValue(host), resolve: vi.fn(), rememberMetadata: vi.fn(),
    connect: vi.fn().mockResolvedValue(client), forget: vi.fn().mockResolvedValue(undefined),
    discover: vi.fn().mockImplementation(async (group) => ({ ...group, machines: [] })),
  };
  vi.mocked(openHubAccess).mockResolvedValue(service);
});
afterEach(() => { cleanup(); restore_host?.(); restore_host = undefined; selectMachine(); Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); localStorage.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); });
async function ready() { await screen.findByLabelText("Machine address or reference"); }

const discovery_group = { namespace: "personal", secret_digest: "A".repeat(43) };
const discovered_laptop = { host_id: "550e8400-e29b-41d4-a716-446655440001", name: "Laptop", online: true };

it("takes an account machine to its existing passkey flow without opening it automatically", async () => {
  const remembered = { host_id: host.host_id, host_public_key: host.host_public_key };
  vi.mocked(service.status).mockResolvedValue({ hosts: [remembered], selected_host_id: null, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" initial_machine={{
    host_id: host.host_id, machine_address: host.machine_address, name: host.name, online: true,
  }} />);
  expect(await screen.findByLabelText("Machine address or reference")).toHaveValue(host.machine_address);
  expect(screen.getByRole("radio", { name: "Use machine passkey" })).toBeChecked();
  expect(service.connect).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  await waitFor(() => expect(service.loginMachine).toHaveBeenCalledWith(host.machine_address, expect.any(AbortSignal)));
});

it("discovers the first paired machine's group and requires a separate pairing for another machine", async () => {
  const paired = { ...host, discovery_group };
  const status = { hosts: [] as SavedHubHost[], selected_host_id: null, device_public_key: "D".repeat(43) };
  vi.mocked(service.status).mockImplementation(async () => status);
  vi.mocked(service.pairMachine).mockImplementation(async () => { status.hosts = [paired]; return paired; });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [discovered_laptop] });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  expect(service.discover).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.host_id } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  await screen.findByText("Encrypted sessions");
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  fireEvent.click(await screen.findByRole("button", { name: "Pair Laptop" }));
  expect(service.discover).toHaveBeenCalledWith(discovery_group, expect.any(AbortSignal));
  expect(service.pairMachine).toHaveBeenCalledOnce();
  expect(screen.getByLabelText("Machine address or reference")).toHaveValue(discovered_laptop.host_id);
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
  expect(screen.getByLabelText("Authenticator code")).toHaveFocus();
  expect(screen.getByRole("list", { name: "Saved machines" })).not.toHaveTextContent("Laptop");
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "654321" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  await waitFor(() => expect(service.pairMachine).toHaveBeenLastCalledWith(discovered_laptop.host_id, "654321", expect.any(AbortSignal)));
});

it("keeps equal namespace names with different digests in separate discovery groups", async () => {
  const other_group = { ...discovery_group, secret_digest: `${"B".repeat(42)}A` };
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }, { ...host, host_id: discovered_laptop.host_id, machine_address: "alice:laptop", discovery_group: other_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockImplementation(async (group) => ({ ...group, machines: [{ ...discovered_laptop, name: group.secret_digest === discovery_group.secret_digest ? "First group member" : "Second group member" }] }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const first = await screen.findByRole("region", { name: "Discovery group personal AAAAAAAA" });
  const second = screen.getByRole("region", { name: "Discovery group personal BBBBBBBB" });
  expect(await within(first).findByText("First group member")).toBeInTheDocument();
  expect(await within(second).findByText("Second group member")).toBeInTheDocument();
  expect(within(first).queryByText("Second group member")).not.toBeInTheDocument();
  expect(service.pairMachine).not.toHaveBeenCalled();
});

it("cancels discovery when switching Hubs and ignores its late machine list", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  let finish!: (result: DiscoveryResult) => void;
  vi.mocked(service.discover).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  const next = { ...service, hub_url: "https://other.example", status: vi.fn().mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) }) };
  vi.mocked(openHubAccess).mockResolvedValueOnce(service).mockResolvedValueOnce(next);
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  await waitFor(() => expect(service.discover).toHaveBeenCalledOnce());
  const signal = vi.mocked(service.discover).mock.calls[0][1];
  fireEvent.click(screen.getByRole("button", { name: "Change Hub" }));
  fireEvent.change(screen.getByLabelText("Hub address"), { target: { value: next.hub_url } });
  fireEvent.click(screen.getByRole("button", { name: "Connect to Hub" }));
  await waitFor(() => expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent(next.hub_url));
  expect(signal.aborted).toBe(true);
  finish({ ...discovery_group, machines: [discovered_laptop] });
  await Promise.resolve();
  expect(screen.queryByRole("button", { name: "Pair Laptop" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Discovered groups" })).not.toBeInTheDocument();
});

it("offers retry for failed group discovery and shows its empty result", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockRejectedValueOnce(new Error("Group unavailable"));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  expect(await screen.findByRole("alert")).toHaveTextContent("Group unavailable");
  fireEvent.click(screen.getByRole("button", { name: "Retry discovery group personal AAAAAAAA" }));
  expect(await screen.findByText("No machines are registered in this group.")).toBeInTheDocument();
  expect(service.discover).toHaveBeenCalledTimes(2);
});

it("applet first launch waits for a Hub address without trying a placeholder URL", () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  render(<HubAccess initial_hub_url="https://" startup_host_id={null} />);
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://");
  expect(openHubAccess).not.toHaveBeenCalled();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("applet pairs through Hub without offering local hosting or an unsupported passkey ceremony", async () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />);
  await ready();
  expect(screen.queryByRole("heading", { name: "This machine" })).not.toBeInTheDocument();
  expect(screen.queryByText("Host setup")).not.toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "Hub administration" })).not.toBeInTheDocument();
  expect(screen.queryByRole("radio", { name: "Use machine passkey" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Change Hub" })).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and connect" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pairMachine).toHaveBeenCalledWith(host.machine_address, "123456", expect.any(AbortSignal));
  expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  expect(screen.queryByRole("button", { name: "Add a machine passkey" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
});

it("applet reopens a remembered machine using its native pairing", async () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledWith(host, expect.any(AbortSignal));
  expect(service.pairMachine).not.toHaveBeenCalled();
});

it("pairs in browser without a local bearer token and closes the selected machine on switching", async () => {
  const close = vi.spyOn(client, "close");
  render(<StrictMode><HubAccess initial_hub_url="https://hub.example" /></StrictMode>);
  await ready();
  expect(screen.getByRole("link", { name: "Hub administration" })).toHaveAttribute("href", "/admin");
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pairMachine).toHaveBeenCalledWith(host.machine_address, "123456", expect.any(AbortSignal));
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  expect(screen.getByRole("dialog")).toHaveTextContent("alice:workstation");
  expect(screen.getByRole("dialog")).toHaveTextContent("https://hub.example");
  expect(screen.getByRole("dialog")).toHaveTextContent("End-to-end encrypted");
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
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
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pair).not.toHaveBeenCalled(); expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Add a machine passkey" }));
  await waitFor(() => expect(service.authenticate).toHaveBeenCalledWith(host, true, expect.any(AbortSignal)));
});

it("cancels abandoned pairing and rejects a late connection after unmount", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
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

it("preserves a successor machine when an old picker's delayed cleanup runs", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  service.dispose = vi.fn();
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const old_close = vi.spyOn(client, "close");
  const view = render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  view.unmount();
  const successor = new RemoteClient("https://other.example/encrypted/successor", "");
  const successor_close = vi.spyOn(successor, "close");
  selectMachine(successor);
  await Promise.resolve();
  expect(viewerStorageScope()).toBe(successor.endpoint);
  expect(successor_close).not.toHaveBeenCalled();
  expect(old_close).toHaveBeenCalledOnce();
  expect(service.dispose).toHaveBeenCalledOnce();
});

it("cancels a remembered reconnect and ignores its late connection", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  let resolve_connection!: (client: RemoteClient) => void;
  vi.mocked(service.connect).mockImplementation(() => new Promise((resolve) => { resolve_connection = resolve; }));
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" on_machine_open={on_machine_open} />);
  expect(await screen.findByText("Connecting to machine…")).toBeInTheDocument();
  const signal = vi.mocked(service.connect).mock.calls[0][1];
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(signal.aborted).toBe(true);
  expect(screen.getByRole("button", { name: "Open alice:workstation" })).toBeEnabled();
  resolve_connection(client);
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
  expect(on_machine_open).not.toHaveBeenCalled();
});

it("keeps a failed remembered machine available for an explicit retry without pairing again", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
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
  expect(screen.getByRole("button", { name: `Sign in to ${machine_address}` })).toHaveTextContent(/^Sign in$/);
});

it("clears the code and does not open a machine after canceled pairing", async () => {
  let finish_pairing!: (result: typeof host) => void;
  vi.mocked(service.pairMachine).mockImplementation(() => new Promise((resolve) => { finish_pairing = resolve; }));
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(screen.getByText("No saved machines")).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  const signal = vi.mocked(service.pairMachine).mock.calls[0][2];
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(signal.aborted).toBe(true);
  finish_pairing(host);
  await waitFor(() => expect(screen.getByRole("button", { name: "Pair and set up passkey" })).toBeDisabled());
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

it("opens the desktop picker without a Hub and leaves This machine available", () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const on_local = vi.fn();
  render(<HubAccess initial_hub_url="" startup_host_id={null} on_local={on_local} />);
  expect(screen.getByRole("heading", { name: "Machines" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "This machine" })).toBeInTheDocument();
  expect(openHubAccess).not.toHaveBeenCalled(); expect(service.connect).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(on_local).toHaveBeenCalledOnce();
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("leaves This machine available after a Hub failure and exposes local retry feedback", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(openHubAccess).mockRejectedValueOnce(new Error("Hub unavailable"));
  const on_local = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} on_local={on_local} local_error="This machine could not open its index." />);
  expect(await screen.findByText("Hub unavailable")).toBeInTheDocument();
  expect(screen.getByText("This machine could not open its index.")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Open This machine" })).toHaveTextContent("Retry");
  fireEvent.click(screen.getByRole("button", { name: "Open This machine" }));
  expect(on_local).toHaveBeenCalledOnce();
});

it("honors picker startup instead of the Hub's remembered selected host", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} on_machine_open={on_machine_open} />); await ready();
  expect(screen.getByRole("button", { name: "Open alice:workstation" })).toBeEnabled();
  expect(service.connect).not.toHaveBeenCalled(); expect(on_machine_open).not.toHaveBeenCalled();
});

it("opens only the explicit startup UUID and reports successful selection with its Hub", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const other = { ...host, host_id: "550e8400-e29b-41d4-a716-446655440001", machine_address: "alice:laptop" };
  vi.mocked(service.status).mockResolvedValue({ hosts: [host, other], selected_host_id: other.host_id, device_public_key: "D".repeat(43) });
  const on_machine_open = vi.fn();
  const on_hub_ready = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} on_machine_open={on_machine_open} on_hub_ready={on_hub_ready} />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledWith(host, expect.any(AbortSignal));
  expect(on_machine_open).toHaveBeenCalledOnce();
  expect(on_machine_open).toHaveBeenCalledWith({ kind: "hub", hub_url: "https://hub.example", host_id: host.host_id });
  expect(on_hub_ready).toHaveBeenCalledWith("https://hub.example");
});

it("does not substitute the Hub's last host for a missing explicit startup UUID", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id="550e8400-e29b-41d4-a716-446655440099" on_machine_open={on_machine_open} />); await ready();
  expect(screen.getByText(/selected machine is no longer saved/)).toBeInTheDocument();
  expect(service.connect).not.toHaveBeenCalled(); expect(on_machine_open).not.toHaveBeenCalled();
});

it("does not publish a failed startup selection", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  vi.mocked(service.connect).mockRejectedValueOnce(new Error("Machine unavailable"));
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} on_machine_open={on_machine_open} />);
  expect(await screen.findByRole("alert")).toHaveTextContent("Machine unavailable");
  expect(on_machine_open).not.toHaveBeenCalled();
});

it("keeps known Hubs accessible and loads the same UUID independently through each Hub", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const other_service = { ...service, hub_url: "https://other.example", connect: vi.fn().mockResolvedValue(client) };
  vi.mocked(openHubAccess).mockResolvedValueOnce(service).mockResolvedValueOnce(other_service);
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} known_hub_urls={["https://other.example", "https://hub.example/"]} on_machine_open={on_machine_open} on_local={vi.fn()} />); await ready();
  expect(screen.getByRole("button", { name: "https://hub.example" })).toHaveAttribute("aria-pressed", "true");
  fireEvent.click(screen.getByRole("button", { name: "https://other.example" }));
  await waitFor(() => expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent("https://other.example"));
  expect(service.connect).not.toHaveBeenCalled(); expect(other_service.connect).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Open alice:workstation" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(other_service.connect).toHaveBeenCalledWith(host, expect.any(AbortSignal));
  expect(on_machine_open).toHaveBeenCalledWith({ kind: "hub", hub_url: "https://other.example", host_id: host.host_id });
});

it("does not reopen a machine when callbacks persist startup preferences", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  const view = render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  fireEvent.click(screen.getByRole("button", { name: "Open alice:workstation" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  view.rerender(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} />);
  expect(service.connect).toHaveBeenCalledOnce(); expect(openHubAccess).toHaveBeenCalledOnce();
});

it("keeps remembered browser machines locked until explicit sign-in and destroys keys on pagehide", async () => {
  service.dispose = vi.fn();
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(service.connect).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Sign in to alice:workstation" }));
  await screen.findByText("Encrypted sessions");
  fireEvent(window, new PageTransitionEvent("pagehide"));
  expect(close).toHaveBeenCalledOnce();
  expect(service.dispose).toHaveBeenCalledOnce();
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("requires explicit browser sign-in even with a remembered startup UUID", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} on_machine_open={on_machine_open} />); await ready();
  expect(screen.getByRole("button", { name: "Sign in to alice:workstation" })).toBeEnabled();
  expect(service.connect).not.toHaveBeenCalled(); expect(on_machine_open).not.toHaveBeenCalled();
  expect(screen.queryByText(/selected machine is no longer saved/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Sign in to alice:workstation" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(on_machine_open).toHaveBeenCalledWith({ kind: "hub", hub_url: "https://hub.example", host_id: host.host_id });
});

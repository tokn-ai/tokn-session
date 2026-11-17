import { installViewerHost } from "../lib/platform";
import { createApellHost } from "../lib/apellHost";
import { StrictMode, useState, type ReactNode } from "react";
import { flushSync } from "react-dom";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubAccess } from "./HubAccess";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { machineReference } from "../lib/hubDeviceStore";
import type { DiscoveryResult, SavedHubHost } from "../lib/types";
import { RemoteClient, selectMachine, viewerStorageScope } from "../lib/transport";
vi.mock("./HostSetup", async (importOriginal) => ({ ...await importOriginal<typeof import("./HostSetup")>(), HostSetup: () => <div>Host setup</div> }));
vi.mock("../lib/hubAccess", () => ({ openHubAccess: vi.fn() }));
vi.mock("../pages/ViewerPage", () => ({ ViewerPage: ({ visible = true, connection }: { visible?: boolean; connection: ReactNode }) =>
  <div hidden={!visible}><div>Encrypted sessions</div><input aria-label="Workspace draft" />{connection}</div> }));
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
    presence: vi.fn().mockImplementation(async (host_ids: string[]) => ({ machines: host_ids.map((host_id) => ({ host_id, online: true })) })),
  };
  vi.mocked(openHubAccess).mockResolvedValue(service);
});
afterEach(() => { cleanup(); restore_host?.(); restore_host = undefined; selectMachine(); Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); localStorage.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); });
async function ready() { await screen.findByRole("button", { name: "Add machine" }); }
async function pairingReady() {
  await ready();
  fireEvent.click(screen.getByRole("button", { name: "Add machine" }));
  await screen.findByLabelText("Machine address or reference");
}

const discovery_group = { namespace: "personal", secret_digest: "A".repeat(43) };
const discovered_laptop = { host_id: "550e8400-e29b-41d4-a716-446655440001", name: "Laptop", online: true };

it("merges the account roster, saved pairing and discovery without replacing the saved alias", async () => {
  const remembered = { ...host, machine_address: "legacy:workstation", discovery_group };
  const account_machine = { host_id: host.host_id, machine_address: host.machine_address, name: host.name, online: true };
  const roster_only = { ...discovered_laptop, machine_address: "alice:laptop" };
  vi.mocked(service.status).mockResolvedValue({ hosts: [remembered], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [account_machine] });
  render(<HubAccess initial_hub_url="https://hub.example" account_roster={{
    hub_url: "https://hub.example", machines: [account_machine, roster_only], on_retry: vi.fn(),
  }} />);
  const card = (await screen.findByRole("heading", { name: "Workstation" })).closest("li")!;
  expect(within(card).getByText("alice:workstation")).toBeVisible();
  expect(screen.queryByText("Account machine")).not.toBeInTheDocument();
  expect(within(card).getByText("Paired")).toBeInTheDocument();
  const gear = within(card).getByRole("button", { name: "Settings for alice:workstation" });
  expect(within(card).queryByText(machineReference(remembered))).not.toBeInTheDocument();
  fireEvent.click(gear);
  const settings = screen.getByRole("dialog", { name: "Machine settings" });
  expect(within(settings).getByText(machineReference(remembered))).toBeVisible();
  fireEvent.click(within(settings).getByRole("button", { name: "Close Machine settings" }));
  expect(gear).toHaveFocus();
  expect(screen.getByRole("list", { name: "Machines" }).children).toHaveLength(3);
  const pair = screen.getByRole("button", { name: "Pair alice:laptop" });
  expect(within(pair.closest("li")!).queryByRole("button", { name: /Settings for/ })).not.toBeInTheDocument();
  expect(within(pair.closest("li")!).getByText("Pairing required")).toBeInTheDocument();
  fireEvent.click(pair);
  expect(screen.getByLabelText("Machine address or reference")).toHaveValue(roster_only.host_id);
  expect(service.connect).not.toHaveBeenCalled();
  expect(service.pairMachine).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Close Add machine" }));
  fireEvent.click(within(card).getByRole("button", { name: "Sign in to alice:workstation" }));
  await screen.findByText("Encrypted sessions");
  expect(service.connect).toHaveBeenCalledWith(remembered, expect.any(AbortSignal));
  expect(service.rememberMetadata).not.toHaveBeenCalled();
});

it("keeps the account roster scoped to the same Hub", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" account_roster={{
    hub_url: "https://other.example", machines: [{ ...discovered_laptop, machine_address: "alice:laptop" }], on_retry: vi.fn(),
  }} />);
  expect(await screen.findByText(host.machine_address)).toBeInTheDocument();
  expect(screen.queryByText("alice:laptop")).not.toBeInTheDocument();
});

it("runs settings actions on the selected pairing and shows the result in the gallery", async () => {
  const other = { ...host, host_id: discovered_laptop.host_id, machine_address: "alice:laptop" };
  const status = { hosts: [host, other], selected_host_id: null, device_public_key: "D".repeat(43) };
  vi.mocked(service.status).mockResolvedValue(status);
  vi.mocked(service.authenticate).mockRejectedValueOnce(new Error("Passkey canceled"));
  vi.mocked(service.forget).mockImplementation(async (host_id) => {
    status.hosts = status.hosts.filter((saved) => saved.host_id !== host_id);
  });
  render(<HubAccess initial_hub_url="https://hub.example" />);
  const gear = await screen.findByRole("button", { name: "Settings for alice:laptop" });
  fireEvent.click(gear);
  fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Add a passkey" }));
  expect(service.authenticate).toHaveBeenCalledWith(other, true, expect.any(AbortSignal));
  expect(await screen.findByRole("alert")).toHaveTextContent("Passkey canceled");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  fireEvent.click(gear);
  fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Forget machine" }));
  expect(service.forget).toHaveBeenCalledWith(other.host_id);
  await waitFor(() => expect(gear).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: "Settings for alice:workstation" })).toBeEnabled();
});

it("discovers the first paired machine's group and requires a separate pairing for another machine", async () => {
  const paired = { ...host, discovery_group };
  const status = { hosts: [] as SavedHubHost[], selected_host_id: null, device_public_key: "D".repeat(43) };
  vi.mocked(service.status).mockImplementation(async () => status);
  vi.mocked(service.pairMachine).mockImplementation(async () => { status.hosts = [paired]; return paired; });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [discovered_laptop] });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await pairingReady();
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
  expect(screen.getByRole("list", { name: "Machines" })).toHaveTextContent("Laptop");
  expect(screen.queryByRole("button", { name: "Sign in to Laptop" })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "654321" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  await waitFor(() => expect(service.pairMachine).toHaveBeenLastCalledWith(discovered_laptop.host_id, "654321", expect.any(AbortSignal)));
});

it("combines equal namespace names while discovering and refreshing each distinct capability", async () => {
  const other_group = { ...discovery_group, secret_digest: `${"B".repeat(42)}A` };
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }, { ...host, host_id: "550e8400-e29b-41d4-a716-446655440003", machine_address: "alice:laptop", discovery_group: other_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockImplementation(async (group) => ({ ...group, machines: [{ ...discovered_laptop, host_id: group.secret_digest === discovery_group.secret_digest ? discovered_laptop.host_id : "550e8400-e29b-41d4-a716-446655440002", name: group.secret_digest === discovery_group.secret_digest ? "First group member" : "Second group member" }] }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const discovery = await screen.findByRole("region", { name: "Discovery for personal" });
  expect(screen.getAllByRole("region", { name: "Discovery for personal" })).toHaveLength(1);
  expect(within(discovery).getAllByText("personal")).toHaveLength(1);
  expect(discovery).not.toHaveTextContent("personal ·");
  expect(discovery.outerHTML).not.toContain(discovery_group.secret_digest.slice(0, 8));
  expect(discovery.outerHTML).not.toContain(other_group.secret_digest.slice(0, 8));
  expect(await screen.findByRole("button", { name: "Pair First group member" })).toBeEnabled();
  expect(await screen.findByRole("button", { name: "Pair Second group member" })).toBeEnabled();
  expect(service.discover).toHaveBeenCalledWith(discovery_group, expect.any(AbortSignal));
  expect(service.discover).toHaveBeenCalledWith(other_group, expect.any(AbortSignal));
  expect(service.discover).toHaveBeenCalledTimes(2);
  fireEvent.click(within(discovery).getByRole("button", { name: "Refresh discovery for personal" }));
  await waitFor(() => expect(service.discover).toHaveBeenCalledTimes(4));
  for (const group of [discovery_group, other_group]) {
    expect(vi.mocked(service.discover).mock.calls.filter(([requested]) => requested.secret_digest === group.secret_digest)).toHaveLength(2);
  }
  expect(service.pairMachine).not.toHaveBeenCalled();
});

it("keeps successful machines available when another capability in the same namespace fails", async () => {
  const other_group = { ...discovery_group, secret_digest: `${"B".repeat(42)}A` };
  vi.mocked(service.status).mockResolvedValue({ hosts: [
    { ...host, discovery_group },
    { ...host, host_id: "550e8400-e29b-41d4-a716-446655440003", discovery_group: other_group },
  ], selected_host_id: null, device_public_key: "D".repeat(43) });
  let fail_first_group = true;
  vi.mocked(service.discover).mockImplementation(async (group) => {
    if (fail_first_group && group.secret_digest === discovery_group.secret_digest) throw new Error("Directory unavailable");
    return { ...group, machines: [discovered_laptop] };
  });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const discovery = screen.getByRole("region", { name: "Discovery for personal" });
  const alert = await within(discovery).findByRole("alert");
  expect(alert).toHaveTextContent("Some discovery results are unavailable.");
  expect(alert).toHaveTextContent("Directory unavailable");
  expect(await screen.findByRole("button", { name: "Pair Laptop" })).toBeEnabled();
  fail_first_group = false;
  fireEvent.click(within(discovery).getByRole("button", { name: "Retry discovery for personal" }));
  await waitFor(() => expect(service.discover).toHaveBeenCalledTimes(4));
  await waitFor(() => expect(within(discovery).queryByRole("alert")).not.toBeInTheDocument());
  const card = screen.getByRole("heading", { name: "Laptop" }).closest("li")!;
  expect(within(card).getAllByText("personal")).toHaveLength(1);
  expect(within(card).getByRole("button", { name: "Pair Laptop" })).toBeEnabled();
});

it("merges saved and discovered machines without replacing saved identity or pairing candidates", async () => {
  const saved = { ...host, discovery_group };
  vi.mocked(service.status).mockResolvedValue({ hosts: [saved], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [
    { host_id: host.host_id, name: "Unverified directory name", online: false },
    discovered_laptop,
  ] });
  vi.mocked(service.presence).mockImplementation(async (host_ids) => ({ machines: host_ids.map((host_id) => ({ host_id, online: host_id !== host.host_id })) }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  await screen.findByRole("button", { name: "Pair Laptop" });
  const gallery = screen.getByRole("list", { name: "Machines" });
  expect(within(gallery).getAllByRole("listitem")).toHaveLength(3);
  expect(within(gallery).queryByText("Unverified directory name")).not.toBeInTheDocument();
  const saved_card = within(gallery).getByText(host.machine_address).closest("li")!;
  expect(within(saved_card).getByText("Paired")).toBeInTheDocument();
  expect(await within(saved_card).findByText("Offline")).toBeInTheDocument();
  const open = within(saved_card).getByRole("button", { name: `Sign in to ${host.machine_address}` });
  expect(open).toBeEnabled();
  expect(screen.getAllByRole("button", { name: "Add machine" })).toHaveLength(1);
  expect(within(gallery).getByRole("button", { name: "Add machine" }).closest("li")).not.toBeNull();
  expect(service.pairMachine).not.toHaveBeenCalled();
  fireEvent.click(open);
  await screen.findByText("Encrypted sessions");
  expect(service.connect).toHaveBeenCalledWith(saved, expect.any(AbortSignal));
});

it("labels UUID-paired cards with a real discovery address or a display name without changing their identity", async () => {
  const saved = { host_id: host.host_id, host_public_key: host.host_public_key, discovery_group };
  const other = { ...saved, host_id: discovered_laptop.host_id, name: "Saved laptop" };
  vi.mocked(service.status).mockResolvedValue({ hosts: [saved, other], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [
    { host_id: saved.host_id, machine_address: "personal:workstation", name: "Workstation", online: true },
    discovered_laptop,
  ] });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const card = (await screen.findByText("personal:workstation")).closest("li")!;
  const gear = within(card).getByRole("button", { name: "Settings for personal:workstation" });
  fireEvent.click(gear);
  const settings = screen.getByRole("dialog", { name: "Machine settings" });
  expect(within(settings).getByText(machineReference(saved))).toBeVisible();
  fireEvent(settings, new Event("cancel", { cancelable: true }));
  expect(gear).toHaveFocus();
  expect(screen.getByRole("heading", { name: "Saved laptop" })).toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "personal:Saved laptop" })).not.toBeInTheDocument();
  fireEvent.click(within(card).getByRole("button", { name: "Sign in to personal:workstation" }));
  await screen.findByText("Encrypted sessions");
  expect(service.connect).toHaveBeenCalledWith(saved, expect.any(AbortSignal));
});

it("shows offline discoveries as cards without permitting pairing", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockResolvedValue({ ...discovery_group, machines: [{ ...discovered_laptop, online: false }] });
  vi.mocked(service.presence).mockImplementation(async (host_ids) => ({ machines: host_ids.map((host_id) => ({ host_id, online: host_id !== discovered_laptop.host_id })) }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const pair = await screen.findByRole("button", { name: "Pair Laptop" });
  expect(pair).toBeDisabled();
  expect(await within(pair.closest("li")!).findByText("Offline")).toBeInTheDocument();
  expect(service.pairMachine).not.toHaveBeenCalled();
});

it("shows Hub presence for every saved machine without an address or discovery group", async () => {
  const saved = { host_id: host.host_id, host_public_key: host.host_public_key, name: "Workstation" };
  const other = { ...saved, host_id: discovered_laptop.host_id, name: "Laptop" };
  vi.mocked(service.status).mockResolvedValue({ hosts: [saved, other], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.presence).mockImplementation(async (host_ids) => ({
    machines: host_ids.map((host_id) => host_id === saved.host_id
      ? { host_id, online: true, connector_version: "0.1.1" }
      : { host_id, online: false }),
  }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const first = screen.getByRole("heading", { name: "Workstation" }).closest("li")!;
  const second = screen.getByRole("heading", { name: "Laptop" }).closest("li")!;
  expect(await within(first).findByText("Online")).toBeInTheDocument();
  expect(within(first).getByTitle("Connector version")).toHaveTextContent("v0.1.1");
  expect(await within(second).findByText("Offline")).toBeInTheDocument();
  expect(within(second).queryByTitle("Connector version")).not.toBeInTheDocument();
  expect(service.presence).toHaveBeenCalledWith([saved.host_id, other.host_id], expect.any(AbortSignal));
  expect(service.discover).not.toHaveBeenCalled();
});

it("does not label an unreachable Hub as an offline machine", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.presence).mockRejectedValue(new Error("Hub unreachable"));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  expect(await screen.findByText("Status unavailable")).toHaveAttribute("title", "Hub unreachable");
  expect(screen.queryByText("Offline")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: `Sign in to ${host.machine_address}` })).toBeEnabled();
});

it("allows verified pairing when another group reports a duplicated candidate online", async () => {
  const other_group = { ...discovery_group, secret_digest: `${"B".repeat(42)}A` };
  vi.mocked(service.status).mockResolvedValue({ hosts: [
    { ...host, discovery_group },
    { ...host, host_id: "550e8400-e29b-41d4-a716-446655440003", discovery_group: other_group },
  ], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockImplementation(async (group) => ({ ...group, machines: [
    { ...discovered_laptop, online: group.secret_digest === other_group.secret_digest },
  ] }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  await waitFor(() => expect(screen.getByRole("button", { name: "Pair Laptop" })).toBeEnabled());
  expect(screen.getAllByRole("heading", { name: "Laptop" })).toHaveLength(1);
  expect(within(screen.getByRole("list", { name: "Machines" })).getAllByRole("listitem")).toHaveLength(4);
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
  expect(screen.getByRole("list", { name: "Machines" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Discovery for personal" })).not.toBeInTheDocument();
  expect(service.discover).toHaveBeenCalledOnce();
});

it("offers retry for failed group discovery and shows its empty result", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [{ ...host, discovery_group }], selected_host_id: null, device_public_key: "D".repeat(43) });
  vi.mocked(service.discover).mockRejectedValueOnce(new Error(`Group unavailable: ${discovery_group.secret_digest}`));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("Group unavailable");
  expect(alert).not.toHaveTextContent(discovery_group.secret_digest);
  fireEvent.click(screen.getByRole("button", { name: "Retry discovery for personal" }));
  expect(await screen.findByText("No other machines found.")).toBeInTheDocument();
  expect(service.discover).toHaveBeenCalledTimes(2);
});

it("shows initial connection progress without an address form and disposes a canceled late service", async () => {
  let finish!: (result: HubAccessService) => void;
  vi.mocked(openHubAccess).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
  service.dispose = vi.fn();
  const on_hub_ready = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" on_hub_ready={on_hub_ready} />);
  expect(screen.getByRole("heading", { name: "Connecting to your Hub" })).toBeInTheDocument();
  expect(screen.getByText(/hub\.example/)).toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveTextContent(/loading|connecting/i);
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Connect to Hub" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.getByRole("heading", { name: "Connect to your Hub" })).toBeInTheDocument();
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://hub.example");
  expect(screen.getByRole("button", { name: "Connect to Hub" })).toBeEnabled();
  fireEvent.change(screen.getByLabelText("Hub address"), { target: { value: "https://edited.example" } });
  await act(async () => { finish(service); });
  expect(service.dispose).toHaveBeenCalledOnce();
  expect(service.status).not.toHaveBeenCalled();
  expect(on_hub_ready).not.toHaveBeenCalled();
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://edited.example");
  expect(screen.queryByRole("button", { name: "Add machine" })).not.toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it.each(["resolves", "rejects"])("ignores canceled initial status that %s while a newer Hub connection is pending", async (outcome) => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  let finish_status!: (result: Awaited<ReturnType<HubAccessService["status"]>>) => void;
  let fail_status!: (error: Error) => void;
  vi.mocked(service.status).mockImplementationOnce(() => new Promise((resolve, reject) => { finish_status = resolve; fail_status = reject; }));
  service.dispose = vi.fn();
  const next = { ...service, hub_url: "https://other.example", status: vi.fn().mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) }), dispose: vi.fn() };
  let finish_next!: (result: HubAccessService) => void;
  vi.mocked(openHubAccess).mockResolvedValueOnce(service).mockImplementationOnce(() => new Promise((resolve) => { finish_next = resolve; }));
  const on_hub_ready = vi.fn();
  const on_machine_open = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" on_hub_ready={on_hub_ready} on_machine_open={on_machine_open} />);
  await waitFor(() => expect(service.status).toHaveBeenCalledOnce());
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  fireEvent.change(screen.getByLabelText("Hub address"), { target: { value: next.hub_url } });
  fireEvent.click(screen.getByRole("button", { name: "Connect to Hub" }));
  expect(openHubAccess).toHaveBeenLastCalledWith(next.hub_url);
  await act(async () => {
    if (outcome === "resolves") finish_status({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
    else fail_status(new Error("Abandoned Hub failed"));
  });
  expect(service.dispose).toHaveBeenCalledOnce();
  expect(service.connect).not.toHaveBeenCalled();
  expect(on_hub_ready).not.toHaveBeenCalled();
  expect(on_machine_open).not.toHaveBeenCalled();
  expect(screen.getByRole("heading", { name: "Connecting to your Hub" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Cancel" })).toBeEnabled();
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  await act(async () => { finish_next(next); });
  await ready();
  expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent(next.hub_url);
  expect(on_hub_ready).toHaveBeenCalledExactlyOnceWith(next.hub_url);
  expect(next.dispose).not.toHaveBeenCalled();
});

it("offers a compact initial connection failure and retries the same Hub", async () => {
  let finish!: (result: HubAccessService) => void;
  vi.mocked(openHubAccess).mockRejectedValueOnce(new Error("Hub unavailable"))
    .mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByRole("heading", { name: "Couldn’t connect to your Hub" })).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("Hub unavailable");
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Change Hub" })).toBeEnabled();
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  expect(screen.getByRole("heading", { name: "Connecting to your Hub" })).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(openHubAccess).toHaveBeenCalledTimes(2);
  expect(openHubAccess).toHaveBeenLastCalledWith("https://hub.example");
  await act(async () => { finish(service); });
  await ready();
  expect(screen.queryByRole("heading", { name: "Connecting to your Hub" })).not.toBeInTheDocument();
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
});

it("lets the user edit the Hub address after initial connection failure", async () => {
  const next = { ...service, hub_url: "https://other.example" };
  vi.mocked(openHubAccess).mockRejectedValueOnce(new Error("Hub unavailable")).mockResolvedValueOnce(next);
  render(<HubAccess initial_hub_url="https://hub.example" />);
  fireEvent.click(await screen.findByRole("button", { name: "Change Hub" }));
  expect(screen.getByRole("heading", { name: "Connect to your Hub" })).toBeInTheDocument();
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://hub.example");
  expect(screen.getByLabelText("Hub address")).toBeEnabled();
  fireEvent.change(screen.getByLabelText("Hub address"), { target: { value: next.hub_url } });
  fireEvent.click(screen.getByRole("button", { name: "Connect to Hub" }));
  await ready();
  expect(openHubAccess).toHaveBeenLastCalledWith(next.hub_url);
  expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent(next.hub_url);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("applet first launch waits for a Hub address without trying a placeholder URL", () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  render(<HubAccess initial_hub_url="https://" startup_host_id={null} />);
  expect(screen.getByRole("heading", { name: "Connect to your Hub" })).toBeInTheDocument();
  expect(screen.getByLabelText("Hub address")).toHaveValue("https://");
  expect(screen.getByRole("button", { name: "Connect to Hub" })).toBeEnabled();
  expect(openHubAccess).not.toHaveBeenCalled();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("applet pairs through Hub without offering local hosting or an unsupported passkey ceremony", async () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />);
  await pairingReady();
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
  await pairingReady();
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
});

it("applet reopens a remembered machine using its native pairing", async () => {
  restore_host = installViewerHost(createApellHost({ invoke: vi.fn(), onEvent: () => () => {} }));
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={host.host_id} />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledWith(host, expect.any(AbortSignal));
  expect(service.pairMachine).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  fireEvent.click(await screen.findByRole("button", { name: "Settings for alice:workstation" }));
  const settings = within(screen.getByRole("dialog", { name: "Machine settings" }));
  expect(settings.getByText(machineReference(host))).toBeVisible();
  expect(settings.queryByRole("button", { name: "Add a passkey" })).not.toBeInTheDocument();
});

it("retains the browser viewer and its drafts while navigating away, then releases it on disconnect", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  const listen = vi.spyOn(client, "setStateListener");
  service.lock = vi.fn();
  render(<StrictMode><HubAccess initial_hub_url="https://hub.example" /></StrictMode>);
  await pairingReady();
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
  fireEvent.change(screen.getByLabelText("Workspace draft"), { target: { value: "In progress" } });
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  expect(close).not.toHaveBeenCalled();
  expect(screen.getByText("Encrypted sessions")).not.toBeVisible();
  act(() => listen.mock.calls[0][0]("reconnecting"));
  fireEvent.click(await screen.findByRole("button", { name: "Resume alice:workstation" }));
  await waitFor(() => expect(screen.getByText("Encrypted sessions")).toBeVisible());
  expect(screen.getByLabelText("Workspace draft")).toHaveValue("In progress");
  expect(service.connect).toHaveBeenCalledOnce();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /Reconnecting.*Connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Disconnect machine" }));
  expect(await screen.findByRole("button", { name: "Sign in to alice:workstation" })).toBeVisible();
  expect(close).toHaveBeenCalledOnce();
  expect(service.lock).toHaveBeenCalledWith(host.host_id);
  await pairingReady();
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
});

it("releases a parked machine when the user forgets it or selects another host", async () => {
  const other = { ...host, host_id: discovered_laptop.host_id, machine_address: "alice:laptop" };
  const status = { hosts: [host, other], selected_host_id: null, device_public_key: "D".repeat(43) };
  vi.mocked(service.status).mockResolvedValue(status);
  vi.mocked(service.forget).mockImplementation(async () => { status.hosts = [host]; });
  const other_client = new RemoteClient("https://hub.example/encrypted/other", "");
  vi.mocked(service.connect).mockResolvedValueOnce(client).mockResolvedValueOnce(other_client);
  const first_close = vi.spyOn(client, "close");
  const other_close = vi.spyOn(other_client, "close");
  render(<HubAccess initial_hub_url="https://hub.example" />);
  for (const label of ["alice:workstation", "alice:laptop"]) {
    fireEvent.click(await screen.findByRole("button", { name: `Sign in to ${label}` }));
    fireEvent.click(await screen.findByRole("button", { name: /connection settings/i }));
    fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  }
  expect(first_close).toHaveBeenCalledOnce();
  expect(other_close).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Settings for alice:laptop" }));
  fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Forget machine" }));
  await waitFor(() => expect(other_close).toHaveBeenCalledOnce());
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("does not navigate back to a machine when a connection finishes after leaving the gallery", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  let finish!: (value: RemoteClient) => void;
  vi.mocked(service.connect).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  const close = vi.spyOn(client, "close");
  const browser_view = { host_id: null, on_change: vi.fn() };
  const props = { browser_view, initial_hub_url: "https://hub.example" };
  const view = render(<HubAccess {...props} />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to alice:workstation" }));
  view.rerender(<HubAccess {...props} visible={false} />);
  await act(async () => finish(client));
  expect(browser_view.on_change).not.toHaveBeenCalled();
  expect(close).toHaveBeenCalledOnce();
  view.rerender(<HubAccess {...props} />);
  expect(await screen.findByRole("button", { name: "Sign in to alice:workstation" })).toBeEnabled();
  expect(openHubAccess).toHaveBeenCalledOnce();
});

it("completes the opening gesture before publishing the browser route", async () => {
  function RoutedAccess() {
    const [host_id, setHostId] = useState<string | null>(null);
    return <HubAccess initial_hub_url="https://hub.example" browser_view={{
      host_id, on_change: (id) => flushSync(() => setHostId(id)),
    }} />;
  }
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  render(<RoutedAccess />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to alice:workstation" }));
  fireEvent.click(await screen.findByRole("button", { name: /connection settings/i }));
  expect(screen.getByRole("dialog")).not.toHaveTextContent(/connection canceled/i);
  expect(vi.mocked(service.connect).mock.calls[0][1].aborted).toBe(false);
});

it("checks the current saved pin before revealing a machine through browser history", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  const close = vi.spyOn(client, "close");
  const on_change = vi.fn();
  const access = (host_id: string | null) => <HubAccess initial_hub_url="https://hub.example" browser_view={{ host_id, on_change }} />;
  const view = render(access(null));
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to alice:workstation" }));
  await waitFor(() => expect(on_change).toHaveBeenCalledWith(host.host_id));
  view.rerender(access(host.host_id));
  await waitFor(() => expect(screen.getByText("Encrypted sessions")).toBeVisible());
  view.rerender(access(null));
  vi.mocked(service.status).mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) });
  view.rerender(access(host.host_id));
  expect(screen.getByText("Encrypted sessions")).not.toBeVisible();
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledOnce();
});

it("locks a parked machine when its tab authorization expires", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  const deadline = Date.now() + 100_000;
  service.authorizationDeadline = () => deadline;
  const close = vi.spyOn(client, "close");
  render(<HubAccess initial_hub_url="https://hub.example" />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to alice:workstation" }));
  fireEvent.click(await screen.findByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  expect(screen.getByRole("button", { name: "Resume alice:workstation" })).toBeVisible();
  vi.spyOn(Date, "now").mockReturnValue(deadline);
  fireEvent(window, new Event("focus"));
  expect(await screen.findByRole("button", { name: "Sign in to alice:workstation" })).toBeEnabled();
  expect(close).toHaveBeenCalledOnce();
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("rejects a connection if its saved pin changes while opening", async () => {
  const status = { hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) };
  vi.mocked(service.status).mockResolvedValueOnce(status).mockResolvedValue({
    ...status, hosts: [{ ...host, host_public_key: "K".repeat(43) }],
  });
  const close = vi.spyOn(client, "close");
  const on_change = vi.fn();
  render(<HubAccess initial_hub_url="https://hub.example" browser_view={{ host_id: null, on_change }} />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to alice:workstation" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(/saved pairing changed/i);
  expect(on_change).not.toHaveBeenCalled();
  expect(close).toHaveBeenCalledOnce();
  expect(screen.queryByText("Encrypted sessions")).not.toBeInTheDocument();
});

it("requires the complete machine reference before passkey first use", async () => {
  render(<HubAccess initial_hub_url="https://hub.example" />); await pairingReady();
  vi.mocked(service.loginMachine).mockRejectedValueOnce(new Error("Paste the complete machine reference."));
  fireEvent.click(screen.getByRole("radio", { name: "Use machine passkey" }));
  expect(screen.queryByLabelText("Authenticator code")).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("complete machine reference");
  expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: machineReference(host) } });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
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
  expect(screen.getByText(machine_address)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: `Sign in to ${machine_address}` })).toHaveTextContent(/^Sign in$/);
});

it("clears the code and does not open a machine after canceled pairing", async () => {
  let finish_pairing!: (result: typeof host) => void;
  vi.mocked(service.pairMachine).mockImplementation(() => new Promise((resolve) => { finish_pairing = resolve; }));
  render(<HubAccess initial_hub_url="https://hub.example" />); await pairingReady();
  expect(screen.getByRole("list", { name: "Machines" })).toBeInTheDocument();
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

it("closing the pairing dialog cancels pending pairing and preserves only the address draft", async () => {
  let finish_pairing!: (result: typeof host) => void;
  vi.mocked(service.pairMachine).mockImplementation(() => new Promise((resolve) => { finish_pairing = resolve; }));
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  expect(screen.queryByRole("dialog", { name: "Add machine" })).not.toBeInTheDocument();
  await pairingReady();
  fireEvent.change(screen.getByLabelText("Machine address or reference"), { target: { value: host.machine_address } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and set up passkey" }));
  const signal = vi.mocked(service.pairMachine).mock.calls[0][2];
  fireEvent.click(screen.getByRole("button", { name: "Close Add machine" }));
  expect(signal.aborted).toBe(true);
  expect(screen.queryByRole("dialog", { name: "Add machine" })).not.toBeInTheDocument();
  finish_pairing(host);
  await pairingReady();
  expect(screen.getByLabelText("Machine address or reference")).toHaveValue(host.machine_address);
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
  expect(service.connect).not.toHaveBeenCalled();
  expect(service.pairMachine).toHaveBeenCalledOnce();
});

it("disables the Hub editor during a machine connection and restores its draft after failure", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) });
  let fail_connection!: (error: Error) => void;
  vi.mocked(service.connect).mockImplementationOnce(() => new Promise<RemoteClient>((_resolve, reject) => { fail_connection = reject; }));
  render(<HubAccess initial_hub_url="https://hub.example" startup_host_id={null} />); await ready();
  fireEvent.click(screen.getByRole("button", { name: "Change Hub" }));
  const hub_address = screen.getByLabelText("Hub address");
  fireEvent.change(hub_address, { target: { value: "https://other.example" } });
  fireEvent.click(screen.getByRole("button", { name: "Open alice:workstation" }));
  const signal = vi.mocked(service.connect).mock.calls[0][1];
  expect(hub_address).toBeDisabled();
  expect(screen.getByRole("button", { name: "Connect to Hub" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Keep current Hub" })).toBeDisabled();
  fireEvent.submit(hub_address.closest("form")!);
  expect(openHubAccess).toHaveBeenCalledOnce();
  expect(signal.aborted).toBe(false);
  expect(screen.getByText("Connecting to machine…")).toBeInTheDocument();
  await act(async () => { fail_connection(new Error("Machine unavailable")); });
  expect(screen.getByRole("alert")).toHaveTextContent("Machine unavailable");
  expect(hub_address).toBeEnabled();
  expect(hub_address).toHaveValue("https://other.example");
  expect(screen.getByRole("button", { name: "Connect to Hub" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Keep current Hub" })).toBeEnabled();
  expect(screen.getByRole("region", { name: "Current Hub" })).toHaveTextContent("https://hub.example");
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
  expect(screen.queryByLabelText("Hub address")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Change Hub" }));
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

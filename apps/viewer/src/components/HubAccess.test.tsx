import { StrictMode, type ReactNode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubAccess } from "./HubAccess";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { machineReference } from "../lib/hubDeviceStore";
import { RemoteClient, selectMachine } from "../lib/transport";
vi.mock("../lib/hubAccess", () => ({ openHubAccess: vi.fn() }));
vi.mock("../pages/ViewerPage", () => ({ ViewerPage: ({ connection }: { connection: ReactNode }) => <><div>Encrypted sessions</div>{connection}</> }));
const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
let service: HubAccessService;
let client: RemoteClient;
beforeEach(() => {
  client = new RemoteClient("https://hub.example/encrypted/machine", "");
  service = {
    hub_url: "https://hub.example", status: vi.fn().mockResolvedValue({ hosts: [], selected_host_id: null, device_public_key: "D".repeat(43) }),
    pair: vi.fn().mockResolvedValue(host), authenticate: vi.fn().mockResolvedValue(undefined),
    connect: vi.fn().mockResolvedValue(client), forget: vi.fn().mockResolvedValue(undefined),
  };
  vi.mocked(openHubAccess).mockResolvedValue(service);
});
afterEach(() => { cleanup(); selectMachine(); vi.restoreAllMocks(); vi.clearAllMocks(); });
async function ready() { await screen.findByLabelText("Machine ID or reference"); }

it("pairs in browser without a local bearer token and closes the selected machine on switching", async () => {
  const close = vi.spyOn(client, "close");
  render(<StrictMode><HubAccess initial_hub_url="https://hub.example" /></StrictMode>);
  await ready();
  expect(screen.getByRole("link", { name: "Hub administration" })).toHaveAttribute("href", "/admin");
  fireEvent.change(screen.getByLabelText("Machine ID or reference"), { target: { value: host.host_id } });
  fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Pair and connect" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pair).toHaveBeenCalledWith(host.host_id, "123456", expect.any(AbortSignal), undefined);
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Change machine" }));
  expect(close).toHaveBeenCalledOnce();
  expect(screen.getByLabelText("Authenticator code")).toHaveValue("");
});

it("requires the complete machine reference before passkey first use", async () => {
  render(<HubAccess initial_hub_url="https://hub.example" />); await ready();
  fireEvent.change(screen.getByLabelText("Machine ID or reference"), { target: { value: host.host_id } });
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("complete machine reference");
  expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("Machine ID or reference"), { target: { value: machineReference(host) } });
  fireEvent.click(screen.getByRole("button", { name: "Sign in with machine passkey" }));
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.authenticate).toHaveBeenCalledWith(host, false, expect.any(AbortSignal));
});

it("reopens the remembered machine without another code and offers host-owned passkey enrollment", async () => {
  vi.mocked(service.status).mockResolvedValue({ hosts: [host], selected_host_id: host.host_id, device_public_key: "D".repeat(43) });
  render(<HubAccess initial_hub_url="https://hub.example" />);
  expect(await screen.findByText("Encrypted sessions")).toBeInTheDocument();
  expect(service.pair).not.toHaveBeenCalled(); expect(service.authenticate).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Add a passkey" }));
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

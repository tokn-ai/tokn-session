import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState, type ReactNode } from "react";
import { afterEach, expect, it, vi } from "vitest";
import App from "../App";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { RemoteClient, selectMachine } from "../lib/transport";

vi.mock("../lib/hubAccess", () => ({ openHubAccess: vi.fn() }));
vi.mock("./ViewerPage", () => ({
  ViewerPage: ({ visible, connection }: { visible: boolean; connection: ReactNode }) => {
    const [search, setSearch] = useState("");
    return <main hidden={!visible}><label>Session search<input value={search} onChange={(event) => setSearch(event.target.value)} /></label>{connection}</main>;
  },
}));

afterEach(() => {
  cleanup();
  selectMachine();
  window.history.replaceState(null, "", "/");
  vi.clearAllMocks();
  vi.restoreAllMocks();
});

const identity = { user_id: "44314cf9-9fc9-486a-9652-a280ba614422", username: "studio" };
const host = {
  host_id: "c16337e3-ed65-47cd-9556-4a98ab2046e2", host_public_key: "Q".repeat(43),
  name: "Build server", machine_address: "studio:build",
};

function setupHub() {
  let signed_in = true;
  const request = vi.spyOn(globalThis, "fetch").mockImplementation(async (input) => {
    const path = new URL(String(input)).pathname;
    switch (path) {
      case "/hub/v1/auth/status": return Response.json({ configured: true, authenticated: false });
      case "/hub/v1/account/capabilities": return Response.json({ enabled: true });
      case "/hub/v1/account/browser/refresh": return signed_in ? Response.json(identity) : new Response(null, { status: 401 });
      case "/hub/v1/account/machines": return Response.json({ machines: [{ ...host, online: true }] });
      case "/hub/v1/account/keys": return Response.json({ keys: [] });
      case "/hub/v1/account/logout": signed_in = false; return new Response(null, { status: 204 });
      default: throw new Error(`Unexpected Hub request: ${path}`);
    }
  });
  const client = new RemoteClient(`${location.origin}/machine/${host.host_id}`, "");
  vi.spyOn(client, "setStateListener").mockImplementation((listener) => listener("connected"));
  const close = vi.spyOn(client, "close");
  const service: HubAccessService = {
    hub_url: location.origin,
    status: vi.fn().mockResolvedValue({ hosts: [host], selected_host_id: null, device_public_key: "D".repeat(43) }),
    connect: vi.fn().mockResolvedValue(client),
    presence: vi.fn().mockResolvedValue({ machines: [{ host_id: host.host_id, online: true }] }),
    dispose: vi.fn(), pair: vi.fn(), pairMachine: vi.fn(), authenticate: vi.fn(), loginMachine: vi.fn(),
    forget: vi.fn(), rememberMetadata: vi.fn(), resolve: vi.fn(), discover: vi.fn(),
  };
  vi.mocked(openHubAccess).mockResolvedValue(service);
  return { request, close, service };
}

async function visitProfile() {
  fireEvent.click(await screen.findByRole("button", { name: "Account menu for studio" }));
  fireEvent.click(screen.getByRole("menuitem", { name: "Profile" }));
  expect(await screen.findByRole("heading", { name: "Your account" })).toBeVisible();
}

async function traverseHistory(delta: number) {
  await act(async () => {
    const popped = new Promise<void>((resolve) => window.addEventListener("popstate", () => resolve(), { once: true }));
    window.history.go(delta);
    await popped;
  });
}

it("preserves the open machine and its data through gallery, profile and browser history until sign-out", async () => {
  const { request, service, close } = setupHub();
  render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign in to studio:build" }));
  const search = await screen.findByRole("textbox", { name: "Session search" });
  fireEvent.change(search, { target: { value: "deploy notes" } });

  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("link", { name: "Hub account" }));
  expect(await screen.findByRole("heading", { name: "Your account" })).toBeVisible();
  await traverseHistory(-1);
  expect(await screen.findByRole("textbox", { name: "Session search" })).toHaveValue("deploy notes");

  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Machines" }));
  await visitProfile();
  expect(location.pathname).toBe("/account");
  expect(screen.queryByRole("textbox", { name: "Session search" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("link", { name: /Back to Machines/ }));
  expect(await screen.findByRole("heading", { name: host.name })).toBeVisible();
  await traverseHistory(-1);
  expect(await screen.findByRole("heading", { name: "Your account" })).toBeVisible();
  await traverseHistory(-1);
  expect(await screen.findByRole("heading", { name: host.name })).toBeVisible();
  await traverseHistory(-1);
  expect(await screen.findByRole("textbox", { name: "Session search" })).toHaveValue("deploy notes");

  await traverseHistory(1);
  fireEvent.click(await screen.findByRole("link", { name: "Hub administration" }));
  expect(await screen.findByRole("heading", { name: "Hub administration" })).toBeVisible();
  await traverseHistory(-1);
  fireEvent.click(await screen.findByRole("button", { name: "Resume studio:build" }));
  expect(await screen.findByRole("textbox", { name: "Session search" })).toHaveValue("deploy notes");
  expect(openHubAccess).toHaveBeenCalledOnce();
  expect(service.connect).toHaveBeenCalledOnce();
  expect(close).not.toHaveBeenCalled();
  expect(request.mock.calls.filter(([url]) => String(url).endsWith("/auth/status"))).toHaveLength(1);
  expect(request.mock.calls.filter(([url]) => String(url).endsWith("/account/browser/refresh"))).toHaveLength(1);

  await traverseHistory(-1);
  await visitProfile();
  fireEvent.click(screen.getByRole("button", { name: "Account menu for studio" }));
  fireEvent.click(screen.getByRole("menuitem", { name: "Sign out" }));
  expect(await screen.findByLabelText("Email address")).toBeVisible();
  // The signed-out form can render before the old picker's queued cleanup.
  await waitFor(() => {
    expect(close).toHaveBeenCalledOnce();
    expect(service.dispose).toHaveBeenCalledOnce();
  });
  await traverseHistory(-1);
  expect(await screen.findByRole("link", { name: "Sign in" })).toBeVisible();
  await traverseHistory(-1); // The previous machine entry belongs to the signed-out session.
  expect(await screen.findByRole("button", { name: "Sign in to studio:build" })).toBeVisible();
  expect(screen.queryByRole("textbox", { name: "Session search" })).not.toBeInTheDocument();
  expect(service.connect).toHaveBeenCalledOnce();
});

it("reuses a fresh roster and refreshes stale cards on return without reopening the Hub", async () => {
  const { request, service } = setupHub();
  const original = request.getMockImplementation()!;
  let finish_roster!: (response: Response) => void;
  const deferred = new Promise<Response>((resolve) => { finish_roster = resolve; });
  const laptop = { ...host, host_id: "10000000-0000-4000-8000-000000000001", name: "Laptop", machine_address: "studio:laptop", online: false };
  const added = { ...laptop, host_id: "10000000-0000-4000-8000-000000000002", name: "New desktop", machine_address: "studio:desktop" };
  let reads = 0;
  request.mockImplementation((input, init) => {
    if (String(input).endsWith("/account/machines")) {
      return ++reads === 1 ? Promise.resolve(Response.json({ machines: [laptop] })) : deferred;
    }
    return original(input, init);
  });
  const now = Date.now();
  const clock = vi.spyOn(Date, "now").mockReturnValue(now);
  render(<App />);
  expect(await screen.findByRole("heading", { name: "Laptop" })).toBeVisible();

  await visitProfile();
  fireEvent.click(screen.getByRole("link", { name: /Back to Machines/ }));
  expect(await screen.findByRole("heading", { name: "Laptop" })).toBeVisible();
  expect(reads).toBe(1);

  await visitProfile();
  clock.mockReturnValue(now + 31_000);
  fireEvent.click(screen.getByRole("link", { name: /Back to Machines/ }));
  expect(await screen.findByRole("heading", { name: "Laptop" })).toBeVisible();
  expect(reads).toBe(2);
  await act(async () => finish_roster(Response.json({ machines: [added] })));
  expect(screen.queryByRole("heading", { name: "Laptop" })).not.toBeInTheDocument();
  expect(await screen.findByRole("heading", { name: "New desktop" })).toBeVisible();
  expect(request.mock.calls.filter(([url]) => String(url).endsWith("/account/browser/refresh"))).toHaveLength(1);
  expect(openHubAccess).toHaveBeenCalledOnce();
  expect(service.connect).not.toHaveBeenCalled();
});

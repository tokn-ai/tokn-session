import { StrictMode, type ReactNode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubConnection } from "./HubConnection";
import { HubClient } from "../lib/hub";
import { RemoteClient } from "../lib/transport";

vi.mock("../pages/ViewerPage", () => ({ ViewerPage: ({ connection }: { connection?: ReactNode }) => <><div>Remote sessions</div>{connection}</> }));

const hosts = [
  { host_id: "host/a", name: "Workstation", online: true, access: "control" },
  { host_id: "host-b", name: "Laptop", online: false, access: "view" },
];
const enrollment = { host_id: "new-host", name: "Build server", pairing_code: "12345678", access: "control", expires_in: 300 };

beforeEach(() => {
  vi.spyOn(HubClient.prototype, "authenticate").mockImplementation(async () => new HubClient("owner-token"));
  vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const url = String(input);
    if (url.endsWith("/enrollments/approve")) return new Response(null, { status: 204 });
    if (url.endsWith("/enrollments")) return Response.json({ enrollments: [enrollment] });
    if (url.endsWith("/hosts")) return Response.json({ hosts });
    if (init?.method === "DELETE" || url.endsWith("/auth/logout")) return new Response(null, { status: 204 });
    throw new Error(`Unexpected request ${url}`);
  });
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });

async function signIn() {
  fireEvent.click(screen.getByRole("button", { name: "Sign in with passkey" }));
  await screen.findByRole("button", { name: "Open Workstation" });
}

it("sets up the owner with the fragment bootstrap credential and presents online hosts", async () => {
  render(<StrictMode><HubConnection initial_status={{ configured: false, authenticated: false }} bootstrap_token="setup-secret" /></StrictMode>);
  expect(screen.getByLabelText("Setup token")).toHaveValue("setup-secret");
  fireEvent.click(screen.getByRole("button", { name: "Create passkey" }));
  expect(await screen.findByRole("button", { name: "Open Workstation" })).toBeEnabled();
  expect(HubClient.prototype.authenticate).toHaveBeenCalledWith(true, "setup-secret");
  expect(screen.getByRole("button", { name: "Open Laptop" })).toBeDisabled();
  expect(screen.getByText(/View only/)).toBeInTheDocument();
  expect(screen.queryByLabelText("Setup token")).not.toBeInTheDocument();
  expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/hosts`, expect.objectContaining({ headers: expect.objectContaining({ Authorization: "Bearer owner-token" }) }));
});

it("routes selected hosts through Hub, closes old sessions on switching, and signs out", async () => {
  const remote = new RemoteClient(`${window.location.origin}/hosts/host%2Fa`, "owner-token");
  const close = vi.spyOn(remote, "close");
  vi.spyOn(RemoteClient, "connect").mockResolvedValue(remote);
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
  fireEvent.click(screen.getByRole("button", { name: "Open Workstation" }));
  expect(await screen.findByText("Remote sessions")).toBeInTheDocument();
  expect(RemoteClient.connect).toHaveBeenCalledWith(`${window.location.origin}/hosts/host%2Fa`, "owner-token", expect.any(AbortSignal));
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Change host" }));
  expect(close).toHaveBeenCalledOnce();
  expect(screen.queryByText("Remote sessions")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
  expect(screen.getByRole("button", { name: "Sign in with passkey" })).toBeInTheDocument();
  expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/auth/logout`, expect.objectContaining({ method: "POST", headers: expect.objectContaining({ Authorization: "Bearer owner-token" }) }));
});

it("directs encrypted hosts to machine access without opening a plaintext viewer", async () => {
  const original = vi.mocked(fetch).getMockImplementation()!;
  vi.mocked(fetch).mockImplementation(async (input, init) => String(input).endsWith("/hosts")
    ? Response.json({ hosts: [{ host_id: "protected", name: "Protected host", online: true, access: "view", secure_only: true }] })
    : original(input, init));
  vi.spyOn(RemoteClient, "connect");
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  fireEvent.click(screen.getByRole("button", { name: "Sign in with passkey" }));
  expect(await screen.findByText(/End-to-end encrypted/)).toBeInTheDocument();
  expect(screen.getByText(/Open your machines to pair/)).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Open Protected host" })).not.toBeInTheDocument();
  expect(RemoteClient.connect).not.toHaveBeenCalled();
});

it("requires explicit approval of a displayed pairing code and confirmation before revocation", async () => {
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
  expect(screen.getByText("12345678")).toBeInTheDocument();
  expect(screen.getByText(/Requests view and agent control access/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Approve Build server" }));
  await waitFor(() => expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/enrollments/approve`, expect.objectContaining({ method: "POST", body: JSON.stringify({ pairing_code: "12345678" }) })));
  await waitFor(() => expect(screen.getByRole("button", { name: "Revoke Laptop" })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: "Revoke Laptop" }));
  expect(vi.mocked(fetch).mock.calls.some(([, options]) => options?.method === "DELETE")).toBe(false);
  fireEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
  await waitFor(() => expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/hosts/host-b`, expect.objectContaining({ method: "DELETE" })));
});

it("clears the local session and returns to passkey login when Hub expires it", async () => {
  vi.mocked(fetch).mockResolvedValue(Response.json({ error: "Session expired" }, { status: 401 }));
  const close = vi.spyOn(HubClient.prototype, "close");
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  fireEvent.click(screen.getByRole("button", { name: "Sign in with passkey" }));
  expect(await screen.findByText("Your Hub session expired. Sign in again.")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Sign in with passkey" })).toBeInTheDocument();
  expect(close).toHaveBeenCalled();
});

it("adds another passkey using the current session and confirms the new credential", async () => {
  vi.mocked(HubClient.prototype.authenticate)
    .mockResolvedValueOnce(new HubClient("owner-token"))
    .mockResolvedValueOnce(new HubClient("replacement-token"));
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
  fireEvent.click(screen.getByRole("button", { name: "Add passkey" }));
  expect(await screen.findByText("Passkey added. You can use it the next time you sign in.")).toBeInTheDocument();
  expect(HubClient.prototype.authenticate).toHaveBeenLastCalledWith(true);
  await waitFor(() => expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/hosts`, expect.objectContaining({ headers: expect.objectContaining({ Authorization: "Bearer replacement-token" }) })));
});

it("cancels an unfinished host connection when signing out", async () => {
  let resolve_connection!: (client: RemoteClient) => void;
  vi.spyOn(RemoteClient, "connect").mockImplementation(() => new Promise((resolve) => { resolve_connection = resolve; }));
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
  fireEvent.click(screen.getByRole("button", { name: "Open Workstation" }));
  const signal = vi.mocked(RemoteClient.connect).mock.calls[0][2]!;
  fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
  expect(signal.aborted).toBe(true);
  const stale = new RemoteClient(`${window.location.origin}/hosts/host%2Fa`, "owner-token");
  const close = vi.spyOn(stale, "close");
  resolve_connection(stale);
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(screen.queryByText("Remote sessions")).not.toBeInTheDocument();
});

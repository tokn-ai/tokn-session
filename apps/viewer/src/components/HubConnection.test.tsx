import { StrictMode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HubConnection } from "./HubConnection";
import { HubClient } from "../lib/hub";

const hosts = [
  { host_id: "host/a", machine_address: "alice:workstation", name: "Workstation", online: true, access: "control" },
  { host_id: "host-b", machine_address: "alice:laptop", name: "Laptop", online: false, access: "view" },
];
beforeEach(() => {
  vi.spyOn(HubClient.prototype, "authenticate").mockImplementation(async () => new HubClient("owner-token"));
  vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const url = String(input);
    if (url.endsWith("/hosts")) return Response.json({ hosts });
    if (init?.method === "DELETE" || url.endsWith("/auth/logout")) return new Response(null, { status: 204 });
    throw new Error(`Unexpected request ${url}`);
  });
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });

async function signIn() {
  fireEvent.click(screen.getByRole("button", { name: "Sign in with passkey" }));
  await screen.findByRole("heading", { name: "alice:workstation" });
}

it("sets up the owner with the fragment bootstrap credential and presents online hosts", async () => {
  render(<StrictMode><HubConnection initial_status={{ configured: false, authenticated: false }} bootstrap_token="setup-secret" /></StrictMode>);
  expect(screen.getByLabelText("Setup token")).toHaveValue("setup-secret");
  fireEvent.click(screen.getByRole("button", { name: "Create passkey" }));
  expect(await screen.findByRole("heading", { name: "alice:workstation" })).toBeInTheDocument();
  expect(HubClient.prototype.authenticate).toHaveBeenCalledWith(true, "setup-secret");
  expect(screen.getByRole("heading", { name: "alice:laptop" })).toBeInTheDocument();
  expect(screen.getByText("Online")).toBeInTheDocument();
  expect(screen.getByText("Offline")).toBeInTheDocument();
  expect(screen.getByText(/View only/)).toBeInTheDocument();
  expect(screen.queryByLabelText("Setup token")).not.toBeInTheDocument();
  expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/hosts`, expect.objectContaining({ headers: expect.objectContaining({ Authorization: "Bearer owner-token" }) }));
});

it("signs out and revokes the administrator session", async () => {
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
  fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
  expect(screen.getByRole("button", { name: "Sign in with passkey" })).toBeInTheDocument();
  expect(fetch).toHaveBeenCalledWith(`${window.location.origin}/hub/v1/auth/logout`, expect.objectContaining({ method: "POST", headers: expect.objectContaining({ Authorization: "Bearer owner-token" }) }));
});

it("requires confirmation before revocation", async () => {
  render(<HubConnection initial_status={{ configured: true, authenticated: false }} />);
  await signIn();
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

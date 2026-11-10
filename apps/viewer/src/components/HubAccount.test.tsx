import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { HubAccount } from "./HubAccount";
import type { HubAccessProps } from "./HubAccess";

vi.mock("./HubAccess", () => ({
  HubAccess: ({ initial_machine, account_control }: HubAccessProps) =>
    <><p>Machine picker for {initial_machine?.machine_address}</p>{account_control}</>,
}));

const credentials = {
  user_id: "550e8400-e29b-41d4-a716-446655440000",
  username: "alice",
  token: `tkna_${"A".repeat(43)}`,
};
const machine = {
  host_id: "550e8400-e29b-41d4-a716-446655440001",
  machine_address: "alice:macbook",
  name: "Work laptop",
  online: true,
};
const key = {
  key_id: "deployment-key",
  name: "Deploy",
  scopes: ["machines:read", "machines:register"],
  expires_at: 2_000_000_000,
  token: `tkna_${"B".repeat(43)}`,
};

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
  vi.restoreAllMocks();
});

function mockAccount({
  signup = false, machines = [] as typeof machine[],
}: { signup?: boolean; machines?: typeof machine[] } = {}) {
  let keys: Omit<typeof key, "token">[] = [];
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const path = new URL(String(input)).pathname;
    const request = init?.body ? JSON.parse(String(init.body)) : undefined;
    if (path.endsWith("/capabilities")) return Response.json({ enabled: true });
    if (path.endsWith("/email/start")) return Response.json({}, { status: 202 });
    if (path.endsWith("/email/verify")) {
      if (signup && !request.username) return Response.json({ error: "Choose a username", username_required: true }, { status: 409 });
      return Response.json(credentials);
    }
    expect(init?.headers).toMatchObject({ Authorization: `Bearer ${credentials.token}` });
    if (path.endsWith("/machines")) return Response.json({ machines });
    if (path.endsWith("/keys") && init?.method === "POST") {
      const { token, ...metadata } = key;
      keys = [metadata];
      return Response.json({ ...metadata, token }, { status: 201 });
    }
    if (path.endsWith("/keys")) return Response.json({ keys });
    if (path.endsWith("/keys/deployment-key")) keys = [];
    return new Response(null, { status: 204 });
  });
}

async function submitEmailCode() {
  fireEvent.change(await screen.findByLabelText("Email address"), { target: { value: "alice@example.com" } });
  fireEvent.click(screen.getByRole("button", { name: "Send email code" }));
  fireEvent.change(await screen.findByLabelText("Email code"), { target: { value: "123456" } });
  fireEvent.click(screen.getByRole("button", { name: "Verify email code" }));
}

it("explains the email setup requirement while keeping account details behind sign-in", async () => {
  vi.spyOn(globalThis, "fetch").mockResolvedValue(Response.json({ enabled: false }));
  render(<HubAccount />);
  expect(await screen.findByText("Ask your Hub administrator to set up email delivery for account sign-in.")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Open your machines" })).toHaveAttribute("href", "/");
  expect(screen.queryByRole("heading", { name: "Profile" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "API keys" })).not.toBeInTheDocument();
});

it("asks for a username only after email verification and requires explicit CLI approval", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD-EFGH");
  const request = mockAccount({ signup: true, machines: [machine] });
  render(<HubAccount />);
  expect(await screen.findByText("ABCD-EFGH")).toBeInTheDocument();
  expect(screen.queryByLabelText("Username")).not.toBeInTheDocument();
  await submitEmailCode();
  fireEvent.change(await screen.findByLabelText("Username"), { target: { value: "alice" } });
  fireEvent.click(screen.getByRole("button", { name: "Create account" }));
  expect(await screen.findByLabelText("CLI device code")).toHaveValue("ABCD-EFGH");
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/email/verify"), expect.objectContaining({
    body: JSON.stringify({ email: "alice@example.com", code: "123456", username: "alice" }),
  }));
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
  fireEvent.click(screen.getByRole("button", { name: "Approve CLI login" }));
  expect(await screen.findByText("CLI login approved. Return to your terminal.")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "ABCD-EFGH" }),
  }));
  fireEvent.click(await screen.findByRole("button", { name: "Connect to alice:macbook" }));
  expect(screen.getByText("Machine picker for alice:macbook")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Back to Hub account" }));
  expect(await screen.findByRole("heading", { name: "alice:macbook" })).toBeInTheDocument();
});

it("creates a scoped key, shows its secret once, and revokes it after confirmation", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  await submitEmailCode();
  const profile = await screen.findByRole("region", { name: "Profile" });
  expect(profile).toHaveTextContent(credentials.username);
  expect(profile).toHaveTextContent(credentials.user_id);
  fireEvent.change(await screen.findByLabelText("Key name"), { target: { value: "Deploy" } });
  fireEvent.click(screen.getByLabelText("Register machines"));
  fireEvent.click(screen.getByRole("button", { name: "Create API key" }));
  expect(await screen.findByText(key.token)).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/keys"), expect.objectContaining({
    method: "POST", body: JSON.stringify({ name: "Deploy", scopes: ["machines:read", "machines:register"] }),
  }));
  fireEvent.click(screen.getByRole("button", { name: "Done" }));
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
  const keys = await screen.findByRole("list", { name: "Account API keys" });
  fireEvent.click(within(keys).getByRole("button", { name: "Revoke Deploy" }));
  fireEvent.click(within(keys).getByRole("button", { name: "Confirm revoke" }));
  expect(await screen.findByText("No API keys yet.")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
  expect(await screen.findByLabelText("Email address")).toBeInTheDocument();
  await waitFor(() => expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/logout"), expect.objectContaining({ method: "POST" })));
});

import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { HubAccount } from "./HubAccount";

const identity = {
  user_id: "550e8400-e29b-41d4-a716-446655440000",
  username: "alice",
};
const other_identity = {
  user_id: "550e8400-e29b-41d4-a716-446655440002",
  username: "bob",
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
  signup = false, email_configured = true, signed_in = false, logout,
  current_identity = () => identity,
}: {
  signup?: boolean; email_configured?: boolean; signed_in?: boolean;
  logout?: () => Promise<Response>; current_identity?: () => typeof identity;
} = {}) {
  let keys: Omit<typeof key, "token">[] = [];
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const path = new URL(String(input)).pathname;
    const request = init?.body ? JSON.parse(String(init.body)) : undefined;
    const headers = new Headers(init?.headers);
    expect(init).toMatchObject({ credentials: "same-origin", mode: "same-origin", referrerPolicy: "strict-origin" });
    expect(headers.has("Authorization")).toBe(false);
    if (path.endsWith("/capabilities")) return Response.json({ enabled: true, email_configured });
    if (path.endsWith("/email/start")) return Response.json({}, { status: 202 });
    if (path.endsWith("/account/browser/email/verify")) {
      if (signup && !request.username) return Response.json({ error: "Choose a username", username_required: true }, { status: 409 });
      signed_in = true;
      return Response.json(current_identity());
    }
    if (!signed_in) return Response.json({ error: "Sign in required" }, { status: 401 });
    if (path.endsWith("/account/browser/refresh")) {
      expect(headers.has("X-Tokn-Account-Id")).toBe(false);
      return Response.json(current_identity());
    }
    if (headers.get("X-Tokn-Account-Id") !== current_identity().user_id) {
      return Response.json({ error: "Hub account changed", account_session_changed: true }, { status: 409 });
    }
    if (path.endsWith("/account/logout")) {
      const response = logout ? await logout() : new Response(null, { status: 204 });
      if (response.ok) signed_in = false;
      return response;
    }
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

function codeDigits(): HTMLInputElement[] {
  return within(screen.getByRole("group", { name: "Email code" })).getAllByRole<HTMLInputElement>("textbox");
}

function readCode(): string {
  return codeDigits().map((input) => input.value).join("");
}

function pasteCode(code: string, input = codeDigits()[0]) {
  fireEvent.paste(input, { clipboardData: { getData: () => code } });
}

async function openCodeStep() {
  fireEvent.change(await screen.findByLabelText("Email address"), { target: { value: "alice@example.com" } });
  fireEvent.click(screen.getByRole("button", { name: "Send email code" }));
  await screen.findByRole("textbox", { name: "Digit 1 of 6" });
}

async function submitEmailCode() {
  await openCodeStep();
  pasteCode("123456");
  fireEvent.click(screen.getByRole("button", { name: "Verify email code" }));
}

it("explains the email setup requirement while keeping account details behind sign-in", async () => {
  mockAccount({ email_configured: false });
  render(<HubAccount />);
  expect(await screen.findByText("Ask your Hub administrator to set up email delivery for account sign-in.")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Open your machines" })).toHaveAttribute("href", "/");
  expect(screen.queryByRole("heading", { name: "Profile" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "API keys" })).not.toBeInTheDocument();
});

it("moves keyboard focus through sign-in steps and back to the retained email", async () => {
  mockAccount({ signup: true });
  render(<HubAccount />);
  const email = await screen.findByLabelText("Email address");
  expect(email).toHaveFocus();
  fireEvent.change(email, { target: { value: "alice@example.com" } });
  fireEvent.submit(email.closest("form")!);

  const code = await screen.findByRole("textbox", { name: "Digit 1 of 6" });
  expect(code).toHaveFocus();
  fireEvent.change(code, { target: { value: "123456" } });
  fireEvent.submit(code.closest("form")!);

  const username = await screen.findByLabelText("Username");
  expect(username).toHaveFocus();
  fireEvent.change(username, { target: { value: "alice" } });
  fireEvent.click(screen.getByRole("button", { name: "Change email" }));

  expect(screen.getByLabelText("Email address")).toHaveFocus();
  expect(screen.getByLabelText("Email address")).toHaveValue("alice@example.com");
  expect(screen.queryByRole("group", { name: "Email code" })).not.toBeInTheDocument();
  expect(screen.queryByLabelText("Username")).not.toBeInTheDocument();
});

it("lets the keyboard edit individual digits without shifting the remaining code", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  await openCodeStep();
  const digits = codeDigits();
  expect(digits).toHaveLength(6);

  digits.forEach((input, index) => {
    expect(input).toHaveAccessibleName(`Digit ${index + 1} of 6`);
    expect(input).toBeRequired();
    fireEvent.change(input, { target: { value: String(index + 1) } });
    expect(digits[Math.min(index + 1, 5)]).toHaveFocus();
  });
  expect(readCode()).toBe("123456");
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/email/verify"), expect.anything());

  fireEvent.keyDown(digits[5], { key: "ArrowLeft" });
  expect(digits[4]).toHaveFocus();
  fireEvent.keyDown(digits[4], { key: "Home" });
  expect(digits[0]).toHaveFocus();
  fireEvent.keyDown(digits[0], { key: "End" });
  expect(digits[5]).toHaveFocus();
  fireEvent.keyDown(digits[5], { key: "Backspace" });
  expect(digits[5]).toHaveFocus();
  expect(digits[5]).toHaveValue("");
  fireEvent.keyDown(digits[5], { key: "Backspace" });
  expect(digits[4]).toHaveFocus();
  expect(digits[4]).toHaveValue("");

  fireEvent.keyDown(digits[4], { key: "ArrowLeft" });
  fireEvent.keyDown(digits[3], { key: "ArrowLeft" });
  fireEvent.keyDown(digits[2], { key: "Delete" });
  expect(digits[2]).toHaveFocus();
  expect(digits.map((input) => input.value)).toEqual(["1", "2", "", "4", "", ""]);
  fireEvent.keyDown(digits[2], { key: "ArrowRight" });
  expect(digits[3]).toHaveFocus();
});

it("distributes pasted and autofilled codes while preserving partial-paste positions", async () => {
  mockAccount();
  render(<HubAccount />);
  await openCodeStep();
  const digits = codeDigits();
  expect(digits[0]).toHaveAttribute("autocomplete", "one-time-code");

  pasteCode("12 3-456", digits[3]);
  expect(readCode()).toBe("123456");
  expect(digits[5]).toHaveFocus();
  expect([digits[5].selectionStart, digits[5].selectionEnd]).toEqual([0, 1]);
  pasteCode("98", digits[2]);
  expect(readCode()).toBe("129856");
  expect(digits[4]).toHaveFocus();
  fireEvent.change(digits[2], { target: { value: "" } });
  expect(digits.map((input) => input.value)).toEqual(["1", "2", "", "8", "5", "6"]);
  fireEvent.change(digits[2], { target: { value: "x" } });
  expect(digits[2]).toHaveValue("");

  fireEvent.change(digits[0], { target: { value: "654321" } });
  expect(readCode()).toBe("654321");
  expect(digits[5]).toHaveFocus();
  expect([digits[5].selectionStart, digits[5].selectionEnd]).toEqual([0, 1]);
});

it("requires all six digits and disables every box during verification", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  await openCodeStep();
  const digits = codeDigits();
  fireEvent.submit(digits[0].closest("form")!);
  pasteCode("12345");
  fireEvent.submit(digits[0].closest("form")!);
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/email/verify"), expect.anything());

  pasteCode("123456");
  let resolve_verify!: (response: Response) => void;
  request.mockReturnValueOnce(new Promise<Response>((resolve) => { resolve_verify = resolve; }));
  fireEvent.click(screen.getByRole("button", { name: "Verify email code" }));
  digits.forEach((input) => expect(input).toBeDisabled());
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/browser/email/verify"), expect.objectContaining({
    body: JSON.stringify({ email: "alice@example.com", code: "123456" }),
  }));
  resolve_verify(Response.json({ error: "Incorrect email code" }, { status: 400 }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Incorrect email code");
  digits.forEach((input) => expect(input).toBeEnabled());
  expect(readCode()).toBe("123456");
});

it("announces a resent code, clears the previous code, and allows retry after request or verification errors", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  fireEvent.change(await screen.findByLabelText("Email address"), { target: { value: "alice@example.com" } });
  fireEvent.click(screen.getByRole("button", { name: "Send email code" }));
  await screen.findByRole("textbox", { name: "Digit 1 of 6" });
  pasteCode("123456");

  request.mockResolvedValueOnce(Response.json({ error: "Email delivery unavailable" }, { status: 503 }));
  fireEvent.click(screen.getByRole("button", { name: "Send another code" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Email delivery unavailable");
  expect(readCode()).toBe("123456");
  expect(screen.getByRole("button", { name: "Send another code" })).toBeEnabled();

  fireEvent.click(screen.getByRole("button", { name: "Send another code" }));
  expect(await screen.findByRole("status")).toHaveTextContent("A new code has been sent. Check your inbox.");
  expect(codeDigits().map((input) => input.value)).toEqual(["", "", "", "", "", ""]);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(request.mock.calls.filter(([input]) => String(input).endsWith("/email/start"))).toHaveLength(3);

  pasteCode("654321");
  request.mockResolvedValueOnce(Response.json({ error: "Incorrect email code" }, { status: 400 }));
  fireEvent.click(screen.getByRole("button", { name: "Verify email code" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Incorrect email code");
  expect(readCode()).toBe("654321");
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  await waitFor(() => expect(screen.getByRole("button", { name: "Verify email code" })).toBeEnabled());

  pasteCode("111222");
  fireEvent.click(screen.getByRole("button", { name: "Verify email code" }));
  expect(await screen.findByRole("region", { name: "Profile" })).toHaveTextContent(identity.username);
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/browser/email/verify"), expect.objectContaining({
    body: JSON.stringify({ email: "alice@example.com", code: "111222" }),
  }));
});

it("asks for a username only after email verification and requires explicit CLI approval", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD-EFGH");
  const request = mockAccount({ signup: true });
  render(<HubAccount />);
  expect(await screen.findByText("ABCD-EFGH")).toBeInTheDocument();
  expect(screen.queryByLabelText("Username")).not.toBeInTheDocument();
  await submitEmailCode();
  fireEvent.change(await screen.findByLabelText("Username"), { target: { value: "alice" } });
  fireEvent.click(screen.getByRole("button", { name: "Create account" }));
  expect(await screen.findByLabelText("CLI device code")).toHaveValue("ABCD-EFGH");
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/browser/email/verify"), expect.objectContaining({
    body: JSON.stringify({ email: "alice@example.com", code: "123456", username: "alice" }),
  }));
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
  fireEvent.click(screen.getByRole("button", { name: "Approve CLI login" }));
  expect(await screen.findByText("CLI login approved. Return to your terminal.")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "ABCD-EFGH" }),
  }));
  expect(screen.getByRole("link", { name: "Open your machines" })).toHaveAttribute("href", "/");
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/account/machines"), expect.anything());
});

it("creates a scoped key, shows its secret once, and revokes it after confirmation", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  await submitEmailCode();
  const profile = await screen.findByRole("region", { name: "Profile" });
  expect(profile).toHaveTextContent(identity.username);
  expect(profile).toHaveTextContent(identity.user_id);
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
});

it("restores a signed-in account after reopening the page", async () => {
  const request = mockAccount();
  const first = render(<HubAccount />);
  await submitEmailCode();
  expect(await screen.findByRole("region", { name: "Profile" })).toHaveTextContent(identity.username);
  first.unmount();
  request.mockClear();

  render(<HubAccount />);
  expect(await screen.findByRole("region", { name: "Profile" })).toHaveTextContent(identity.username);
  expect(screen.queryByLabelText("Email address")).not.toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/browser/refresh"), expect.objectContaining({ method: "POST" }));
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/email/"), expect.anything());
});

it("restores an existing account when email delivery is unavailable", async () => {
  mockAccount({ email_configured: false, signed_in: true });
  render(<HubAccount />);
  expect(await screen.findByRole("region", { name: "Profile" })).toHaveTextContent(identity.username);
  expect(screen.getByRole("link", { name: "Open your machines" })).toHaveAttribute("href", "/");
});

it("recovers from a stale account action without replaying it against the active account", async () => {
  let current = identity;
  const request = mockAccount({ signed_in: true, current_identity: () => current });
  render(<HubAccount />);
  fireEvent.change(await screen.findByLabelText("Key name"), { target: { value: "Deploy" } });
  current = other_identity;
  fireEvent.click(screen.getByRole("button", { name: "Create API key" }));

  expect(await screen.findByText(other_identity.username)).toBeInTheDocument();
  expect(within(screen.getByRole("region", { name: "Profile" })).getByRole("status"))
    .toHaveTextContent("Your Hub account changed in another tab.");
  expect(screen.getByLabelText("Key name")).toHaveValue("");
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
  const writes = request.mock.calls.filter(([input, init]) => String(input).endsWith("/account/keys") && init?.method === "POST");
  expect(writes).toHaveLength(1);
  expect(new Headers(writes[0][1]?.headers).get("X-Tokn-Account-Id")).toBe(identity.user_id);
});

it("restores a changed browser account when the account page regains focus", async () => {
  let current = identity;
  mockAccount({ signed_in: true, current_identity: () => current });
  render(<HubAccount />);
  fireEvent.change(await screen.findByLabelText("Key name"), { target: { value: "Previous account draft" } });
  current = other_identity;
  fireEvent.focus(window);

  expect(await screen.findByText(other_identity.username)).toBeInTheDocument();
  expect(screen.getByLabelText("Key name")).toHaveValue("");
});

it("waits for logout, reports a failure, and clears the session after a successful retry", async () => {
  let resolve_failure!: (response: Response) => void;
  const failure = new Promise<Response>((resolve) => { resolve_failure = resolve; });
  const logout = vi.fn().mockReturnValueOnce(failure).mockResolvedValueOnce(new Response(null, { status: 204 }));
  const request = mockAccount({ signed_in: true, logout });
  const first = render(<HubAccount />);
  fireEvent.click(await screen.findByRole("button", { name: "Sign out" }));
  expect(screen.getByRole("button", { name: "Signing out…" })).toBeDisabled();
  expect(screen.getByRole("region", { name: "Profile" })).toBeInTheDocument();
  resolve_failure(Response.json({ error: "Logout unavailable" }, { status: 503 }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Logout unavailable");
  expect(screen.getByRole("region", { name: "Profile" })).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
  expect(await screen.findByLabelText("Email address")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/logout"), expect.objectContaining({ method: "POST" }));
  expect(logout).toHaveBeenCalledTimes(2);
  first.unmount();

  render(<HubAccount />);
  expect(await screen.findByLabelText("Email address")).toBeInTheDocument();
});

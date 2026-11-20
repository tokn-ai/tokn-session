import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { HubAccount as AccountPage } from "./HubAccount";
import { useBrowserHubAccount } from "../lib/useBrowserHubAccount";

function HubAccount({ visible = true }: { visible?: boolean }) {
  const account = useBrowserHubAccount(false, visible, false);
  return visible ? <AccountPage account={account} /> : null;
}

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
const github_base = "/hub/v1/account/browser/github";

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
  vi.restoreAllMocks();
});

function mockAccount({
  signup = false, email_configured = true, signed_in = false, logout,
  current_identity = () => identity, github,
}: {
  signup?: boolean; email_configured?: boolean; signed_in?: boolean;
  logout?: () => Promise<Response>; current_identity?: () => typeof identity;
  github?: { complete?: "signed_in" | "linked" | "link_required" | "signup_required"; connection?: string };
} = {}) {
  let keys: Omit<typeof key, "token">[] = [];
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const path = new URL(String(input)).pathname;
    const request = init?.body ? JSON.parse(String(init.body)) : undefined;
    const headers = new Headers(init?.headers);
    expect(init).toMatchObject({ credentials: "same-origin", mode: "same-origin", referrerPolicy: "strict-origin" });
    expect(headers.has("Authorization")).toBe(false);
    if (path.endsWith("/capabilities")) return Response.json({ enabled: true, email_configured, ...(github && { github_configured: true }) });
    if (path.endsWith("/email/start")) return Response.json({}, { status: 202 });
    if (path.endsWith("/account/browser/email/verify")) {
      if (signup && !request.username) return Response.json({ error: "Choose a username", username_required: true }, { status: 409 });
      signed_in = true;
      return Response.json(current_identity());
    }
    if (github && path === `${github_base}/start`) {
      if (request.mode === "link") expect(headers.get("X-Tokn-Account-Id")).toBe(current_identity().user_id);
      else expect(headers.has("X-Tokn-Account-Id")).toBe(false);
      // The UI must refuse to navigate to an invalid OAuth destination.
      return Response.json({ authorization_url: "https://github.com.example.test/login/oauth/authorize" });
    }
    if (github?.complete && path === `${github_base}/complete`) {
      if (github.complete === "linked") expect(headers.get("X-Tokn-Account-Id")).toBe(current_identity().user_id);
      else expect(headers.has("X-Tokn-Account-Id")).toBe(false);
      if (github.complete === "signup_required") return Response.json({ status: github.complete, suggested_username: "existing" });
      if (github.complete === "signed_in") signed_in = true;
      return Response.json({ status: github.complete, ...(github.complete === "signed_in" && current_identity()) });
    }
    if (github && path === `${github_base}/signup`) {
      expect(headers.has("X-Tokn-Account-Id")).toBe(false);
      if (request.username === "existing") return Response.json({ error: "That username is already reserved" }, { status: 409 });
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
    if (github && path === github_base) return Response.json({ connection: github.connection ? { github_login: github.connection } : null });
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

function deviceCodeSlots(): HTMLInputElement[] {
  return within(screen.getByRole("group", { name: "CLI device code" })).getAllByRole<HTMLInputElement>("textbox");
}

function pasteDeviceCode(code: string, input = deviceCodeSlots()[0]) {
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

async function openCreateKeyDialog() {
  fireEvent.click(await screen.findByRole("button", { name: "Create API key" }));
  return screen.findByRole<HTMLDialogElement>("dialog", { name: "Create API key" });
}

it("explains the email setup requirement while keeping account details behind sign-in", async () => {
  mockAccount({ email_configured: false });
  render(<HubAccount />);
  expect(await screen.findByText("Ask your Hub administrator to set up email delivery for account sign-in.")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Back to Machines" })).toHaveAttribute("href", "/");
  expect(screen.queryByRole("heading", { name: "Profile" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "API keys" })).not.toBeInTheDocument();
});

it("starts GitHub sign-in even without email configured and sends the Apell device request", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD-EFGH&device=viewer");
  const request = mockAccount({ github: {}, email_configured: false });
  render(<HubAccount />);
  expect(await screen.findByText("ABCD-EFGH")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Use an email code" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Continue with GitHub" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Hub returned an invalid GitHub sign-in address.");
  expect(request).toHaveBeenCalledWith(expect.stringContaining(`${github_base}/start`), expect.objectContaining({
    method: "POST", body: JSON.stringify({ mode: "sign_in", user_code: "ABCD-EFGH", device: "viewer" }),
  }));
});

it("finishes the GitHub callback once and leaves the Apell device code for explicit approval", async () => {
  window.history.replaceState(null, "", "/account?github=complete&user_code=ABCD-EFGH&device=viewer");
  const request = mockAccount({ github: { complete: "signed_in" }, email_configured: false });
  render(<StrictMode><HubAccount /></StrictMode>);
  const dialog = await screen.findByRole("dialog", { name: "Approve a Tokn Sessions sign-in" });
  expect(within(dialog).getByText("ABCD-EFGH")).toBeInTheDocument();
  expect(window.location.search).toBe("?user_code=ABCD-EFGH&device=viewer");
  expect(request.mock.calls.filter(([url]) => String(url).endsWith(`${github_base}/complete`))).toHaveLength(1);
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/viewer/approve"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Approve Tokn Sessions sign-in" }));
  expect(await within(dialog).findByText("Sign-in approved. Return to the Tokn Sessions app.")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/viewer/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "ABCD-EFGH" }),
  }));
});

it("asks an existing email account to sign in before connecting GitHub", async () => {
  window.history.replaceState(null, "", "/account?github=complete");
  const request = mockAccount({ github: { complete: "link_required" } });
  render(<HubAccount />);
  expect(await screen.findByRole("alert")).toHaveTextContent("This email already belongs to a Hub account.");
  await submitEmailCode();
  const section = await screen.findByRole("region", { name: "GitHub" });
  fireEvent.click(await within(section).findByRole("button", { name: "Connect GitHub" }));
  expect(await within(section).findByRole("alert")).toHaveTextContent("Hub returned an invalid GitHub sign-in address.");
  expect(request).toHaveBeenCalledWith(expect.stringContaining(`${github_base}/start`), expect.objectContaining({
    method: "POST", body: JSON.stringify({ mode: "link" }),
    headers: expect.objectContaining({ "X-Tokn-Account-Id": identity.user_id }),
  }));
});

it("asks first-time GitHub users for a Hub username and allows them to retry a collision", async () => {
  window.history.replaceState(null, "", "/account?github=complete");
  mockAccount({ github: { complete: "signup_required", connection: "alice-on-github" } });
  render(<HubAccount />);
  const username = await screen.findByLabelText("Username");
  expect(username).toHaveValue("existing");
  expect(screen.getByText(/Already have a Hub account\?/)).toBeVisible();
  expect(screen.getByRole("button", { name: "Use an email code" })).toBeVisible();
  expect(window.location.search).toBe("?github=complete");
  fireEvent.click(screen.getByRole("button", { name: "Create account" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("That username is already reserved");
  fireEvent.change(username, { target: { value: "alice" } });
  fireEvent.click(screen.getByRole("button", { name: "Create account" }));
  expect(await screen.findByText("alice-on-github")).toBeVisible();
  expect(window.location.search).toBe("");
});

it("finishes connecting GitHub with the restored Hub account's ID", async () => {
  window.history.replaceState(null, "", "/account?github=complete");
  const request = mockAccount({ github: { complete: "linked", connection: "alice-on-github" }, signed_in: true });
  render(<HubAccount />);
  const section = await screen.findByRole("region", { name: "GitHub" });
  expect(within(section).getByText("GitHub connected.")).toBeInTheDocument();
  expect(await within(section).findByText("alice-on-github")).toBeVisible();
  expect(screen.getByRole("button", { name: "Account menu for alice" })).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining(`${github_base}/complete`), expect.objectContaining({
    headers: expect.objectContaining({ "X-Tokn-Account-Id": identity.user_id }),
  }));
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
  const dialog = await screen.findByRole("dialog", { name: "Approve a CLI login" });
  expect(deviceCodeSlots().map((input) => input.value).join("")).toBe("ABCDEFGH");
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/browser/email/verify"), expect.objectContaining({
    body: JSON.stringify({ email: "alice@example.com", code: "123456", username: "alice" }),
  }));
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Approve CLI login" }));
  expect(await within(dialog).findByText("CLI login approved. Return to your terminal.")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "ABCD-EFGH" }),
  }));
  expect(screen.getByRole("link", { name: "Back to Machines" })).toHaveAttribute("href", "/");
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/account/machines"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Done" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByRole("group", { name: "CLI device code" })).not.toBeInTheDocument();
});

it("keeps the overview free of forms and clears dismissed dialog drafts while restoring focus", async () => {
  mockAccount({ signed_in: true });
  render(<HubAccount />);
  await screen.findByRole("region", { name: "Profile" });
  expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

  const approve = screen.getByRole("button", { name: "Approve a CLI login" });
  fireEvent.click(approve);
  const cli_dialog = screen.getByRole("dialog", { name: "Approve a CLI login" });
  const code = deviceCodeSlots()[0];
  expect(code).toHaveFocus();
  pasteDeviceCode("DRAF-TCOD");
  fireEvent(cli_dialog, new Event("cancel", { cancelable: true }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(approve).toHaveFocus();
  fireEvent.click(approve);
  expect(deviceCodeSlots().map((input) => input.value)).toEqual(Array(8).fill(""));
  fireEvent.click(screen.getByRole("button", { name: "Close Approve a CLI login" }));

  const create = screen.getByRole("button", { name: "Create API key" });
  const key_dialog = await openCreateKeyDialog();
  const name = within(key_dialog).getByLabelText("Key name");
  expect(name).toHaveFocus();
  fireEvent.change(name, { target: { value: "Unfinished key" } });
  fireEvent.click(within(key_dialog).getByLabelText("Register machines"));
  fireEvent.click(within(key_dialog).getByRole("button", { name: "Cancel" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(create).toHaveFocus();
  const reopened = await openCreateKeyDialog();
  expect(within(reopened).getByLabelText("Key name")).toHaveValue("");
  expect(within(reopened).getByLabelText("Read machines")).toBeChecked();
  expect(within(reopened).getByLabelText("Register machines")).not.toBeChecked();
});

it("opens a linked CLI request after restoring sign-in without approving it automatically", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD-EFGH");
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  const dialog = await screen.findByRole("dialog", { name: "Approve a CLI login" });
  expect(deviceCodeSlots().map((input) => input.value).join("")).toBe("ABCDEFGH");
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
});

it("approves a linked Apell request through the read-only viewer endpoint and keeps manual CLI approval separate", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD-EFGH&device=viewer");
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  const dialog = await screen.findByRole("dialog", { name: "Approve a Tokn Sessions sign-in" });
  expect(within(dialog).getByText("ABCD-EFGH")).toBeInTheDocument();
  expect(dialog).toHaveAccessibleDescription(/list the machines registered to this account/);
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/viewer/approve"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Approve Tokn Sessions sign-in" }));
  expect(await within(dialog).findByText("Sign-in approved. Return to the Tokn Sessions app.")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/viewer/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "ABCD-EFGH" }),
  }));
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());
  fireEvent.click(within(dialog).getByRole("button", { name: "Done" }));

  fireEvent.click(screen.getByRole("button", { name: "Approve a CLI login" }));
  expect(screen.getByRole("dialog", { name: "Approve a CLI login" })).toBeInTheDocument();
  expect(deviceCodeSlots().map((input) => input.value)).toEqual(Array(8).fill(""));
});

it("ignores an invalid Apell approval code without opening a CLI approval", async () => {
  window.history.replaceState(null, "", "/account?user_code=ABCD&device=viewer");
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  await screen.findByRole("region", { name: "Profile" });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/approve"), expect.anything());
});

it("advances across CLI code groups and preserves later characters while editing", async () => {
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  fireEvent.click(await screen.findByRole("button", { name: "Approve a CLI login" }));
  const slots = deviceCodeSlots();
  expect(slots).toHaveLength(8);
  slots.forEach((input, index) => {
    expect(input).toHaveAccessibleName(`Character ${index + 1} of 8`);
    fireEvent.change(input, { target: { value: "abcdefgh"[index] } });
    expect(slots[Math.min(index + 1, 7)]).toHaveFocus();
  });
  expect(slots.map((input) => input.value).join("")).toBe("ABCDEFGH");
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());

  for (const index of [7, 6, 5]) fireEvent.keyDown(slots[index], { key: "ArrowLeft" });
  expect(slots[4]).toHaveFocus();
  fireEvent.keyDown(slots[4], { key: "Delete" });
  expect(slots.map((input) => input.value)).toEqual(["A", "B", "C", "D", "", "F", "G", "H"]);
  fireEvent.keyDown(slots[4], { key: "Backspace" });
  expect(slots[3]).toHaveFocus();
  expect(slots.map((input) => input.value)).toEqual(["A", "B", "C", "", "", "F", "G", "H"]);
  expect(screen.getByRole("button", { name: "Approve CLI login" })).toBeDisabled();
});

it("pastes a complete CLI code from any box and requires all eight characters before approval", async () => {
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  await screen.findByText("No API keys yet.");
  fireEvent.click(screen.getByRole("button", { name: "Approve a CLI login" }));
  const dialog = screen.getByRole("dialog", { name: "Approve a CLI login" });
  const slots = deviceCodeSlots();
  pasteDeviceCode("abc");
  expect(within(dialog).getByRole("button", { name: "Approve CLI login" })).toBeDisabled();
  fireEvent.submit(slots[0].closest("form")!);
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.anything());

  pasteDeviceCode("ab12-cd34", slots[5]);
  expect(slots.map((input) => input.value).join("")).toBe("AB12CD34");
  expect(slots[7]).toHaveFocus();
  expect([slots[7].selectionStart, slots[7].selectionEnd]).toEqual([0, 1]);
  let resolve_approval!: (response: Response) => void;
  request.mockReturnValueOnce(new Promise<Response>((resolve) => { resolve_approval = resolve; }));
  fireEvent.click(within(dialog).getByRole("button", { name: "Approve CLI login" }));
  slots.forEach((input) => expect(input).toBeDisabled());
  expect(within(dialog).getByRole("button", { name: "Approving…" })).toBeDisabled();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/device/approve"), expect.objectContaining({
    body: JSON.stringify({ user_code: "AB12-CD34" }),
  }));
  resolve_approval(new Response(null, { status: 204 }));
  expect(await within(dialog).findByText("CLI login approved. Return to your terminal.")).toBeInTheDocument();
});

it("keeps pending key creation open and contains its one-time secret until dismissal", async () => {
  const request = mockAccount({ signed_in: true });
  render(<HubAccount />);
  await screen.findByText("No API keys yet.");
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Deploy" } });
  let resolve_create!: (response: Response) => void;
  request.mockReturnValueOnce(new Promise<Response>((resolve) => { resolve_create = resolve; }));
  fireEvent.click(within(dialog).getByRole("button", { name: "Create API key" }));
  expect(within(dialog).getByRole("button", { name: "Close Create API key" })).toBeDisabled();
  expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeDisabled();
  const cancel = new Event("cancel", { cancelable: true });
  fireEvent(dialog, cancel);
  expect(cancel.defaultPrevented).toBe(true);
  expect(dialog).toBeInTheDocument();
  expect(within(dialog).getByLabelText("Key name")).toBeDisabled();
  resolve_create(Response.json(key, { status: 201 }));
  expect(await within(dialog).findByText(key.token)).toBeInTheDocument();
  expect(within(screen.getByRole("region", { name: "API keys" })).queryByText(key.token)).not.toBeInTheDocument();
  expect(request.mock.calls.filter(([input, init]) => String(input).endsWith("/account/keys") && init?.method === "POST")).toHaveLength(1);
  fireEvent.click(within(dialog).getByRole("button", { name: "Done" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
});

it("keeps a created key and its secret when an older empty list arrives before the refresh", async () => {
  const request = mockAccount({ signed_in: true });
  const respond = request.getMockImplementation()!;
  let resolve_initial!: (response: Response) => void;
  let resolve_refresh!: (response: Response) => void;
  const initial = new Promise<Response>((resolve) => { resolve_initial = resolve; });
  const refresh = new Promise<Response>((resolve) => { resolve_refresh = resolve; });
  let reads = 0;
  request.mockImplementation((input, init) => {
    if (String(input).endsWith("/account/keys") && init?.method !== "POST") {
      reads++;
      return reads === 1 ? initial : refresh;
    }
    return respond(input, init);
  });
  render(<HubAccount />);
  await screen.findByText("Loading keys…");
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Deploy" } });
  fireEvent.click(within(dialog).getByRole("button", { name: "Create API key" }));
  expect(await within(dialog).findByText(key.token)).toBeInTheDocument();
  await waitFor(() => expect(reads).toBe(2));

  await act(async () => { resolve_initial(Response.json({ keys: [] })); });
  expect(within(dialog).getByText(key.token)).toBeInTheDocument();
  fireEvent.click(within(dialog).getByRole("button", { name: "Done" }));
  expect(within(screen.getByRole("list", { name: "Account API keys" })).getByRole("heading", { name: "Deploy" })).toBeInTheDocument();
  expect(screen.queryByText("No API keys yet.")).not.toBeInTheDocument();
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();

  await act(async () => { resolve_refresh(Response.json({ keys: [{ ...key, token: undefined }] })); });
  expect(within(screen.getByRole("list", { name: "Account API keys" })).getAllByRole("heading", { name: "Deploy" })).toHaveLength(1);
});

it("creates a scoped key, shows its secret once, and revokes it after confirmation", async () => {
  const request = mockAccount();
  render(<HubAccount />);
  await submitEmailCode();
  const profile = await screen.findByRole("region", { name: "Profile" });
  expect(profile).toHaveTextContent(identity.username);
  expect(profile).toHaveTextContent(identity.user_id);
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Deploy" } });
  fireEvent.click(within(dialog).getByLabelText("Register machines"));
  fireEvent.click(within(dialog).getByRole("button", { name: "Create API key" }));
  expect(await within(dialog).findByText(key.token)).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/keys"), expect.objectContaining({
    method: "POST", body: JSON.stringify({ name: "Deploy", scopes: ["machines:read", "machines:register"] }),
  }));
  fireEvent.click(within(dialog).getByRole("button", { name: "Done" }));
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
  const keys = await screen.findByRole("list", { name: "Account API keys" });
  fireEvent.click(within(keys).getByRole("button", { name: "Revoke Deploy" }));
  const confirmation = screen.getByRole("dialog", { name: "Revoke API key" });
  expect(request).not.toHaveBeenCalledWith(expect.stringContaining("/keys/deployment-key"), expect.anything());
  fireEvent.click(within(confirmation).getByRole("button", { name: "Confirm revoke" }));
  expect(await screen.findByText("No API keys yet.")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Create API key" })).toHaveFocus();
});

it("shows cached profile metadata on return and clears the one-time key dialog on leaving", async () => {
  const request = mockAccount({ signed_in: true });
  const view = render(<HubAccount />);
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Deploy" } });
  fireEvent.click(within(dialog).getByRole("button", { name: "Create API key" }));
  expect(await within(dialog).findByText(key.token)).toBeVisible();
  view.rerender(<HubAccount visible={false} />);
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();

  let finish!: (response: Response) => void;
  request.mockReturnValueOnce(new Promise<Response>((resolve) => { finish = resolve; }));
  view.rerender(<HubAccount />);
  expect(screen.getByRole("heading", { name: key.name })).toBeVisible();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
  expect(request.mock.calls.filter(([url]) => String(url).endsWith("/account/browser/refresh"))).toHaveLength(1);

  await act(async () => finish(Response.json({ keys: [] })));
  expect(await screen.findByText("No API keys yet.")).toBeVisible();
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
  expect(screen.getByRole("link", { name: "Back to Machines" })).toHaveAttribute("href", "/");
});

it("recovers from a stale account action without replaying it against the active account", async () => {
  let current = identity;
  const request = mockAccount({ signed_in: true, current_identity: () => current });
  render(<HubAccount />);
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Deploy" } });
  current = other_identity;
  fireEvent.click(within(dialog).getByRole("button", { name: "Create API key" }));

  await waitFor(() => expect(screen.getByRole("region", { name: "Profile" })).toHaveTextContent(other_identity.username));
  expect(within(screen.getByRole("region", { name: "Profile" })).getByRole("status"))
    .toHaveTextContent("Your Hub account changed in another tab.");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByText(key.token)).not.toBeInTheDocument();
  const writes = request.mock.calls.filter(([input, init]) => String(input).endsWith("/account/keys") && init?.method === "POST");
  expect(writes).toHaveLength(1);
  expect(new Headers(writes[0][1]?.headers).get("X-Tokn-Account-Id")).toBe(identity.user_id);
  const reopened = await openCreateKeyDialog();
  expect(within(reopened).getByLabelText("Key name")).toHaveValue("");
});

it("restores a changed browser account when the account page regains focus", async () => {
  let current = identity;
  mockAccount({ signed_in: true, current_identity: () => current });
  render(<HubAccount />);
  const dialog = await openCreateKeyDialog();
  fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "Previous account draft" } });
  current = other_identity;
  fireEvent.focus(window);

  await waitFor(() => expect(screen.getByRole("region", { name: "Profile" })).toHaveTextContent(other_identity.username));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  const reopened = await openCreateKeyDialog();
  expect(within(reopened).getByLabelText("Key name")).toHaveValue("");
});

it("waits for logout, reports a failure, and clears the session after a successful retry", async () => {
  let resolve_failure!: (response: Response) => void;
  const failure = new Promise<Response>((resolve) => { resolve_failure = resolve; });
  const logout = vi.fn().mockReturnValueOnce(failure).mockResolvedValueOnce(new Response(null, { status: 204 }));
  const request = mockAccount({ signed_in: true, logout });
  const first = render(<HubAccount />);
  fireEvent.click(await screen.findByRole("button", { name: `Account menu for ${identity.username}` }));
  const menu = screen.getByRole("menu");
  fireEvent.click(within(menu).getByRole("menuitem", { name: "Sign out" }));
  expect(within(menu).getByRole("menuitem", { name: "Signing out…" })).toBeDisabled();
  expect(screen.getByRole("region", { name: "Profile" })).toBeInTheDocument();
  resolve_failure(Response.json({ error: "Logout unavailable" }, { status: 503 }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Logout unavailable");
  expect(screen.getByRole("region", { name: "Profile" })).toBeInTheDocument();

  fireEvent.click(within(menu).getByRole("menuitem", { name: "Sign out" }));
  expect(await screen.findByLabelText("Email address")).toBeInTheDocument();
  expect(request).toHaveBeenCalledWith(expect.stringContaining("/account/logout"), expect.objectContaining({ method: "POST" }));
  expect(logout).toHaveBeenCalledTimes(2);
  first.unmount();

  render(<HubAccount />);
  expect(await screen.findByLabelText("Email address")).toBeInTheDocument();
});

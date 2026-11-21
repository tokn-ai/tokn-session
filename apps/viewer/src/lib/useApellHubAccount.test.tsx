import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { invokeHost } from "./platform";
import { useApellHubAccount } from "./useApellHubAccount";

vi.mock("./platform", () => ({ invokeHost: vi.fn() }));

const hub = "https://hub.example";
const studio = { user_id: "44314cf9-9fc9-486a-9652-a280ba614422", username: "studio" };
const machine = {
  host_id: "c16337e3-ed65-47cd-9556-4a98ab2046e2", machine_address: "studio:build",
  name: "Build server", online: true,
};
const user_code = "ABCD-2345";

function challenge(origin = hub) {
  return {
    user_code, verification_url: `${origin}/account?user_code=${user_code}&device=viewer`,
    expires_at: Math.floor(Date.now() / 1000) + 600, interval_ms: 5_000,
  };
}

afterEach(() => { cleanup(); vi.useRealTimers(); vi.resetAllMocks(); });

it("approves an Apell device, shows the account machines, and clears them on sign-out", async () => {
  vi.useFakeTimers();
  let signed_in = false;
  vi.mocked(invokeHost).mockImplementation(async (command) => {
    switch (command) {
      case "hub_account_status": return { supported: true, account: signed_in ? studio : undefined };
      case "hub_account_login_start": return challenge();
      case "hub_account_login_poll": signed_in = true; return { account: studio, interval_ms: 5_000 };
      case "hub_account_machines": return { account: studio, machines: [machine] };
      case "hub_account_logout": signed_in = false; return null;
      default: throw new Error(`Unexpected guest command: ${command}`);
    }
  });
  const { result } = renderHook(() => useApellHubAccount(hub));
  await act(async () => {});
  expect(result.current.supported).toBe(true);
  await act(async () => { await result.current.startSignIn(); });
  expect(result.current.challenge?.user_code).toBe(user_code);
  const start = vi.mocked(invokeHost).mock.calls.find(([command]) => command === "hub_account_login_start");
  expect(start?.[1]).toEqual({ hub_url: hub, attempt_id: expect.any(String) });
  await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
  expect(invokeHost).toHaveBeenCalledWith("hub_account_login_poll", start?.[1]);
  expect(result.current.challenge).toBeUndefined();
  expect(result.current.session).toEqual(studio);
  expect(result.current.machines).toEqual([machine]);
  await act(async () => { await result.current.logout(); });
  expect(result.current.session).toBeUndefined();
  expect(result.current.machines).toEqual([]);
});

it("drops a late account roster after switching Hubs", async () => {
  let finish!: (value: unknown) => void;
  const delayed = new Promise((resolve) => { finish = resolve; });
  vi.mocked(invokeHost).mockImplementation(async (command, params) => {
    if (command === "hub_account_machines") return delayed;
    if (command === "hub_account_status") return params?.hub_url === hub
      ? { supported: true, account: studio } : { supported: false };
    throw new Error(`Unexpected guest command: ${command}`);
  });
  const { result, rerender } = renderHook(({ url }) => useApellHubAccount(url), { initialProps: { url: hub } });
  await waitFor(() => expect(result.current.session).toEqual(studio));
  rerender({ url: "https://other.example" });
  expect(result.current.session).toBeUndefined();
  await act(async () => { finish({ account: studio, machines: [machine] }); });
  expect(result.current.supported).toBe(false);
  expect(result.current.machines).toEqual([]);
});

it("rejects an approval link for another origin, cancels its attempt, and never polls", async () => {
  vi.mocked(invokeHost).mockImplementation(async (command) => {
    if (command === "hub_account_status") return { supported: true };
    if (command === "hub_account_login_start") return challenge("https://other.example");
    if (command === "hub_account_login_cancel") return null;
    throw new Error(`Unexpected guest command: ${command}`);
  });
  const { result } = renderHook(() => useApellHubAccount(hub));
  await waitFor(() => expect(result.current.supported).toBe(true));
  await act(async () => { await result.current.startSignIn(); });
  expect(result.current.sign_in_error).toMatch(/invalid sign-in request/i);
  expect(result.current.challenge).toBeUndefined();
  expect(invokeHost).toHaveBeenCalledWith("hub_account_login_cancel", { hub_url: hub, attempt_id: expect.any(String) });
  expect(invokeHost).not.toHaveBeenCalledWith("hub_account_login_poll", expect.anything());
});

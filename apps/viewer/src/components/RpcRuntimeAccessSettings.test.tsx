import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RpcRuntimeAccessSettings } from "./RpcRuntimeAccessSettings";
import { getRpcRuntimeAccess, setRpcRuntimeAccess } from "../lib/tauri";
vi.mock("../lib/tauri", () => ({ getRpcRuntimeAccess: vi.fn(), setRpcRuntimeAccess: vi.fn() }));
beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);
it("requires an explicit local opt-in and keeps the authoritative value after failure", async () => {
  vi.mocked(getRpcRuntimeAccess).mockResolvedValue({ available: true, enabled: false, reason: null });
  vi.mocked(setRpcRuntimeAccess).mockRejectedValueOnce(new Error("Host settings could not be saved"))
    .mockResolvedValueOnce({ available: true, enabled: true, reason: null });
  const view = render(<RpcRuntimeAccessSettings active={false} />);
  expect(getRpcRuntimeAccess).not.toHaveBeenCalled();
  view.rerender(<RpcRuntimeAccessSettings active />);
  const toggle = screen.getByRole("checkbox", { name: "Remote runtime management" });
  await waitFor(() => expect(toggle).toBeEnabled());
  expect(toggle).not.toBeChecked();
  fireEvent.click(toggle);
  await screen.findByText("Host settings could not be saved");
  expect(toggle).not.toBeChecked();
  fireEvent.click(toggle);
  await waitFor(() => expect(toggle).toBeChecked());
  expect(setRpcRuntimeAccess).toHaveBeenLastCalledWith({ enabled: true });
});
it("explains why the hosting capability is unavailable", async () => {
  vi.mocked(getRpcRuntimeAccess).mockResolvedValue({ available: false, enabled: false, reason: "Configure this computer as a host first." });
  render(<RpcRuntimeAccessSettings active />);
  await screen.findByText("Configure this computer as a host first.");
  expect(screen.getByRole("checkbox")).toBeDisabled();
});

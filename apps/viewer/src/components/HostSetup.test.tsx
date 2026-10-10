import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RemoteConnection } from "./RemoteConnection";
import { LocalHostProvider } from "./LocalHostProvider";
import { HostSetup, HostQuickControl } from "./HostSetup";
import { getLocalHostPairing, getLocalHostStatus, listenForLocalHostStatus, startLocalHost, stopLocalHost, stopExternalLocalHost } from "../lib/tauri";
import type { LocalHostStatus } from "../lib/types";
vi.mock("../lib/tauri", () => ({ getLocalHostPairing: vi.fn(), getLocalHostStatus: vi.fn(), listenForLocalHostStatus: vi.fn(), startLocalHost: vi.fn(), stopLocalHost: vi.fn(), stopExternalLocalHost: vi.fn() }));
const stopped: LocalHostStatus = { phase: "stopped", hub_url: "https://hub.example", name: "Workstation", allow_control: false, machine_reference: "machine@key", error: null };
const online: LocalHostStatus = { ...stopped, phase: "online" };
let status_event: (status: LocalHostStatus) => void;
let unlisten = vi.fn<() => void>();
beforeEach(() => {
  unlisten = vi.fn<() => void>();
  vi.mocked(listenForLocalHostStatus).mockImplementation(async (callback) => { status_event = callback; return unlisten; });
  vi.mocked(getLocalHostStatus).mockResolvedValue(stopped);
  vi.mocked(startLocalHost).mockResolvedValue(online);
  vi.mocked(stopLocalHost).mockResolvedValue(stopped);
  vi.mocked(stopExternalLocalHost).mockResolvedValue(stopped);
  vi.mocked(getLocalHostPairing).mockResolvedValue({ machine_reference: "machine@key", qr_data_url: "data:image/svg+xml;base64,test", setup_uri: "otpauth://totp/test?secret=PRIVATE&algorithm=SHA256", current_code: "123456", host_time: 100, expires_at: 130 });
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.clearAllMocks(); localStorage.clear(); });

it("starts and stops hosting without loading or persisting pairing secrets", async () => {
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByText("Hosting off");
  expect(getLocalHostPairing).not.toHaveBeenCalled();
  expect(screen.getByLabelText("Allow remote agent input")).not.toBeChecked();
  fireEvent.click(screen.getByRole("button", { name: "Start hosting", hidden: true }));
  await waitFor(() => expect(startLocalHost).toHaveBeenCalledWith({ hub_url: "https://hub.example", name: "Workstation", allow_control: false }));
  await screen.findByText("Hosting online");
  fireEvent.click(screen.getByRole("button", { name: "Stop hosting", hidden: true }));
  await screen.findByText("Hosting off");
  expect(stopLocalHost).toHaveBeenCalledOnce(); expect(localStorage.length).toBe(0);
});
it("loads QR and current code on disclosure and clears them when hidden", async () => {
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" })); await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Show pairing setup", hidden: true }));
  await screen.findByText("123456");
  expect(screen.getByAltText("SHA-256 authenticator setup QR")).toHaveAttribute("src", "data:image/svg+xml;base64,test");
  expect(screen.getByText("Valid for 30s")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Hide pairing setup", hidden: true }));
  expect(screen.queryByText("123456")).not.toBeInTheDocument();
  expect(screen.queryByAltText("SHA-256 authenticator setup QR")).not.toBeInTheDocument();
});
it("preserves newer connection events over a late start response", async () => {
  let finish!: (status: LocalHostStatus) => void;
  vi.mocked(startLocalHost).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" })); await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Start hosting", hidden: true }));
  await waitFor(() => expect(startLocalHost).toHaveBeenCalled());
  status_event(online); finish({ ...stopped, phase: "connecting" });
  await screen.findByText("Hosting online");
  expect(screen.queryByText("Connecting to Hub…")).not.toBeInTheDocument();
});
it("reports external-connector conflicts without starting another flow", async () => {
  vi.mocked(startLocalHost).mockRejectedValue(new Error("Another connector is already hosting this machine"));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" })); await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Start hosting", hidden: true }));
  expect(await screen.findByRole("alert", { hidden: true })).toHaveTextContent("Another connector");
  expect(stopLocalHost).not.toHaveBeenCalled(); expect(getLocalHostPairing).not.toHaveBeenCalled();
});
it("unsubscribes in StrictMode and ignores pairing responses after the disclosure closes", async () => {
  let finish!: (pairing: Awaited<ReturnType<typeof getLocalHostPairing>>) => void;
  vi.mocked(getLocalHostPairing).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  const { unmount } = render(<StrictMode><LocalHostProvider><HostSetup /></LocalHostProvider></StrictMode>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Show pairing setup", hidden: true }));
  await waitFor(() => expect(getLocalHostPairing).toHaveBeenCalled());
  fireEvent.click(screen.getByRole("button", { name: "Hide pairing setup", hidden: true }));
  finish({ machine_reference: "machine@key", qr_data_url: "secret", setup_uri: "secret", current_code: "654321", host_time: 100, expires_at: 130 });
  await waitFor(() => expect(screen.queryByText("654321")).not.toBeInTheDocument());
  unmount(); expect(unlisten).toHaveBeenCalledTimes(2);
});

it("refreshes verification codes at the host boundary without relying on the client clock", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "performance"] });
  const initial = { machine_reference: "machine@key", qr_data_url: "data:image/svg+xml;base64,test", setup_uri: "private", current_code: "123456", host_time: 100, expires_at: 130 };
  vi.mocked(getLocalHostPairing).mockResolvedValueOnce(initial).mockResolvedValue({ ...initial, current_code: "654321", host_time: 130, expires_at: 160 });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" })); await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Show pairing setup", hidden: true }));
  await screen.findByText("123456");
  await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
  expect(screen.getByText("654321")).toBeInTheDocument();
  expect(getLocalHostPairing).toHaveBeenCalledTimes(2);
  fireEvent.click(screen.getByRole("button", { name: "Hide pairing setup", hidden: true }));
  await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
  expect(getLocalHostPairing).toHaveBeenCalledTimes(2);
});

it("shares live status with the quick switch and uses saved settings", async () => {
  const close = vi.fn();
  render(<LocalHostProvider><HostSetup /><HostQuickControl on_open_settings={close} /></LocalHostProvider>);
  const toggle = await screen.findByRole("switch", { name: "Host this computer" });
  await waitFor(() => expect(toggle).toBeEnabled());
  fireEvent.click(toggle);
  await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "true"));
  expect(startLocalHost).toHaveBeenCalledWith({ hub_url: stopped.hub_url, name: stopped.name, allow_control: false });
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  expect(screen.queryByRole("button", { name: "Hosting settings" })).not.toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Host this computer" })).toHaveTextContent("Hosting online");
  expect(screen.queryByLabelText("Host name")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Close hosting settings" }));
  fireEvent.click(toggle);
  await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "false"));
});
it("opens setup from the switch when configuration is missing", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, hub_url: "", machine_reference: null });
  render(<LocalHostProvider><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  const toggle = await screen.findByRole("switch");
  await waitFor(() => expect(toggle).toBeEnabled());
  fireEvent.click(toggle);
  expect(screen.getByRole("dialog")).toBeInTheDocument();
  expect(startLocalHost).not.toHaveBeenCalled();
});
it("clears pairing secrets on dialog dismissal and restores focus", async () => {
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  const trigger = screen.getByRole("button", { name: "Host this computer" });
  trigger.focus(); fireEvent.click(trigger);
  await screen.findByText("Hosting off");
  fireEvent.click(screen.getByRole("button", { name: "Show pairing setup" }));
  await screen.findByText("123456");
  fireEvent(screen.getByRole("dialog"), new Event("cancel", { bubbles: true, cancelable: true }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByText("123456")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
  fireEvent.click(trigger);
  expect(screen.queryByText("123456")).not.toBeInTheDocument();
});

it("labels the local host separately while viewing a remote machine", async () => {
  render(<LocalHostProvider><RemoteConnection name="Remote workstation" state="connected"><button>Machines</button></RemoteConnection></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: /Remote workstation.*Connection settings/ }));
  expect(screen.getByRole("region", { name: "This computer hosting" })).toHaveTextContent("This computer");
  const toggle = screen.getByRole("switch");
  await waitFor(() => expect(toggle).toBeEnabled());
  fireEvent.click(toggle);
  await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "true"));
  expect(screen.queryByRole("button", { name: "Hosting settings" })).not.toBeInTheDocument();
});
it("does not expose local hosting on a browser connection", () => {
  render(<RemoteConnection name="Browser machine" state="connected"><button>Machines</button></RemoteConnection>);
  fireEvent.click(screen.getByRole("button", { name: /Connection settings/ }));
  expect(screen.queryByRole("switch")).not.toBeInTheDocument();
  expect(getLocalHostStatus).not.toHaveBeenCalled();
});

it("shows external hosting and disables app controls without trying to stop it", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...online, phase: "external" });
  render(<LocalHostProvider><HostSetup /><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  const toggle = await screen.findByRole("switch");
  await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "true"));
  expect(toggle).toBeDisabled();
  expect(screen.getByText("Hosting externally")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  expect(screen.queryByRole("button", { name: "Start hosting" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Stop hosting" })).not.toBeInTheDocument();
  expect(screen.getByRole("dialog")).toHaveTextContent("Stop it outside the app");
  expect(screen.getByRole("button", { name: "Stop external hosting" })).toBeDisabled();
  expect(startLocalHost).not.toHaveBeenCalled(); expect(stopLocalHost).not.toHaveBeenCalled();
});
it("refreshes external ownership on focus and keeps unavailable status distinct from off", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValueOnce({ ...online, phase: "external" }).mockResolvedValue(stopped);
  render(<LocalHostProvider><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  await screen.findByText("Hosting externally");
  fireEvent(window, new Event("focus"));
  await screen.findByText("Hosting off");
  expect(screen.getByRole("switch")).toBeEnabled();
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, phase: "unavailable", error: "Hub check timed out" });
  fireEvent(window, new Event("focus"));
  await screen.findByText("Hosting status unavailable");
  expect(screen.getByRole("switch")).toBeDisabled();
});

it("updates a rejected start to external ownership instead of retaining the conflict alert", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValueOnce(stopped).mockResolvedValue({ ...online, phase: "external" });
  vi.mocked(startLocalHost).mockRejectedValue(new Error("Another connector is already hosting"));
  render(<LocalHostProvider><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  const toggle = await screen.findByRole("switch");
  await waitFor(() => expect(toggle).toBeEnabled());
  fireEvent.click(toggle);
  await screen.findByText("Hosting externally");
  expect(toggle).toBeDisabled();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("stops a verified external service only from the floating dialog", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...online, phase: "external", external_stop_supported: true });
  render(<LocalHostProvider><HostSetup /><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  await screen.findByText("Hosting externally");
  expect(screen.queryByRole("button", { name: "Stop external hosting" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(screen.getByRole("button", { name: "Stop external hosting" }));
  await waitFor(() => expect(stopExternalLocalHost).toHaveBeenCalledOnce());
  await screen.findByRole("button", { name: "Start hosting" });
  expect(stopLocalHost).not.toHaveBeenCalled();
  expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "false");
});
it("preserves external hosting status when stopping its service fails", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...online, phase: "external", external_stop_supported: true });
  vi.mocked(stopExternalLocalHost).mockRejectedValue(new Error("Service identity changed"));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByText("Hosting externally");
  fireEvent.click(screen.getByRole("button", { name: "Stop external hosting" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Service identity changed");
  expect(screen.getByText("Hosting externally")).toBeInTheDocument();
});

import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RemoteConnection } from "./RemoteConnection";
import { LocalHostProvider } from "./LocalHostProvider";
import { HostSetup, HostQuickControl } from "./HostSetup";
import { getLocalHostPairing, getLocalHostStatus, listenForLocalHostStatus, startLocalHost, stopLocalHost, stopExternalLocalHost, removeExternalLocalHost, renameLocalHost, openLocalHostLogin } from "../lib/tauri";
import type { LocalHostStatus } from "../lib/types";
vi.mock("../lib/tauri", () => ({ getLocalHostPairing: vi.fn(), getLocalHostStatus: vi.fn(), listenForLocalHostStatus: vi.fn(), startLocalHost: vi.fn(), stopLocalHost: vi.fn(), stopExternalLocalHost: vi.fn(), removeExternalLocalHost: vi.fn(), renameLocalHost: vi.fn(), openLocalHostLogin: vi.fn() }));
const stopped: LocalHostStatus = { phase: "stopped", hub_url: "https://hub.example", name: "Workstation", allow_control: false, machine_reference: "machine@key", machine_address: "alice:workstation", error: null };
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
  vi.mocked(removeExternalLocalHost).mockResolvedValue(stopped);
  vi.mocked(renameLocalHost).mockResolvedValue({ ...stopped, machine_address: "alice:studio", machine_name: "studio" });
  vi.mocked(openLocalHostLogin).mockResolvedValue();
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

it("opens hosting setup for an unclaimed machine without requiring a registration command", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, machine_address: null });
  render(<LocalHostProvider><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  const toggle = await screen.findByRole("switch");
  await waitFor(() => expect(toggle).toBeEnabled());
  fireEvent.click(toggle);
  expect(await screen.findByLabelText("Machine name")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Start hosting", hidden: true })).toBeEnabled();
  expect(screen.queryByText(/tokn-session-hub/)).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Show pairing setup", hidden: true })).not.toBeInTheDocument();
  expect(startLocalHost).not.toHaveBeenCalled();
});

it("automatically signs in and registers the chosen Hub and machine name on Start", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, hub_url: "", machine_address: null, machine_reference: null });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByLabelText("Machine name");
  const hub_input = screen.getByLabelText("Hosting Hub address");
  fireEvent.change(hub_input, { target: { value: "https://hub.example/" } });
  fireEvent.change(screen.getByLabelText("Machine name"), { target: { value: "studio" } });
  fireEvent.click(screen.getByRole("button", { name: "Start hosting", hidden: true }));
  await waitFor(() => expect(startLocalHost).toHaveBeenCalledWith({ hub_url: "https://hub.example/", name: "Workstation", machine_name: "studio", allow_control: false }));
  expect(await screen.findByText("Hosting online")).toBeInTheDocument();
});

it.each(["", "https://hub.example/path", "http://hub.example"])("blocks hosting for an invalid Hub address %j", async (hub_url) => {
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByText("Hosting off");
  const start = screen.getByRole<HTMLButtonElement>("button", { name: "Start hosting", hidden: true });
  expect(start).toBeEnabled();
  fireEvent.change(screen.getByLabelText("Hosting Hub address"), { target: { value: hub_url } });
  expect(start).toBeDisabled();
  fireEvent.submit(start.form!);
  expect(startLocalHost).not.toHaveBeenCalled();
});

it.each([
  ["online", "Hosting online"],
  ["external", "Hosting externally"],
] as const)("shows the saved address when hosting becomes %s during a Hub edit", async (phase, label) => {
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  await screen.findByText("Hosting off");
  fireEvent.change(screen.getByLabelText("Hosting Hub address"), { target: { value: "https://another.example" } });
  expect(screen.getByLabelText("Machine name")).toBeInTheDocument();
  act(() => status_event({ ...online, phase }));
  expect(await screen.findByText(label)).toBeInTheDocument();
  expect(screen.getByText(stopped.hub_url)).toBeInTheDocument();
  expect(screen.getByText("alice:workstation")).toBeInTheDocument();
  expect(screen.queryByLabelText("Machine name")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Start hosting", hidden: true })).not.toBeInTheDocument();
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
it("shows browser approval and lets cancellation supersede a pending start", async () => {
  let reject_start!: (error: Error) => void;
  const unclaimed = { ...stopped, machine_address: null, machine_reference: null };
  vi.mocked(getLocalHostStatus).mockResolvedValue(unclaimed);
  vi.mocked(stopLocalHost).mockResolvedValue(unclaimed);
  vi.mocked(startLocalHost).mockImplementation(() => new Promise((_, reject) => { reject_start = reject; }));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(await screen.findByRole("button", { name: "Start hosting" }));
  await waitFor(() => expect(startLocalHost).toHaveBeenCalledOnce());
  act(() => status_event({ ...unclaimed, phase: "signing_in", login: { verification_url: "https://hub.example/account?user_code=ABCD-1234", user_code: "ABCD-1234", expires_at: 1234567890 } }));
  expect(await screen.findByText("ABCD-1234")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Open sign-in page" }));
  await waitFor(() => expect(openLocalHostLogin).toHaveBeenCalledOnce());
  expect(screen.getByRole("button", { name: "Cancel" })).toBeEnabled();
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  await screen.findByText("Hosting off");
  expect(stopLocalHost).toHaveBeenCalledOnce();
  await act(async () => reject_start(new Error("Sign-in canceled")));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.queryByText("ABCD-1234")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Start hosting" })).toBeEnabled();
});
it("clears browser approval when registration starts and preserves the final live status", async () => {
  let finish!: (status: LocalHostStatus) => void;
  vi.mocked(startLocalHost).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(await screen.findByRole("button", { name: "Start hosting" }));
  await waitFor(() => expect(startLocalHost).toHaveBeenCalledOnce());
  act(() => status_event({ ...stopped, phase: "signing_in", login: { verification_url: "https://hub.example/account?user_code=ABCD-1234", user_code: "ABCD-1234", expires_at: 1234567890 } }));
  await screen.findByText("ABCD-1234");
  act(() => status_event({ ...stopped, phase: "registering", login: null }));
  expect(await screen.findByText("Registering this computer…")).toBeInTheDocument();
  expect(screen.queryByText("ABCD-1234")).not.toBeInTheDocument();
  await act(async () => { status_event(online); finish({ ...stopped, phase: "connecting" }); });
  expect(await screen.findByText("Hosting online")).toBeInTheDocument();
});
it("renames the account address while preserving the paired machine", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue(online);
  vi.mocked(renameLocalHost).mockResolvedValue({ ...online, machine_address: "alice:studio", machine_name: "studio" });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(await screen.findByRole("button", { name: "Rename" }));
  const name = screen.getByLabelText("New machine name");
  expect(name).toHaveValue("workstation");
  fireEvent.change(name, { target: { value: "studio" } });
  fireEvent.click(screen.getByRole("button", { name: "Save name" }));
  expect(await screen.findByText("alice:studio")).toBeInTheDocument();
  expect(renameLocalHost).toHaveBeenCalledWith({ machine_name: "studio" });
  expect(startLocalHost).not.toHaveBeenCalled();
  expect(stopLocalHost).not.toHaveBeenCalled();
  expect(getLocalHostPairing).not.toHaveBeenCalled();
});
it("keeps the rename draft and existing address when the requested name is taken", async () => {
  vi.mocked(renameLocalHost).mockRejectedValue(new Error("Machine name is already reserved"));
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(await screen.findByRole("button", { name: "Rename" }));
  fireEvent.change(screen.getByLabelText("New machine name"), { target: { value: "studio" } });
  fireEvent.click(screen.getByRole("button", { name: "Save name" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Machine name is already reserved");
  expect(screen.getByLabelText("New machine name")).toHaveValue("studio");
  expect(screen.getByText("alice:workstation")).toBeInTheDocument();
});
it("disables rename for an externally owned connector", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...online, phase: "external" });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  expect(await screen.findByRole("button", { name: "Rename" })).toBeDisabled();
  expect(screen.getByText("Stop external hosting before renaming this computer.")).toBeInTheDocument();
  expect(renameLocalHost).not.toHaveBeenCalled();
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
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, hub_url: "", machine_address: null, machine_reference: null });
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
  expect(screen.getByRole("dialog")).toHaveTextContent("Manage them outside the app");
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

const installedService = { registered: true, loaded: false, running: false, verified: true, error: null };
const absentService = { registered: false, loaded: false, running: false, verified: false, error: null };
it("detects and stops an orphaned API without presenting app hosting as online", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, phase: "background", external_stop_supported: true,
    external_remove_supported: true, background_services: { api: { ...installedService, loaded: true, running: true }, connector: installedService } });
  render(<LocalHostProvider><HostSetup /><HostQuickControl on_open_settings={() => {}} /></LocalHostProvider>);
  await screen.findByText("Background API running");
  expect(screen.getByRole("switch")).toBeDisabled();
  expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "false");
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  expect(screen.getByRole("region", { name: "Background hosting services" })).toHaveTextContent("Session APIRunning");
  fireEvent.click(screen.getByRole("button", { name: "Stop external hosting" }));
  await waitFor(() => expect(stopExternalLocalHost).toHaveBeenCalledOnce());
  await screen.findByRole("button", { name: "Start hosting" });
  expect(startLocalHost).not.toHaveBeenCalled();
});
it("removes stopped login registrations only after explicit confirmation", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, external_remove_supported: true,
    background_services: { api: installedService, connector: installedService } });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  fireEvent.click(await screen.findByRole("button", { name: "Remove background hosting" }));
  expect(removeExternalLocalHost).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Cancel removal" }));
  expect(removeExternalLocalHost).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Remove background hosting" }));
  fireEvent.click(screen.getByRole("button", { name: "Remove registrations" }));
  await waitFor(() => expect(removeExternalLocalHost).toHaveBeenCalledOnce());
  await waitFor(() => expect(screen.queryByRole("region", { name: "Background hosting services" })).not.toBeInTheDocument());
});
it("does not offer removal of an unverified leftover API", async () => {
  vi.mocked(getLocalHostStatus).mockResolvedValue({ ...stopped, phase: "background",
    background_services: { api: { ...installedService, loaded: true, running: true, verified: false, error: "Unknown service definition" }, connector: absentService } });
  render(<LocalHostProvider><HostSetup /></LocalHostProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Host this computer" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Unknown service definition");
  expect(screen.getByRole("button", { name: "Stop external hosting" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Remove background hosting" })).toBeDisabled();
});

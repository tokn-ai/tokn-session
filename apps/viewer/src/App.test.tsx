import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import App from "./App";
import { RemoteClient } from "./lib/transport";
import { StrictMode, type ReactNode } from "react";
vi.mock("./components/HubAccess", () => ({ HubAccess: () => <div>Machine access</div> }));
vi.mock("./pages/ViewerPage", () => ({ ViewerPage: ({ remote, connection }: { remote?: boolean; connection?: ReactNode }) => <><div>{remote ? "Remote sessions" : "Desktop sessions"}</div>{connection}</> }));
beforeEach(() => { vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 404 })); });
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
it("automatically connects to its own origin and closes the abandoned StrictMode connection", async () => {
  const stale = new RemoteClient(window.location.origin, "login-secret");
  const active = new RemoteClient(window.location.origin, "login-secret");
  const close = vi.spyOn(stale, "close");
  const connect = vi.spyOn(RemoteClient, "connect").mockResolvedValueOnce(stale).mockResolvedValueOnce(active);
  render(<StrictMode><App initial_token="login-secret" /></StrictMode>);
  expect(await screen.findByText("Remote sessions")).toBeInTheDocument();
  expect(connect).toHaveBeenCalledWith(window.location.origin, "login-secret");
  expect(close).toHaveBeenCalledOnce();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Change machine" }));
  expect(screen.getByLabelText("Access token")).toHaveValue("");
  expect(connect).toHaveBeenCalledTimes(2);
});

it("returns to manual login when the login link is rejected", async () => {
  vi.spyOn(RemoteClient, "connect").mockRejectedValue(new Error("Invalid viewer API token"));
  render(<App initial_token="expired-secret" />);
  expect(await screen.findByRole("alert")).toHaveTextContent("Invalid viewer API token");
  expect(screen.getByRole("button", { name: "Connect" })).toBeEnabled();
  expect(screen.getByLabelText("Access token")).toHaveValue("");
});
it("reports connection failures, retries, and disconnects before switching machines", async () => {
  const client = new RemoteClient("http://selected-machine:5558", "secret");
  const close = vi.spyOn(client, "close");
  vi.spyOn(RemoteClient, "connect").mockRejectedValueOnce(new Error("Invalid viewer API token")).mockResolvedValueOnce(client);
  render(<App />);
  expect(await screen.findByRole("textbox", { name: "Viewer address" })).toHaveValue(window.location.origin);
  fireEvent.click(screen.getByRole("button", { name: "Connect" }));
  expect(RemoteClient.connect).toHaveBeenCalledWith(window.location.origin, "");
  expect(await screen.findByRole("alert")).toHaveTextContent("Invalid viewer API token");
  fireEvent.click(screen.getByRole("button", { name: "Connect" }));
  expect(await screen.findByText("Remote sessions")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  fireEvent.click(screen.getByRole("button", { name: "Change machine" }));
  await waitFor(() => expect(close).toHaveBeenCalled());
  expect(screen.getByRole("button", { name: "Connect" })).toBeInTheDocument();
  expect(screen.queryByText("Remote sessions")).not.toBeInTheDocument();
});

it("detects Hub machine access and retries failed discovery without exposing direct login", async () => {
  vi.mocked(fetch).mockRejectedValueOnce(new Error("Network unavailable")).mockResolvedValueOnce(Response.json({ configured: true, authenticated: false }));
  render(<App />);
  expect(await screen.findByRole("alert")).toHaveTextContent("Network unavailable");
  expect(screen.queryByLabelText("Access token")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Retry connection" }));
  expect(await screen.findByText("Machine access")).toBeInTheDocument();
});

it("keeps Hub administrator login separate from machine access", async () => {
  window.history.replaceState(null, "", "/admin");
  vi.mocked(fetch).mockResolvedValue(Response.json({ configured: true, authenticated: false }));
  render(<App />);
  expect(await screen.findByRole("heading", { name: "Hub administration" })).toBeInTheDocument();
  expect(screen.queryByText("Machine access")).not.toBeInTheDocument();
  window.history.replaceState(null, "", "/");
});

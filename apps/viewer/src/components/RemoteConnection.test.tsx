import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { RemoteConnection } from "./RemoteConnection";

afterEach(cleanup);

it("keeps host identity visible and shows current recovery state and actions on demand", () => {
  const change_host = vi.fn();
  const view = (state: "connected" | "reconnecting") => <RemoteConnection name="alice:workstation" hub_url="https://hub.example" encrypted state={state}>
    <button onClick={change_host}>Change host</button>
  </RemoteConnection>;
  const { rerender } = render(view("connected"));
  expect(screen.getByText("Connected · alice:workstation")).toBeInTheDocument();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  rerender(view("reconnecting"));
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  expect(screen.getByRole("dialog", { name: "Connection" })).toHaveFocus();
  expect(screen.getByText("Reconnecting · showing last received data")).toBeInTheDocument();
  expect(screen.getByRole("dialog")).toHaveTextContent("https://hub.example");
  expect(screen.getByRole("dialog")).toHaveTextContent("End-to-end encrypted");
  fireEvent.click(screen.getByRole("button", { name: "Change host" }));
  expect(change_host).toHaveBeenCalledOnce();
  fireEvent.keyDown(document, { key: "Escape" });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: /connection settings/i })).toHaveFocus();
});

it("shows the encrypted traffic path and the relay fallback reason", () => {
  const view = (kind: "direct" | "relay", reason?: string) => <RemoteConnection name="alice:workstation" hub_url="https://hub.example" encrypted state="connected" transport={{ kind, reason }}>
    <button>Machines</button>
  </RemoteConnection>;
  const { rerender } = render(view("direct"));
  expect(screen.getByText("Direct · alice:workstation")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /connection settings/i }));
  expect(screen.getByRole("dialog")).toHaveTextContent("Direct · encrypted");
  rerender(view("relay", "Direct connection was interrupted."));
  expect(screen.getByText("Relayed · alice:workstation")).toBeInTheDocument();
  expect(screen.getByRole("dialog")).toHaveTextContent("Relayed · encrypted");
  expect(screen.getByRole("dialog")).toHaveTextContent("Direct connection was interrupted.");
});

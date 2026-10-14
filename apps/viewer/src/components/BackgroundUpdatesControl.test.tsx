import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { BackgroundUpdatesScope } from "../lib/types";
import { BackgroundUpdatesControl } from "./BackgroundUpdatesControl";

afterEach(cleanup);

function renderControl(scope: BackgroundUpdatesScope = "all", status: string | null = null, on_change = vi.fn()) {
  function Harness() {
    const [current, setCurrent] = useState(scope);
    return <div className="status-region">
      <button type="button">Outside</button>
      <BackgroundUpdatesControl scope={current} status={status} on_scope_change={(next) => { on_change(next); setCurrent(next); }} />
    </div>;
  }
  render(<Harness />);
  return on_change;
}

describe("BackgroundUpdatesControl", () => {
  it("keeps scope controls in an upward status popup with a compact live trigger", () => {
    renderControl();
    const trigger = screen.getByRole("button", { name: "Background updates. All sessions. Live. Change scope" });
    expect(trigger).toHaveTextContent("Updates · All");
    expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    fireEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Background updates" });
    expect(dialog).toHaveFocus();
    expect(dialog).toHaveAttribute("aria-modal", "false");
    expect(dialog).toHaveAttribute("id", trigger.getAttribute("aria-controls"));
    expect(within(dialog).getByRole("radio", { name: "All sessions" })).toBeChecked();
    expect(within(dialog).getByRole("radio", { name: "Sidebar sessions" })).not.toBeChecked();
    expect(within(dialog).queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
    expect(within(dialog).getByText("Receiving live background updates.")).toBeInTheDocument();
  });

  it("applies native radio changes immediately while keeping the popup available", () => {
    const change = renderControl();
    fireEvent.click(screen.getByRole("button", { name: /Background updates/ }));
    const all = screen.getByRole("radio", { name: "All sessions" });
    const sidebar = screen.getByRole("radio", { name: "Sidebar sessions" });
    expect(all).toHaveAttribute("type", "radio");
    expect(sidebar).toHaveAttribute("name", all.getAttribute("name"));
    expect(sidebar).toHaveAccessibleDescription("Receive updates for sessions loaded in the sidebar.");
    fireEvent.click(sidebar);
    expect(change).toHaveBeenCalledExactlyOnceWith("sidebar");
    expect(sidebar).toBeChecked();
    expect(all).not.toBeChecked();
    expect(screen.getByRole("dialog", { name: "Background updates" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Background updates. Sidebar sessions. Live. Change scope" })).toHaveTextContent("Updates · Sidebar");
  });

  it("restores the chosen scope and closes on Escape or outside click with trigger focus", () => {
    renderControl("sidebar");
    const trigger = screen.getByRole("button", { name: /Sidebar sessions/ });
    fireEvent.click(trigger);
    expect(screen.getByRole("radio", { name: "Sidebar sessions" })).toBeChecked();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
    fireEvent.click(trigger);
    fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("lets keyboard focus leave the popup without returning it to the trigger", () => {
    renderControl();
    fireEvent.click(screen.getByRole("button", { name: /Background updates/ }));
    const outside = screen.getByRole("button", { name: "Outside" });
    act(() => outside.focus());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(outside).toHaveFocus();
  });

  it("supports the explicit close control", () => {
    renderControl();
    const trigger = screen.getByRole("button", { name: /Background updates/ });
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("button", { name: "Close background updates" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it.each([
    { status: "Connecting background updates…", phase: "Connecting", tone: "active", spinner: true },
    { status: "Background updates unavailable on this host. Open sessions still update.", phase: "Unavailable", tone: "warning", spinner: false },
  ])("shows $phase in the trigger and keeps detailed status inside the popup", ({ status, phase, tone, spinner }) => {
    renderControl("all", status);
    const trigger = screen.getByRole("button", { name: `Background updates. All sessions. ${phase}. Change scope` });
    expect(trigger).toHaveTextContent("Updates · All");
    expect(trigger).toHaveAttribute("data-tone", tone);
    expect(!!trigger.querySelector(".status-bar__spinner")).toBe(spinner);
    expect(screen.queryByText(status)).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    fireEvent.click(trigger);
    expect(within(screen.getByRole("dialog")).getByText(status)).toBeInTheDocument();
  });
});

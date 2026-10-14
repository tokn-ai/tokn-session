import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { BackgroundUpdateLevel, BackgroundUpdatesScope, ConversationUpdateLevel } from "../lib/types";
import { BackgroundUpdatesControl } from "./BackgroundUpdatesControl";

afterEach(cleanup);

function renderControl(
  scope: BackgroundUpdatesScope = "all",
  status: string | null = null,
  on_change = vi.fn(),
  level: BackgroundUpdateLevel = "messages",
  on_level_change = vi.fn(),
  conversation_level: ConversationUpdateLevel = "steps",
  on_conversation_level_change = vi.fn(),
  conversation_available = false,
) {
  function Harness() {
    const [current, setCurrent] = useState(scope);
    const [current_level, setCurrentLevel] = useState(level);
    const [current_conversation_level, setCurrentConversationLevel] = useState(conversation_level);
    return <div className="status-region">
      <button type="button">Outside</button>
      <BackgroundUpdatesControl
        scope={current}
        level={current_level}
        conversation_level={current_conversation_level}
        conversation_available={conversation_available}
        status={status}
        on_scope_change={(next) => { on_change(next); setCurrent(next); }}
        on_level_change={(next) => { on_level_change(next); setCurrentLevel(next); }}
        on_conversation_level_change={(next) => { on_conversation_level_change(next); setCurrentConversationLevel(next); }}
      />
    </div>;
  }
  render(<Harness />);
  return on_change;
}

describe("BackgroundUpdatesControl", () => {
  it("keeps scope and delivery filters in separate accessible groups with a compact live trigger", () => {
    renderControl();
    const trigger = screen.getByRole("button", { name: "Live updates. All sessions. All assistant messages. Live. Change settings" });
    expect(trigger).toHaveTextContent("Messages · All");
    expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    fireEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Live updates" });
    expect(dialog).toHaveFocus();
    expect(dialog).toHaveAttribute("aria-modal", "false");
    expect(dialog).toHaveAttribute("id", trigger.getAttribute("aria-controls"));
    expect(within(dialog).getByRole("radio", { name: "All sessions" })).toBeChecked();
    expect(within(dialog).getByRole("radio", { name: "Sidebar sessions" })).not.toBeChecked();
    const sessions = within(dialog).getByRole("group", { name: "Sessions" });
    const delivery = within(dialog).getByRole("group", { name: "Background delivery" });
    const conversation = within(dialog).getByRole("group", { name: "Open conversation" });
    expect(within(sessions).getAllByRole("radio")).toHaveLength(2);
    expect(within(delivery).getByRole("combobox", { name: "Background delivery" })).toHaveValue("messages");
    expect(within(delivery).getAllByRole("option")).toHaveLength(2);
    expect(within(conversation).getByRole("combobox", { name: "Open conversation" })).toHaveValue("steps");
    expect(within(conversation).getAllByRole("option")).toHaveLength(4);
    expect(within(conversation).getByRole("combobox")).toBeDisabled();
    expect(within(conversation).getByText("Open a session to choose its live delivery level.")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
    expect(within(dialog).getByText("Receiving live background updates.")).toBeInTheDocument();
  });

  it("applies native radio changes immediately while keeping the popup available", () => {
    const change = renderControl();
    fireEvent.click(screen.getByRole("button", { name: /Live updates/ }));
    const all = screen.getByRole("radio", { name: "All sessions" });
    const sidebar = screen.getByRole("radio", { name: "Sidebar sessions" });
    expect(all).toHaveAttribute("type", "radio");
    expect(sidebar).toHaveAttribute("name", all.getAttribute("name"));
    expect(sidebar).toHaveAccessibleDescription("Receive updates for sessions loaded in the sidebar.");
    fireEvent.click(sidebar);
    expect(change).toHaveBeenCalledExactlyOnceWith("sidebar");
    expect(sidebar).toBeChecked();
    expect(all).not.toBeChecked();
    expect(screen.getByRole("dialog", { name: "Live updates" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Live updates. Sidebar sessions. All assistant messages. Live. Change settings" })).toHaveTextContent("Messages · Sidebar");
    expect(screen.getByRole("combobox", { name: "Background delivery" })).toHaveValue("messages");
  });

  it("applies the delivery filter without changing session scope and retains it when reopened", () => {
    const scope_change = vi.fn();
    const level_change = vi.fn();
    renderControl("sidebar", null, scope_change, "messages", level_change);
    const trigger = screen.getByRole("button", { name: /Live updates/ });
    fireEvent.click(trigger);
    const delivery = screen.getByRole("combobox", { name: "Background delivery" });
    expect(delivery).toHaveAccessibleDescription("Receive intermediate and final assistant messages, plus attention updates.");
    fireEvent.change(delivery, { target: { value: "final" } });
    expect(level_change).toHaveBeenCalledExactlyOnceWith("final");
    expect(scope_change).not.toHaveBeenCalled();
    expect(delivery).toHaveValue("final");
    expect(delivery).toHaveAccessibleDescription("Receive final assistant replies and attention updates.");
    expect(screen.getByRole("radio", { name: "Sidebar sessions" })).toBeChecked();
    expect(trigger).toHaveTextContent("Final · Sidebar");
    expect(trigger).toHaveAccessibleName("Live updates. Sidebar sessions. Final replies. Live. Change settings");
    expect(screen.getByRole("dialog", { name: "Live updates" })).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(trigger).toHaveFocus();
    fireEvent.click(trigger);
    const reopened = screen.getByRole("combobox", { name: "Background delivery" });
    expect(reopened).toHaveValue("final");
    fireEvent.change(reopened, { target: { value: "messages" } });
    expect(level_change).toHaveBeenLastCalledWith("messages");
    expect(trigger).toHaveTextContent("Messages · Sidebar");
  });

  it("changes open conversation delivery independently from background settings", () => {
    const scope_change = vi.fn();
    const level_change = vi.fn();
    const conversation_change = vi.fn();
    renderControl("sidebar", null, scope_change, "final", level_change, "steps", conversation_change, true);
    const trigger = screen.getByRole("button", { name: /Live updates/ });
    fireEvent.click(trigger);
    const conversation = screen.getByRole("combobox", { name: "Open conversation" });
    expect(conversation).toHaveValue("steps");
    expect(conversation).not.toBeDisabled();
    expect(conversation).toHaveAccessibleDescription("Include collapsed activity group summaries. Expand groups to load their details.");
    fireEvent.change(conversation, { target: { value: "all" } });
    expect(conversation_change).toHaveBeenCalledExactlyOnceWith("all");
    expect(scope_change).not.toHaveBeenCalled();
    expect(level_change).not.toHaveBeenCalled();
    expect(conversation).toHaveValue("all");
    expect(conversation).toHaveAccessibleDescription("Receive every live event, including tool calls and their details.");
    expect(trigger).toHaveTextContent("Final · Sidebar");
    expect(screen.getByRole("combobox", { name: "Background delivery" })).toHaveValue("final");
    expect(screen.getByRole("dialog", { name: "Live updates" })).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(trigger).toHaveFocus();
    fireEvent.click(trigger);
    expect(screen.getByRole("combobox", { name: "Open conversation" })).toHaveValue("all");
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
    fireEvent.click(screen.getByRole("button", { name: /Live updates/ }));
    const outside = screen.getByRole("button", { name: "Outside" });
    act(() => outside.focus());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(outside).toHaveFocus();
  });

  it("supports the explicit close control", () => {
    renderControl();
    const trigger = screen.getByRole("button", { name: /Live updates/ });
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("button", { name: "Close live updates" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it.each([
    { status: "Connecting background updates…", phase: "Connecting", tone: "active", spinner: true },
    { status: "Background updates unavailable on this host. Open sessions still update.", phase: "Unavailable", tone: "warning", spinner: false },
  ])("shows $phase in the trigger and keeps detailed status inside the popup", ({ status, phase, tone, spinner }) => {
    renderControl("all", status);
    const trigger = screen.getByRole("button", { name: `Live updates. All sessions. All assistant messages. ${phase}. Change settings` });
    expect(trigger).toHaveTextContent("Messages · All");
    expect(trigger).toHaveAttribute("data-tone", tone);
    expect(!!trigger.querySelector(".status-bar__spinner")).toBe(spinner);
    expect(screen.queryByText(status)).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    fireEvent.click(trigger);
    expect(within(screen.getByRole("dialog")).getByText(status)).toBeInTheDocument();
  });
});

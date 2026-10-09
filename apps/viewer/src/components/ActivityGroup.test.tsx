import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { EventSummary } from "../lib/types";
import { ActivityGroup, groupActivity } from "./ActivityGroup";
import { ShellOutputSection } from "./ShellOutputSection";

afterEach(cleanup);
function activity(key: string, kind: string, overrides: Partial<EventSummary> = {}): EventSummary {
  return { event_key: key, type: "tool_call", tool: {kind, status: "completed"}, ...overrides } as EventSummary;
}

describe("activity folding", () => {
  it("groups only adjacent completed exploration and leaves failures and commands visible", () => {
    const events = [activity("read", "file_read"), activity("search", "search"), activity("shell", "shell"),
      activity("failed", "file_read", {is_error: true}), activity("another", "file_read"),
      activity("running", "search", {tool: {kind: "search", status: "running"} as EventSummary["tool"]})];
    expect(groupActivity(events).map((group) => group.map((event) => event.event_key)))
      .toEqual([["read", "search"], ["shell"], ["failed"], ["another"], ["running"]]);
  });

  it("folds exploration until requested and reveals a selected child", () => {
    const events = [activity("a", "file_read"), activity("b", "search")];
    const {rerender} = render(<ActivityGroup events={events}><span>Captured activity</span></ActivityGroup>);
    expect(screen.getByText("Captured activity")).not.toBeVisible();
    fireEvent.click(screen.getByRole("button", {name: "Explored 1 file, 1 search"}));
    expect(screen.getByText("Captured activity")).toBeVisible();
    fireEvent.click(screen.getByRole("button", {name: "Explored 1 file, 1 search"}));
    rerender(<ActivityGroup events={events} selected_event_key="b"><span>Captured activity</span></ActivityGroup>);
    expect(screen.getByText("Captured activity")).toBeVisible();
  });

  it("bounds shell output locally and restores the exact captured text on expansion", () => {
    const text = Array.from({length: 12}, (_, index) => `line ${index}`).join("\n");
    render(<ShellOutputSection command="cargo test" section={{label: "stdout", format: "text", text}} />);
    const output = screen.getByLabelText("cargo test stdout output");
    expect(output.textContent).toBe(text.split("\n").slice(0, 8).join("\n"));
    fireEvent.click(screen.getByRole("button", {name: "Show 4 more lines"}));
    expect(output.textContent).toBe(text);
    fireEvent.click(screen.getByRole("button", {name: "Show fewer lines"}));
    expect(output.textContent).not.toContain("line 11");
  });
});

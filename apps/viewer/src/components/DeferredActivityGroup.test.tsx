import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { EventSummary, TrajectoryEventPageState } from "../lib/types";
import { DeferredActivityGroup } from "./DeferredActivityGroup";

afterEach(cleanup);
it("distinguishes an empty loaded activity group from an outstanding read", () => {
  const event = { event_key: "activity:empty", type: "activity_group", summary: "Tool activity" } as EventSummary;
  const children = vi.fn(() => null);
  const { rerender } = render(<DeferredActivityGroup event={event} parent_visible>{children}</DeferredActivityGroup>);
  fireEvent.click(screen.getByRole("button", { name: "Tool activity" }));
  expect(screen.getByRole("status")).toHaveTextContent("Loading activity…");

  const page: TrajectoryEventPageState = { total_events: 0, events: [], has_loaded: true, is_loading: false, error: null };
  rerender(<DeferredActivityGroup event={event} page={page} parent_visible>{children}</DeferredActivityGroup>);
  expect(screen.getByRole("status")).toHaveTextContent("No activity in this group.");
  expect(children).not.toHaveBeenCalled();
});

it("reveals running or failed activity when its parent is visible and respects a manual collapse", () => {
  const event = { event_key: "activity:a", type: "activity_group", summary: "1 command", phase: "running" } as EventSummary;
  const props = { event, parent_visible: false, on_load: vi.fn(), children: () => null };
  const { rerender } = render(<DeferredActivityGroup {...props} />);
  expect(props.on_load).not.toHaveBeenCalled();
  rerender(<DeferredActivityGroup {...props} parent_visible />);
  expect(props.on_load).toHaveBeenCalledWith(event.event_key);
  const button = screen.getByRole("button", { name: /Running/ });
  fireEvent.click(button);
  rerender(<DeferredActivityGroup {...props} parent_visible event={{ ...event }} />);
  expect(button).toHaveAttribute("aria-expanded", "false");
  rerender(<DeferredActivityGroup {...props} parent_visible event={{ ...event, phase: "finished", is_error: true }} />);
  expect(screen.getByRole("button", { name: /Failed activity/ })).toHaveAttribute("aria-expanded", "true");
});
it("defers complete rows until expansion, retains them, and reports actual visibility", () => {
  const event = { event_key: "activity:a", type: "activity_group", summary: "2 commands", child_keys: ["a", "b"] } as EventSummary;
  const on_load = vi.fn(), on_visibility = vi.fn();
  const rows = vi.fn((items: EventSummary[]) => items.map((item) => <span key={item.event_key}>{item.event_key}</span>));
  const props = { event, parent_visible: true, on_load, on_visibility, children: rows };
  const { rerender } = render(<DeferredActivityGroup {...props} />);
  expect(on_load).not.toHaveBeenCalled();
  expect(rows).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "2 commands" }));
  expect(on_load).toHaveBeenCalledWith("activity:a");
  expect(on_visibility).toHaveBeenLastCalledWith("activity:a", true);
  const page = { events: [{ event_key: "a" }, { event_key: "b" }], has_loaded: true, is_loading: false, error: null } as TrajectoryEventPageState;
  rerender(<DeferredActivityGroup {...props} page={page} />);
  expect(screen.getByText("b")).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "2 commands" }));
  expect(screen.getByText("b")).not.toBeVisible();
  expect(on_visibility).toHaveBeenLastCalledWith("activity:a", false);
  expect(on_load).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole("button", { name: "2 commands" }));
  rerender(<DeferredActivityGroup {...props} page={page} parent_visible={false} />);
  expect(on_visibility).toHaveBeenLastCalledWith("activity:a", false);
});

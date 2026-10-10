import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { EventSummary, TrajectoryEventPageState } from "../lib/types";
import { DeferredActivityGroup } from "./DeferredActivityGroup";

afterEach(cleanup);
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

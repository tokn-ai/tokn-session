import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { SessionLiveOutput } from "./SessionLiveOutput";

afterEach(cleanup);

it("renders streamed work safely and retires it on completion", () => {
  const view = render(<SessionLiveOutput output={{ prompt_id: "prompt", truncated: true, parts: [
    { kind: "text", text: "<script>literal preview</script>" },
    { kind: "text", text: "The result is **complete**." },
    { kind: "reasoning", text: "Checking a fixture" },
    { kind: "tool", text: "Read fixture", status: "in_progress" },
  ] }} />);
  expect(screen.getByRole("region", { name: "Live response" })).toBeVisible();
  expect(screen.getByText("Working")).toBeVisible();
  expect(screen.getByText("<script>literal preview</script>")).toBeVisible();
  expect(view.container.querySelector("script")).toBeNull();
  expect(screen.getByText("complete").tagName).toBe("STRONG");
  expect(screen.getByText("Read fixture")).toBeVisible();
  expect(screen.getByText("in progress")).toBeVisible();
  expect(screen.getByText("Thinking")).toBeVisible();
  expect(screen.getByText(/Live preview truncated/)).toBeVisible();
  view.rerender(<SessionLiveOutput output={undefined} />);
  expect(screen.queryByRole("region", { name: "Live response" })).not.toBeInTheDocument();
});

it("resets reasoning disclosures when the active prompt changes", () => {
  const live = (prompt_id: string) => <SessionLiveOutput output={{ prompt_id, truncated: false,
    parts: [{ kind: "reasoning", text: "Checking" }] }} />;
  const view = render(live("first"));
  fireEvent.click(screen.getByText("Thinking"));
  expect(screen.getByText("Thinking").closest("details")).toHaveAttribute("open");
  view.rerender(live("second"));
  expect(screen.getByText("Thinking").closest("details")).not.toHaveAttribute("open");
});

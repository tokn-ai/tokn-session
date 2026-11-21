import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { SessionLiveOutput } from "./SessionLiveOutput";

afterEach(cleanup);

it("keeps streamed work visibly separate from saved messages and retires it on completion", () => {
  const view = render(<SessionLiveOutput output={{ prompt_id: "prompt", truncated: true, parts: [
    { kind: "text", text: "<script>literal preview</script>" },
    { kind: "reasoning", text: "Checking a fixture" },
    { kind: "tool", text: "Read fixture", status: "in_progress" },
  ] }} />);
  expect(screen.getByRole("region", { name: "Live response" })).toBeVisible();
  expect(screen.getByText("Working")).toBeVisible();
  expect(screen.getByText("<script>literal preview</script>")).toBeVisible();
  expect(view.container.querySelector("script")).toBeNull();
  expect(screen.getByText("Read fixture")).toBeVisible();
  expect(screen.getByText("in progress")).toBeVisible();
  expect(screen.getByText("Thinking")).toBeVisible();
  expect(screen.getByText(/Live preview truncated/)).toBeVisible();
  view.rerender(<SessionLiveOutput output={undefined} />);
  expect(screen.queryByRole("region", { name: "Live response" })).not.toBeInTheDocument();
});

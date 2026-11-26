import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { SessionLiveOutput } from "./SessionLiveOutput";

afterEach(cleanup);

it("keeps streamed work visibly separate from saved messages and retires it on completion", () => {
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

it("follows streamed content until the reader scrolls up and resumes at the bottom", () => {
  const live = (text: string) => <SessionLiveOutput output={{ prompt_id: "one", truncated: false,
    parts: [{ kind: "text", text }] }} />;
  const view = render(live("Starting"));
  const content = screen.getByRole("region", { name: "Live response content" });
  Object.defineProperties(content, { scrollHeight: { value: 200 }, clientHeight: { value: 100 } });
  content.scrollTop = 30;
  fireEvent.scroll(content);
  view.rerender(live("Continuing"));
  expect(content.scrollTop).toBe(30);
  content.scrollTop = 100;
  fireEvent.scroll(content);
  view.rerender(live("Finishing"));
  expect(content.scrollTop).toBe(200);
});

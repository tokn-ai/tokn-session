import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getSessionControl } from "../lib/tauri";
import type { SessionControlStatus } from "../lib/types";
import { Conversation } from "./Conversation";
import { event, props, SESSION } from "./conversationTestFixtures";

vi.mock("../lib/tauri", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/tauri")>(), getSessionControl: vi.fn(),
}));
vi.mock("./SessionComposer", () => ({ SessionComposer: () => null }));

const session = { ...SESSION, provider: "qoder" as const, session_key: "qoder:session" };
const question = event("question", { type: "message", provider: "qoder", role: "user", is_bookkeeping: false,
  summary: "Check the parser." });
const answer = event("answer", { type: "message", provider: "qoder", role: "assistant", is_bookkeeping: false,
  summary: "The parser handles all inputs." });
const status: SessionControlStatus = {
  session_key: session.session_key, provider: "qoder", transport: "rpc", can_select: false, can_cancel: true,
  available: true, message: "", runtime_id: "runtime", instance_id: "instance", is_running: true,
  live_output: { prompt_id: "prompt", truncated: false, parts: [{ kind: "text", text: "Checking the parser fixtures." }] },
};

beforeEach(() => {
  localStorage.clear();
  vi.mocked(getSessionControl).mockReset().mockResolvedValue(status);
});
afterEach(cleanup);

it("replaces a transient reply with a recorded event when the runtime completes", async () => {
  const refreshHistory = vi.fn();
  const conversation = props({ session });
  function Chat({ revision }: { revision: number }) {
    const [events, setEvents] = useState([question]);
    return <Conversation {...conversation} events={events} input_refresh_key={revision}
      on_input_accepted={(key) => { refreshHistory(key); setEvents([question, answer]); }} />;
  }
  const view = render(<Chat revision={0} />);
  const timeline = within(screen.getByLabelText("Session event timeline"));
  const live = await timeline.findByRole("region", { name: "Live response" });
  const prompt = timeline.getByText(question.summary);
  expect(prompt.compareDocumentPosition(live) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(live.closest("[data-reading-slot]")).toBeNull();
  expect(within(live).getByText("Checking the parser fixtures.")).toBeVisible();

  vi.mocked(getSessionControl).mockResolvedValue({ ...status, is_running: false, live_output: undefined });
  view.rerender(<Chat revision={1} />);
  await waitFor(() => expect(timeline.queryByRole("region", { name: "Live response" })).toBeNull());
  expect(refreshHistory).toHaveBeenCalledExactlyOnceWith(session.session_key);
  expect(timeline.getAllByText(answer.summary)).toHaveLength(1);
  expect(timeline.queryByText("Checking the parser fixtures.")).toBeNull();
});

it("shows live output before any history is recorded, and reserves older pages for history", async () => {
  const conversation = props({ session, events: [], total_events: 0 });
  const view = render(<Conversation {...conversation} />);
  const history = within(screen.getByRole("region", { name: "Conversation history" }));
  expect(await history.findByRole("region", { name: "Live response" })).toBeVisible();
  expect(history.queryByText("No events in this session")).toBeNull();

  view.rerender(<Conversation {...conversation} events={[question]} has_newer />);
  expect(history.queryByRole("region", { name: "Live response" })).toBeNull();
  expect(history.getByRole("button", { name: "Load newer events" })).toBeVisible();
});

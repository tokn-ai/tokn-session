import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { CommandInvoker } from "../lib/transport";
import { parseQuestionDisplay } from "../lib/questionDisplay";
import type { SubmitSessionInputResponse, SubmitSessionQuestionAnswersRequest } from "../lib/types";
import { QuestionAnswerForm } from "./QuestionAnswerForm";
import { QuestionAnswersProvider } from "./QuestionAnswersProvider";

afterEach(cleanup);
const display = parseQuestionDisplay({ type: "question_request", request_id: "call", is_blocking: false, questions: [
  { id: "q1", header: "Storage", question: "Choose storage?", options: [{ label: "Local (Recommended)", description: "Keep it local" }, { label: "Remote" }], allows_free_text: true },
] })!;
const status = { available: true, message: "Choose", question_ids: ["q1"], max_length: 1000 };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function form(session_key = "session") {
  return <QuestionAnswerForm session_key={session_key} display={display} recorded_card={<p>Recorded card</p>} />;
}

describe("async question answering", () => {
  it("sends option identities once and waits for the recorded reply after acceptance", async () => {
    const pending = deferred<SubmitSessionInputResponse>();
    const send = vi.fn((command: string) => command === "get_session_question_status" ? Promise.resolve(status) : pending.promise);
    render(<QuestionAnswersProvider send={send as CommandInvoker}>{form()}</QuestionAnswersProvider>);
    const submit = await screen.findByRole("button", { name: "Submit answers" });
    expect(submit).toBeDisabled();
    fireEvent.click(screen.getByRole("radio", { name: /Local/ }));
    fireEvent.click(submit); fireEvent.click(submit);
    await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "submit_session_question_answers")).toHaveLength(1));
    const call = send.mock.calls.find(([command]) => command === "submit_session_question_answers")! as unknown as [string, { request: SubmitSessionQuestionAnswersRequest }];
    expect(call[1].request.answers).toEqual([{ question_id: "q1", option_index: 0 }]);
    pending.resolve({ request_id: call[1].request.request_id, status: "accepted", message: "Accepted" });
    await screen.findByText("Answer submitted; waiting for the recorded reply.");
    expect(screen.queryByRole("button", { name: "Submit answers" })).not.toBeInTheDocument();
  });

  it("retains written drafts across card closures and keeps session drafts separate", async () => {
    const send = vi.fn((_command: string) => Promise.resolve(status));
    const view = render(<QuestionAnswersProvider send={send as CommandInvoker}>{form()}</QuestionAnswersProvider>);
    await screen.findByRole("button", { name: "Submit answers" });
    fireEvent.click(screen.getByRole("radio", { name: "Write an answer" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "No external service." } });
    view.rerender(<QuestionAnswersProvider send={send as CommandInvoker}>{form("another")}</QuestionAnswersProvider>);
    await screen.findByRole("button", { name: "Submit answers" });
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    view.rerender(<QuestionAnswersProvider send={send as CommandInvoker}><p>Closed card</p></QuestionAnswersProvider>);
    view.rerender(<QuestionAnswersProvider send={send as CommandInvoker}>{form()}</QuestionAnswersProvider>);
    expect(await screen.findByRole("textbox")).toHaveValue("No external service.");
    expect(send.mock.calls.every(([command]) => command === "get_session_question_status")).toBe(true);
  });

  it("preserves drafts after a definite rejection and does not retry uncertain delivery", async () => {
    let result_status: "not_sent" | "unknown" = "not_sent";
    const send = vi.fn(async (command: string, payload?: { request: SubmitSessionQuestionAnswersRequest }) => command === "get_session_question_status" ? status
      : { request_id: payload!.request.request_id, status: result_status, message: "Try again" });
    render(<QuestionAnswersProvider send={send as CommandInvoker}>{form()}</QuestionAnswersProvider>);
    await screen.findByRole("button", { name: "Submit answers" });
    fireEvent.click(screen.getByRole("radio", { name: "Write an answer" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Custom answer" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await screen.findByText("Try again");
    expect(screen.getByRole("textbox")).toHaveValue("Custom answer");
    const call = send.mock.calls.find(([command]) => command === "submit_session_question_answers")!;
    expect(call[1]!.request.answers).toEqual([{ question_id: "q1", text: "Custom answer" }]);
    result_status = "unknown";
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await screen.findByText(/Delivery is not confirmed/);
    expect(screen.queryByRole("button", { name: "Submit answers" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Check availability" }));
    await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "get_session_question_status").length).toBeGreaterThan(2));
    expect(send.mock.calls.filter(([command]) => command === "submit_session_question_answers")).toHaveLength(2);
  });

  it("uses live recorded selections to stop offering resolved questions while status catches up", async () => {
    const changed = { ...display, questions: [
      { ...display.questions[0], options: display.questions[0].options.map((option, index) => ({ ...option, is_selected: index === 0 })) },
      { ...display.questions[0], question_id: "q2", header: "Constraints", question: "Any constraints?", options: [] },
    ] };
    const send = vi.fn(async (command: string, payload?: { request: SubmitSessionQuestionAnswersRequest }) => command === "get_session_question_status"
      ? { ...status, question_ids: ["q1", "q2"] } : { request_id: payload!.request.request_id, status: "accepted", message: "Accepted" });
    render(<QuestionAnswersProvider send={send as CommandInvoker}><QuestionAnswerForm session_key="session" display={changed} recorded_card={<p>Recorded card</p>} /></QuestionAnswersProvider>);
    const field = await screen.findByRole("textbox");
    expect(screen.queryByRole("radio", { name: /Local/ })).not.toBeInTheDocument();
    expect(screen.getByText("✓ Selected")).toBeInTheDocument();
    fireEvent.change(field, { target: { value: "Offline only" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await screen.findByText("Answer submitted; waiting for the recorded reply.");
    expect(send.mock.calls.find(([command]) => command === "submit_session_question_answers")![1]!.request.answers)
      .toEqual([{ question_id: "q2", text: "Offline only" }]);
  });

  it("preserves original option indices when tolerant display parsing skips a malformed row", async () => {
    const projected = parseQuestionDisplay({ type: "question_request", request_id: "call", is_blocking: false, questions: [
      { id: "q1", question: "Which?", options: [{ invalid: true }, { label: "Local" }] },
    ] })!;
    const send = vi.fn(async (command: string, payload?: { request: SubmitSessionQuestionAnswersRequest }) => command === "get_session_question_status"
      ? status : { request_id: payload!.request.request_id, status: "accepted", message: "Accepted" });
    render(<QuestionAnswersProvider send={send as CommandInvoker}><QuestionAnswerForm session_key="session" display={projected} recorded_card={<p>Recorded card</p>} /></QuestionAnswersProvider>);
    fireEvent.click(await screen.findByRole("radio", { name: "Local" }));
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await screen.findByText("Answer submitted; waiting for the recorded reply.");
    expect(send.mock.calls.find(([command]) => command === "submit_session_question_answers")![1]!.request.answers)
      .toEqual([{ question_id: "q1", option_index: 1 }]);
  });

  it("keeps remaining questions answerable after a partial submission", async () => {
    let ids = ["q1"];
    const pair = { ...display, questions: [display.questions[0], { ...display.questions[0], question_id: "q2", header: "Constraints", options: [] }] };
    const send = vi.fn(async (command: string, payload?: { request: SubmitSessionQuestionAnswersRequest }) => {
      if (command === "get_session_question_status") return { ...status, question_ids: ids };
      ids = ["q2"];
      return { request_id: payload!.request.request_id, status: "accepted", message: "Accepted" };
    });
    render(<QuestionAnswersProvider send={send as CommandInvoker}><QuestionAnswerForm session_key="session" display={pair} recorded_card={<p>Recorded card</p>} /></QuestionAnswersProvider>);
    fireEvent.click(await screen.findByRole("radio", { name: /Local/ }));
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await screen.findByText("Answer submitted; waiting for the recorded reply.");
    fireEvent.click(screen.getByRole("button", { name: "Check availability" }));
    const field = await screen.findByRole("textbox");
    fireEvent.change(field, { target: { value: "Offline only" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "submit_session_question_answers")).toHaveLength(2));
    expect(send.mock.calls.filter(([command]) => command === "submit_session_question_answers")[1][1]!.request.answers)
      .toEqual([{ question_id: "q2", text: "Offline only" }]);
  });

  it("keeps read-only connections and resolved requests free of answer controls", async () => {
    const send = vi.fn((_command: string) => Promise.resolve({ ...status, available: false, question_ids: [], message: "Viewing only" }));
    render(<QuestionAnswersProvider send={send as CommandInvoker}>{form()}</QuestionAnswersProvider>);
    await screen.findByText("Viewing only");
    expect(screen.getByText("Recorded card")).toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Submit answers" })).not.toBeInTheDocument();
    expect(send.mock.calls.every(([command]) => command === "get_session_question_status")).toBe(true);
  });
});

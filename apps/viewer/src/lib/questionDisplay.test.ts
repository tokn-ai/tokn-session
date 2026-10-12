import { describe, expect, it } from "vitest";
import { parseQuestionDisplay } from "./questionDisplay";

describe("question display parsing", () => {
  it("preserves option descriptions and recognizes only explicit recommendation suffixes", () => {
    const display = parseQuestionDisplay({ type: "question_request", is_blocking: false, questions: [
      { id: "q1", question: "Storage?", options: [
        { label: "SQLite (Recommended)", description: "Local database" },
        { label: "Recommended reading", description: null },
        { label: "Postgres", description: "" },
      ], allows_free_text: true },
    ] })!;
    expect(display.is_blocking).toBe(false);
    expect(display.questions[0]).toMatchObject({ question_id: "q1", allows_free_text: true, options: [
      { label: "SQLite", description: "Local database", is_recommended: true, is_selected: false },
      { label: "Recommended reading", description: null, is_recommended: false, is_selected: false },
      { label: "Postgres", description: null, is_recommended: false, is_selected: false },
    ] });
  });

  it("tolerates malformed rows without inventing choices or answers", () => {
    const request = parseQuestionDisplay({ type: "question_request", questions: [null, { question: "" },
      { question: "Constraints?", options: [null, { label: 5 }, { label: "  " }], is_secret: true },
    ] })!;
    expect(request.is_blocking).toBeNull();
    expect(request.questions).toHaveLength(1);
    expect(request.questions[0]).toMatchObject({ options: [], answers: [], is_secret: true });
    expect(parseQuestionDisplay({ type: "question_reply", replies: [null, { question_id: "q1", answers: ["custom answer", null, 4] }] })?.questions[0])
      .toMatchObject({ question_id: "q1", answers: ["custom answer"], options: [] });
    expect(parseQuestionDisplay({ type: "question_request", questions: "invalid" })).toBeNull();
    expect(parseQuestionDisplay({ type: "message" })).toBeNull();
  });
});

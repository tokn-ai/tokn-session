import type { JsonValue, QuestionDisplay, QuestionDisplayItem, QuestionDisplayOption, QuestionSelectionSummary } from "./types";

function object(value: JsonValue | undefined): { [key: string]: JsonValue } | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value : null;
}

function text(value: JsonValue | undefined): string | null {
  return typeof value === "string" && value.trim() ? value : null;
}

function option(value: JsonValue, option_index: number): QuestionDisplayOption | null {
  const recorded = object(value);
  const label = text(recorded?.label);
  if (!label) return null;
  // Only the explicit provider label establishes a recommendation.
  const match = label.match(/\s*\(recommended\)\s*$/i);
  const display_label = match ? label.slice(0, match.index).trim() : label;
  return { option_index, label: display_label || label, description: text(recorded?.description), is_recommended: !!match && !!display_label, is_selected: false };
}

export function parseQuestionDisplay(value: JsonValue, selections: QuestionSelectionSummary[] = []): QuestionDisplay | null {
  const recorded = object(value);
  const kind = recorded?.type === "question_request" ? "request" : recorded?.type === "question_reply" ? "reply" : null;
  if (!kind || !recorded) return null;
  const rows = kind === "request" ? recorded.questions : recorded.replies;
  if (!Array.isArray(rows)) return null;
  const questions: QuestionDisplayItem[] = [];
  for (const value of rows) {
    const row = object(value);
    if (!row || (kind === "request" && !text(row.question)) || (kind === "reply" && !Array.isArray(row.answers))) continue;
    const selection = selections.find((selection) => selection.question_id === text(row.id));
    questions.push({
      question_id: text(kind === "request" ? row.id : row.question_id),
      header: text(row.header),
      question: text(row.question),
      options: kind === "request" && Array.isArray(row.options)
        ? row.options.flatMap((value, index) => {
          const parsed = option(value, index);
          return parsed ? [{ ...parsed, is_selected: selections.some((selection) => selection.question_id === text(row.id)
            && selection.option_indices.includes(index)) }] : [];
        }) : [],
      answers: kind === "reply" && Array.isArray(row.answers) ? row.answers.filter((value): value is string => typeof value === "string") : [],
      written_answer: selection?.written_answer ?? null,
      written_answer_truncated: selection?.written_answer_truncated === true,
      allows_free_text: row.allows_free_text === true,
      is_secret: row.is_secret === true,
    });
  }
  return { kind, request_id: text(recorded.request_id), is_blocking: typeof recorded.is_blocking === "boolean" ? recorded.is_blocking : null, questions };
}

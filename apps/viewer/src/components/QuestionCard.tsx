import type { EventDetail, EventSummary, JsonValue } from "../lib/types";
import { parseQuestionDisplay } from "../lib/questionDisplay";
import { DetailRefreshError } from "./DetailRefreshError";
import { QuestionDisplaySection } from "./QuestionDisplaySection";
import { QuestionAnswerForm } from "./QuestionAnswerForm";

function object(value: JsonValue | undefined): { [key: string]: JsonValue } | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value : null;
}

export function QuestionCard({ event, detail, error, is_loading, on_retry, session_key, is_pending = false, on_accepted }: {
  session_key?: string;
  is_pending?: boolean;
  on_accepted?: (session_key: string) => void;
  event: EventSummary;
  detail: EventDetail | null;
  error: string | null;
  is_loading: boolean;
  on_retry: () => void;
}) {
  const matches = detail?.event_key === event.event_key;
  const label = event.type === "question_reply" ? "Answers" : "Questions";
  if (event.is_hidden || (matches && detail?.is_hidden)) {
    return <p>{label} are hidden by the provider.</p>;
  }
  const recorded = matches ? object(detail?.event) : null;
  if (!recorded) {
    return error ? (
      <div role="alert">
        <p>{label} unavailable: {error}</p>
        <button className="text-button" onClick={on_retry} type="button">Try again</button>
      </div>
    ) : <p role="status">Loading {label.toLowerCase()}…</p>;
  }
  if (recorded.truncated === true) {
    return <p role="status">{label} exceed the viewer’s detail size limit.</p>;
  }
  const display = parseQuestionDisplay(recorded, event.question_selections);
  if (!display) return <p>No structured questions were recorded.</p>;
  const is_reply = display.kind === "reply";
  const notice = is_reply ? "Recorded user answers" : display.is_blocking === true ? "Recorded blocking question request"
    : display.is_blocking === false ? "Recorded asynchronous question request" : "Recorded question request";
  const recorded_card = (
    <div aria-busy={is_loading} className="question-card" data-kind={display.kind}>
      <DetailRefreshError error={error} on_retry={on_retry} />
      <div className="question-card__heading">
        <span className="question-card__symbol" aria-hidden="true">{is_reply ? "✓" : "?"}</span>
        <span className="question-card__notice">{notice}</span>
      </div>
      {display.questions.length === 0 ? <p>{is_reply ? "No answers recorded" : "No structured questions were recorded."}</p> : null}
      {display.questions.map((question, index) => (
        <QuestionDisplaySection question={question} index={index} total={display.questions.length} is_reply={is_reply} key={index} />
      ))}
    </div>
  );
  if (is_pending && session_key && display.kind === "request" && display.request_id) {
    return <QuestionAnswerForm session_key={session_key} display={display} recorded_card={recorded_card} on_accepted={on_accepted} />;
  }
  return recorded_card;
}

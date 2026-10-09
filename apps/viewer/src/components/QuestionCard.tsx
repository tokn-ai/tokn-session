import type { EventDetail, EventSummary, JsonValue } from "../lib/types";
import { DetailRefreshError } from "./DetailRefreshError";
import { MarkdownContent } from "./MarkdownContent";

function object(value: JsonValue | undefined): { [key: string]: JsonValue } | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value : null;
}

export function QuestionCard({ event, detail, error, is_loading, on_retry }: {
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
  if (recorded.type === "question_reply" && Array.isArray(recorded.replies)) {
    return (
      <div aria-busy={is_loading} className="question-card">
        <DetailRefreshError error={error} on_retry={on_retry} />
        <p className="question-card__notice">Recorded user answers</p>
        {recorded.replies.length === 0 ? <p>No answers recorded</p> : null}
        {recorded.replies.map((value, index) => {
          const reply = object(value);
          if (!reply || !Array.isArray(reply.answers)) return null;
          return <section className="question-card__question" key={index}>
            {typeof reply.header === "string" && reply.header ? <h4>{reply.header}</h4> : null}
            {typeof reply.question === "string" ? <MarkdownContent content={reply.question} />
              : <h4>{typeof reply.question_id === "string" ? reply.question_id : "Question"}</h4>}
            {reply.answers.length === 0 ? <p>No answer recorded</p> : reply.answers.map((answer, answer_index) =>
              typeof answer === "string" ? <MarkdownContent key={answer_index} content={answer} /> : null)}
          </section>;
        })}
      </div>
    );
  }
  if (recorded.type !== "question_request" || !Array.isArray(recorded.questions)) {
    return <p>No structured questions were recorded.</p>;
  }
  return (
    <div aria-busy={is_loading} className="question-card">
      <DetailRefreshError error={error} on_retry={on_retry} />
      <p className="question-card__notice">
        {recorded.is_blocking === true ? "Recorded blocking question request"
          : recorded.is_blocking === false ? "Recorded asynchronous question request"
            : "Recorded question request"}
      </p>
      {recorded.questions.map((value, index) => {
        const question = object(value);
        if (!question || typeof question.question !== "string") return null;
        return (
          <section className="question-card__question" key={index}>
            {typeof question.header === "string" && question.header ? <h4>{question.header}</h4> : null}
            <MarkdownContent content={question.question} />
            {Array.isArray(question.options) && question.options.length > 0 ? (
              <ul className="question-card__options">
                {question.options.map((value, option_index) => {
                  const option = object(value);
                  if (!option || typeof option.label !== "string") return null;
                  return <li key={option_index}>
                    <strong>{option.label}</strong>
                    {typeof option.description === "string" && option.description
                      ? <span> — {option.description}</span> : null}
                  </li>;
                })}
              </ul>
            ) : null}
            {question.allows_free_text === true ? <p className="question-card__notice">Free-text answer allowed</p> : null}
            {question.is_secret === true ? <p className="question-card__notice">Secret answer requested</p> : null}
          </section>
        );
      })}
    </div>
  );
}

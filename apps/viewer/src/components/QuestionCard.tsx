import type { EventDetail, EventSummary, JsonValue } from "../lib/types";
import { parseQuestionDisplay } from "../lib/questionDisplay";
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
  const display = parseQuestionDisplay(recorded);
  if (!display) return <p>No structured questions were recorded.</p>;
  const is_reply = display.kind === "reply";
  const notice = is_reply ? "Recorded user answers" : display.is_blocking === true ? "Recorded blocking question request"
    : display.is_blocking === false ? "Recorded asynchronous question request" : "Recorded question request";
  return (
    <div aria-busy={is_loading} className="question-card" data-kind={display.kind}>
      <DetailRefreshError error={error} on_retry={on_retry} />
      <div className="question-card__heading">
        <span className="question-card__symbol" aria-hidden="true">{is_reply ? "✓" : "?"}</span>
        <span className="question-card__notice">{notice}</span>
      </div>
      {display.questions.length === 0 ? <p>{is_reply ? "No answers recorded" : "No structured questions were recorded."}</p> : null}
      {display.questions.map((question, index) => (
        <section className="question-card__question" key={index}>
          <div className="question-card__question-heading">
            <h4>{question.header ?? (is_reply && !question.question ? question.question_id : null) ?? "Question"}</h4>
            {display.questions.length > 1 ? <span className="question-card__count">{index + 1} of {display.questions.length}</span> : null}
          </div>
          {question.question ? <div className="question-card__prompt"><MarkdownContent content={question.question} /></div> : null}
          {question.options.length > 0 ? (
            <ol className="question-card__options" aria-label="Recorded options">
              {question.options.map((option, option_index) => (
                <li className="question-card__option" key={option_index}>
                  <span className="question-card__option-number" aria-hidden="true">{option_index + 1}</span>
                  <div className="question-card__option-content">
                    <div className="question-card__option-label">
                      <strong>{option.label}</strong>
                      {option.is_recommended ? <span className="question-card__recommended">Recommended</span> : null}
                    </div>
                    {option.description ? <p className="question-card__description">{option.description}</p> : null}
                  </div>
                </li>
              ))}
            </ol>
          ) : null}
          {is_reply ? <div className="question-card__answers">
            <span className="question-card__answer-label">Recorded answer</span>
            {question.answers.length === 0 ? <p className="question-card__notice">No answer recorded</p>
              : question.answers.map((answer, answer_index) => <MarkdownContent key={answer_index} content={answer} />)}
          </div> : null}
          {question.allows_free_text || question.is_secret ? <div className="question-card__hints">
            {question.allows_free_text ? <span>Free-text answer allowed</span> : null}
            {question.is_secret ? <span>Secret answer requested</span> : null}
          </div> : null}
        </section>
      ))}
    </div>
  );
}

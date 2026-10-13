import type { QuestionDisplayItem } from "../lib/types";
import { MarkdownContent } from "./MarkdownContent";

export function QuestionDisplaySection({ question, index, total, is_reply = false }: {
  question: QuestionDisplayItem; index: number; total: number; is_reply?: boolean;
}) {
  return (
    <section className="question-card__question">
      <div className="question-card__question-heading">
        <h4>{question.header ?? (is_reply && !question.question ? question.question_id : null) ?? "Question"}</h4>
        {total > 1 ? <span className="question-card__count">{index + 1} of {total}</span> : null}
      </div>
      {question.question ? <div className="question-card__prompt"><MarkdownContent content={question.question} /></div> : null}
      {question.options.length > 0 ? (
        <ol className="question-card__options" aria-label="Recorded options">
          {question.options.map((option, option_index) => (
            <li className="question-card__option" data-selected={option.is_selected} key={option_index}>
              <span className="question-card__option-number" aria-hidden="true">{option_index + 1}</span>
              <div className="question-card__option-content">
                <div className="question-card__option-label">
                  <strong>{option.label}</strong>
                  {option.is_selected ? <span className="question-card__selected">✓ Selected</span> : null}
                  {option.is_recommended ? <span className="question-card__recommended">Recommended</span> : null}
                </div>
                {option.description ? <p className="question-card__description">{option.description}</p> : null}
              </div>
            </li>
          ))}
        </ol>
      ) : null}
      {question.written_answer !== null ? <div className="question-card__answers question-card__answers--selected">
        <span className="question-card__selected">✓ Recorded answer</span>
        <MarkdownContent content={question.written_answer} />
        {question.written_answer_truncated ? <span className="question-card__notice">Answer preview truncated; inspect the recorded reply for full text.</span> : null}
      </div> : null}
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
  );
}

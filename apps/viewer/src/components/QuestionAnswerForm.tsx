import { useContext, useEffect, useId, useState, type ReactNode } from "react";
import { createUuid } from "../lib/id";
import { errorMessage } from "../lib/state";
import { getSessionQuestionStatus, submitSessionQuestionAnswers } from "../lib/tauri";
import type { QuestionDisplay, SessionQuestionStatus, SessionQuestionAnswer } from "../lib/types";
import { QuestionAnswersContext, type QuestionDraft } from "./QuestionAnswersProvider";
import { QuestionDisplaySection } from "./QuestionDisplaySection";
import { MarkdownContent } from "./MarkdownContent";
import { SessionControlContext } from "../lib/useSessionControl";

export function QuestionAnswerForm({ session_key, display, recorded_card, on_accepted }: {
  session_key: string; display: QuestionDisplay; recorded_card: ReactNode;
  on_accepted?: (session_key: string) => void;
}) {
  const store = useContext(QuestionAnswersContext);
  const control = useContext(SessionControlContext);
  const control_blocked = control?.session_key === session_key && control.blocked;
  const control_revision = control?.session_key === session_key ? control.revision : undefined;
  const prefix = useId();
  const [status, setStatus] = useState<SessionQuestionStatus | null>(null);
  const [checking, setChecking] = useState(true);
  const [retry, setRetry] = useState(0);
  const request_id = display.request_id!;
  const key = JSON.stringify([session_key, request_id]);
  const resolved_revision = JSON.stringify(display.questions.filter((question) => question.options.some((option) => option.is_selected)
    || question.written_answer !== null).map((question) => question.question_id));
  const send = store?.send;
  const state = store?.get(key);
  useEffect(() => {
    let disposed = false;
    if (!send) return;
    setChecking(true);
    setStatus(null);
    if (control_blocked) return;
    void getSessionQuestionStatus({ session_key, question_request_id: request_id }, send)
      .then((result) => { if (!disposed) setStatus(result); })
      .catch((error) => { if (!disposed) setStatus({ available: false, message: errorMessage(error), question_ids: [], max_length: 0 }); })
      .finally(() => { if (!disposed) setChecking(false); });
    return () => { disposed = true; };
  }, [session_key, request_id, send, retry, resolved_revision, control_revision, control_blocked]);
  if (!store || !state) return recorded_card;
  const disabled = state.sending || control_blocked;
  // Live recorded selections can resolve one question before a status read
  // catches up. Preserve the other drafts without offering it for resubmission.
  const pending = display.questions.filter((question) => question.question_id && status?.question_ids.includes(question.question_id)
    && !state.blocked_ids.includes(question.question_id) && !question.options.some((option) => option.is_selected) && question.written_answer === null);
  const pending_ids = new Set(pending.map((question) => question.question_id));
  const draft = (id: string, has_options: boolean): QuestionDraft => state.drafts[id] ?? { option_index: null, text: "", write: !has_options };
  const setDraft = (id: string, value: QuestionDraft) => store.update(key, (state) => ({ ...state, drafts: { ...state.drafts, [id]: value }, notice: null }));
  const valid = pending.length > 0 && pending.every((question) => {
    const value = draft(question.question_id!, question.options.length > 0);
    return value.write ? question.allows_free_text && value.text.trim() && [...value.text].length <= (status?.max_length ?? 0)
      : question.options.some((option) => option.option_index === value.option_index);
  });
  async function submit() {
    if (!status?.available || !valid || checking || control_blocked) return;
    const current = store!.get(key);
    if (current.sending || pending.some((question) => current.blocked_ids.includes(question.question_id!))) return;
    const answers: SessionQuestionAnswer[] = pending.map((question) => {
      const value = draft(question.question_id!, question.options.length > 0);
      return { question_id: question.question_id!, ...(value.write ? { text: value.text } : { option_index: value.option_index! }) };
    });
    const attempt = createUuid();
    store!.update(key, (state) => ({ ...state, sending: true, accepted: false, uncertain: false, notice: null }));
    let accepted = false;
    try {
      const result = await submitSessionQuestionAnswers({ session_key, question_request_id: request_id, request_id: attempt, answers }, store!.send);
      if (result.request_id === attempt && result.status === "accepted") {
        store!.update(key, (state) => {
          const drafts = { ...state.drafts };
          for (const answer of answers) delete drafts[answer.question_id];
          return { ...state, sending: false, accepted: true, drafts,
            blocked_ids: [...new Set([...state.blocked_ids, ...answers.map((answer) => answer.question_id)])],
            notice: "Answer submitted; waiting for the recorded reply." };
        });
        accepted = true;
      } else if (result.request_id === attempt && result.status === "not_sent") {
        store!.update(key, (state) => ({ ...state, sending: false, notice: result.message || "Answer was not sent. Your draft is preserved." }));
        setRetry((value) => value + 1);
      } else {
        store!.update(key, (state) => ({ ...state, sending: false, uncertain: true, blocked_ids: [...new Set([...state.blocked_ids, ...answers.map((answer) => answer.question_id)])], notice: "Delivery is not confirmed. Check the conversation before submitting again." }));
      }
    } catch (error) {
      store!.update(key, (state) => ({ ...state, sending: false, uncertain: true, blocked_ids: [...new Set([...state.blocked_ids, ...answers.map((answer) => answer.question_id)])], notice: `Delivery is not confirmed. Check the conversation before submitting again. ${errorMessage(error)}` }));
    }
    if (accepted) on_accepted?.(session_key);
  }
  if (checking || !status?.available || pending.length === 0) {
    return <>{recorded_card}<div className="question-answer__status" role="status">
      {state.notice ?? (checking ? "Checking answer availability…" : status?.message)}
      {!checking && !state.sending ? <button className="text-button" type="button" onClick={() => setRetry((value) => value + 1)}>Check availability</button> : null}
    </div></>;
  }
  return <form className="question-card question-answer" aria-label="Answer questions" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
    <div className="question-card__heading"><span className="question-card__symbol" aria-hidden="true">?</span><strong>Questions</strong></div>
    {display.questions.map((question, index) => {
      const id = question.question_id;
      if (!id || !pending_ids.has(id)) return <QuestionDisplaySection key={index} question={question} index={index} total={display.questions.length} />;
      const value = draft(id, question.options.length > 0);
      return <fieldset className="question-answer__question" key={id} disabled={disabled}>
        <legend>{question.header ?? `Question ${index + 1}`}<span className="question-card__count">{index + 1} of {display.questions.length}</span></legend>
        <MarkdownContent content={question.question ?? ""} />
        {question.options.length > 0 ? <div className="question-answer__choices">
          {question.options.map((option) => <label className="question-card__option question-answer__choice" data-chosen={!value.write && value.option_index === option.option_index} key={option.option_index}>
            <input type="radio" name={`${prefix}-${id}`} checked={!value.write && value.option_index === option.option_index}
              onChange={() => setDraft(id, { ...value, write: false, option_index: option.option_index })} />
            <div className="question-card__option-content"><div className="question-card__option-label"><strong>{option.label}</strong>
              {option.is_recommended ? <span className="question-card__recommended">Recommended</span> : null}</div>
              {option.description ? <p className="question-card__description">{option.description}</p> : null}</div>
          </label>)}
        </div> : null}
        {question.allows_free_text ? <label className="question-answer__written" data-chosen={value.write}>
          <span className="sr-only">Written answer for {question.header ?? question.question}</span>
          <input type={question.is_secret || status.secret_question_ids?.includes(id) ? "password" : "text"} value={value.text} maxLength={status.max_length}
            placeholder={question.options.length > 0 ? "Type your own answer…" : "Your answer…"}
            onFocus={() => { if (!value.write) setDraft(id, { ...value, write: true, option_index: null }); }}
            onChange={(event) => setDraft(id, { ...value, write: true, option_index: null, text: event.target.value })} />
        </label> : null}
      </fieldset>;
    })}
    <div className="question-answer__footer"><span role="status">{state.notice ?? (state.sending ? "Submitting answer…" : "")}</span>
      <button className="question-answer__submit" disabled={!valid || disabled} type="submit">{state.sending ? "Submitting…" : "Submit answers"}</button></div>
  </form>;
}

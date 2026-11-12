import { useEffect, useId, useRef, useState } from "react";
import { useSessionInteractions, type InteractionSubmission } from "../lib/useSessionInteractions";
import type { SessionInteraction, SessionInteractionQuestion, SessionInteractionResponse, SessionSummary } from "../lib/types";
import "./SessionInteractions.css";

const MAX_ANSWER_LENGTH = 16_384;
interface QuestionDraft { selected_indices: number[]; text: string }
const emptyDraft = (): QuestionDraft => ({ selected_indices: [], text: "" });

export function SessionInteractions({ session, refresh_key = 0, on_accepted }: {
  session: SessionSummary | null;
  refresh_key?: number;
  on_accepted?: (session_key: string) => void;
}) {
  const enabled = session?.provider === "zcode" && !session.is_subagent;
  const requests = useSessionInteractions(enabled ? session.session_key : null, refresh_key, on_accepted);
  const drafts = useRef(new Map<string, Record<string, QuestionDraft>>());
  useEffect(() => {
    const snapshot = requests.snapshot;
    if (!snapshot) return;
    const prefix = `${JSON.stringify([requests.owner]).slice(0, -1)},`;
    const pending = new Set(snapshot.interactions.map((interaction) => JSON.stringify([
      requests.owner, snapshot.runtime_id, snapshot.instance_id, interaction.interaction_id,
    ])));
    for (const key of drafts.current.keys()) {
      if (key.startsWith(prefix) && !pending.has(key)) drafts.current.delete(key);
    }
  }, [requests.owner, requests.snapshot]);
  if (!enabled || !requests.snapshot?.interactions.length && !requests.error && !requests.live_error) return null;
  return <section className="session-interactions" aria-label="Pending requests">
    <div className="session-interactions__heading">
      <strong>Agent requests</strong>
      <button className="text-button" type="button" disabled={requests.checking} onClick={() => { void requests.refresh(); }}>Refresh requests</button>
    </div>
    {requests.error || requests.live_error ? <p className="session-interactions__error" role="status">{requests.error ?? requests.live_error}</p> : null}
    {requests.snapshot?.interactions.map((interaction) => <InteractionCard
      key={requests.interactionKey(interaction.interaction_id)} interaction={interaction}
      submission={requests.submission(interaction.interaction_id)} unavailable={requests.checking || !!requests.error}
      initial_drafts={drafts.current.get(requests.interactionKey(interaction.interaction_id))}
      on_draft_change={(next) => {
        const key = requests.interactionKey(interaction.interaction_id);
        drafts.current.delete(key);
        drafts.current.set(key, next);
        if (drafts.current.size > 128) drafts.current.delete(drafts.current.keys().next().value!);
      }}
      on_respond={(response) => { void requests.respond(interaction.interaction_id, response); }} />)}
  </section>;
}

function InteractionCard({ interaction, submission, unavailable, initial_drafts, on_draft_change, on_respond }: {
  interaction: SessionInteraction;
  submission: InteractionSubmission | undefined;
  unavailable: boolean;
  initial_drafts: Record<string, QuestionDraft> | undefined;
  on_draft_change: (drafts: Record<string, QuestionDraft>) => void;
  on_respond: (response: SessionInteractionResponse) => void;
}) {
  const prefix = useId();
  const [drafts, setDrafts] = useState<Record<string, QuestionDraft>>(() => initial_drafts ?? {});
  const submitted = !!submission && submission.status !== "not_sent";
  const disabled = unavailable || submitted || interaction.response_pending;
  const draftFor = (question: SessionInteractionQuestion) => drafts[question.question_id] ?? emptyDraft();
  const valid = interaction.questions.length > 0 && interaction.questions.every((question) => {
    const draft = draftFor(question);
    const has_text = !!draft.text.trim();
    return (draft.selected_indices.length > 0 || question.allows_free_text && has_text)
      && (!has_text || question.allows_free_text && Array.from(draft.text).length <= MAX_ANSWER_LENGTH)
      && (!draft.selected_indices.length || !has_text || question.allows_option_notes);
  });
  function answer() {
    if (!valid || disabled) return;
    on_respond({ type: "answers", answers: interaction.questions.map((question) => {
      const draft = draftFor(question);
      return { question_id: question.question_id, answers: draft.selected_indices.map((index) => question.options[index].label),
        ...(draft.text.trim() ? { custom: draft.text } : {}) };
    }) });
  }
  function setDraft(question: SessionInteractionQuestion, draft: QuestionDraft) {
    const next = { ...drafts, [question.question_id]: draft };
    on_draft_change(next);
    setDrafts(next);
  }
  function chooseOption(question: SessionInteractionQuestion, index: number) {
    const draft = draftFor(question);
    let selected_indices = [index];
    if (question.multiple) {
      selected_indices = draft.selected_indices.includes(index)
        ? draft.selected_indices.filter((selected) => selected !== index)
        : [...draft.selected_indices, index];
    }
    setDraft(question, { selected_indices, text: question.allows_option_notes ? draft.text : "" });
  }
  const message = interaction.response_pending ? "Response sent. Waiting for the request to close." : submission?.message;
  return <form className="session-interaction" aria-label={interaction.title || "Agent request"}
    onSubmit={(event) => { event.preventDefault(); answer(); }}>
    <h3>{interaction.title || (interaction.kind === "permission" ? "Permission requested" : "Questions")}</h3>
    {interaction.details ? <pre className="session-interaction__details">{interaction.details}</pre> : null}
    {interaction.kind === "question" ? interaction.questions.map((question) => {
      const draft = draftFor(question);
      const field_id = `${prefix}-${question.question_id}`;
      return <fieldset className="session-interaction__question" key={question.question_id} disabled={disabled}>
        <legend>{question.prompt}</legend>
        {question.options.length > 0 ? <div className="session-interaction__options">
          {question.options.map((option, index) => <label className="session-interaction__option" key={index}>
            <input type={question.multiple ? "checkbox" : "radio"} name={field_id} checked={draft.selected_indices.includes(index)}
              onChange={() => chooseOption(question, index)} />
            <span><strong>{option.label}</strong>{option.description ? <small>{option.description}</small> : null}</span>
          </label>)}
        </div> : null}
        {question.allows_free_text ? <label className="session-interaction__written" htmlFor={`${field_id}-text`}>
          {draft.selected_indices.length > 0 && question.allows_option_notes ? "Additional note" : "Written answer"}
          <input id={`${field_id}-text`} type={question.is_secret ? "password" : "text"} value={draft.text}
            maxLength={MAX_ANSWER_LENGTH} autoComplete="off" spellCheck={!question.is_secret}
            onChange={(event) => setDraft(question, { selected_indices: question.allows_option_notes ? draft.selected_indices : [], text: event.currentTarget.value })} />
        </label> : null}
        {question.allows_free_text && draft.selected_indices.length > 0 && question.allows_option_notes && !question.multiple
          ? <button className="text-button" type="button" onClick={() => setDraft(question, { ...draft, selected_indices: [] })}>Clear selection</button> : null}
      </fieldset>;
    }) : null}
    <footer className="session-interaction__actions">
      <span role="status">{message}</span>
      {interaction.kind === "permission" ? interaction.permission_decisions.map((decision) => <button
        className={decision === "allow_once" ? "question-answer__submit" : "text-button"} type="button" key={decision} disabled={disabled}
        onClick={() => on_respond({ type: "permission", decision })}>{decision === "allow_once" ? "Allow once" : "Deny"}</button>) : null}
      {interaction.kind === "question" ? <>
        <button className="text-button" type="button" disabled={disabled} onClick={() => on_respond({ type: "reject" })}>Decline</button>
        <button className="question-answer__submit" type="submit" disabled={disabled || !valid}>Submit answers</button>
      </> : null}
    </footer>
  </form>;
}

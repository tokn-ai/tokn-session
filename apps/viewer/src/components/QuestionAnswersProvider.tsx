import { createContext, useCallback, useRef, useState, type ReactNode } from "react";
import { captureTransport, type CommandInvoker } from "../lib/transport";

export interface QuestionDraft {
  option_index: number | null;
  text: string;
  write: boolean;
}
export interface QuestionAnswerState {
  drafts: Record<string, QuestionDraft>;
  blocked_ids: string[];
  sending: boolean;
  uncertain: boolean;
  accepted: boolean;
  notice: string | null;
}
const EMPTY: QuestionAnswerState = { drafts: {}, blocked_ids: [], sending: false, uncertain: false, accepted: false, notice: null };
interface AnswerStore {
  send: CommandInvoker;
  get: (key: string) => QuestionAnswerState;
  update: (key: string, apply: (state: QuestionAnswerState) => QuestionAnswerState) => void;
  revision: number;
}
export const QuestionAnswersContext = createContext<AnswerStore | null>(null);

/** Viewer lifetime scopes drafts/delivery results to one machine. Nothing is
 * persisted to browser storage; closing a card retains its in-memory draft. */
export function QuestionAnswersProvider({ children, send: supplied }: { children: ReactNode; send?: CommandInvoker }) {
  const [send] = useState(() => supplied ?? captureTransport().invoke);
  const records = useRef(new Map<string, QuestionAnswerState>());
  const [revision, render] = useState(0);
  const get = useCallback((key: string) => ({ ...EMPTY, ...records.current.get(key) }), []);
  const update = useCallback((key: string, apply: (state: QuestionAnswerState) => QuestionAnswerState) => {
    const next = apply({ ...EMPTY, ...records.current.get(key) });
    records.current.delete(key);
    records.current.set(key, next);
    // Keep in-flight sends pinned. Older drafts have bounded recent coverage;
    // host-side claims still protect an evicted uncertain submission.
    if (records.current.size > 128) {
      for (const [old_key, state] of records.current) {
        if (old_key !== key && !state.sending) {
          records.current.delete(old_key); break;
        }
      }
    }
    render((value) => value + 1);
  }, []);
  return <QuestionAnswersContext.Provider value={{ send, get, update, revision }}>{children}</QuestionAnswersContext.Provider>;
}

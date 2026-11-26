import type { SessionActivityFilter, SessionSummary } from "./types";

export const SESSION_ACTIVITY_LABELS: Record<SessionActivityFilter, string> = {
  all: "All sessions",
  needs_input: "Needs input",
  running: "Running",
  unread: "Unread",
};

export function matchesSessionActivity(session: SessionSummary, filter: SessionActivityFilter) {
  switch (filter) {
    case "needs_input": return (session.question_attention?.required_count ?? 0) > 0;
    case "running": return session.is_running === true && !session.question_attention?.required_count;
    case "unread": return session.has_unread;
    case "all": return true;
  }
}

export function sessionActivity(session: SessionSummary, filter: SessionActivityFilter = "all") {
  const required = session.question_attention?.required_count ?? 0;
  const available = session.question_attention?.available_count ?? 0;
  if (filter === "unread" && session.has_unread) return { label: "Unread reply", tone: "unread", question: false } as const;
  if (filter === "running" && session.is_running) return { label: "Running", tone: "running", question: false } as const;
  if (required > 0) return { label: "Needs input", tone: "input", question: true } as const;
  if (available > 0) return { label: "Question available", tone: "question", question: true } as const;
  if (session.is_running) return { label: "Running", tone: "running", question: false } as const;
  if (session.has_running_descendant) return { label: "Subagent running", tone: "running", question: false } as const;
  if (session.has_unread) return { label: "Unread reply", tone: "unread", question: false } as const;
  return { label: "", tone: "quiet", question: false } as const;
}

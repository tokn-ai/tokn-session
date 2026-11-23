import { createHash } from "node:crypto";
import type { SessionListRow, SessionSummary } from "../src/lib/types";

export function revisionedSession(session: SessionSummary): SessionListRow {
  return { session, revision: createHash("sha256").update(JSON.stringify(session)).digest("hex") };
}

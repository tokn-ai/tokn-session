import { createHash } from "node:crypto";
import type { SessionListEntry, SessionSummary } from "../src/lib/types";

export function revisionedSession(session: SessionSummary): SessionListEntry {
  return { session, revision: createHash("sha256").update(JSON.stringify(session)).digest("hex") };
}

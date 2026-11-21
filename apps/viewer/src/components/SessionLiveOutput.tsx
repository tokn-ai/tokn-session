import type { SessionLiveOutput as LiveOutput } from "../lib/types";
import "./SessionLiveOutput.css";

/** Current native work is a preview, never an invented saved transcript row. */
export function SessionLiveOutput({ output }: { output: LiveOutput | null | undefined }) {
  if (!output?.parts.length) return null;
  return <section className="session-live-output" aria-label="Live response">
    <header><strong>Live response</strong><small>Working</small></header>
    {output.parts.map((part, index) => part.kind === "reasoning"
      ? <details key={index}><summary>Thinking</summary><pre>{part.text}</pre></details>
      : part.kind === "tool" ? <p className="session-live-output__tool" key={index}><strong>{part.text}</strong>{part.status ? <small>{part.status.replace(/_/g, " ")}</small> : null}</p>
      : <pre key={index}>{part.text}</pre>)}
    {output.truncated ? <small>Live preview truncated.</small> : null}
  </section>;
}

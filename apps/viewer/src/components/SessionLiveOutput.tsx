import type { SessionLiveOutput as LiveOutput } from "../lib/types";
import { ToolIcon } from "./Icons";
import { MarkdownContent } from "./MarkdownContent";
import "./SessionLiveOutput.css";

/** Native output stays transient until a recorded event arrives. */
export function SessionLiveOutput({ output }: { output: LiveOutput | null | undefined }) {
  if (!output?.parts.length) return null;
  return <section className="session-live-output" key={output.prompt_id} aria-label="Live response">
    {output.parts.map((part, index) => part.kind === "reasoning"
      ? <details key={index}><summary>Thinking</summary><MarkdownContent content={part.text} partial literal_html /></details>
      : part.kind === "tool" ? <p className="session-live-output__tool" key={index}>
          <ToolIcon width={14} height={14} />{part.text}
          {part.status && <small>{part.status.replace(/_/g, " ")}</small>}
        </p>
      : <MarkdownContent key={index} class_name="message-event__text" content={part.text} partial literal_html />)}
    {output.truncated && <small>Live preview truncated.</small>}
    <span className="session-live-output__status" role="status">Working</span>
  </section>;
}

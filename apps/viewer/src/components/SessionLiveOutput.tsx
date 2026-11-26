import { useLayoutEffect, useRef } from "react";
import type { SessionLiveOutput as LiveOutput } from "../lib/types";
import { ActivityIcon, ToolIcon } from "./Icons";
import { MarkdownContent } from "./MarkdownContent";
import "./SessionLiveOutput.css";

/** Current native work is a preview, never an invented saved transcript row. */
export function SessionLiveOutput({ output }: { output: LiveOutput | null | undefined }) {
  if (!output?.parts.length) return null;
  return <LiveResponse key={output.prompt_id} output={output} />;
}

function LiveResponse({ output }: { output: LiveOutput }) {
  const content = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  useLayoutEffect(() => {
    if (following.current && content.current) content.current.scrollTop = content.current.scrollHeight;
  }, [output]);

  return <section className="session-live-output" aria-label="Live response">
    <header className="session-live-output__header">
      <ActivityIcon width={16} height={16} />
      <strong>Live response</strong>
      <span className="session-live-output__status">Working</span>
    </header>
    <div className="session-live-output__parts" ref={content} role="region" aria-label="Live response content" tabIndex={0}
      onScroll={({ currentTarget }) => {
        following.current = currentTarget.scrollHeight - currentTarget.scrollTop - currentTarget.clientHeight <= 12;
      }}>
      {output.parts.map((part, index) => part.kind === "reasoning"
        ? <details key={index}><summary>Thinking</summary><MarkdownContent content={part.text} partial literal_html /></details>
        : part.kind === "tool" ? <p className="session-live-output__tool" key={index}>
            <ToolIcon width={14} height={14} /><strong>{part.text}</strong>
            {part.status ? <small>{part.status.replace(/_/g, " ")}</small> : null}
          </p>
        : <MarkdownContent key={index} content={part.text} partial literal_html />)}
      {output.truncated ? <small>Live preview truncated.</small> : null}
    </div>
  </section>;
}

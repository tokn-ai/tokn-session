import { useState } from "react";

const PREVIEW_LINES = 8;
const TAIL_LINES = 3;

/** A local disclosure never changes the provider's captured output. */
export function ToolTextSection({ text, heading, label, format = "text", terminal = false }: {
  text: string;
  heading?: string;
  label: string;
  format?: "text" | "json";
  terminal?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = text.split("\n");
  const hiddenLines = Math.max(0, lines.length - PREVIEW_LINES);
  const collapsed = !expanded && hiddenLines > 0;
  return (
    <section className={terminal ? "tool-output__section shell-output__section" : "tool-output__section"}>
      {heading ? <h4>{heading}</h4> : null}
      <pre aria-label={label} data-format={format} tabIndex={0}>
        {collapsed ? lines.slice(0, PREVIEW_LINES - TAIL_LINES).join("\n") : text}
      </pre>
      {hiddenLines > 0 ? (
        <button className="shell-output__disclosure" aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)} type="button">
          {expanded ? "Show fewer lines" : `Show ${hiddenLines} more lines`}
        </button>
      ) : null}
      {collapsed ? <pre aria-label={`${label} last lines`} data-format={format} tabIndex={0}>
        {lines.slice(-TAIL_LINES).join("\n")}
      </pre> : null}
    </section>
  );
}

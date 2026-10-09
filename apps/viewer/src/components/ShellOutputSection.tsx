import { useState } from "react";
import type { ToolOutputSection } from "../lib/types";

const PREVIEW_LINES = 8;

/** A local disclosure never changes the provider's captured output. */
export function ShellOutputSection({ section, command }: {
  section: ToolOutputSection;
  command: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = section.text.split("\n");
  const hiddenLines = Math.max(0, lines.length - PREVIEW_LINES);
  const text = expanded || hiddenLines === 0 ? section.text : lines.slice(0, PREVIEW_LINES).join("\n");
  return (
    <section className="tool-output__section shell-output__section">
      {section.label ? <h4>{section.label}</h4> : null}
      <pre aria-label={`${command} ${section.label ? `${section.label} output` : "output"}`}
        data-format={section.format} tabIndex={0}>{text}</pre>
      {hiddenLines > 0 ? (
        <button className="shell-output__disclosure" aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)} type="button">
          {expanded ? "Show fewer lines" : `Show ${hiddenLines} more lines`}
        </button>
      ) : null}
    </section>
  );
}

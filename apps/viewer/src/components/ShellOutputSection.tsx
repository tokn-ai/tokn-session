import { useState } from "react";
import type { ToolOutputSection } from "../lib/types";

const PREVIEW_LINES = 8;
const TAIL_LINES = 3;

/** A local disclosure never changes the provider's captured output. */
export function ShellOutputSection({ section, command }: {
  section: ToolOutputSection;
  command: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = section.text.split("\n");
  const hiddenLines = Math.max(0, lines.length - PREVIEW_LINES);
  const collapsed = !expanded && hiddenLines > 0;
  const label = `${command} ${section.label ? `${section.label} output` : "output"}`;
  return (
    <section className="tool-output__section shell-output__section">
      {section.label ? <h4>{section.label}</h4> : null}
      <pre aria-label={label} data-format={section.format} tabIndex={0}>
        {collapsed ? lines.slice(0, PREVIEW_LINES - TAIL_LINES).join("\n") : section.text}
      </pre>
      {hiddenLines > 0 ? (
        <button className="shell-output__disclosure" aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)} type="button">
          {expanded ? "Show fewer lines" : `Show ${hiddenLines} more lines`}
        </button>
      ) : null}
      {collapsed ? <pre aria-label={`${label} last lines`} data-format={section.format} tabIndex={0}>
        {lines.slice(-TAIL_LINES).join("\n")}
      </pre> : null}
    </section>
  );
}

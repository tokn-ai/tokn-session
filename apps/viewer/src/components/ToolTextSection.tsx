import { Fragment, useState } from "react";

const PREVIEW_LINES = 8;
const TAIL_LINES = 3;
const MAX_STYLED_LINES = 400;
type SectionFormat = "text" | "json" | "diff" | "before" | "after";

function sectionLines(lines: string[], format: SectionFormat) {
  if (format === "before") return <del>{lines.join("\n")}</del>;
  if (format === "after") return <ins>{lines.join("\n")}</ins>;
  // Keep large expanded patches selectable without mounting a node per line.
  // Their original unified-diff signs remain in the text.
  if (format !== "diff" || lines.length > MAX_STYLED_LINES) return lines.join("\n");
  return lines.map((line, index) => {
    const header = /^(?:@@|\*\*\* |diff --git |--- |\+\+\+ )/.test(line);
    const content = header ? <span className="tool-diff__header">{line}</span>
      : line.startsWith("+") ? <ins>{line}</ins> : line.startsWith("-") ? <del>{line}</del> : line;
    return <Fragment key={index}>{index > 0 ? "\n" : null}{content}</Fragment>;
  });
}

/** A local disclosure never changes the provider's captured output. */
export function ToolTextSection({ text, heading, label, format = "text", terminal = false }: {
  text: string;
  heading?: string;
  label: string;
  format?: SectionFormat;
  terminal?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = text.split("\n");
  const hiddenLines = Math.max(0, lines.length - PREVIEW_LINES);
  const collapsed = !expanded && hiddenLines > 0;
  return (
    <section className={terminal ? "tool-output__section shell-output__section" : "tool-output__section"}>
      {heading ? <h4>
        {format === "before" || format === "after" ? <span aria-hidden="true">{format === "before" ? "− " : "+ "}</span> : null}
        {heading}
      </h4> : null}
      <pre aria-label={label} data-format={format} tabIndex={0}>
        {sectionLines(collapsed ? lines.slice(0, PREVIEW_LINES - TAIL_LINES) : lines, format)}
      </pre>
      {hiddenLines > 0 ? (
        <button className="shell-output__disclosure" aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)} type="button">
          {expanded ? "Show fewer lines" : `Show ${hiddenLines} more lines`}
        </button>
      ) : null}
      {collapsed ? <pre aria-label={`${label} last lines`} data-format={format} tabIndex={0}>
        {sectionLines(lines.slice(-TAIL_LINES), format)}
      </pre> : null}
    </section>
  );
}

import { useState } from "react";
import type { JsonValue } from "../lib/types";
import { ToolTextSection } from "./ToolTextSection";

type InputBlock = { heading: string; text: string; format?: "json" };

function object(value: JsonValue | undefined): value is Record<string, JsonValue> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function editBlocks(input: JsonValue): InputBlock[] | null {
  if (typeof input === "string") return [{ heading: "Patch", text: input }];
  if (!object(input)) return null;
  const patch = input.unified_diff ?? input.patchText ?? input.patch;
  if (typeof patch === "string") return [{ heading: "Patch", text: patch }];
  const before = input.oldText ?? input.oldString ?? input.old_string;
  const after = input.newText ?? input.newString ?? input.new_string;
  if (typeof before === "string" && typeof after === "string") {
    return [{ heading: "Find", text: before }, { heading: "Replace with", text: after }];
  }
  // Codex records multi-file patches as a path-keyed change map. Keep larger or
  // mixed operation maps together so the preview never silently omits a file.
  const entries = Object.entries(input);
  if (entries.length > 0 && entries.length <= 8) {
    const blocks: InputBlock[] = [];
    for (const [heading, change] of entries) {
      if (!object(change) || typeof change.unified_diff !== "string") return null;
      blocks.push({ heading, text: change.unified_diff });
    }
    return blocks;
  }
  return null;
}

function inputBlocks(kind: string, input: JsonValue): InputBlock[] {
  if (kind === "file_edit") {
    const edits = editBlocks(input);
    if (edits) return edits;
  }
  if (kind === "file_write" && object(input) && typeof input.content === "string") {
    return [{ heading: "File content", text: input.content }];
  }
  if (kind === "code_execution") {
    const code = object(input) ? input.code : input;
    if (typeof code === "string") return [{ heading: "Code", text: code }];
  }
  return typeof input === "string" ? [{ heading: "Input", text: input }]
    : [{ heading: "Parameters", text: JSON.stringify(input, null, 2), format: "json" }];
}

/** Only render the operation's bounded normalized input, never provider-native data. */
export function ToolInputDetails({ kind, record, has_output }: { kind: string; record: JsonValue; has_output: boolean }) {
  const authored = kind === "file_edit" || kind === "file_write" || kind === "code_execution";
  const [open, setOpen] = useState(authored && !has_output);
  if (!object(record) || record.redacted === true || record.truncated === true || record.input == null) return null;
  const label = kind === "file_edit" ? "Requested changes" : kind === "file_write" ? "Requested file content"
    : kind === "code_execution" ? "Submitted code" : "Tool input";
  return <details className="tool-input" open={open} onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>{label}</summary>
    {open && <div className="tool-input__blocks">
      {inputBlocks(kind, record.input).map((block, index) => <ToolTextSection key={index} heading={block.heading}
        text={block.text} format={block.format} label={`${label} · ${block.heading}`} />)}
    </div>}
  </details>;
}

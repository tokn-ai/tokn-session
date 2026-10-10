import type { Nodes, Text } from "mdast";
import { decodeString } from "micromark-util-decode-string";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import { unified } from "unified";

const MAX_CONTENT_BYTES = 512 * 1024;
const MAX_SEGMENT_BYTES = 16 * 1024;
const MAX_BATCH_BYTES = 64 * 1024;
const MAX_BATCH_SEGMENTS = 128;
const MAX_SEGMENTS = 4096;
const MAX_OUTPUT_BYTES = 4 * 1024 * 1024;
const encoder = new TextEncoder();
const parser = unified().use(remarkParse).use(remarkGfm);
const token_pattern = /\\[!-/:-@\[-`{-~]|&(?:#(?:\d{1,7}|x[\da-f]{1,6})|[\da-z]{1,31});|[\s\S]/giu;

interface SourceToken {
  start: number;
  end: number;
  value: string;
}

interface Replacement {
  start: number;
  end: number;
  text: string;
  translated?: string;
}

interface SourceSpan {
  start: number;
  end: number;
}

class MarkdownSourceError extends Error {}

/**
 * Translate prose while retaining the original Markdown source around it.
 * Text nodes and existing line breaks are translation boundaries. This keeps
 * code and formatting exact, at the cost of context across inline formatting.
 */
export async function translateMarkdown(
  content: string,
  translate: (texts: string[]) => Promise<string[]>,
): Promise<string> {
  if (encoder.encode(content).length > MAX_CONTENT_BYTES) {
    throw new Error("This response is too large to translate (512 KiB maximum).");
  }

  const tree = parser.parse(content);
  const replacements: Replacement[] = [];
  const reference_expansions: Replacement[] = [];

  function visit(node: Nodes, inferred_span?: SourceSpan) {
    // Autolinks display their destinations, which must never be translated.
    if (node.type === "link" && (!node.position || !sourceFor(node, content).startsWith("["))) return;

    if (node.type === "text") {
      // GFM synthesizes positionless text around literal www autolinks. If its
      // source span cannot be recovered, leave that text alone.
      if (node.position) {
        replacements.push(...proseReplacements(node, content));
      } else if (inferred_span) {
        try {
          replacements.push(...proseReplacements(node, content, inferred_span));
        } catch (error) {
          if (!(error instanceof MarkdownSourceError)) throw error;
          // Preserving an uncertain fragment is safer than replacing a link.
        }
      }
      if (replacements.length > MAX_SEGMENTS) {
        throw new Error("This response has too many text fragments to translate.");
      }
    }

    const before = replacements.length;
    if ("children" in node) {
      const spans = childSpans(node, content);
      node.children.forEach((child, index) => visit(child, spans[index]));
    }

    // A translated shortcut label would otherwise refer to a different target.
    if (node.type === "linkReference" && node.referenceType !== "full"
      && replacements.length > before) {
      const end = node.position?.end.offset;
      if (end === undefined || !node.label) throw new Error("Cannot preserve this Markdown reference.");
      const start = node.referenceType === "collapsed" ? end - 1 : end;
      const text = node.referenceType === "collapsed" ? node.label : `[${node.label}]`;
      reference_expansions.push({ start, end: start, text, translated: text });
    }
  }

  visit(tree);
  if (replacements.length === 0) return content;

  let output_bytes = 0;
  for (let start = 0; start < replacements.length;) {
    let end = start;
    let batch_bytes = 0;
    while (end < replacements.length && end - start < MAX_BATCH_SEGMENTS) {
      const size = encoder.encode(replacements[end].text).length;
      if (batch_bytes + size > MAX_BATCH_BYTES) break;
      batch_bytes += size;
      end += 1;
    }
    const batch = replacements.slice(start, end);
    const translated = await translate(batch.map((part) => part.text));
    if (!Array.isArray(translated) || translated.length !== batch.length
      || batch.some((_, index) => typeof translated[index] !== "string" || !translated[index].trim())) {
      throw new Error("The translator returned an incomplete response. Try again.");
    }
    translated.forEach((text, index) => {
      output_bytes += encoder.encode(text).length;
      if (output_bytes > MAX_OUTPUT_BYTES) throw new Error("The translated response is too large.");
      const part = batch[index];
      part.translated = text === part.text
        ? content.slice(part.start, part.end)
        : escapeMarkdownText(text);
    });
    start = end;
  }

  let result = "";
  let cursor = 0;
  for (const part of [...replacements, ...reference_expansions].sort((a, b) => a.start - b.start)) {
    result += content.slice(cursor, part.start) + part.translated;
    cursor = part.end;
  }
  result += content.slice(cursor);

  // Escaping protects against newly introduced Markdown. Reparse as a final
  // guard against contextual emphasis and GFM's decoded-text autolink pass,
  // which can recognize even entity-escaped URLs introduced by a translator.
  if (JSON.stringify(structure(tree)) !== JSON.stringify(structure(parser.parse(result)))) {
    throw new Error("The translation changed Markdown formatting. Try again.");
  }
  return result;
}

function sourceFor(node: Nodes, content: string): string {
  return content.slice(node.position?.start.offset, node.position?.end.offset);
}

function childSpans(parent: Nodes & { children: Nodes[] }, content: string): (SourceSpan | undefined)[] {
  const spans = parent.children.map((child): SourceSpan | undefined => {
    const start = child.position?.start.offset;
    const end = child.position?.end.offset;
    return start === undefined || end === undefined ? undefined : { start, end };
  });
  if (spans.every(Boolean)) return spans;
  const parent_start = parent.position?.start.offset;
  const parent_end = parent.position?.end.offset;
  if (parent_start === undefined || parent_end === undefined) return spans;

  const next_positioned_start: number[] = Array(parent.children.length);
  let next_start = parent_end;
  for (let index = parent.children.length - 1; index >= 0; index -= 1) {
    next_positioned_start[index] = next_start;
    if (spans[index]) next_start = spans[index]!.start;
  }

  let cursor = parent_start;
  let found_synthetic_link = false;
  let search_safe = true;
  const synthetic_links = Array(parent.children.length).fill(false) as boolean[];
  for (let index = 0; index < parent.children.length; index += 1) {
    if (spans[index]) {
      cursor = spans[index]!.end;
      search_safe = true;
      continue;
    }
    const child = parent.children[index];
    // remark-gfm omits positions on literal www links and normalizes their
    // targets to http://. Their visible labels remain exact source slices.
    if (child.type !== "link" || child.children.length !== 1
      || child.children[0].type !== "text" || !child.children[0].value.startsWith("www.")
      || child.url !== `http://${child.children[0].value}`) {
      if (child.type !== "text") search_safe = false;
      continue;
    }
    if (!search_safe) continue;
    const start = content.indexOf(child.children[0].value, cursor);
    if (start < 0 || start + child.children[0].value.length > next_positioned_start[index]) continue;
    spans[index] = { start, end: start + child.children[0].value.length };
    synthetic_links[index] = true;
    found_synthetic_link = true;
    cursor = spans[index]!.end;
  }
  if (!found_synthetic_link) return spans;

  for (let index = 0; index < parent.children.length;) {
    if (parent.children[index].type !== "text" || spans[index]) {
      index += 1;
      continue;
    }
    let run_end = index + 1;
    while (run_end < parent.children.length
      && parent.children[run_end].type === "text" && !spans[run_end]) run_end += 1;
    // A missing position on another kind of node leaves the intervening raw
    // source ambiguous. Only infer text touching a validated www link.
    if ((index > 0 && !spans[index - 1])
      || (run_end < parent.children.length && !spans[run_end])
      || !(synthetic_links[index - 1] || synthetic_links[run_end])) {
      index = run_end;
      continue;
    }
    let start = index === 0 ? parent_start : spans[index - 1]!.end;
    const end = run_end === parent.children.length ? parent_end : spans[run_end]!.start;
    for (let part = index; part < run_end && start <= end; part += 1) {
      const next = part + 1 === run_end ? end
        : decodedTextEnd(content, (parent.children[part] as Text).value, start, end);
      if (next === undefined) break;
      spans[part] = { start, end: next };
      start = next;
    }
    index = run_end;
  }
  return spans;
}

function decodedTextEnd(content: string, value: string, start: number, end: number): number | undefined {
  if (!value) return start;
  const tokens = [...content.slice(start, end).matchAll(token_pattern)].map((match) => ({
    end: start + match.index! + match[0].length,
    value: decodeString(match[0]).replace(/\0/g, "\uFFFD"),
  }));
  const offset = tokens.map((token) => token.value).join("").indexOf(value);
  if (offset < 0) return undefined;
  let decoded_end = 0;
  for (const token of tokens) {
    decoded_end += token.value.length;
    if (decoded_end >= offset + value.length) return token.end;
  }
  return undefined;
}

function proseReplacements(node: Text, content: string, inferred_span?: SourceSpan): Replacement[] {
  const start = inferred_span?.start ?? node.position?.start.offset;
  const end = inferred_span?.end ?? node.position?.end.offset;
  if (start === undefined || end === undefined) throw new MarkdownSourceError("Cannot locate Markdown text.");

  const lines = [...content.slice(start, end).matchAll(/([^\r\n]*)(\r\n|\r|\n|$)/g)]
    .filter((match) => match[0].length > 0);
  const replacements: Replacement[] = [];
  let remaining = node.value;

  // Text positions span container prefixes (e.g. '> ' and list indentation).
  // Match decoded source tokens backwards to retain those prefixes exactly,
  // including escaped characters, entities, and CRLF line endings.
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const line = lines[index];
    const raw = line[2] ? line[1].replace(/[\t ]+$/, "") : line[1];
    const tokens = [...raw.matchAll(token_pattern)].map((match): SourceToken => ({
      start: start + line.index! + match.index!,
      end: start + line.index! + match.index! + match[0].length,
      value: decodeString(match[0]).replace(/\0/g, "\uFFFD"),
    }));

    let token_start = tokens.length;
    while (token_start > 0 && remaining.endsWith(tokens[token_start - 1].value)) {
      token_start -= 1;
      remaining = remaining.slice(0, remaining.length - tokens[token_start].value.length);
    }
    const prefix_end = tokens[token_start]?.start ?? start + line.index! + raw.length;
    const prefix = content.slice(start + line.index!, prefix_end);
    if (!inferred_span && !/^[\t >]*$/.test(prefix)) {
      throw new MarkdownSourceError("Cannot preserve this Markdown text.");
    }
    if (index > 0) {
      const line_ending = lines[index - 1][2];
      if (!remaining.endsWith(line_ending)) throw new MarkdownSourceError("Cannot preserve Markdown line breaks.");
      remaining = remaining.slice(0, -line_ending.length);
    }

    const prose = tokens.slice(token_start);
    let group_start = 0;
    for (let token_index = 0; token_index <= prose.length; token_index += 1) {
      // Encoded line endings also become breaks in the existing renderer.
      if (token_index === prose.length || /[\r\n]/.test(prose[token_index].value)) {
        replacements.push(...proseOnlyReplacements(prose.slice(group_start, token_index)));
        group_start = token_index + 1;
      }
    }
  }
  if (remaining) throw new MarkdownSourceError("Cannot preserve this Markdown text.");
  return replacements.sort((a, b) => a.start - b.start);
}

function proseOnlyReplacements(tokens: SourceToken[]): Replacement[] {
  // GFM handles ordinary autolinks. Also protect displayed URL labels and
  // provider/file URLs that its autolink grammar does not recognize.
  const value = tokens.map((token) => token.value).join("");
  const urls = [...value.matchAll(/\b[a-z][a-z\d+.-]*:\/\/[^\s<>]+|\bwww\.[^\s<>]+|\b[^\s<>@]+@[^\s<>@]+\.[^\s<>@]+/giu)];
  if (urls.length === 0) return boundedReplacements(tokens);
  const result: Replacement[] = [];
  let group_start = 0;
  let offset = 0;
  let url_index = 0;
  for (let index = 0; index < tokens.length; index += 1) {
    const next_offset = offset + tokens[index].value.length;
    while (url_index < urls.length && offset >= urls[url_index].index! + urls[url_index][0].length) url_index += 1;
    if (url_index < urls.length && next_offset > urls[url_index].index!) {
      result.push(...boundedReplacements(tokens.slice(group_start, index)));
      group_start = index + 1;
    }
    offset = next_offset;
  }
  result.push(...boundedReplacements(tokens.slice(group_start)));
  return result;
}

function boundedReplacements(tokens: SourceToken[]): Replacement[] {
  const result: Replacement[] = [];
  for (let start = 0; start < tokens.length;) {
    let end = start;
    let bytes = 0;
    let word_boundary = start;
    while (end < tokens.length) {
      const size = encoder.encode(tokens[end].value).length;
      if (bytes + size > MAX_SEGMENT_BYTES) break;
      bytes += size;
      end += 1;
      if (/\s/u.test(tokens[end - 1].value)) word_boundary = end;
    }
    if (end < tokens.length && word_boundary > start) end = word_boundary;
    const next = end;
    while (start < end && /^\s+$/u.test(tokens[start].value)) start += 1;
    while (end > start && /^\s+$/u.test(tokens[end - 1].value)) end -= 1;
    const text = tokens.slice(start, end).map((token) => token.value).join("");
    if (/\p{L}/u.test(text)) result.push({ start: tokens[start].start, end: tokens[end - 1].end, text });
    start = next;
  }
  return result;
}

function escapeMarkdownText(text: string): string {
  // Preserve source whitespace separately. Translation cannot introduce a new
  // line, indentation, a table cell, an autolink, HTML, or a Markdown delimiter.
  return text.trim().replace(/\s+/gu, " ").replace(/[!-/:-@\[-`{-~]/g,
    (character) => `&#${character.charCodeAt(0)};`);
}

function structure(node: Nodes): unknown {
  if (node.type === "text") return [node.type, node.value.match(/\r\n|\r|\n/g)];
  return Object.fromEntries(Object.entries(node)
    .filter(([key]) => !["position", "data", "referenceType"].includes(key))
    .map(([key, value]) => [key, key === "children" ? value.map(structure) : value]));
}

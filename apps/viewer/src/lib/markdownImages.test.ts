import { describe, expect, it } from "vitest";
import { bindMarkdownImages } from "./markdownImages";
import type { MessageImageSummary } from "./types";
function image(start: number, end: number, image_id = "markdown-image"): MessageImageSummary {
  return { image_id, mime_type: "image/png", name: null, availability: "available", markdown_range: { start, end } };
}
describe("authorized Markdown image positions", () => {
  it("uses UTF-16 source positions after Unicode and reference definitions", () => {
    const inline = "![screen](/tmp/hello.png)";
    const reference = "![screen][image]";
    const prefix = "📸 演示\n\n";
    const content = `${prefix}${inline}\n\n${reference}\n\n[image]: /tmp/second.png`;
    const first = image(prefix.length, prefix.length + inline.length, "first");
    const start = content.indexOf(reference);
    const second = image(start, start + reference.length, "second");
    expect([...bindMarkdownImages(content, [first, second]).values()]).toEqual([first, second]);
  });
  it("keeps code examples, malformed references, mismatched and ambiguous ranges unbound", () => {
    const markdown = "![screen](/tmp/image.png)";
    expect(bindMarkdownImages(`\`${markdown}\``, [image(1, markdown.length + 1)]).size).toBe(0);
    expect(bindMarkdownImages("![screen][missing]", [image(0, 18)]).size).toBe(0);
    expect(bindMarkdownImages(markdown, [image(0, markdown.length - 1)]).size).toBe(0);
    expect(bindMarkdownImages(markdown, [image(0, markdown.length), image(0, markdown.length, "duplicate")]).size).toBe(0);
  });
  it("does not bind a host-authorized image outside a clipped first-screen message", () => {
    const content = "Opening text…";
    expect(bindMarkdownImages(content, [image(100, 150)]).size).toBe(0);
  });
});

it("rebases a leading BOM while preserving canonical metadata and emoji offsets", () => {
  const markdown = "![screen](/tmp/image.png)";
  const content = `\uFEFF📸 ${markdown}`;
  const start = content.indexOf(markdown);
  const authorized = image(start, start + markdown.length);
  const bindings = bindMarkdownImages(content, [authorized]);
  expect(bindings.get(`${start - 1}:${content.length - 1}`)).toBe(authorized);
  expect(authorized.markdown_range).toEqual({ start, end: content.length });
});

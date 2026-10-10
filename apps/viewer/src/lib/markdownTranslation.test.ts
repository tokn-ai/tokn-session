import { describe, expect, it, vi } from "vitest";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import { unified } from "unified";
import { translateMarkdown } from "./markdownTranslation";

const parser = unified().use(remarkParse).use(remarkGfm);

function translator(values: Record<string, string>) {
  return vi.fn(async (texts: string[]) => texts.map((text) => {
    if (!(text in values)) throw new Error(`Unexpected translation input: ${text}`);
    return values[text];
  }));
}

describe("translateMarkdown", () => {
  it("preserves headings, nested formatting, task lists, code, and table syntax", async () => {
    const source = [
      "# Overview",
      "",
      "Read **the *important* details** and `run --help`.",
      "",
      "- [x] Complete",
      "  - Pending",
      "",
      "| Name | Value |",
      "| :--- | ---: |",
      "| Example | `a \\| b` |",
      "",
      "```ts",
      "const message = 'do not translate';",
      "```",
    ].join("\n");
    const translate = translator({
      Overview: "概述", Read: "阅读", the: "这些", important: "重要", details: "细节", and: "以及",
      Complete: "完成", Pending: "待处理", Name: "名称", Value: "值", Example: "示例",
    });
    const translated = await translateMarkdown(source, translate);
    expect(translated).toBe(source
      .replace("Overview", "概述").replace("Read", "阅读").replace("the", "这些")
      .replace("important", "重要").replace("details", "细节").replace("and", "以及")
      .replace("Complete", "完成").replace("Pending", "待处理").replace("Name", "名称")
      .replace("Value", "值").replace("Example", "示例"));
  });

  it("retains soft breaks, hard breaks, container indentation, and CRLF", async () => {
    const source = "- > First line \r\n  > second line  \r\n  > third line\\\r\n  > final line";
    const translated = await translateMarkdown(source, translator({
      "First line": "第一行", "second line": "第二行", "third line": "第三行", "final line": "最后一行",
    }));
    expect(translated).toBe("- > 第一行 \r\n  > 第二行  \r\n  > 第三行\\\r\n  > 最后一行");
  });

  it("decodes escapes and entities while preserving entity line breaks and source whitespace", async () => {
    const source = "Use \\*literal\\* &amp; friendly &#10; words\n > here";
    const translate = translator({ "Use *literal* & friendly": "使用文字和友好", words: "词语", here: "这里" });
    expect(await translateMarkdown(source, translate)).toBe("使用文字和友好 &#10; 词语\n > 这里");
  });

  it("preserves URLs, HTML, image destinations, and reference definitions", async () => {
    const source = [
      "See [the guide](https://example.com/docs?q=hello#next \"title\") and [a reference][guide].",
      "",
      "https://example.com www.example.com <https://example.com> person@example.com",
      "",
      "![Image description](https://example.com/a.png)",
      "",
      "<div>Raw HTML remains unchanged</div>",
      "",
      "[guide]: https://example.com/reference \"Reference title\"",
    ].join("\n");
    expect(await translateMarkdown(source, translator({
      See: "查看", "the guide": "指南", and: "以及", "a reference": "参考资料",
    }))).toBe(source.replace("See", "查看").replace("the guide", "指南")
      .replace("and", "以及").replace("a reference", "参考资料"));
  });

  it("expands shortcut and collapsed reference links so translated labels still resolve", async () => {
    const source = "[Guide] and [Guide][]\n\n[Guide]: https://example.com";
    expect(await translateMarkdown(source, translator({ Guide: "指南", and: "以及" })))
      .toBe("[指南][Guide] 以及 [指南][Guide]\n\n[Guide]: https://example.com");
  });

  it("preserves URL labels and file or provider URLs embedded in ordinary prose", async () => {
    const source = "See [https://example.com](https://example.com) and file:///tmp/report.txt or s3://bucket/key";
    expect(await translateMarkdown(source, translator({ See: "查看", and: "以及", or: "或者" })))
      .toBe("查看 [https://example.com](https://example.com) 以及 file:///tmp/report.txt 或者 s3://bucket/key");
  });

  it("translates prose next to GFM www autolinks without changing the link", async () => {
    const source = "## Website:www.example.com and A &amp; B:www.example.org";
    expect(await translateMarkdown(source, translator({
      "Website:": "网站：", "and A & B:": "以及 甲和乙：",
    }))).toBe("## 网站：www.example.com 以及 甲和乙：www.example.org");
  });

  it("keeps punctuation and adjacent www autolinks out of translation requests", async () => {
    const source = "A:www.one.test! Then B:www.two.test.";
    const translate = translator({ "A:": "甲：", "Then B:": "然后乙：" });
    expect(await translateMarkdown(source, translate))
      .toBe("甲：www.one.test! 然后乙：www.two.test.");
    expect(translate).toHaveBeenCalledWith(["A:", "Then B:"]);
  });

  it("treats translator-provided Markdown, HTML, and newlines as prose", async () => {
    const malicious = "# Header\n\n- item | **bold** [link](/target) <script>alert(1)</script> &amp;";
    const result = await translateMarkdown("Original", async () => [malicious]);
    const tree = parser.parse(result);
    expect(tree.children).toHaveLength(1);
    expect(tree.children[0]).toMatchObject({ type: "paragraph", children: [
      { type: "text", value: malicious.replace(/\s+/g, " ") },
    ] });
    expect((tree.children[0] as { children: unknown[] }).children).toHaveLength(1);
  });

  it("rejects new GFM autolinks, which recognize even entity-escaped text", async () => {
    await expect(translateMarkdown("Original", async () => ["https://example.com www.example.com"]))
      .rejects.toThrow("changed Markdown formatting");
  });

  it("keeps translated punctuation inside existing table cells", async () => {
    const result = await translateMarkdown("| Column |\n| --- |\n| Cell |", translator({
      Column: "名称 | 属性", Cell: "[值] <文本>",
    }));
    expect(parser.parse(result).children[0]).toMatchObject({ type: "table", children: [
      { children: [{ children: [{ type: "text", value: "名称 | 属性" }] }] },
      { children: [{ children: [{ type: "text", value: "[值] <文本>" }] }] },
    ] });
  });

  it("rejects contextual formatting changes instead of rendering broken Markdown", async () => {
    await expect(translateMarkdown("a**word**b", translator({ a: "a", word: "[文本]", b: "b" })))
      .rejects.toThrow("changed Markdown formatting");
  });

  it.each([
    [], ["only one"], ["one", "two", "three"], ["one", 7], ["one", "  "], new Array(2), null,
  ])("rejects incomplete or malformed translation results: %j", async (result) => {
    await expect(translateMarkdown("First **second**", async () => result as string[]))
      .rejects.toThrow("incomplete response");
  });

  it("propagates translation failures without returning a partial response", async () => {
    await expect(translateMarkdown("Hello", async () => { throw new Error("Language pack unavailable"); }))
      .rejects.toThrow("Language pack unavailable");
  });

  it("returns code-only, HTML-only, and URL-only responses without invoking translation", async () => {
    const translate = vi.fn(async (texts: string[]) => texts);
    for (const source of ["", "123 + 456", "`source code`", "```ts\nconst value = 1;\n```",
      "https://example.com", "www.example.com", "www.one.test www.two.test", "<div>Hidden text</div>"]) {
      expect(await translateMarkdown(source, translate)).toBe(source);
    }
    expect(translate).not.toHaveBeenCalled();
  });

  it("retains the original escapes and entities when translation returns the same text", async () => {
    const source = "Text &amp; \\*other\\*\n  continuation";
    expect(await translateMarkdown(source, async (texts) => texts)).toBe(source);
  });

  it("batches many spans within native segment and UTF-8 byte limits", async () => {
    const source = Array.from({ length: 270 }, (_, index) => `- Paragraph ${index}`).join("\n");
    const translate = vi.fn(async (texts: string[]) => texts);
    expect(await translateMarkdown(source, translate)).toBe(source);
    expect(translate.mock.calls.map(([texts]) => texts.length)).toEqual([128, 128, 14]);
  });

  it("splits long Unicode spans without breaking code points or exceeding batch limits", async () => {
    const source = "漢字𠀀".repeat(12_000);
    const translate = vi.fn(async (texts: string[]) => texts);
    expect(await translateMarkdown(source, translate)).toBe(source);
    expect(translate.mock.calls.length).toBeGreaterThan(1);
    const encoder = new TextEncoder();
    for (const [texts] of translate.mock.calls) {
      expect(texts.length).toBeLessThanOrEqual(128);
      expect(texts.reduce((bytes, text) => bytes + encoder.encode(text).length, 0)).toBeLessThanOrEqual(64 * 1024);
      for (const text of texts) {
        expect(encoder.encode(text).length).toBeLessThanOrEqual(16 * 1024);
        expect(text).not.toMatch(/^[\uDC00-\uDFFF]|[\uD800-\uDBFF]$/);
      }
    }
  });

  it("rejects oversized source and translation output", async () => {
    const translate = vi.fn(async () => ["x".repeat(4 * 1024 * 1024 + 1)]);
    await expect(translateMarkdown("漢".repeat(175_000), translate)).rejects.toThrow("too large to translate");
    expect(translate).not.toHaveBeenCalled();
    await expect(translateMarkdown("Hello", translate)).rejects.toThrow("translated response is too large");
  });
});

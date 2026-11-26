import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MarkdownContent } from "./MarkdownContent";

afterEach(cleanup);

describe("MarkdownContent", () => {
  it("renders GFM tables, strikethrough, and task lists", () => {
    const { container } = render(
      <MarkdownContent
        content={[
          "~~removed~~",
          "",
          "| Name | State |",
          "| --- | --- |",
          "| viewer | ready |",
          "",
          "- [x] Render Markdown",
        ].join("\n")}
      />,
    );

    expect(screen.getByText("removed").tagName).toBe("DEL");
    expect(screen.getByRole("table")).toHaveTextContent("viewer");
    expect(screen.getByRole("table").parentElement).toHaveClass(
      "markdown-content__table-wrap",
    );
    expect(screen.getByRole("checkbox", { name: "Completed task" })).toBeChecked();
    expect(container.firstElementChild).toHaveClass("markdown-content");
  });

  it("renders fenced and inline code without interpreting either as HTML", () => {
    const { container } = render(
      <MarkdownContent content={"Use `<safe>` inline.\n\n```ts\nconst value = 1;\n```"} />,
    );
    const code = container.querySelectorAll("code");

    expect(code).toHaveLength(2);
    expect(code[0]).toHaveTextContent("<safe>");
    expect(code[0].parentElement?.tagName).toBe("P");
    expect(code[1]).toHaveClass("language-ts");
    expect(code[1].parentElement?.tagName).toBe("PRE");
    expect(container.querySelector("safe")).not.toBeInTheDocument();
  });

  it("copies the fenced block with its indentation and leaves the surrounding prose out", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });
    try {
      render(<MarkdownContent content={"Run this example.\n\n```sh\n  echo '<literal>'\n\n  echo finished\n```"} />);
      expect(screen.getByText("sh")).toBeVisible();
      fireEvent.click(screen.getByRole("button", { name: "Copy code" }));
      expect(await screen.findByRole("status")).toHaveTextContent("Copied");
      expect(writeText).toHaveBeenCalledWith("  echo '<literal>'\n\n  echo finished");
    } finally { vi.unstubAllGlobals(); }
  });

  it("keeps earlier blocks copyable while a later block is clipped or streaming", () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });
    try {
      const first = "~~~sh\necho first\n~~~\n\n";
      const { rerender } = render(<MarkdownContent partial content={first + "~~~~sh\necho sec…"} />);
      expect(screen.getByText("echo sec…")).toBeVisible();
      expect(screen.getAllByRole("button", { name: "Copy code" })).toHaveLength(1);
      fireEvent.click(screen.getByRole("button", { name: "Copy code" }));
      expect(writeText).toHaveBeenCalledWith("echo first");

      // Three tildes are still code inside a four-tilde fence.
      rerender(<MarkdownContent partial content={first + "~~~~sh\necho second\n~~~"} />);
      expect(screen.getAllByRole("button", { name: "Copy code" })).toHaveLength(1);
      rerender(<MarkdownContent partial content={first + "~~~~sh\necho second\n~~~~\n"} />);
      expect(screen.getAllByRole("button", { name: "Copy code" })).toHaveLength(2);
    } finally { vi.unstubAllGlobals(); }
  });

  it("handles a clipped quoted block and allows a complete message to end inside a fence", () => {
    vi.stubGlobal("navigator", { clipboard: { writeText: vi.fn() } });
    try {
      const content = "\uFEFF> ~~~sh\r\n> echo quoted\r\n";
      const { rerender } = render(<MarkdownContent partial content={content + "…"} />);
      expect(screen.queryByRole("button", { name: "Copy code" })).toBeNull();
      rerender(<MarkdownContent partial content={content + "> ~~~"} />);
      expect(screen.getByRole("button", { name: "Copy code" })).toBeVisible();
      rerender(<MarkdownContent content={content} />);
      expect(screen.getByRole("button", { name: "Copy code" })).toBeVisible();
    } finally { vi.unstubAllGlobals(); }
  });

  it("turns soft line breaks into visible break elements", () => {
    const { container } = render(<MarkdownContent content={"first line\nsecond line"} />);
    const paragraph = container.querySelector("p");

    expect(paragraph).toHaveTextContent("first line second line");
    expect(paragraph?.querySelector("br")).toBeInTheDocument();
  });

  it("drops raw HTML and never creates scriptable elements", () => {
    const { container } = render(
      <MarkdownContent
        content={'before<script src="https://example.test/run.js">alert(1)</script><iframe src="https://example.test"></iframe>after'}
      />,
    );

    expect(container.querySelector("script")).not.toBeInTheDocument();
    expect(container.querySelector("iframe")).not.toBeInTheDocument();
    expect(container.innerHTML).not.toContain("run.js");
  });

  it("renders links as inert text and rejects unsafe or custom destinations", () => {
    const { container } = render(
      <MarkdownContent
        content={[
          "[web](https://example.com)",
          "[script](javascript:alert(1))",
          "[data](data:text/html,unsafe)",
          "[custom](codex://task/123)",
        ].join(" ")}
      />,
    );

    expect(container.querySelectorAll(".markdown-content__link")).toHaveLength(4);
    expect(container.querySelector("a")).not.toBeInTheDocument();
    expect(container.querySelector("[href]")).not.toBeInTheDocument();
    expect(container.innerHTML).not.toMatch(/javascript:|data:text|codex:/i);
  });

  it("replaces images with accessible placeholders without loading a URL", () => {
    const { container } = render(
      <MarkdownContent
        content={'![architecture](https://example.com/tracker.png "diagram") ![](data:image/svg+xml,unsafe)'}
      />,
    );

    expect(container.querySelector("img")).not.toBeInTheDocument();
    expect(screen.getByRole("img", { name: "Image: architecture" })).toHaveClass(
      "markdown-content__image",
    );
    expect(screen.getByRole("img", { name: "Image" })).toHaveTextContent("[Image]");
    expect(container.innerHTML).not.toMatch(/tracker\.png|data:image/i);
  });
});

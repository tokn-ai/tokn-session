import { createContext, useContext } from "react";
import { InlineMessageImage } from "./MessageImages";
import { CopyTextButton } from "./CopyTextButton";
import { markdownImageRangeKey } from "../lib/markdownImages";
import type { MessageImageSummary } from "../lib/types";
import ReactMarkdown, { type Components, type UrlTransform } from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

export interface MarkdownImageContext {
  session_key: string;
  event_key: string;
  revision: string | null;
  bindings: ReadonlyMap<string, MessageImageSummary>;
}
interface MarkdownRenderContext {
  images?: MarkdownImageContext;
  boundary: { end: number; ends_with_fence: boolean } | null;
}
const MarkdownContext = createContext<MarkdownRenderContext>({ boundary: null });

interface MarkdownContentProps {
  content: string;
  class_name?: string;
  image_context?: MarkdownImageContext;
  literal_html?: boolean;
  partial?: boolean;
}

const ALLOWED_ELEMENTS = [
  "a",
  "blockquote",
  "br",
  "code",
  "del",
  "em",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "hr",
  "img",
  "input",
  "li",
  "ol",
  "p",
  "pre",
  "strong",
  "table",
  "tbody",
  "td",
  "th",
  "thead",
  "tr",
  "ul",
] as const;

const SAFE_LINK_PROTOCOLS = new Set(["http:", "https:"]);

const safeUrlTransform: UrlTransform = (url) => {
  try {
    const parsed = new URL(url);
    return SAFE_LINK_PROTOCOLS.has(parsed.protocol) ? parsed.href : "";
  } catch {
    return "";
  }
};

const MARKDOWN_COMPONENTS = {
  pre({ node, children }) {
    const { boundary } = useContext(MarkdownContext);
    const code = node?.children[0];
    if (code?.type !== "element" || code.tagName !== "code" || code.children[0]?.type !== "text") {
      return <pre>{children}</pre>;
    }
    const classes = code.properties.className;
    const language = Array.isArray(classes) && classes.find(name => typeof name === "string" && name.startsWith("language-"));
    const text = code.children[0].value.replace(/\n$/, "");
    const position = node?.position;
    const touchesBoundary = boundary && (position?.end.offset ?? Infinity) >= boundary.end;
    // A parsed closing fence occupies a source line that is absent from the code
    // body. This also distinguishes fence-like code inside lists and blockquotes.
    const closedAtBoundary = touchesBoundary && boundary.ends_with_fence && position && position.end.column > 1
      && position.end.line - position.start.line > (text ? text.split(/\r\n|[\r\n]/).length : 0);
    return <figure className="markdown-code">
      <figcaption>
        <span>{typeof language === "string" ? language.slice("language-".length) : "Code"}</span>
        {(!touchesBoundary || closedAtBoundary) && <span className="markdown-code__actions">
          <CopyTextButton text={text} label="Copy code" class_name="markdown-code__copy" show_label />
        </span>}
      </figcaption>
      <pre>{children}</pre>
    </figure>;
  },
  a({ children }) {
    return <span className="markdown-content__link">{children}</span>;
  },
  img({ alt, node }) {
    const { images: context } = useContext(MarkdownContext);
    const key = markdownImageRangeKey(node?.position?.start.offset, node?.position?.end.offset);
    const image = key ? context?.bindings.get(key) : null;
    if (context && image) return <InlineMessageImage session_key={context.session_key} event_key={context.event_key}
      revision={context.revision} image={image} alt_text={alt} />;
    const label = alt ? `Image: ${alt}` : "Image";
    return (
      <span aria-label={label} className="markdown-content__image" role="img">
        {alt ? `[Image: ${alt}]` : "[Image]"}
      </span>
    );
  },
  input({ checked }) {
    return (
      <input
        aria-label={checked ? "Completed task" : "Incomplete task"}
        checked={checked ?? false}
        disabled
        readOnly
        type="checkbox"
      />
    );
  },
  table({ children }) {
    return (
      <div className="markdown-content__table-wrap">
        <table>{children}</table>
      </div>
    );
  },
} satisfies Components;

export function MarkdownContent({ content, class_name, image_context, literal_html = false, partial = false }: MarkdownContentProps) {
  const rootClassName = ["markdown-content", class_name].filter(Boolean).join(" ");
  // remark counts positions after a leading BOM. The preview's ellipsis is not
  // evidence that the preceding block finished in the original message.
  const prefix = partial ? content.replace(/^\uFEFF/, "").replace(/…$/, "").trimEnd() : null;
  const boundary = prefix === null ? null : { end: prefix.length, ends_with_fence: /(?:`{3,}|~{3,})$/.test(prefix) };

  return (
    <div className={rootClassName}>
      <MarkdownContext.Provider value={{ images: image_context, boundary }}>
        <ReactMarkdown
          allowedElements={ALLOWED_ELEMENTS}
          components={MARKDOWN_COMPONENTS}
          remarkPlugins={[remarkGfm, remarkBreaks]}
          skipHtml={!literal_html}
          urlTransform={safeUrlTransform}
        >
          {content}
        </ReactMarkdown>
      </MarkdownContext.Provider>
    </div>
  );
}

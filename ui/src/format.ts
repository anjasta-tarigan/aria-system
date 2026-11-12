// Message formatting for the transcript.
//
// Same engine streamdown is built on (unified/remark/rehype) but rendered to an
// HTML string instead of React, because this frontend has no React runtime.
//
// Safety: we do NOT use rehype-raw, so model text can never become raw HTML —
// only trusted transformer output (KaTeX/MathML) enters the string. We then
// strip any remaining `javascript:` href that a transformer might have kept.
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import remarkRehype from "remark-rehype";
import rehypeKatex from "rehype-katex";
import rehypeStringify from "rehype-stringify";

const pipeline = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkMath)
  .use(remarkRehype)
  .use(rehypeKatex)
  .use(rehypeStringify);

const DANGEROUS = /^javascript:/i;

// Render message markdown (GFM + LaTeX) to a sanitized HTML string. Re-render
// is cheap enough to run on every streamed chunk, so partial input renders
// incrementally as the model streams.
export function renderMarkdown(src: string): string {
  const html = String(pipeline.processSync(src));
  const doc = new DOMParser().parseFromString(`<body>${html}</body>`, "text/html");
  doc.querySelectorAll("a[href]").forEach((a) => {
    const href = a.getAttribute("href") ?? "";
    if (DANGEROUS.test(href)) a.removeAttribute("href");
  });
  return doc.body.innerHTML;
}

// Artifacts engine: parsing, state tracking, and rendering for enterprise-grade Claude-style artifacts.
import hljs from "highlight.js";
import { h } from "./dom";
import { renderMarkdown } from "./format";
import type { MessageView } from "./api";

export type ArtifactType =
  | "application/vnd.ant.code"
  | "text/html"
  | "image/svg+xml"
  | "text/markdown"
  | "application/vnd.ant.mermaid"
  | string;

export interface ArtifactVersion {
  identifier: string;
  type: ArtifactType;
  title: string;
  language?: string;
  content: string;
  version: number;
  messageId?: string;
  isStreaming?: boolean;
}

export type MessageSegment =
  | { type: "text"; text: string }
  | { type: "artifact"; artifact: ArtifactVersion };

export interface ConversationArtifactIndex {
  byIdentifier: Map<string, ArtifactVersion[]>;
  all: ArtifactVersion[];
}

const TAG_START = /<(antArtifact|ariaArtifact)\b([^>]*)>/i;
const TAG_END = /<\/(antArtifact|ariaArtifact)>/i;
const PARTIAL_START = /<(antArtifact|ariaArtifact)[^>]*$/i;

function parseAttributes(attrStr: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  const re = /([a-zA-Z0-9_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(attrStr)) !== null) {
    const key = m[1].toLowerCase();
    const val = m[2] ?? m[3] ?? m[4] ?? "";
    attrs[key] = val;
  }
  return attrs;
}

export interface RawParsedSegment {
  type: "text" | "artifact";
  text?: string;
  artifact?: {
    identifier: string;
    type: ArtifactType;
    title: string;
    language?: string;
    content: string;
    isStreaming: boolean;
  };
}

export function parseRawSegments(text: string): RawParsedSegment[] {
  const segments: RawParsedSegment[] = [];
  let remaining = text;

  while (remaining.length > 0) {
    const startMatch = TAG_START.exec(remaining);
    if (!startMatch) {
      // If there is an incomplete opening tag at the very end of streaming text, suppress it
      const partial = PARTIAL_START.exec(remaining);
      if (partial) {
        const textBefore = remaining.slice(0, partial.index);
        if (textBefore) segments.push({ type: "text", text: textBefore });
      } else {
        segments.push({ type: "text", text: remaining });
      }
      break;
    }

    if (startMatch.index > 0) {
      segments.push({ type: "text", text: remaining.slice(0, startMatch.index) });
    }

    const afterStart = remaining.slice(startMatch.index + startMatch[0].length);
    const endMatch = TAG_END.exec(afterStart);

    const attrs = parseAttributes(startMatch[2]);
    const identifier = attrs.identifier || "artifact";
    const type = attrs.type || "application/vnd.ant.code";
    const title = attrs.title || identifier;
    const language = attrs.language;

    if (endMatch) {
      const rawContent = afterStart.slice(0, endMatch.index);
      const content = rawContent.replace(/^\r?\n/, "").replace(/\r?\n$/, "");
      segments.push({
        type: "artifact",
        artifact: { identifier, type, title, language, content, isStreaming: false },
      });
      remaining = afterStart.slice(endMatch.index + endMatch[0].length);
    } else {
      // Unclosed tag: live streaming in progress!
      const content = afterStart.replace(/^\r?\n/, "");
      segments.push({
        type: "artifact",
        artifact: { identifier, type, title, language, content, isStreaming: true },
      });
      break;
    }
  }

  return segments;
}

export function buildArtifactIndex(
  messages: MessageView[],
  streamingText?: string | null,
): ConversationArtifactIndex {
  const byIdentifier = new Map<string, ArtifactVersion[]>();
  const all: ArtifactVersion[] = [];

  for (const m of messages) {
    if (m.role !== "assistant" || !m.content) continue;
    const segments = parseRawSegments(m.content);
    for (const seg of segments) {
      if (seg.type === "artifact" && seg.artifact) {
        const versions = byIdentifier.get(seg.artifact.identifier) ?? [];
        const ver: ArtifactVersion = {
          ...seg.artifact,
          version: versions.length + 1,
          messageId: m.id,
        };
        versions.push(ver);
        byIdentifier.set(seg.artifact.identifier, versions);
        all.push(ver);
      }
    }
  }

  if (streamingText) {
    const segments = parseRawSegments(streamingText);
    for (const seg of segments) {
      if (seg.type === "artifact" && seg.artifact) {
        const versions = byIdentifier.get(seg.artifact.identifier) ?? [];
        const ver: ArtifactVersion = {
          ...seg.artifact,
          version: versions.length + 1,
          messageId: "streaming",
        };
        versions.push(ver);
        byIdentifier.set(seg.artifact.identifier, versions);
        all.push(ver);
      }
    }
  }

  return { byIdentifier, all };
}

export function segmentMessage(
  content: string,
  messageId: string,
  index: ConversationArtifactIndex,
): MessageSegment[] {
  const raw = parseRawSegments(content);
  // Match each artifact segment to its registered version in the index
  const result: MessageSegment[] = [];
  const localCounts = new Map<string, number>();

  for (const seg of raw) {
    if (seg.type === "text" && seg.text !== undefined) {
      result.push({ type: "text", text: seg.text });
    } else if (seg.type === "artifact" && seg.artifact) {
      const id = seg.artifact.identifier;
      const count = (localCounts.get(id) ?? 0) + 1;
      localCounts.set(id, count);

      const allVersions = index.byIdentifier.get(id) ?? [];
      const matched =
        allVersions.find((v) => v.messageId === messageId) ??
        allVersions[allVersions.length - 1] ?? {
          ...seg.artifact,
          version: count,
          messageId,
        };

      result.push({ type: "artifact", artifact: matched });
    }
  }

  return result;
}

// ------------------------------------------------------------- Icons & Labels

export function artifactIcon(type: string): HTMLElement {
  let svg = "";
  switch (type) {
    case "text/html":
      svg = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="15" x="2" y="3" rx="2"/><line x1="2" x2="22" y1="8" y2="8"/><line x1="6" x2="6.01" y1="5.5" y2="5.5"/><line x1="9" x2="9.01" y1="5.5" y2="5.5"/></svg>`;
      break;
    case "image/svg+xml":
      svg = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="4"/><rect width="8" height="8" x="12" y="12" rx="1"/></svg>`;
      break;
    case "text/markdown":
      svg = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M9 13v4"/><path d="m7 15 2-2 2 2"/><path d="M14 13h3"/><path d="M14 17h3"/></svg>`;
      break;
    case "application/vnd.ant.mermaid":
      svg = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="6" height="5" x="2" y="3" rx="1"/><rect width="6" height="5" x="16" y="3" rx="1"/><rect width="6" height="5" x="9" y="16" rx="1"/><path d="M5 8v3a2 2 0 0 0 2 2h5m0 0h5a2 2 0 0 0 2-2V8m-7 5v3"/></svg>`;
      break;
    case "application/vnd.ant.code":
    default:
      svg = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>`;
      break;
  }
  return h("span", { class: "artifact-icon-wrap", html: svg });
}

export function typeLabel(type: string, language?: string): string {
  if (type === "text/html") return "HTML";
  if (type === "image/svg+xml") return "SVG";
  if (type === "text/markdown") return "Document";
  if (type === "application/vnd.ant.mermaid") return "Mermaid";
  if (language) return language.toUpperCase();
  return "Code";
}

export function isArtifactPreviewable(type: string): boolean {
  return (
    type === "text/html" ||
    type === "image/svg+xml" ||
    type === "text/markdown" ||
    type === "application/vnd.ant.mermaid"
  );
}

const escapeHtml = (s: string) =>
  s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");

// ----------------------------------------------------------- Inline Card

export function renderArtifactCard(
  art: ArtifactVersion,
  isSelected: boolean,
  onClick: () => void,
): HTMLElement {
  const isStreaming = Boolean(art.isStreaming);

  return h(
    "div",
    {
      class: `artifact-card ${isSelected ? "selected" : ""} ${isStreaming ? "streaming" : ""}`,
      role: "button",
      tabindex: "0",
      onclick: onClick,
      onkeydown: (e: KeyboardEvent) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      },
    },
    h("div", { class: "artifact-card-icon" }, artifactIcon(art.type)),
    h(
      "div",
      { class: "artifact-card-meta" },
      h("div", { class: "artifact-card-title" }, art.title || art.identifier),
      h(
        "div",
        { class: "artifact-card-sub" },
        h("span", { class: "artifact-badge" }, typeLabel(art.type, art.language)),
        art.version > 1 ? h("span", { class: "artifact-ver-pill" }, `v${art.version}`) : null,
        isStreaming
          ? h(
              "span",
              { class: "artifact-streaming-label" },
              h("span", { class: "dot pulse" }),
              "Generating…",
            )
          : h("span", { class: "artifact-action-hint" }, "Click to view"),
      ),
    ),
    h(
      "div",
      { class: "artifact-card-action" },
      isSelected
        ? h("span", { class: "artifact-card-pill active" }, "Viewing")
        : h("span", { class: "artifact-card-pill" }, "Open ↗"),
    ),
  );
}

// ------------------------------------------------------- Previews & Security

export function prepareHtmlForPreview(html: string): string {
  if (!/<html[\s>]/i.test(html)) {
    return `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <style>
    body { font-family: system-ui, -apple-system, sans-serif; margin: 0; padding: 16px; background: #0d1117; color: #e6edf3; }
    @media (prefers-color-scheme: dark) {
      body { background: #0d1117; color: #e6edf3; }
    }
  </style>
</head>
<body>
${html}
</body>
</html>`;
  }
  return html;
}

export function prepareSvgForPreview(svg: string): string {
  return `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { margin: 0; display: grid; place-items: center; min-height: 100vh; background: #0d1117; }
    svg { max-width: 90vw; max-height: 90vh; }
  </style>
</head>
<body>
${svg}
</body>
</html>`;
}

export function prepareMermaidForPreview(code: string): string {
  const escaped = escapeHtml(code);
  return `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
  <style>
    body {
      margin: 0;
      padding: 32px;
      display: grid;
      place-items: center;
      min-height: 100vh;
      background: #0d1117;
      color: #e6edf3;
      font-family: system-ui, -apple-system, sans-serif;
      box-sizing: border-box;
    }
    .mermaid { display: flex; justify-content: center; width: 100%; }
    pre.fallback { color: #8b94a3; font-family: monospace; white-space: pre-wrap; }
  </style>
</head>
<body>
  <div class="mermaid">${escaped}</div>
  <script>
    if (window.mermaid) {
      try {
        mermaid.initialize({ startOnLoad: true, theme: 'dark' });
      } catch (err) {
        document.body.innerHTML = '<pre class="fallback">' + ${JSON.stringify(escaped)} + '</pre>';
      }
    } else {
      document.body.innerHTML = '<pre class="fallback">' + ${JSON.stringify(escaped)} + '</pre>';
    }
  </script>
</body>
</html>`;
}

export function renderArtifactPreview(art: ArtifactVersion): HTMLElement {
  if (art.type === "text/html") {
    const iframe = document.createElement("iframe");
    iframe.className = "artifact-preview-iframe";
    iframe.setAttribute("sandbox", "allow-scripts");
    iframe.srcdoc = prepareHtmlForPreview(art.content);
    return iframe;
  }
  if (art.type === "image/svg+xml") {
    const iframe = document.createElement("iframe");
    iframe.className = "artifact-preview-iframe svg-frame";
    iframe.setAttribute("sandbox", "allow-scripts");
    iframe.srcdoc = prepareSvgForPreview(art.content);
    return iframe;
  }
  if (art.type === "application/vnd.ant.mermaid") {
    const iframe = document.createElement("iframe");
    iframe.className = "artifact-preview-iframe mermaid-frame";
    iframe.setAttribute("sandbox", "allow-scripts");
    iframe.srcdoc = prepareMermaidForPreview(art.content);
    return iframe;
  }
  if (art.type === "text/markdown") {
    return h("div", {
      class: "artifact-markdown-container markdown",
      html: renderMarkdown(art.content),
    });
  }
  return renderArtifactCode(art);
}

export function renderArtifactCode(art: ArtifactVersion): HTMLElement {
  let highlighted: string;
  const lang = art.language?.toLowerCase();
  try {
    if (lang && hljs.getLanguage(lang)) {
      highlighted = hljs.highlight(art.content, { language: lang }).value;
    } else {
      highlighted = hljs.highlightAuto(art.content).value;
    }
  } catch {
    highlighted = escapeHtml(art.content);
  }

  const pre = h(
    "pre",
    { class: "artifact-code-pre" },
    h("code", { class: `hljs ${lang ? `language-${lang}` : ""}`, html: highlighted }),
  );
  return h("div", { class: "artifact-code-wrapper" }, pre);
}

// --------------------------------------------------------- Actions

export function getArtifactFileExtension(type: string, language?: string): string {
  if (language) {
    const lang = language.toLowerCase();
    const map: Record<string, string> = {
      typescript: "ts",
      javascript: "js",
      python: "py",
      rust: "rs",
      markdown: "md",
      html: "html",
      css: "css",
      json: "json",
      yaml: "yaml",
      yml: "yml",
      shell: "sh",
      bash: "sh",
      sql: "sql",
      svg: "svg",
      mermaid: "mmd",
    };
    if (map[lang]) return map[lang];
    if (/^[a-z0-9]+$/i.test(lang)) return lang;
  }
  switch (type) {
    case "text/html":
      return "html";
    case "image/svg+xml":
      return "svg";
    case "text/markdown":
      return "md";
    case "application/vnd.ant.mermaid":
      return "mmd";
    case "application/vnd.ant.code":
    default:
      return "txt";
  }
}

export function downloadArtifact(artifact: ArtifactVersion): void {
  const ext = getArtifactFileExtension(artifact.type, artifact.language);
  const safeId = artifact.identifier.replace(/[^a-zA-Z0-9_-]/g, "_") || "artifact";
  const filename = `${safeId}.${ext}`;
  const blob = new Blob([artifact.content], { type: artifact.type || "text/plain" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export async function copyArtifact(content: string, btn?: HTMLElement): Promise<void> {
  try {
    await navigator.clipboard.writeText(content);
    if (btn) {
      const prev = btn.textContent;
      btn.textContent = "✓ Copied";
      btn.classList.add("copied");
      setTimeout(() => {
        btn.textContent = prev;
        btn.classList.remove("copied");
      }, 1500);
    }
  } catch (err) {
    console.error("Failed to copy artifact", err);
  }
}

export function popoutArtifact(artifact: ArtifactVersion): void {
  let html = artifact.content;
  if (artifact.type === "image/svg+xml") {
    html = prepareSvgForPreview(artifact.content);
  } else if (artifact.type === "text/html") {
    html = prepareHtmlForPreview(artifact.content);
  } else if (artifact.type === "application/vnd.ant.mermaid") {
    html = prepareMermaidForPreview(artifact.content);
  } else if (artifact.type === "text/markdown") {
    html = `<!DOCTYPE html><html><head><meta charset="utf-8"><style>body{font-family:sans-serif;padding:32px;background:#0d1117;color:#e6edf3;max-width:800px;margin:auto;}</style></head><body>${renderMarkdown(artifact.content)}</body></html>`;
  }
  const blob = new Blob([html], { type: "text/html;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  window.open(url, "_blank");
}

// ------------------------------------------------------- Side Panel

export function renderArtifactPanel(
  art: ArtifactVersion,
  allVersions: ArtifactVersion[],
  activeTab: "preview" | "code",
  onTabChange: (t: "preview" | "code") => void,
  onVersionChange: (ver: number) => void,
  onClose: () => void,
): HTMLElement {
  const isPreviewable = isArtifactPreviewable(art.type);
  const totalVersions = allVersions.length;
  const currentVersion = art.version;

  const header = h(
    "div",
    { class: "artifact-panel-header" },
    h(
      "div",
      { class: "artifact-header-info" },
      h("div", { class: "artifact-header-icon" }, artifactIcon(art.type)),
      h(
        "div",
        { class: "artifact-header-title-box" },
        h("span", { class: "artifact-header-title", title: art.title || art.identifier }, art.title || art.identifier),
        h("span", { class: "artifact-badge sm" }, typeLabel(art.type, art.language)),
      ),
      totalVersions > 1
        ? h(
            "div",
            { class: "artifact-version-switcher" },
            h(
              "button",
              {
                class: "ghost btn-sm btn-ver-nav",
                disabled: currentVersion <= 1,
                title: "Previous version",
                onclick: () => onVersionChange(currentVersion - 1),
              },
              "◀",
            ),
            h("span", { class: "version-counter" }, `v${currentVersion} of ${totalVersions}`),
            h(
              "button",
              {
                class: "ghost btn-sm btn-ver-nav",
                disabled: currentVersion >= totalVersions,
                title: "Next version",
                onclick: () => onVersionChange(currentVersion + 1),
              },
              "▶",
            ),
          )
        : null,
    ),
    isPreviewable
      ? h(
          "div",
          { class: "artifact-tabs" },
          h(
            "button",
            {
              class: `tab-btn ${activeTab === "preview" ? "active" : ""}`,
              onclick: () => onTabChange("preview"),
            },
            "Preview",
          ),
          h(
            "button",
            {
              class: `tab-btn ${activeTab === "code" ? "active" : ""}`,
              onclick: () => onTabChange("code"),
            },
            "Code",
          ),
        )
      : null,
    h(
      "div",
      { class: "artifact-actions" },
      h(
        "button",
        {
          class: "ghost btn-sm btn-copy",
          title: "Copy content to clipboard",
          onclick: (e: MouseEvent) => void copyArtifact(art.content, e.currentTarget as HTMLElement),
        },
        "Copy",
      ),
      h(
        "button",
        {
          class: "ghost btn-sm btn-download",
          title: "Download file",
          onclick: () => downloadArtifact(art),
        },
        "Download",
      ),
      isPreviewable
        ? h(
            "button",
            {
              class: "ghost btn-sm btn-popout",
              title: "Open preview in new window",
              onclick: () => popoutArtifact(art),
            },
            "↗ Popout",
          )
        : null,
      h(
        "button",
        {
          class: "ghost btn-sm btn-close",
          title: "Close artifact panel",
          onclick: onClose,
        },
        "✕",
      ),
    ),
  );

  const bodyContent =
    isPreviewable && activeTab === "preview"
      ? renderArtifactPreview(art)
      : renderArtifactCode(art);

  return h(
    "aside",
    { class: "artifact-panel" },
    header,
    h("div", { class: "artifact-body" }, bodyContent),
  );
}

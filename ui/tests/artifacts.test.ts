import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  parseRawSegments,
  buildArtifactIndex,
  getArtifactFileExtension,
  isArtifactPreviewable,
  prepareHtmlForPreview,
  prepareSvgForPreview,
  prepareMermaidForPreview,
} from "../src/artifacts";
import type { MessageView } from "../src/api";

describe("parseRawSegments", () => {
  it("returns plain text unchanged when no artifact tag is present", () => {
    const text = "Hello world! This is a simple message without artifacts.";
    const segments = parseRawSegments(text);
    assert.equal(segments.length, 1);
    assert.equal(segments[0].type, "text");
    assert.equal(segments[0].text, text);
  });

  it("parses closed antArtifact tags with all attributes", () => {
    const text = `Check this out:
<antArtifact identifier="counter-app" type="text/html" title="Counter App" language="html">
<!DOCTYPE html>
<html><body><h1>Counter</h1></body></html>
</antArtifact>
Hope you like it!`;

    const segments = parseRawSegments(text);
    assert.equal(segments.length, 3);
    assert.equal(segments[0].type, "text");
    assert.equal(segments[0].text, "Check this out:\n");

    assert.equal(segments[1].type, "artifact");
    assert.equal(segments[1].artifact?.identifier, "counter-app");
    assert.equal(segments[1].artifact?.type, "text/html");
    assert.equal(segments[1].artifact?.title, "Counter App");
    assert.equal(segments[1].artifact?.language, "html");
    assert.equal(
      segments[1].artifact?.content,
      "<!DOCTYPE html>\n<html><body><h1>Counter</h1></body></html>",
    );
    assert.equal(segments[1].artifact?.isStreaming, false);

    assert.equal(segments[2].type, "text");
    assert.equal(segments[2].text, "\nHope you like it!");
  });

  it("parses ariaArtifact variant tags seamlessly", () => {
    const text = `<ariaArtifact identifier="my-script" type="application/vnd.ant.code" title="Script" language="python">
print("hello from aria")
</ariaArtifact>`;

    const segments = parseRawSegments(text);
    assert.equal(segments.length, 1);
    assert.equal(segments[0].type, "artifact");
    assert.equal(segments[0].artifact?.identifier, "my-script");
    assert.equal(segments[0].artifact?.language, "python");
    assert.equal(segments[0].artifact?.content, 'print("hello from aria")');
    assert.equal(segments[0].artifact?.isStreaming, false);
  });

  it("handles streaming unclosed artifact tags gracefully", () => {
    const text = `Generating your application:
<antArtifact identifier="live-app" type="text/html" title="Live Streaming App">
<!DOCTYPE html>
<html><body>Streaming in progress...`;

    const segments = parseRawSegments(text);
    assert.equal(segments.length, 2);
    assert.equal(segments[0].type, "text");
    assert.equal(segments[0].text, "Generating your application:\n");

    assert.equal(segments[1].type, "artifact");
    assert.equal(segments[1].artifact?.identifier, "live-app");
    assert.equal(segments[1].artifact?.title, "Live Streaming App");
    assert.equal(
      segments[1].artifact?.content,
      "<!DOCTYPE html>\n<html><body>Streaming in progress...",
    );
    assert.equal(segments[1].artifact?.isStreaming, true);
  });

  it("suppresses a partial opening tag at the end of streaming text", () => {
    const text = "Here is the code:\n<antArtifact identifier=\"pending\"";
    const segments = parseRawSegments(text);
    assert.equal(segments.length, 1);
    assert.equal(segments[0].type, "text");
    assert.equal(segments[0].text, "Here is the code:\n");
  });
});

describe("buildArtifactIndex & version tracking", () => {
  it("tracks multiple versions of the same artifact across messages", () => {
    const messages: MessageView[] = [
      {
        id: "m1",
        role: "assistant",
        content: `<antArtifact identifier="dashboard" type="text/html" title="Dashboard v1">
<div>v1</div>
</antArtifact>`,
        created_at: 1000,
        tool_calls: [],
      },
      {
        id: "m2",
        role: "user",
        content: "Please update the dashboard",
        created_at: 2000,
        tool_calls: [],
      },
      {
        id: "m3",
        role: "assistant",
        content: `<antArtifact identifier="dashboard" type="text/html" title="Dashboard v2">
<div>v2</div>
</antArtifact>`,
        created_at: 3000,
        tool_calls: [],
      },
    ];

    const index = buildArtifactIndex(messages);
    const versions = index.byIdentifier.get("dashboard");
    assert.ok(versions);
    assert.equal(versions.length, 2);
    assert.equal(versions[0].version, 1);
    assert.equal(versions[0].title, "Dashboard v1");
    assert.equal(versions[0].content, "<div>v1</div>");
    assert.equal(versions[0].messageId, "m1");

    assert.equal(versions[1].version, 2);
    assert.equal(versions[1].title, "Dashboard v2");
    assert.equal(versions[1].content, "<div>v2</div>");
    assert.equal(versions[1].messageId, "m3");
  });
});

describe("getArtifactFileExtension", () => {
  it("resolves extensions based on language first", () => {
    assert.equal(getArtifactFileExtension("application/vnd.ant.code", "typescript"), "ts");
    assert.equal(getArtifactFileExtension("application/vnd.ant.code", "python"), "py");
    assert.equal(getArtifactFileExtension("application/vnd.ant.code", "rust"), "rs");
    assert.equal(getArtifactFileExtension("application/vnd.ant.code", "bash"), "sh");
  });

  it("resolves extensions based on artifact MIME type", () => {
    assert.equal(getArtifactFileExtension("text/html"), "html");
    assert.equal(getArtifactFileExtension("image/svg+xml"), "svg");
    assert.equal(getArtifactFileExtension("text/markdown"), "md");
    assert.equal(getArtifactFileExtension("application/vnd.ant.mermaid"), "mmd");
  });
});

describe("isArtifactPreviewable", () => {
  it("identifies previewable artifact types", () => {
    assert.equal(isArtifactPreviewable("text/html"), true);
    assert.equal(isArtifactPreviewable("image/svg+xml"), true);
    assert.equal(isArtifactPreviewable("text/markdown"), true);
    assert.equal(isArtifactPreviewable("application/vnd.ant.mermaid"), true);
    assert.equal(isArtifactPreviewable("application/vnd.ant.code"), false);
  });
});

describe("preview preparers", () => {
  it("wraps incomplete HTML in standard skeleton", () => {
    const raw = "<h1>Hello</h1>";
    const prepared = prepareHtmlForPreview(raw);
    assert.ok(prepared.includes("<!DOCTYPE html>"));
    assert.ok(prepared.includes("<h1>Hello</h1>"));
  });

  it("leaves complete HTML intact", () => {
    const raw = "<!DOCTYPE html><html><body><h1>Hello</h1></body></html>";
    const prepared = prepareHtmlForPreview(raw);
    assert.equal(prepared, raw);
  });

  it("prepares SVG preview wrapper", () => {
    const svg = '<svg viewBox="0 0 10 10"><circle r="5"/></svg>';
    const prepared = prepareSvgForPreview(svg);
    assert.ok(prepared.includes("<!DOCTYPE html>"));
    assert.ok(prepared.includes(svg));
  });

  it("prepares Mermaid preview wrapper with script loader", () => {
    const mermaid = "graph TD; A-->B;";
    const prepared = prepareMermaidForPreview(mermaid);
    assert.ok(prepared.includes("mermaid.min.js"));
    assert.ok(prepared.includes("graph TD; A--&gt;B;"));
  });
});

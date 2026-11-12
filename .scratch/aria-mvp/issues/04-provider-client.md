# 04 — Provider client: streaming chat completions with tool calls

Status: done

## What

OpenAI-compatible client over `/v1/chat/completions` with `stream: true`.
Decode SSE deltas into events: text delta, tool-call fragment (index, id, name,
argument chunks), finish reason, usage. Accumulate fragments into complete Tool
Calls per Turn. Support cancellation via a `CancellationToken` that aborts the
HTTP request.

## Acceptance

- Unit tests against recorded SSE fixtures: plain text, multiple tool calls,
  interleaved text and tool calls, usage present and absent.
- A truncated stream ends the Turn with an error, not a partial tool call.
- Cancel mid-stream stops reading within a bound and returns `cancelled`.

## Notes

Use `reqwest` with rustls. Base URL, model and key come from config (issue 06).

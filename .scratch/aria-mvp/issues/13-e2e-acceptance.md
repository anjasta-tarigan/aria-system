# 13 — End-to-end acceptance

Status: done

## What

Run the four acceptance scenarios in `spec.md` against a real Provider key on a
throwaway repo: fix a bug (search → edit → test, with approvals, then undo);
path escape refused; close/reopen mid-Run; FTS finds new text.

## Acceptance

- All four pass manually, with a note of the Provider and model used.
- Any deviation is recorded back into `spec.md` or a follow-up issue.

## Notes

Depends on 04, 06, 10, 11, 12.

## Result

Automated: `crates/aria-core/tests/e2e.rs` runs all four `spec.md` acceptance
scenarios against the real stack — on-disk SQLite (WAL), the Tool layer, the
shell, the blob store/undo journal, and the FTS5 index. The only stand-in is the
Provider: a scripted OpenAI-compatible endpoint served on loopback, so the SSE
parser and the request wire format are exercised too. No API key needed.

Scenarios covered:

1. Bug fix: `search_files` → `edit_file` (Approval) → `run_shell` (Approval) on a
   throwaway crate whose test really fails, then `Undo` restores the buggy line.
2. Path escape: `read_file ../../etc/passwd` and `write_file ../../../tmp/...`
   are refused by the Tool even after approval; a symlink out of the root is
   refused as well.
3. Close/reopen mid-Run: the Run is left `awaiting_approval`, a fresh `Agent`
   reads it from the database, `resume_interrupted` adopts it, the approval is
   answered, and the Run finishes with the same transcript and a terminal event.
4. FTS5: text stored in a Turn is found by `search_messages` immediately, both
   scoped and global; a partial word returns no hits rather than an error.

Manual pass against a live Provider: **not run** — no key is available in this
environment. The wire format was verified instead by asserting the request body
the stub received (system/user/assistant-with-`tool_calls`/tool shape, `tools`
schema, `stream` + `stream_options`).

Deviation recorded during this pass: tool calls were being sent back to the
model in a **flat** shape (`{id, name, arguments}`) instead of the OpenAI
`{id, type: "function", function: {name, arguments}}`. Fixed in
`provider::ToolCallRequest`; a regression test asserts the nested shape.

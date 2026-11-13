# ADR-0004: Background tasks detach from the Run and survive its end

Status: accepted (2026-11-13)

## Context

Some shell work runs longer than a single Run and longer than the normal
120s shell timeout: builds, training runs, servers, downloads. The agent (and
the user) need to start such work, do other things, then check on it — and
chain a follow-up Tool from its output.

## Decision

A `run_background` Tool starts a shell command detached via `tokio::spawn`.
It streams combined stdout+stderr to a temp log under the blob dir and, on
exit, records a `background_task` row (schema v6) with the final status,
exit code and a blob reference to the completed log. `poll_task` returns the
row plus a tail of the log; `kill` reaps the whole process group. The same
capability is exposed as Tauri commands (`start/poll/list/kill_background`)
and a UI "Background tasks" panel that auto-polls while any task runs.

Scope is confined to the Workspace root (refused for `None`/`MainSystem`),
kill reuses `shell::kill_group` (negative-pid, so grandchildren die), and a
runtime cap (8h, `shortcut:`-noted) reaps runaway tasks. On startup, any
task still `running` is reaped as `killed` — its process died with the
previous instance — and its partial log is preserved as a blob.

## Trade-offs

- Detached execution needs no new process-manager process; the `Arc<Db>` /
  `Arc<Blobs>` outlive the Run, so completion is recorded after the Run loop
  ends. The cost is that the whole log is read into memory on completion
  (not bounded separately from the 8h runtime cap).
- Reaping every `running` row on startup assumes a restart kills the
  process. That holds for a single-user desktop app (the process group takes
  children with it); a task that somehow survived would be mislabeled
  `killed`, which is the safer failure than a permanent zombie in the UI.
- Chaining is emergent, not a new construct: the model calls `run_background`,
  later calls `poll_task`, and feeds the result into another Tool. The system
  prompt documents the pattern rather than adding orchestration code.

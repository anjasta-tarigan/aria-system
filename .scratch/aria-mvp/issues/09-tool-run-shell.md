# 09 — Tool: run_shell

Status: done

## What

`sh -c` with cwd = workspace root, a configurable timeout (default 120s), stdout
and stderr streamed as Tool Call progress, full exit code captured, and output
truncated head+tail for the model. Kills the whole process group on timeout or
run cancel.

## Acceptance

- Output order preserved across stdout/stderr for the returned text (interleaved
  capture, or clearly labelled sections).
- Timeout kills a sleeping process group; the Tool Result says so.
- Nonzero exit is a normal Tool Result carrying the code, not an error marker.
- Large output is truncated with an explicit marker and byte counts.
- Tests run real commands inside `tempfile` dirs; CI has no network.

## Notes

No PTY, no interactive commands. Class exec → Approval showing the full command.

# 03 — Workspace path containment

Status: done

## What

`workspace` module: given a root, resolve a caller-supplied relative or absolute
path (following symlinks, including symlinked parents and dangling final
components) and reject anything outside the root. Provide `resolve_read` and
`resolve_create` (the latter allows non-existent final components inside the
root).

## Acceptance

- `../x`, absolute `/etc/passwd`, and a symlink inside the workspace pointing
  outside are all rejected with a distinct error.
- A new file in a nested subdirectory is allowed; its parent must exist.
- Tests use `tempfile`, including a symlink case.

## Notes

This is the main safety mechanism; it must be enforced in the Tool layer, never
in the prompt.

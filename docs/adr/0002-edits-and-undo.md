# ADR-0002: Edits are exact search/replace; Undo is blob backups

Status: accepted (2026-11-11)

## Context

The agent must modify existing files. LLM-generated unified diffs are frequently
malformed or have wrong line offsets, and a wrong offset silently corrupts the
wrong region. Undo must be exact, including files the agent created or deleted.

## Decision

An **Edit** is `(path, search, replace)` and applies only when `search` occurs
exactly once in the file; otherwise the Tool Result is an error the model can
retry. Whole-file writes are for new files only.

Before each change to an existing file, its content is written to a
content-addressed blob store and a journal row `(workspace, path, blob, time)`
is recorded. **Undo** restores the previous version from the journal, multi-step
per file. Blobs unreferenced by any journal row are pruned at startup.

## Trade-offs

- Multi-step undo means blobs grow with edit count; startup pruning bounds it.
- Search/replace requires the model to quote context; the exactly-once rule turns
  ambiguity into a visible error instead of a silent bad edit.

## Alternatives rejected

- Unified diff: needs a diff crate and tolerates no offset drift.
- Whole-file overwrite: costly on large files.
- Git commit per Run: fights the user's own git usage and is slow on large folders.

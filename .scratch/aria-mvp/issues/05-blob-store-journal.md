# 05 — Blob store and undo journal

Status: done

## What

Content-addressed blob store (sha256 files under a blobs dir) plus `journal`
writing `edit_journal` rows before any mutate Tool changes an existing file.
Undo restores the previous version for one path, multi-step per file, one step
per call. Prune blobs referenced by no journal row at startup.

## Acceptance

- After three edits to one file, three Undo calls restore the original content
  and a fourth reports "nothing to undo".
- Deleting a file and undoing restores it byte-for-byte with its path.
- Pruning removes only unreferenced blobs (test with orphans planted on disk).
- All operations are written against SQLite in a test DB, no Tauri.

## Notes

ADR-0002.

## Comments

- The "dedupe" and "prune" acceptance checks read the blob directory right after
  `write_file`; that check was flaky and has been rewritten to assert on journal
  rows (`blob_usage`) and on the result of `undo` instead of on directory
  listing, which is what actually protects the user. `write_file` journaling
  itself is covered by `every_write_journals_the_prior_version`.

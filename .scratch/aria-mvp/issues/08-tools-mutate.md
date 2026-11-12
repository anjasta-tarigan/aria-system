# 08 — Tools: edit_file, write_file, delete_file, move_file

Status: done

## What

The four mutate Tools, each journaling the pre-change content of every file it
touches before changing it.

- `edit_file`: `(path, search, replace)`; the search string must occur exactly
  once; zero or multiple matches are errors naming the count.
- `write_file`: new files only; an existing path is an error that points to
  `edit_file`.
- `delete_file` / `move_file`: both source and destination contained; journal
  the source.

## Acceptance

- Exactly-once rule tested: zero matches, one match, two matches.
- Editing a symlink target outside the workspace is refused.
- Every successful mutate produces exactly one journal row per file changed, and
  Undo restores it (integration with issue 05).
- Encoding: non-UTF-8 files are refused with a clear error, not mangled.

## Notes

ADR-0002.

# 02 — SQLite schema, migrations and FTS5 triggers

Status: done

## What

Open the DB in WAL mode, run versioned migrations creating the tables in
`spec.md` (Data model). Create `messages_fts` as an FTS5 external-content table
over `messages.content` with insert/update/delete triggers. Storage location:
the platform app-data dir.

## Acceptance

- Inserting a message makes its text findable via `messages_fts MATCH` in the
  same transaction; updating and deleting keep the index consistent.
- Opening the DB twice (app restart) is a no-op migration.
- A test covers insert → search → update → search → delete → search.

## Notes

ADR-0003. Any future migration touching `messages` must rebuild the FTS table.

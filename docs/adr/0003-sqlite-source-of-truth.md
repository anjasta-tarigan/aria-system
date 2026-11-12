# ADR-0003: SQLite is the transcript of record; FTS5 via triggers

Status: accepted (2026-11-11)

## Context

Conversations, messages, tool calls, approvals, run state and the undo journal
all need durable storage. Search over past messages is a requirement (FTS5 named
explicitly). The database runs in WAL mode.

## Decision

SQLite is the only transcript store; there are no sidecar JSONL files.
Attachments are blobs on disk referenced by hash. Message search uses an FTS5
**external-content** table over message text, kept in sync by triggers on
`messages`, so every insert path is indexed in the same transaction.

## Trade-offs

- Trigger-maintained FTS is correct by construction but couples search to schema
  migrations; a migration touching `messages` must rebuild the FTS table.
- No JSONL means transcripts are only editable through the app or `sqlite3`.

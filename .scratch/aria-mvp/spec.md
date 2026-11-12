# Aria MVP — Spec

Status: design agreed 2026-11-11. Vocabulary: see `GLOSSARY.md`.
Decisions with an ADR: `docs/adr/0001-agent-loop-in-rust.md`,
`0002-edits-and-undo.md`, `0003-sqlite-source-of-truth.md`.

## Goal

A desktop AI assistant on Tauri 2 + Rust + SQLite that takes a natural-language
task and actually performs it on one local folder: reading, editing, creating,
moving and deleting files, running shell commands, fetching web pages, keeping a
todo list — with every destructive or executing action gated on an explicit
Approval.

Single user, Linux desktop. Model backend is one remote OpenAI-compatible
Provider with a user-supplied key.

## Non-goals (v1)

- Packaging for macOS/Windows; multi-device sync; mobile.
- Local models; multiple Providers; per-conversation model choice.
- Embeddings/RAG; FTS over workspace files (the agent can run `rg`).
- Sub-agents, scheduling/cron, background triggers.
- OS-level sandboxing of `run_shell` (approval + path containment only).
- Persistent PTY / interactive commands.
- Undo across conversations or across app restarts beyond the journal.

## Architecture

```
aria/
├── Cargo.toml            workspace
├── crates/aria-core/     no Tauri dependency
│   ├── db/               migrations, repositories, FTS triggers
│   ├── provider/         OpenAI-compatible client (streaming, tool calls)
│   ├── agent/            Run loop, Caps, Turn handling
│   ├── tools/            10 Tools + approval classes
│   ├── workspace/        path containment (symlink-resolved)
│   └── journal/          blob store + undo journal
├── src-tauri/            commands, RunEvent → Tauri events, app init
└── ui/                   Vite + TypeScript, minimal component lib
```

`aria-core` is driven by plain `cargo test`; `src-tauri` only translates.

## Data model (SQLite, WAL)

| table | key columns |
|---|---|
| `workspaces` | id, root_path, created_at |
| `conversations` | id, workspace_id, title, created_at, updated_at |
| `messages` | id, conversation_id, run_id, role, content, created_at |
| `runs` | id, conversation_id, state, started_at, ended_at, turn_count, prompt_tokens, stop_reason |
| `tool_calls` | id, run_id, turn_index, tool, args_json, approval_class, state |
| `approvals` | id, tool_call_id, decision, decided_at |
| `todos` | id, run_id, position, text, done |
| `edit_journal` | id, workspace_id, path, blob_hash, created_at |
| `messages_fts` | FTS5 external content over `messages.content`, trigger-maintained |

Run `state`: `running | awaiting_approval | finished | budget_exhausted |
cancelled | errored`.

## Tools

| tool | class | notes |
|---|---|---|
| `read_file` | read | |
| `list_dir` | read | |
| `search_files` | read | glob + grep, respects ignore files |
| `edit_file` | mutate | `(path, search, replace)`; search must match exactly once |
| `write_file` | mutate | new files only |
| `delete_file` | mutate | journaled first |
| `move_file` | mutate | journaled first |
| `run_shell` | exec | `sh -c`, cwd = workspace, timeout, streamed output, truncation for model |
| `web_fetch` | network | http(s) only; refuses loopback/private ranges |
| `todos` | read | run-scoped list |

Every path argument is resolved through symlinks and must stay inside the
Workspace. Failures (including nonzero exit codes) return a Tool Result marked as
an error; the Run continues.

## Run loop

1. Load Conversation history → build request.
2. Stream a Turn from the Provider; persist assistant message and any Tool Calls.
3. For each Tool Call: if mutate/exec/network, create an Approval and park the
   Run (`awaiting_approval`) until answered; if denied, return a "denied" Tool
   Result.
4. Apply allowed Tool Calls, journaling file changes, appending Tool Results.
5. Repeat while the model calls tools.
6. Stop on: no tool calls → `finished`; Cap (50 Turns / 200k prompt tokens,
   nudge at 70%) → `budget_exhausted`; user cancel → `cancelled`; provider
   transport error → `errored`.

## UI (minimal)

- Conversation list + new conversation (choose Workspace folder).
- Message pane with streaming text, Tool Call cards (args, result, error state).
- Approval prompt: exact diff for Edits, full command for `run_shell`, URL for
  `web_fetch`, with Approve/Deny.
- Todo list panel for the active Run.
- Global search box backed by FTS5, jumps to the matching message.
- Undo button per changed file; run cancel button; budget-exhausted "continue".

## Configuration

Single Provider `(base_url, model)` plus API key. Key in the OS keyring when
available, otherwise a chmod-600 file. Settings screen edits this.

## Acceptance

- Given a workspace with a bug, the agent finds it (`search_files`), edits it
  (`edit_file`), runs the tests (`run_shell`), each with an Approval, and the
  change can be undone.
- A request touching `../../etc` is refused by the Tool, not the model.
- Closing and reopening the UI mid-Run shows the same Run and lets it complete.
- Past message text is findable through FTS5 immediately after it is stored.

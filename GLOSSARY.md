# Glossary

## Workspace

The single directory a conversation is bound to. It is the agent's whole world: the shell runs inside it, and every file tool refuses paths that resolve (after symlinks) outside it.

_Avoid_: project, folder root, cwd.

## Main System

A Conversation with no Workspace that may touch the filesystem system-wide. The no-folder chat starts enabled with empty allowed roots (everywhere); workspace chats are always isolated. Its allowed roots run silently; a path outside them asks for an Approval, even for a read, and the approval unlocks only the paths that call named. Under "Only risky" (auto-approve-safe) only reads run silently: mutations and network ask, and a shell command asks too unless it is one safe read-only command. A Main System restricted to allowed roots asks for every shell command, because shell cannot be confined to those roots.

It has a Base Directory (`<app data>/system`), created automatically: the shell cwd, where relative paths land, the background-task root and the key for its file-edit journal. It is not a Workspace, so no folder is picked and nothing appears in the sidebar; full device access stays.

_Avoid_: global mode, admin mode.

## Conversation

An ordered transcript of user and assistant messages, bound to at most one Workspace and one model configuration. It is the unit the user opens, searches and resumes.

_Avoid_: session, chat, thread.

## Mode

Per-Conversation intent. **Normal** is everyday work. **Creative** adds a prompt section that treats Aria's own source checkout as the project, so the user can build tools or change Aria itself; it is a persona and prompt, not a second provider. Every Conversation also gets a short house block with Aria's version, app-data, settings and database paths (and the source checkout in a dev build) so it can answer questions about the app.

_Avoid_: preset, personality, profile.

## Run

One execution of the agent loop for a Conversation, started by a user message and ending when the model stops calling tools, a cap is reached, or the user cancels. A Run can outlive the window: closing the UI does not stop it.

_Avoid_: task, job, turn sequence.

## Turn

One request/response pair with the model inside a Run. A Turn may contain several Tool Calls and their results. A Run is a sequence of Turns. A Turn that fails for a transient provider reason before any text streamed is retried twice; at most four Turns run at once across every Conversation.

_Avoid_: step, iteration, round.

## Context window

The model's input limit for one request. Aria sends the system prompt, the injected memories and the conversation so far; when that passes 300k characters — about 75k tokens of English, at most ~100k of dense code — it drops the oldest complete Turns, keeping the system prompt and the newest Turn. Trimming cuts at user-message boundaries, because a Tool Result whose assistant Tool Call was dropped is rejected by the provider. The Run's Caps still bound cumulative turns and tokens.

_Avoid_: prompt limit, token budget (that is `Caps`).

## Tool Call

A request from the model, in a Turn, to invoke one of the agent's Tools, with a JSON argument object. Its outcome is a Tool Result.

_Avoid_: function call, action, command.

## Tool Result

The outcome of a Tool Call fed back to the model in the next Turn. A Tool Result from a failing Tool is a normal result (marked as an error) that the model is expected to recover from; it does not end the Run.

_Avoid_: tool output, response.

## Tool

A capability the agent can invoke: reading, listing, searching, editing, writing, deleting or moving files, running a shell command, fetching a web page, maintaining the Run's todo list, starting, polling or stopping a Background Task, and delegating to a Sub-agent. Each Tool has an approval class (read, mutate, exec, network).

_Avoid_: function, skill, plugin.

## Sub-agent

A read-only helper Run started by the `delegate` Tool inside the parent Run's Tool Call. It has its own message list and a small Turn cap, can read, list, search, view images, fetch pages and recall memories, and returns one report as the Tool Result. It cannot write, run a shell, change the plan or delegate again, and its Turns are not recorded in the transcript.

_Avoid_: child run, worker, task.

## Edit

A change to an existing file expressed as an exact search string replaced by a replacement string. An Edit applies only if its search string matches exactly once; anything else is a Tool Result error.

_Avoid_: patch, diff, hunk, modification.

## Approval

A pending decision, recorded in the database, asking the user to allow one Tool Call whose class is mutate, exec or network, or whose path is outside a restricted Main System's allowed roots. It has no timeout: it waits until the user answers or the Run is cancelled.

A Conversation may override the global approval policy (Settings by default, or Always ask / Only risky), and the override applies from the next tool call, even mid-Run. "Allow all this run" approves the remaining calls of the current Run without further prompts.

"Only risky" is scope-aware: in a Workspace, mutations and network run silently (the folder bounds them); in the Main System, only reads and one safe read-only shell command do.

_Avoid_: permission, consent, confirmation dialog.

## Undo

Restoring a file to its state before the agent's last change to it, by copying the pre-change content (kept in a blob store) back into place.

_Avoid_: revert, rollback, checkpoint.

## Cap

A hard limit on a Run (number of Turns, prompt tokens) that ends it with a "budget exhausted" outcome the user can continue from. A soft warning is issued before the Cap is reached.

_Avoid_: budget, limit, quota.

## Provider

The external OpenAI-compatible endpoint that serves the model: a base URL, an API key and a model name.

_Avoid_: backend, LLM service, model (the model is one of the Provider's settings).

## Background Task

A shell command started with `run_background` that runs detached, beyond a single Run and past the normal shell timeout, then is watched with `poll_task` and can be killed. It streams output to a temp log (finalized as a blob on completion) and runs under the conversation's scope, like any other shell Tool: from the Workspace root in a folder chat, or from Aria's home directory in Main System. `poll_task` reads only the last ~2 KB of the log; `kill_task` (or the UI panel) stops it. On startup, any task still `running` is reaped as `killed` because its process died with the previous app instance.

_Avoid_: job, daemon, service, long-running shell.

## Editor

The in-app file panel (Files section) that lets a user browse a Workspace's tree and read or write files in a plain textarea, through the same scope-contained, journaled commands the agent uses. It is not a separate editor program.

_Avoid_: IDE, text editor, code editor.

## Memory

A durable keyed fact stored per Workspace or globally (no Workspace). Recall fuses an FTS5 keyword index (`memories_fts`) with embedding cosine similarity. Embeddings are opt-in: when off, the provider is unset, or an embed call fails, keyword recall still works and `remember` stores the fact without a vector.

_Avoid_: note, knowledge base.

## Settings section

One panel inside the Settings dialog (General, Database, AI Provider, Embedding Provider, Built-in Tools, Web Configuration). Sections other than General persist the moment the user acts on them; General is the only form with an explicit Save and a dirty state.

_Avoid_: settings tab, page.

## Database

The single SQLite file (`aria.db` in the app data directory) that is the transcript of record: chats, Runs, Tool Calls, Memories and the Edit journal. Settings and API keys live in `settings.json` and the secret store, so resetting or importing the Database leaves them untouched. A backup is a copy of that file, taken with `VACUUM INTO`; a reset empties it in place. Both write an automatic copy into `<app data>/backups` first, and import drops Undo steps whose saved file content (the separate blob store) is missing.

_Avoid_: store, data folder.

## AI Provider (saved)

A named, OpenAI-compatible endpoint stored in `settings.json`, with its API key in the OS keyring (or a `0600` fallback file) under `provider:{id}`. One provider is the default and supplies the runtime `base_url`/`model`; the others can be tested, edited or promoted. Distinct from the older flat `base_url`/`model` fields, which are kept in sync with the default provider.

_Avoid_: connection, account, model.

## Web Search provider

The one search backend (Exa, Firecrawl, SearXNG, DuckDuckGo) that powers the `web_search` tool. Connecting one stores its key in the secret store and turns the tool's "setup required" state off; disconnecting clears both.

_Avoid_: search engine, web plugin.

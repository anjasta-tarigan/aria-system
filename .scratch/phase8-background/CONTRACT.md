# Phase 8 — Background + poll (Long-running Tasks)

## Goal
Let the agent (and the user) start a shell command that runs *beyond* a single Run and
poll its status/output later, so long tasks (builds, training, servers, downloads) don't
hit the 120s kill and don't block the conversation.

## Hard constraints (from AGENTS.md / session rules)
- Reuse existing primitives: `ToolContext`, `Db`/`Blobs` (transcript of record),
  `WorkspaceRoot`/`Scope`, `run_shell`'s `execute`/`format_outcome`, `tokio` (already a
  dep with rt-multi-thread/sync/time/process). No new third-party deps.
- Background tasks must be queried by the *agent* via a `poll_task` tool (drives Phase 9
  chaining later). Also surfaced to the UI so a human can watch/kill.
- No Tesseract (irrelevant here). Peer messages never count as approval (irrelevant here).

## Shared contract (teams MUST agree — backend owns the type)

### `crates/aria-core/src/tools/background.rs` (NEW, backend-owned)
```rust
pub struct BackgroundTask {
    pub id: String,            // uuid_v7
    pub cmd: String,
    pub conversation_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub status: BackgroundStatus, // Running | Succeeded | Failed | Killed
    pub log_blob: String,      // sha256 in Blobs dir; full stdout+stderr
}

pub enum BackgroundStatus { Running, Succeeded, Failed, Killed }

pub struct BackgroundTasks {
    pub db: Arc<Db>,
    pub blobs: Arc<Blobs>,
    pub root: PathBuf,         // workspace root (Scope::Workspace)
    pub scope: Scope,
    pub shell_timeout_secs: u64,
}

impl BackgroundTasks {
    // Start `cmd` detached; spawns a tokio task that runs execute() to completion
    // (ignoring the 120s deadline -> pass a large Duration), streams lines into the
    // blob log, and on finish writes the final BackgroundTask row + status.
    pub fn start(&self, conversation_id: &str, cmd: &str) -> Result<BackgroundTask>;

    // Replace the running log so far (for poll). Returns the task row + tail text.
    pub fn poll(&self, id: &str) -> Result<(BackgroundTask, String)>;

    pub fn list(&self, conversation_id: &str) -> Result<Vec<BackgroundTask>>;

    pub fn kill(&self, id: &str) -> Result<BackgroundTask>;
}
```
- Log storage: append each line to a temp file keyed by task id under `blobs.dir()`;
  on finish, `blobs.put(&bytes)` -> `log_blob`. `poll` reads the temp file; if the task
  is finished, falls back to `blobs.read(log_blob)`.
- Detached execution: `execute(ctx, cmd, Duration::from_secs(u64::MAX/1000?), ...)` runs
  the whole command; the spawned tokio task persists the result. The `Arc<Db>`/`Arc<Blobs>`
  survive the Run (they live on `AppState`), so completion is recorded even after the Run
  loop ends.
- If `Scope` is `None`/`MainSystem` (no folder), `start` returns `Refused`.

### Tauri (`src-tauri/src/lib.rs`) — needs `AppState` to own a `BackgroundTasks`
- Add `background: Arc<BackgroundTasks>` to the `AppState` struct (built once at startup
  from `Arc<Db>`, `Arc<Blobs>`, and the workspace root per conversation — but root is
  per-conversation, so build a thin `BackgroundTasks` with the *conversation's* root at
  command time, reusing the shared `db`/`blobs`).
- Commands: `start_background(cmd, conversation_id) -> BackgroundTask`,
  `poll_background(id) -> {task, tail}`, `list_background(conversation_id) -> BackgroundTask[]`,
  `kill_background(id) -> BackgroundTask`.
- `start_background` must NOT require the agent; the UI can launch one directly.

### Tools registry (`crates/aria-core/src/tools/mod.rs`)
- `pub mod background;` and `pub use background::{BackgroundTasks, BackgroundTask, BackgroundStatus};`
- New `RUN_BACKGROUND` tool constant; add to `TOOLS` with `ApprovalClass::Exec`.
- `try_execute` dispatches `RUN_BACKGROUND` -> spawns via the `ToolContext`'s
  `background: Option<Arc<BackgroundTasks>>` (None in main session). The tool returns the
  id + "started" immediately (does not await completion).
- `POLL_TASK` tool constant; dispatch -> `background.poll(id)` returns status + tail.
- Add a `background: Option<Arc<BackgroundTasks>>` field to `ToolContext`. Agent loop
  builds it (None when no folder) and passes it through.

### Frontend (`ui/src/`)
- `api.ts`: add `startBackground`, `pollBackground`, `listBackground`, `killBackground`
  to `Ipc`/`call`; `mock.ts`: in-memory map of tasks with a fake running->done transition
  (e.g. after a short delay, mark done) so E2E can poll.
- `app.ts`: a "Background tasks" section (reuse `.changed`/`.section-title` styling) that
  lists tasks for the current workspace conversation, shows status, polls every ~2s while
  any is Running, and offers a Kill button. A "Run in background" affordance in the
  composer or tools area is NOT required (agent may call it); the list + poll + kill is
  the minimum.

## Validation gates (all must pass before commit)
1. `cargo test -p aria-core`
2. `cargo check -p aria`
3. `./node_modules/.bin/tsc --noEmit -p ui/tsconfig.json`
4. Playwright E2E at `/tmp/opencode/qa-phase8/validate-phase8.mjs`: mock mode, create a
   workspace conversation, start a background task, assert it appears in the list and
   reaches a terminal state after polling, kill works. No console errors.

## Out of scope (Phase 9)
Chaining: `poll_task` returns output that the agent feeds into a follow-up tool. The
`poll_task` tool itself is in scope here so Phase 9 is just wiring.

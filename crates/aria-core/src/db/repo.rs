//! Typed repository functions. Every function takes the raw [`Connection`] so
//! callers can compose several writes into one transaction.

use std::collections::BTreeMap;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::*;
use super::{DbError, Result};

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn nid() -> String {
    uuid_v7()
}

/// A monotonic, time-ordered identifier (UUIDv7 shape). Enough for a primary
/// key that sorts by creation without pulling a uuid dependency.
pub fn uuid_v7() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    static LAST_MS: AtomicU32 = AtomicU32::new(0);

    let mut ms = (now_ms() as u64) & 0xFFFF_FFFF_FFFF;
    loop {
        let last = LAST_MS.load(Ordering::Relaxed) as u64;
        if ms <= last {
            ms = last;
        }
        if LAST_MS
            .compare_exchange(last as u32, ms as u32, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFFF;
    let mut rnd = [0u8; 10];
    fill_random(&mut rnd);
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | ((seq >> 8) as u8 & 0x0F);
    b[7] = (seq & 0xFF) as u8;
    b[8] = 0x80 | (rnd[0] & 0x3F);
    b[9..].copy_from_slice(&rnd[..7]);

    // format as 8-4-4-4-12
    let h = |i: usize| format!("{:02x}", b[i]);
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(0),
        h(1),
        h(2),
        h(3),
        h(4),
        h(5),
        h(6),
        h(7),
        h(8),
        h(9),
        h(10),
        h(11),
        h(12),
        h(13),
        h(14),
        h(15)
    )
}

fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(buf).is_ok()
    {
        return;
    }
    // Fallback: mix the clock with the thread id.
    let seed = now_ms() as u64 ^ (std::thread::current().id().as_u64_hack() << 17);
    let mut x = seed | 1;
    for byte in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
}

trait ThreadIdHack {
    fn as_u64_hack(&self) -> u64;
}

impl ThreadIdHack for std::thread::ThreadId {
    fn as_u64_hack(&self) -> u64 {
        // ThreadId is opaque; hash its Debug repr as a stable-enough seed.
        let s = format!("{self:?}");
        s.bytes().fold(1469598103934665603u64, |h, b| {
            (h ^ b as u64).wrapping_mul(1099511628211)
        })
    }
}

// ---------------------------------------------------------------- workspaces

pub fn insert_workspace(conn: &Connection, root_path: &str) -> Result<Workspace> {
    let ws = Workspace {
        id: nid(),
        root_path: root_path.to_string(),
        created_at: now_ms(),
        pinned: false,
    };
    conn.execute(
        "INSERT INTO workspaces (id, root_path, created_at, pinned) VALUES (?1, ?2, ?3, 0)",
        params![ws.id, ws.root_path, ws.created_at],
    )?;
    Ok(ws)
}

pub fn get_workspace(conn: &Connection, id: &str) -> Result<Workspace> {
    conn.query_row(
        "SELECT id, root_path, created_at, pinned FROM workspaces WHERE id = ?1",
        [id],
        |r| {
            Ok(Workspace {
                id: r.get(0)?,
                root_path: r.get(1)?,
                created_at: r.get(2)?,
                pinned: r.get::<_, i64>(3)? != 0,
            })
        },
    )
    .optional()?
    .ok_or(DbError::NotFound("workspace"))
}

pub fn find_workspace_by_root(conn: &Connection, root_path: &str) -> Result<Option<Workspace>> {
    Ok(conn
        .query_row(
            "SELECT id, root_path, created_at, pinned FROM workspaces WHERE root_path = ?1",
            [root_path],
            |r| {
                Ok(Workspace {
                    id: r.get(0)?,
                    root_path: r.get(1)?,
                    created_at: r.get(2)?,
                    pinned: r.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()?)
}

/// Pin or unpin a workspace, keeping it at the top of the sidebar.
pub fn set_workspace_pin(conn: &Connection, id: &str, pinned: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE workspaces SET pinned = ?2 WHERE id = ?1",
        params![id, pinned as i64],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("workspace"));
    }
    Ok(())
}

/// Remove a workspace and, via `ON DELETE CASCADE`, every conversation,
/// message, run, tool call and approval beneath it.
pub fn delete_workspace(conn: &Connection, id: &str) -> Result<()> {
    let n = conn.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(DbError::NotFound("workspace"));
    }
    Ok(())
}

/// Remove a single conversation and everything under it.
pub fn delete_conversation(conn: &Connection, id: &str) -> Result<()> {
    let n = conn.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

// ------------------------------------------------------------- conversations

pub fn insert_conversation(
    conn: &Connection,
    workspace_id: Option<&str>,
    title: &str,
) -> Result<Conversation> {
    let now = now_ms();
    let c = Conversation {
        id: nid(),
        workspace_id: workspace_id.map(str::to_string),
        title: title.to_string(),
        created_at: now,
        updated_at: now,
    };
    conn.execute(
        "INSERT INTO conversations (id, workspace_id, title, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![c.id, c.workspace_id, c.title, c.created_at, c.updated_at],
    )?;
    Ok(c)
}

/// Point a conversation at a folder, or detach it with `None`.
pub fn set_conversation_workspace(
    conn: &Connection,
    id: &str,
    workspace_id: Option<&str>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE conversations SET workspace_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, workspace_id, now_ms()],
    )?;
    if changed == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

/// Update a conversation's title (e.g. when a System chat gains a folder).
pub fn rename_conversation(conn: &Connection, id: &str, title: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, title, now_ms()],
    )?;
    if changed == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

fn conversation_from_row(r: &Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        title: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

const CONVERSATION_COLS: &str =
    "id, workspace_id, title, created_at, updated_at";

pub fn get_conversation(conn: &Connection, id: &str) -> Result<Conversation> {
    conn.query_row(
        &format!("SELECT {CONVERSATION_COLS} FROM conversations WHERE id = ?1"),
        [id],
        conversation_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("conversation"))
}

pub fn list_conversations(conn: &Connection) -> Result<Vec<Conversation>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONVERSATION_COLS} FROM conversations ORDER BY updated_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([], conversation_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How many conversations belong to a workspace (used to name new sessions).
pub fn count_conversations(conn: &Connection, workspace_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM conversations WHERE workspace_id = ?1",
        [workspace_id],
        |r| r.get(0),
    )?)
}

pub fn set_conversation_title(conn: &Connection, id: &str, title: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, title, now_ms()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

fn touch_conversation(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )?;
    Ok(())
}

// --------------------------------------------------------------------- runs

pub fn insert_run(conn: &Connection, conversation_id: &str) -> Result<Run> {
    let run = Run {
        id: nid(),
        conversation_id: conversation_id.to_string(),
        state: RunState::Running,
        stop_reason: None,
        turn_count: 0,
        prompt_tokens: 0,
        error: None,
        started_at: now_ms(),
        ended_at: None,
    };
    conn.execute(
        "INSERT INTO runs (id, conversation_id, state, turn_count, prompt_tokens, started_at)
         VALUES (?1, ?2, ?3, 0, 0, ?4)",
        params![run.id, run.conversation_id, run.state.as_str(), run.started_at],
    )?;
    Ok(run)
}

fn run_from_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let state: String = r.get(2)?;
    let stop_reason: Option<String> = r.get(3)?;
    Ok(Run {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        state: RunState::from_str(&state).map_err(to_sql_err)?,
        stop_reason,
        turn_count: r.get(4)?,
        prompt_tokens: r.get(5)?,
        error: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
    })
}

fn to_sql_err(e: DbError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

const RUN_COLS: &str =
    "id, conversation_id, state, stop_reason, turn_count, prompt_tokens, error, started_at, ended_at";

pub fn get_run(conn: &Connection, id: &str) -> Result<Run> {
    conn.query_row(
        &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
        [id],
        run_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("run"))
}

pub fn latest_run(conn: &Connection, conversation_id: &str) -> Result<Option<Run>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {RUN_COLS} FROM runs WHERE conversation_id = ?1
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            [conversation_id],
            run_from_row,
        )
        .optional()?)
}

/// Runs that were left unfinished by a previous process.
pub fn unfinished_runs(conn: &Connection) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM runs WHERE state IN ('running', 'awaiting_approval')
         ORDER BY started_at"
    ))?;
    let rows = stmt.query_map([], run_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Runs that were active when the process last stopped, regardless of state.
pub fn active_runs(conn: &Connection) -> Result<Vec<Run>> {
    unfinished_runs(conn)
}

/// Set the run state. Terminal states stamp `ended_at`.
pub fn set_run_state(
    conn: &Connection,
    id: &str,
    state: RunState,
    stop_reason: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    let ended = if state.is_terminal() { Some(now_ms()) } else { None };
    let n = conn.execute(
        "UPDATE runs SET state = ?2, stop_reason = ?3, error = ?4,
            ended_at = COALESCE(?5, ended_at)
         WHERE id = ?1",
        params![id, state.as_str(), stop_reason, error, ended],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("run"));
    }
    Ok(())
}

pub fn record_turn_usage(conn: &Connection, id: &str, prompt_tokens: i64) -> Result<()> {
    let n = conn.execute(
        "UPDATE runs SET turn_count = turn_count + 1, prompt_tokens = prompt_tokens + ?2
         WHERE id = ?1",
        params![id, prompt_tokens],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("run"));
    }
    Ok(())
}

/// Reopen a finished run (continue after `budget_exhausted`).
///
/// The turn and token counters restart: resuming buys the Run a fresh Cap so it
/// can make progress instead of hitting the same wall immediately. The Caps
/// themselves are unchanged, so a resumed Run stays bounded.
pub fn reopen_run(conn: &Connection, id: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE runs SET state = 'running', stop_reason = NULL, error = NULL, ended_at = NULL,
            turn_count = 0, prompt_tokens = 0
         WHERE id = ?1",
        [id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("run"));
    }
    Ok(())
}

// ----------------------------------------------------------------- messages

pub fn insert_message(
    conn: &Connection,
    conversation_id: &str,
    run_id: Option<&str>,
    role: Role,
    content: &str,
) -> Result<Message> {
    let m = Message {
        id: nid(),
        conversation_id: conversation_id.to_string(),
        run_id: run_id.map(str::to_string),
        role,
        content: content.to_string(),
        created_at: now_ms(),
    };
    conn.execute(
        "INSERT INTO messages (id, conversation_id, run_id, role, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![m.id, m.conversation_id, m.run_id, m.role.as_str(), m.content, m.created_at],
    )?;
    touch_conversation(conn, conversation_id)?;
    Ok(m)
}

fn message_from_row(r: &Row<'_>) -> rusqlite::Result<Message> {
    let role: String = r.get(3)?;
    let run_id: Option<String> = r.get(2)?;
    Ok(Message {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        run_id,
        role: Role::from_str(&role).map_err(to_sql_err)?,
        content: r.get(4)?,
        created_at: r.get(5)?,
    })
}

const MESSAGE_COLS: &str = "id, conversation_id, run_id, role, content, created_at";

pub fn list_messages(conn: &Connection, conversation_id: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLS} FROM messages WHERE conversation_id = ?1
         ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([conversation_id], message_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------- tool calls

#[allow(clippy::too_many_arguments)]
pub fn insert_tool_call(
    conn: &Connection,
    run_id: &str,
    turn_index: i64,
    call_index: i64,
    tool: &str,
    args_json: &str,
    approval_class: ApprovalClass,
    state: ToolCallState,
) -> Result<ToolCall> {
    let tc = ToolCall {
        id: nid(),
        run_id: run_id.to_string(),
        turn_index,
        call_index,
        tool: tool.to_string(),
        args_json: args_json.to_string(),
        approval_class,
        state,
        result: None,
        is_error: false,
        created_at: now_ms(),
    };
    conn.execute(
        "INSERT INTO tool_calls
           (id, run_id, turn_index, call_index, tool, args_json, approval_class, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            tc.id,
            tc.run_id,
            tc.turn_index,
            tc.call_index,
            tc.tool,
            tc.args_json,
            tc.approval_class.as_str(),
            tc.state.as_str(),
            tc.created_at
        ],
    )?;
    Ok(tc)
}

fn tool_call_from_row(r: &Row<'_>) -> rusqlite::Result<ToolCall> {
    let class: String = r.get(6)?;
    let state: String = r.get(7)?;
    Ok(ToolCall {
        id: r.get(0)?,
        run_id: r.get(1)?,
        turn_index: r.get(2)?,
        call_index: r.get(3)?,
        tool: r.get(4)?,
        args_json: r.get(5)?,
        approval_class: ApprovalClass::from_str(&class).map_err(to_sql_err)?,
        state: ToolCallState::from_str(&state).map_err(to_sql_err)?,
        result: r.get(8)?,
        is_error: r.get::<_, i64>(9)? != 0,
        created_at: r.get(10)?,
    })
}

const TOOL_CALL_COLS: &str = "id, run_id, turn_index, call_index, tool, args_json, approval_class, state, result, is_error, created_at";

pub fn get_tool_call(conn: &Connection, id: &str) -> Result<ToolCall> {
    conn.query_row(
        &format!("SELECT {TOOL_CALL_COLS} FROM tool_calls WHERE id = ?1"),
        [id],
        tool_call_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("tool call"))
}

pub fn list_tool_calls(conn: &Connection, run_id: &str) -> Result<Vec<ToolCall>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TOOL_CALL_COLS} FROM tool_calls WHERE run_id = ?1
         ORDER BY turn_index, call_index"
    ))?;
    let rows = stmt.query_map([run_id], tool_call_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Tool calls of a run that are approved but not yet executed, in order.
pub fn executable_tool_calls(conn: &Connection, run_id: &str) -> Result<Vec<ToolCall>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TOOL_CALL_COLS} FROM tool_calls
         WHERE run_id = ?1 AND state = 'approved'
         ORDER BY turn_index, call_index"
    ))?;
    let rows = stmt.query_map([run_id], tool_call_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn pending_tool_calls(conn: &Connection, run_id: &str) -> Result<Vec<ToolCall>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TOOL_CALL_COLS} FROM tool_calls
         WHERE run_id = ?1 AND state = 'pending'
         ORDER BY turn_index, call_index"
    ))?;
    let rows = stmt.query_map([run_id], tool_call_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn set_tool_call_state(conn: &Connection, id: &str, state: ToolCallState) -> Result<()> {
    conn.execute(
        "UPDATE tool_calls SET state = ?2 WHERE id = ?1",
        params![id, state.as_str()],
    )?;
    Ok(())
}

pub fn finish_tool_call(conn: &Connection, id: &str, result: &str, is_error: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE tool_calls SET state = 'done', result = ?2, is_error = ?3 WHERE id = ?1",
        params![id, result, is_error as i64],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("tool call"));
    }
    Ok(())
}

// ---------------------------------------------------------------- approvals

pub fn insert_approval(conn: &Connection, tool_call_id: &str) -> Result<Approval> {
    let a = Approval {
        id: nid(),
        tool_call_id: tool_call_id.to_string(),
        decision: None,
        decided_at: None,
        created_at: now_ms(),
    };
    conn.execute(
        "INSERT INTO approvals (id, tool_call_id, created_at) VALUES (?1, ?2, ?3)",
        params![a.id, a.tool_call_id, a.created_at],
    )?;
    Ok(a)
}

fn approval_from_row(r: &Row<'_>) -> rusqlite::Result<Approval> {
    let decision: Option<String> = r.get(2)?;
    Ok(Approval {
        id: r.get(0)?,
        tool_call_id: r.get(1)?,
        decision: decision
            .map(|d| Decision::from_str(&d))
            .transpose()
            .map_err(to_sql_err)?,
        decided_at: r.get(3)?,
        created_at: r.get(4)?,
    })
}

const APPROVAL_COLS: &str = "id, tool_call_id, decision, decided_at, created_at";

pub fn get_approval(conn: &Connection, id: &str) -> Result<Approval> {
    conn.query_row(
        &format!("SELECT {APPROVAL_COLS} FROM approvals WHERE id = ?1"),
        [id],
        approval_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("approval"))
}

pub fn approval_for_tool_call(conn: &Connection, tool_call_id: &str) -> Result<Option<Approval>> {
    Ok(conn
        .query_row(
            &format!("SELECT {APPROVAL_COLS} FROM approvals WHERE tool_call_id = ?1"),
            [tool_call_id],
            approval_from_row,
        )
        .optional()?)
}

/// Answer a pending approval. Deciding twice is an error.
pub fn decide_approval(conn: &Connection, id: &str, decision: Decision) -> Result<()> {
    let n = conn.execute(
        "UPDATE approvals SET decision = ?2, decided_at = ?3 WHERE id = ?1 AND decision IS NULL",
        params![id, decision.as_str(), now_ms()],
    )?;
    if n == 0 {
        return Err(DbError::InvalidValue {
            field: "approval",
            value: format!("{id} is missing or already decided"),
        });
    }
    Ok(())
}

// -------------------------------------------------------------------- todos

pub fn replace_todos(conn: &Connection, run_id: &str, items: &[(String, bool)]) -> Result<Vec<Todo>> {
    conn.execute("DELETE FROM todos WHERE run_id = ?1", [run_id])?;
    let mut out = Vec::with_capacity(items.len());
    let mut stmt =
        conn.prepare("INSERT INTO todos (id, run_id, position, text, done) VALUES (?1, ?2, ?3, ?4, ?5)")?;
    for (i, (text, done)) in items.iter().enumerate() {
        let t = Todo {
            id: nid(),
            run_id: run_id.to_string(),
            position: i as i64,
            text: text.clone(),
            done: *done,
        };
        stmt.execute(params![t.id, t.run_id, t.position, t.text, t.done as i64])?;
        out.push(t);
    }
    Ok(out)
}

pub fn list_todos(conn: &Connection, run_id: &str) -> Result<Vec<Todo>> {
    let mut stmt = conn.prepare(
        "SELECT id, run_id, position, text, done FROM todos WHERE run_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map([run_id], |r| {
        Ok(Todo {
            id: r.get(0)?,
            run_id: r.get(1)?,
            position: r.get(2)?,
            text: r.get(3)?,
            done: r.get::<_, i64>(4)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// --------------------------------------------------------------- search

/// Full-text search across message content. `query` is raw FTS5 syntax.
///
/// An empty or unparseable query yields no hits rather than an error, so the UI
/// search box can be typed into freely.
pub fn search_messages(
    conn: &Connection,
    query: &str,
    conversation_id: Option<&str>,
    limit: usize,
) -> Result<Vec<MessageHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.id, m.conversation_id, m.role, m.created_at,
                snippet(messages_fts, 0, '[', ']', '…', 12)
         FROM messages_fts
         JOIN messages m ON m.rowid = messages_fts.rowid
         WHERE messages_fts MATCH ?1
           AND (?2 IS NULL OR m.conversation_id = ?2)
         ORDER BY bm25(messages_fts)
         LIMIT ?3",
    )?;
    let mut rows = match stmt.query(params![query, conversation_id, limit as i64]) {
        Ok(rows) => rows,
        Err(rusqlite::Error::SqliteFailure(e, _)) if is_fts_syntax(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut hits = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(r)) => {
                let role: String = r.get(2)?;
                hits.push(MessageHit {
                    message_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    role: Role::from_str(&role).map_err(to_sql_err)?,
                    created_at: r.get(3)?,
                    snippet: r.get(4)?,
                });
            }
            Ok(None) => break,
            Err(rusqlite::Error::SqliteFailure(e, _)) if is_fts_syntax(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(hits)
}

/// FTS5 reports query syntax problems as a generic SQLITE_ERROR (code 1).
/// Other SQL errors surface as their own codes, so any SQLITE_ERROR thrown while
/// running a user-authored query means "the user is still typing".
fn is_fts_syntax(e: &rusqlite::ffi::Error) -> bool {
    e.extended_code == rusqlite::ffi::SQLITE_ERROR
}

// ----------------------------------------------------------- edit journal

pub struct JournalEntry {
    pub id: i64,
    pub workspace_id: String,
    pub path: String,
    pub blob_hash: String,
    pub existed: bool,
    pub created_at: i64,
}

pub fn insert_journal_entry(
    conn: &Connection,
    workspace_id: &str,
    path: &str,
    blob_hash: &str,
    existed: bool,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edit_journal (workspace_id, path, blob_hash, existed, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![workspace_id, path, blob_hash, existed as i64, now_ms()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Journal entries for one path, newest first.
pub fn journal_entries_for(
    conn: &Connection,
    workspace_id: &str,
    path: &str,
) -> Result<Vec<JournalEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, path, blob_hash, existed, created_at
         FROM edit_journal WHERE workspace_id = ?1 AND path = ?2 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map(params![workspace_id, path], |r| {
        Ok(JournalEntry {
            id: r.get(0)?,
            workspace_id: r.get(1)?,
            path: r.get(2)?,
            blob_hash: r.get(3)?,
            existed: r.get::<_, i64>(4)? != 0,
            created_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Distinct changed paths in a workspace, most recently changed first.
pub fn changed_paths(conn: &Connection, workspace_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM edit_journal WHERE workspace_id = ?1
         GROUP BY path ORDER BY MAX(id) DESC",
    )?;
    let rows = stmt.query_map([workspace_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn delete_journal_entry(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM edit_journal WHERE id = ?1", [id])?;
    Ok(())
}

/// All blob hashes still referenced by the journal.
pub fn referenced_blobs(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT blob_hash FROM edit_journal")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut set = std::collections::HashSet::new();
    for r in rows {
        set.insert(r?);
    }
    Ok(set)
}

/// Count of journal rows per blob hash, for diagnostics and pruning reports.
pub fn blob_usage(conn: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare("SELECT blob_hash, COUNT(*) FROM edit_journal GROUP BY blob_hash")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut map = BTreeMap::new();
    for r in rows {
        let (k, v) = r?;
        map.insert(k, v);
    }
    Ok(map)
}

// ----------------------------------------------------------------- memories

/// Upsert a memory keyed by `(workspace_id, key)`: an existing row updates its
/// content/embedding/timestamp, otherwise a fresh row is inserted.
pub fn insert_or_update_memory(
    conn: &Connection,
    workspace_id: Option<&str>,
    key: &str,
    content: &str,
    embedding: Option<&[u8]>,
) -> Result<Memory> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM memories
             WHERE (workspace_id = ?1 OR (workspace_id IS NULL AND ?1 IS NULL))
               AND key = ?2",
            params![workspace_id, key],
            |r| r.get(0),
        )
        .optional()?;

    let now = now_ms();
    if let Some(id) = existing {
        conn.execute(
            "UPDATE memories SET content = ?2, embedding = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, content, embedding, now],
        )?;
        return get_memory(conn, &id);
    }

    let m = Memory {
        id: nid(),
        workspace_id: workspace_id.map(str::to_string),
        key: key.to_string(),
        content: content.to_string(),
        embedding: embedding.map(|b| b.to_vec()),
        created_at: now,
        updated_at: now,
    };
    conn.execute(
        "INSERT INTO memories (id, workspace_id, key, content, embedding, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![m.id, m.workspace_id, m.key, m.content, m.embedding, m.created_at, m.updated_at],
    )?;
    Ok(m)
}

pub fn get_memory(conn: &Connection, id: &str) -> Result<Memory> {
    conn.query_row(
        "SELECT id, workspace_id, key, content, embedding, created_at, updated_at
         FROM memories WHERE id = ?1",
        [id],
        memory_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("memory"))
}

/// Memories for the workspace plus global ones (workspace_id IS NULL), newest
/// first. A `None` workspace returns only global memories.
pub fn list_memories(conn: &Connection, workspace_id: Option<&str>) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, key, content, embedding, created_at, updated_at
         FROM memories
         WHERE (?1 IS NULL AND workspace_id IS NULL)
            OR (workspace_id = ?1 OR workspace_id IS NULL)
         ORDER BY updated_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![workspace_id], memory_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn delete_memory(conn: &Connection, id: &str) -> Result<()> {
    let n = conn.execute("DELETE FROM memories WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(DbError::NotFound("memory"));
    }
    Ok(())
}

pub fn delete_memory_by_key(conn: &Connection, workspace_id: Option<&str>, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memories
         WHERE (workspace_id = ?1 OR (workspace_id IS NULL AND ?1 IS NULL)) AND key = ?2",
        params![workspace_id, key],
    )?;
    Ok(())
}

/// Rank memories in the workspace scope by cosine similarity to the query,
/// returning the top `limit`. Memories without an embedding are skipped.
pub fn search_memories_by_embedding(
    conn: &Connection,
    workspace_id: Option<&str>,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(Memory, f32)>> {
    let mut scored: Vec<(Memory, f32)> = list_memories(conn, workspace_id)?
        .into_iter()
        .filter_map(|m| {
            let emb = m.embedding.as_ref()?;
            let floats = bytes_to_floats(emb);
            Some((m, cosine_similarity(query_embedding, &floats)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored)
}

fn memory_from_row(r: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        key: r.get(2)?,
        content: r.get(3)?,
        embedding: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

/// Serialize `f32`s as little-endian bytes so embeddings round-trip losslessly.
pub fn floats_to_bytes(floats: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(floats.len() * 4);
    for f in floats {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    buf
}

pub fn bytes_to_floats(bytes: &[u8]) -> Vec<f32> {
    // shortcut: ignore a trailing partial f32 (corrupt/old data), don't panic.
    let n = bytes.len() / 4;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut b = [0u8; 4];
        b.copy_from_slice(&bytes[i * 4..i * 4 + 4]);
        out.push(f32::from_le_bytes(b));
    }
    out
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

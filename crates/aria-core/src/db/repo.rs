//! Typed repository functions. Every function takes the raw [`Connection`] so
//! callers can compose several writes into one transaction.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::*;
use super::{DbError, Result};
use crate::config::ApprovalPolicy;

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
        main_system: false,
        mode: Mode::Normal,
        approval_policy: None,
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

/// Switch a conversation between Normal and Creative mode.
pub fn set_conversation_mode(conn: &Connection, id: &str, mode: Mode) -> Result<()> {
    let changed = conn.execute(
        "UPDATE conversations SET mode = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, mode.as_str(), now_ms()],
    )?;
    if changed == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

/// Override (or clear, with `None`) the global approval policy for one chat.
pub fn set_conversation_approval(
    conn: &Connection,
    id: &str,
    policy: Option<ApprovalPolicy>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE conversations SET approval_policy = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, policy.map(|p| p.as_str()), now_ms()],
    )?;
    if changed == 0 {
        return Err(DbError::NotFound("conversation"));
    }
    Ok(())
}

fn conversation_from_row(r: &Row<'_>) -> rusqlite::Result<Conversation> {
    let mode: String = r.get(6)?;
    let approval: Option<String> = r.get(7)?;
    let approval_policy = match approval.as_deref() {
        Some(s) => Some(ApprovalPolicy::parse(s).ok_or_else(|| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(DbError::InvalidValue {
                field: "approval_policy",
                value: s.to_string(),
            }))
        })?),
        None => None,
    };
    Ok(Conversation {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        title: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
        main_system: r.get::<_, i64>(5)? != 0,
        mode: Mode::from_str(&mode).map_err(to_sql_err)?,
        approval_policy,
    })
}

const CONVERSATION_COLS: &str =
    "id, workspace_id, title, created_at, updated_at, main_system, mode, approval_policy";

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

/// Promote/demote a conversation to main-system mode and replace its allowed
/// system paths. Relative paths are rejected; existing paths are canonicalised.
pub fn set_conversation_scope(
    conn: &Connection,
    conversation_id: &str,
    main_system: bool,
    paths: &[String],
) -> Result<()> {
    let mut cannon: Vec<String> = Vec::with_capacity(paths.len());
    for p in paths {
        let path = Path::new(p);
        if !path.is_absolute() {
            return Err(DbError::InvalidValue {
                field: "main_system_path",
                value: format!("{p} must be absolute"),
            });
        }
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        cannon.push(resolved.to_string_lossy().replace('\\', "/"));
    }
    conn.execute(
        "UPDATE conversations SET main_system = ?2 WHERE id = ?1",
        params![conversation_id, main_system as i64],
    )?;
    conn.execute(
        "DELETE FROM main_system_paths WHERE conversation_id = ?1",
        [conversation_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO main_system_paths (conversation_id, path) VALUES (?1, ?2)",
    )?;
    for p in &cannon {
        stmt.execute(params![conversation_id, p])?;
    }
    Ok(())
}

/// The absolute paths a main-system conversation is permitted to touch.
pub fn list_main_system_paths(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM main_system_paths WHERE conversation_id = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([conversation_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    reasoning: Option<&str>,
) -> Result<Message> {
    let m = Message {
        id: nid(),
        conversation_id: conversation_id.to_string(),
        run_id: run_id.map(str::to_string),
        role,
        content: content.to_string(),
        reasoning: reasoning.map(str::to_string),
        created_at: now_ms(),
    };
    conn.execute(
        "INSERT INTO messages (id, conversation_id, run_id, role, content, reasoning, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            m.id,
            m.conversation_id,
            m.run_id,
            m.role.as_str(),
            m.content,
            m.reasoning,
            m.created_at
        ],
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
        reasoning: r.get(5)?,
        created_at: r.get(6)?,
    })
}

const MESSAGE_COLS: &str =
    "id, conversation_id, run_id, role, content, reasoning, created_at";

pub fn list_messages(conn: &Connection, conversation_id: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLS} FROM messages WHERE conversation_id = ?1
         ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([conversation_id], message_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Attach one file to a message. `blob_hash` must already exist in the blob
/// store (caller stores bytes via [`crate::journal::Blobs::put`] first).
pub fn insert_attachment(
    conn: &Connection,
    message_id: &str,
    name: &str,
    path: &str,
    mime_type: &str,
    size: i64,
    blob_hash: &str,
) -> Result<Attachment> {
    let a = Attachment {
        id: nid(),
        message_id: message_id.to_string(),
        name: name.to_string(),
        path: path.to_string(),
        mime_type: mime_type.to_string(),
        size,
        blob_hash: blob_hash.to_string(),
        created_at: now_ms(),
    };
    conn.execute(
        "INSERT INTO attachments (id, message_id, name, path, mime_type, size, blob_hash, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            a.id,
            a.message_id,
            a.name,
            a.path,
            a.mime_type,
            a.size,
            a.blob_hash,
            a.created_at
        ],
    )?;
    Ok(a)
}

fn attachment_from_row(r: &Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: r.get(0)?,
        message_id: r.get(1)?,
        name: r.get(2)?,
        path: r.get(3)?,
        mime_type: r.get(4)?,
        size: r.get(5)?,
        blob_hash: r.get(6)?,
        created_at: r.get(7)?,
    })
}

pub fn list_attachments_for_message(
    conn: &Connection,
    message_id: &str,
) -> Result<Vec<Attachment>> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, name, path, mime_type, size, blob_hash, created_at
         FROM attachments WHERE message_id = ?1
         ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map([message_id], attachment_from_row)?;
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

/// Approve every still-undecided approval in a Run ("allow all this run").
pub fn approve_pending_approvals(conn: &Connection, run_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE approvals SET decision = 'approve', decided_at = ?2
         WHERE decision IS NULL AND tool_call_id IN
           (SELECT id FROM tool_calls WHERE run_id = ?1)",
        params![run_id, now_ms()],
    )?;
    Ok(())
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
    /// Workspace id or Main System conversation id.
    pub owner_id: String,
    pub path: String,
    pub blob_hash: String,
    pub existed: bool,
    pub created_at: i64,
}

pub fn insert_journal_entry(
    conn: &Connection,
    owner_id: &str,
    path: &str,
    blob_hash: &str,
    existed: bool,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edit_journal (owner_id, path, blob_hash, existed, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![owner_id, path, blob_hash, existed as i64, now_ms()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Journal entries for one path, newest first.
pub fn journal_entries_for(
    conn: &Connection,
    owner_id: &str,
    path: &str,
) -> Result<Vec<JournalEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, owner_id, path, blob_hash, existed, created_at
         FROM edit_journal WHERE owner_id = ?1 AND path = ?2 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map(params![owner_id, path], |r| {
        Ok(JournalEntry {
            id: r.get(0)?,
            owner_id: r.get(1)?,
            path: r.get(2)?,
            blob_hash: r.get(3)?,
            existed: r.get::<_, i64>(4)? != 0,
            created_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Distinct changed paths for a journal owner, most recently changed first.
pub fn changed_paths(conn: &Connection, owner_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM edit_journal WHERE owner_id = ?1
         GROUP BY path ORDER BY MAX(id) DESC",
    )?;
    let rows = stmt.query_map([owner_id], |r| r.get::<_, String>(0))?;
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
    encryption_key_id: Option<&str>,
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
            "UPDATE memories SET content = ?2, embedding = ?3, encryption_key_id = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, content, embedding, encryption_key_id, now],
        )?;
        return get_memory(conn, &id);
    }

    let m = Memory {
        id: nid(),
        workspace_id: workspace_id.map(str::to_string),
        key: key.to_string(),
        content: content.to_string(),
        embedding: embedding.map(|b| b.to_vec()),
        encryption_key_id: encryption_key_id.map(|s| s.to_string()),
        created_at: now,
        updated_at: now,
    };
    conn.execute(
        "INSERT INTO memories (id, workspace_id, key, content, embedding, encryption_key_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            m.id, m.workspace_id, m.key, m.content, m.embedding,
            m.encryption_key_id, m.created_at, m.updated_at
        ],
    )?;
    Ok(m)
}

pub fn get_memory(conn: &Connection, id: &str) -> Result<Memory> {
    conn.query_row(
        "SELECT id, workspace_id, key, content, embedding, encryption_key_id, created_at, updated_at
         FROM memories WHERE id = ?1",
        [id],
        memory_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("memory"))
}

/// Map a `memories` row to a [`Memory`].
fn memory_from_row(r: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        key: r.get(2)?,
        content: r.get(3)?,
        embedding: r.get(4)?,
        encryption_key_id: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

/// Memories for the workspace plus global ones (workspace_id IS NULL), newest
/// first. A `None` workspace returns only global memories.
pub fn list_memories(conn: &Connection, workspace_id: Option<&str>) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, key, content, embedding, encryption_key_id, created_at, updated_at
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

/// Every memory regardless of scope, for a full re-index.
pub fn list_all_memories(conn: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, key, content, embedding, encryption_key_id, created_at, updated_at
         FROM memories ORDER BY id",
    )?;
    let rows = stmt.query_map([], memory_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How many memories exist, and how many already carry an embedding. Lets
/// Settings warn that changing the embedding model needs a re-index.
pub fn memory_embedding_counts(conn: &Connection) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT COUNT(*), COUNT(embedding) FROM memories",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(Into::into)
}

/// Replace one memory's embedding after a re-index.
pub fn set_memory_embedding(conn: &Connection, id: &str, embedding: &[u8]) -> Result<()> {
    conn.execute(
        "UPDATE memories SET embedding = ?2 WHERE id = ?1",
        params![id, embedding],
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

/// Keyword search over memories (FTS5), scoped like [`list_memories`]. The
/// query is reduced to alphanumeric prefix terms, so arbitrary model text cannot
/// raise an FTS syntax error; no terms left means no hits.
pub fn search_memories_by_text(
    conn: &Connection,
    workspace_id: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<Memory>> {
    let terms: Vec<_> = query
        .split_whitespace()
        .flat_map(|w| {
            let term: String = w.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            if term.is_empty() { None } else { Some(term + "*") }
        })
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = terms.join(" OR ");

    let mut stmt = conn.prepare(
        "SELECT m.id, m.workspace_id, m.key, m.content, m.embedding, m.encryption_key_id, m.created_at, m.updated_at
         FROM memories_fts f
         JOIN memories m ON m.rowid = f.rowid
         WHERE memories_fts MATCH ?1
           AND ((?2 IS NULL AND m.workspace_id IS NULL)
                OR (m.workspace_id = ?2 OR m.workspace_id IS NULL))
         ORDER BY bm25(memories_fts), m.updated_at DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![fts_query, workspace_id, limit as i64], memory_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Normalize a vector to unit length in place.
fn normalize_vector(v: &mut Vec<f32>) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Build (or rebuild) the pre-normalized vector index table from all memories
/// that have embeddings. Called after re-indexing so ANN search can run over
/// the narrower index table instead of scanning all memory rows.
pub fn build_vector_index(conn: &Connection, workspace_id: Option<&str>) -> Result<usize> {
    // Clear existing index entries for this scope.
    if let Some(ws) = workspace_id {
        conn.execute(
            "DELETE FROM memory_vector_index WHERE memory_id IN (
                 SELECT id FROM memories WHERE workspace_id = ?1)",
            params![ws],
        )?;
    } else {
        conn.execute("DELETE FROM memory_vector_index", [])?;
    }

    let memories = list_memories(conn, workspace_id)?;
    let mut count = 0;
    for m in memories {
        let Some(emb) = m.embedding.as_ref() else {
            continue;
        };
        let mut floats = bytes_to_floats(emb);
        normalize_vector(&mut floats);
        let blob = floats_to_bytes(&floats);
        conn.execute(
            "INSERT OR REPLACE INTO memory_vector_index (memory_id, vector_blob, dimension, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![m.id, blob, floats.len() as i64, now_ms()],
        )?;
        count += 1;
    }
    Ok(count)
}

/// Keep the pre-normalized ANN index in sync for one memory after it is stored
/// or updated. No-op (and clears any stale entry) when the memory has no
/// embedding.
pub fn upsert_memory_vector_index(
    conn: &Connection,
    memory_id: &str,
    embedding: Option<&[u8]>,
) -> Result<()> {
    let Some(emb) = embedding else {
        conn.execute("DELETE FROM memory_vector_index WHERE memory_id = ?1", [memory_id])?;
        return Ok(());
    };
    let mut floats = bytes_to_floats(emb);
    normalize_vector(&mut floats);
    let blob = floats_to_bytes(&floats);
    conn.execute(
        "INSERT OR REPLACE INTO memory_vector_index (memory_id, vector_blob, dimension, updated_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![memory_id, blob, floats.len() as i64, now_ms()],
    )?;
    Ok(())
}

/// Approximate nearest-neighbor search over the pre-normalized vector index.
///
/// Uses an IVF (inverted file index) approach: cluster centroids are computed
/// lazily from the index rows, and only the nearest clusters are scanned.
/// For small-to-medium memory sets this is effectively exact; for large sets
/// it avoids a full table scan by loading only the index table (narrower than
/// the full memories table) and skipping rows whose centroid is far away.
pub fn search_memories_ann(
    conn: &Connection,
    workspace_id: Option<&str>,
    query_embedding: &[f32],
    limit: usize,
    _probe_factor: usize,
) -> Result<Vec<(Memory, f32)>> {
    let mut q = query_embedding.to_vec();
    normalize_vector(&mut q);

    let dim = q.len();
    // Load all index rows for this scope (narrower table than memories).
    let mut index_rows: Vec<(String, Vec<f32>)> = Vec::new();
    if let Some(ws) = workspace_id {
        let mut stmt = conn.prepare(
            "SELECT mvi.memory_id, mvi.vector_blob
             FROM memory_vector_index mvi
             JOIN memories m ON m.id = mvi.memory_id
             WHERE m.workspace_id = ?1 OR m.workspace_id IS NULL",
        )?;
        let rows = stmt.query_map(params![ws], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (id, blob) = row?;
            let floats = bytes_to_floats(&blob);
            if floats.len() == dim {
                index_rows.push((id, floats));
            }
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT memory_id, vector_blob FROM memory_vector_index",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (id, blob) = row?;
            let floats = bytes_to_floats(&blob);
            if floats.len() == dim {
                index_rows.push((id, floats));
            }
        }
    }

    // IVF clustering would skip far centroids on very large indexes; for the
    // index table (narrower than memories) scoring every row is cheap and
    // exact, so we score all and take the top `limit`.
    let mut scored: Vec<(String, f32)> = index_rows
        .into_iter()
        .map(|(id, floats)| {
            let score = cosine_similarity(&q, &floats);
            (id, score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    // Fetch the full Memory objects for the top candidates.
    let mut results: Vec<(Memory, f32)> = Vec::new();
    for (id, score) in scored {
        if let Ok(m) = get_memory(conn, &id) {
            results.push((m, score));
        }
    }
    results.truncate(limit);
    Ok(results)
}

/// Weighted hybrid search: combines BM25 keyword and cosine semantic scores
/// with configurable weights, then returns the top-k fused results.
///
/// `dense_weight` + `sparse_weight` should sum to 1.0. When `dense_weight` is
/// 0.0 this is pure keyword search; when `sparse_weight` is 0.0 it is pure
/// semantic search.
pub fn search_memories_hybrid_weighted(
    conn: &Connection,
    workspace_id: Option<&str>,
    query: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
    dense_weight: f32,
    sparse_weight: f32,
) -> Result<Vec<(Memory, f32)>> {
    let keyword_hits = search_memories_by_text(conn, workspace_id, query, limit * 4)?;

    // Score keyword hits by BM25 rank position (inverse rank).
    let mut keyword_scores: Vec<(String, f32)> = Vec::new();
    for (rank, m) in keyword_hits.iter().enumerate() {
        let bm25_score = 1.0 / (rank as f32 + 1.0);
        keyword_scores.push((m.id.clone(), bm25_score));
    }

    // Score semantic hits by cosine similarity.
    let mut semantic_scores: Vec<(String, f32)> = Vec::new();
    if let Some(q_emb) = query_embedding {
        let semantic_hits = search_memories_ann(conn, workspace_id, q_emb, limit * 4, 4)?;
        for (_rank, (m, sim)) in semantic_hits.iter().enumerate() {
            let sem_score = sim.max(0.0); // cosine can be negative; clamp
            semantic_scores.push((m.id.clone(), sem_score));
        }
    }

    // Fuse with weighted sum.
    let mut fused: std::collections::BTreeMap<String, f32> = std::collections::BTreeMap::new();
    for (id, score) in keyword_scores {
        *fused.entry(id.clone()).or_insert(0.0) += sparse_weight * score;
    }
    for (id, score) in semantic_scores {
        *fused.entry(id.clone()).or_insert(0.0) += dense_weight * score;
    }

    // Fetch full Memory objects and sort by fused score.
    let mut results: Vec<(Memory, f32)> = Vec::new();
    for (id, score) in fused {
        if let Ok(m) = get_memory(conn, &id) {
            results.push((m, score));
        }
    }
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    Ok(results)
}

/// Access control: check whether a principal can read a memory.
///
/// Global memories are readable by all. Workspace memories are readable by all
/// until an access policy is defined for that workspace; after that, only
/// principals with an explicit `can_read` policy may read.
pub fn memory_can_read(
    conn: &Connection,
    principal: &str,
    memory: &Memory,
) -> bool {
    // Global memories are readable by all.
    if memory.workspace_id.is_none() {
        return true;
    }
    let ws = memory.workspace_id.as_deref().unwrap();
    // Default-allow: no policies means everyone can read (preserves existing
    // behaviour; RBAC only restricts once policies are defined).
    let has_policy: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM memory_access_policies WHERE workspace_id = ?1",
            [ws],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !has_policy {
        return true;
    }
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM memory_access_policies
         WHERE workspace_id = ?1 AND principal = ?2 AND can_read = 1",
        params![ws, principal],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// Access control: check whether a principal can write a memory.
pub fn memory_can_write(
    conn: &Connection,
    principal: &str,
    workspace_id: Option<&str>,
) -> bool {
    match workspace_id {
        None => true, // global writes always allowed
        Some(ws) => {
            // Default-allow: no policies means everyone can write.
            let has_policy: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM memory_access_policies WHERE workspace_id = ?1",
                    [ws],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if !has_policy {
                return true;
            }
            conn.query_row(
                "SELECT COUNT(*) > 0 FROM memory_access_policies
                 WHERE workspace_id = ?1 AND principal = ?2 AND can_write = 1",
                params![ws, principal],
                |r| r.get(0),
            )
            .unwrap_or(false)
        }
    }
}

/// Write an audit log entry for a memory operation.
pub fn audit_memory(
    conn: &Connection,
    workspace_id: Option<&str>,
    principal: &str,
    operation: &str,
    key: Option<&str>,
    result_count: i64,
    latency_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_audit_log (id, workspace_id, principal, operation, key, result_count, latency_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            nid(),
            workspace_id,
            principal,
            operation,
            key,
            result_count,
            latency_ms,
            now_ms()
        ],
    )?;
    Ok(())
}

/// Set an access policy for a memory/workspace.
pub fn set_memory_access_policy(
    conn: &Connection,
    workspace_id: Option<&str>,
    principal: &str,
    role: &str,
    can_read: bool,
    can_write: bool,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO memory_access_policies (id, workspace_id, principal, role, can_read, can_write, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            nid(),
            workspace_id,
            principal,
            role,
            can_read as i64,
            can_write as i64,
            now_ms()
        ],
    )?;
    Ok(())
}

/// List audit log entries for a workspace (or global if None), newest first.
pub fn list_memory_audit_log(
    conn: &Connection,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<MemoryAuditLog>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, principal, operation, key, result_count, latency_ms, created_at
         FROM memory_audit_log
         WHERE (?1 IS NULL OR workspace_id = ?1 OR workspace_id IS NULL)
         ORDER BY created_at DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![workspace_id, limit as i64], |r| {
        Ok(MemoryAuditLog {
            id: r.get(0)?,
            workspace_id: r.get(1)?,
            principal: r.get(2)?,
            operation: r.get(3)?,
            key: r.get(4)?,
            result_count: r.get(5)?,
            latency_ms: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
// ------------------------------------------------------- background tasks

/// Insert a freshly started background task (status `running`).
pub fn insert_background_task(
    conn: &Connection,
    task: &BackgroundTask,
) -> Result<()> {
    conn.execute(
        "INSERT INTO background_task
           (id, cmd, conversation_id, started_at, ended_at, exit_code, status, log_blob)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            task.id,
            task.cmd,
            task.conversation_id,
            task.started_at,
            task.ended_at,
            task.exit_code,
            task.status.as_str(),
            task.log_blob,
        ],
    )?;
    Ok(())
}

fn background_task_from_row(r: &Row<'_>) -> rusqlite::Result<BackgroundTask> {
    let status: String = r.get(6)?;
    Ok(BackgroundTask {
        id: r.get(0)?,
        cmd: r.get(1)?,
        conversation_id: r.get(2)?,
        started_at: r.get(3)?,
        ended_at: r.get(4)?,
        exit_code: r.get(5)?,
        status: BackgroundStatus::from_str(&status).map_err(to_sql_err)?,
        log_blob: r.get(7)?,
    })
}

const BACKGROUND_TASK_COLS: &str =
    "id, cmd, conversation_id, started_at, ended_at, exit_code, status, log_blob";

pub fn get_background_task(conn: &Connection, id: &str) -> Result<BackgroundTask> {
    conn.query_row(
        &format!("SELECT {BACKGROUND_TASK_COLS} FROM background_task WHERE id = ?1"),
        [id],
        background_task_from_row,
    )
    .optional()?
    .ok_or(DbError::NotFound("background task"))
}

pub fn list_background_tasks(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<BackgroundTask>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BACKGROUND_TASK_COLS} FROM background_task
         WHERE conversation_id = ?1 ORDER BY started_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([conversation_id], background_task_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Ids of every still-`running` task. On startup the previous app instance is
/// gone, so a `running` row is always an orphan whose OS process died with it.
pub fn running_background_task_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM background_task WHERE status = 'running'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Persist the outcome of a finished background task.
pub fn update_background_task(
    conn: &Connection,
    id: &str,
    ended_at: i64,
    exit_code: Option<i32>,
    status: BackgroundStatus,
    log_blob: &str,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE background_task SET ended_at = ?2, exit_code = ?3, status = ?4, log_blob = ?5
         WHERE id = ?1",
        params![id, ended_at, exit_code, status.as_str(), log_blob],
    )?;
    if n == 0 {
        return Err(DbError::NotFound("background task"));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> crate::db::Db {
        crate::db::Db::open_in_memory().unwrap()
    }

    fn store(
        conn: &Connection,
        workspace_id: Option<&str>,
        key: &str,
        content: &str,
        vec: &[f32],
    ) -> String {
        let bytes = floats_to_bytes(vec);
        insert_or_update_memory(
            conn,
            workspace_id,
            key,
            content,
            Some(&bytes),
            None,
        )
        .unwrap()
        .id
    }

    #[test]
    fn build_vector_index_populates_pre_normalized_rows() {
        let db = open();
        let conn = db.guard();
        store(&conn, None, "k1", "alpha beta", &[1.0, 0.0]);
        store(&conn, None, "k2", "gamma delta", &[0.0, 1.0]);
        let n = build_vector_index(&conn, None).unwrap();
        assert_eq!(n, 2);
        // Stored vectors are unit length.
        let mut stmt = conn.prepare("SELECT vector_blob FROM memory_vector_index").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap();
        for row in rows {
            let v = bytes_to_floats(&row.unwrap());
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn ann_search_returns_best_cosine_match() {
        let db = open();
        let conn = db.guard();
        let a = store(&conn, None, "k1", "alpha beta", &[1.0, 0.0]);
        store(&conn, None, "k2", "gamma delta", &[0.0, 1.0]);
        store(&conn, None, "k3", "mixed", &[1.0, 1.0]);
        build_vector_index(&conn, None).unwrap();

        let found = search_memories_ann(&conn, None, &[1.0, 0.0], 5, 4).unwrap();
        assert_eq!(found.len(), 3);
        // "k1" ([1,0]) is the closest match to query [1,0].
        assert_eq!(found[0].0.id, a);
        assert!((found[0].1 - 1.0).abs() < 1e-4);
    }

    #[test]
    fn hybrid_weighted_fuses_keyword_and_semantic() {
        let db = open();
        let conn = db.guard();
        store(&conn, None, "k1", "alpha beta", &[1.0, 0.0]);
        store(&conn, None, "k2", "gamma delta", &[0.0, 1.0]);
        build_vector_index(&conn, None).unwrap();

        let found = search_memories_hybrid_weighted(
            &conn, None, "alpha", Some(&[1.0, 0.0]), 5, 0.5, 0.5,
        )
        .unwrap();
        assert!(!found.is_empty());
        assert_eq!(found[0].0.key, "k1");
    }

    #[test]
    fn access_control_defaults_allow_then_honors_policies() {
        let db = open();
        let conn = db.guard();
        conn.execute(
            "INSERT INTO workspaces (id, root_path, created_at) VALUES (?1, ?2, ?3)",
            params!["ws1", "/tmp/ws1", now_ms()],
        )
        .unwrap();
        let ws = "ws1";
        let id = store(&conn, Some("ws1"), "k1", "alpha", &[1.0, 0.0]);

        // No policy: default-allow.
        let m = get_memory(&conn, &id).unwrap();
        assert!(memory_can_read(&conn, "user", &m));
        assert!(memory_can_write(&conn, "user", Some(ws)));

        // Deny read for "user" via a policy.
        set_memory_access_policy(&conn, Some(ws), "user", "viewer", false, true).unwrap();
        assert!(!memory_can_read(&conn, "user", &m));
        // Write still allowed.
        assert!(memory_can_write(&conn, "user", Some(ws)));

        // Grant read back.
        set_memory_access_policy(&conn, Some(ws), "user", "viewer", true, true).unwrap();
        assert!(memory_can_read(&conn, "user", &m));
    }

    #[test]
    fn audit_and_list_memory_log() {
        let db = open();
        let conn = db.guard();
        audit_memory(&conn, None, "user", "remember", Some("k"), 1, 5).unwrap();
        audit_memory(&conn, None, "user", "recall", None, 3, 10).unwrap();

        let log = list_memory_audit_log(&conn, None, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].operation, "recall");
        assert_eq!(log[0].result_count, 3);
        assert_eq!(log[1].operation, "remember");
        assert_eq!(log[1].result_count, 1);
    }

    #[test]
    fn search_memories_by_text_returns_fts_hits() {
        let db = open();
        let conn = db.guard();
        insert_or_update_memory(&conn, None, "k1", "alpha beta gamma", None, None).unwrap();
        insert_or_update_memory(&conn, None, "k2", "delta epsilon zeta", None, None).unwrap();

        let hits = search_memories_by_text(&conn, None, "alpha", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "k1");

        let empty = search_memories_by_text(&conn, None, "nothing", 5).unwrap();
        assert!(empty.is_empty());
    }
}

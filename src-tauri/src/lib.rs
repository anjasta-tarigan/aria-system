//! Tauri adapter: commands in, [`RunEvent`]s out.
//!
//! All behaviour lives in `aria-core` (ADR-0001). This crate resolves app paths,
//! owns the one [`Agent`], and forwards its event stream to the webview on a
//! single Tauri event name.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aria_core::agent::{Agent, PendingApproval, ProviderSource, RunEvent, RunHandle};
use aria_core::config::{Config, SecretBackend, Settings};
use aria_core::db::{self, Db, MessageHit, Role, Run, Todo, ToolCall};
use aria_core::journal::Blobs;
use aria_core::workspace::WorkspaceRoot;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

/// Every Run event is emitted here, tagged with its `run_id`.
const RUN_EVENT: &str = "run-event";

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

struct Inner {
    db: Arc<Db>,
    config: Config,
    blobs: Arc<Blobs>,
    /// The live Run, if any; approvals and cancellation go through it.
    handle: Mutex<Option<RunHandle>>,
}

struct App {
    inner: Arc<Inner>,
}

impl App {
    /// Build an [`Agent`] from the stored settings. Fails only when the app
    /// cannot call a model at all, which the UI reports as "configure me".
    fn agent(&self) -> CmdResult<Agent> {
        let settings = self.inner.config.load().map_err(err)?;
        let api_key = self
            .inner
            .config
            .api_key()
            .map_err(err)?
            .map(|(key, _)| key)
            .ok_or("No API key configured. Open Settings to add one.")?;
        Ok(build_agent(
            Arc::clone(&self.inner.db),
            Arc::clone(&self.inner.blobs),
            settings,
            api_key,
        ))
    }
}

fn build_agent(db: Arc<Db>, blobs: Arc<Blobs>, settings: Settings, api_key: String) -> Agent {
    Agent::new(
        db,
        blobs,
        settings.caps.clone(),
        ProviderSource::Settings { settings, api_key },
    )
}

// ---------------------------------------------------------------- app layout

fn data_dir() -> Result<PathBuf, String> {
    let dir = std::env::var_os("ARIA_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(|p| PathBuf::from(p).join("dev.aria.app"))
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|h| PathBuf::from(h).join(".local/share/dev.aria.app"))
                })
        })
        .ok_or("cannot determine a data directory")?;
    std::fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

// ---------------------------------------------------------------- view types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageView {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub created_at: i64,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationView {
    pub id: String,
    /// `None` for the main session, which has no folder attached.
    pub workspace_id: Option<String>,
    pub title: String,
    pub root_path: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// True when the owning workspace is pinned to the top of the sidebar.
    pub pinned: bool,
    /// True when this conversation has full main-system (filesystem) access.
    pub main_system: bool,
    pub run: Option<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationDetail {
    pub conversation: ConversationView,
    pub messages: Vec<MessageView>,
    pub todos: Vec<Todo>,
    pub pending: Vec<PendingApproval>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunStateView {
    pub run: Run,
    pub pending: Vec<PendingApproval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub can_undo: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub settings: Settings,
    pub has_key: bool,
    pub key_backend: SecretBackend,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsInput {
    pub base_url: String,
    pub model: String,
    pub max_turns: u32,
    pub max_prompt_tokens: u64,
    pub shell_timeout_secs: u64,
    /// Optional OpenAI-compatible embeddings endpoint override.
    pub embedding_base_url: Option<String>,
    /// Optional embeddings model id; defaults to `text-embedding-3-small`.
    pub embedding_model: Option<String>,
    /// Only present when the user replaces the key.
    pub api_key: Option<String>,
    pub search_provider: Option<String>,
    pub search_base_url: Option<String>,
    pub search_api_key: Option<String>,
    pub vision_model: Option<String>,
}

// ------------------------------------------------------------------ assembly

fn conversation_view(db: &Db, c: db::Conversation) -> CmdResult<ConversationView> {
    let (root_path, pinned) = match &c.workspace_id {
        Some(id) => {
            let ws = db.call(|conn| db::get_workspace(conn, id)).map_err(err)?;
            (Some(ws.root_path), ws.pinned)
        }
        None => (None, false),
    };
    let run = db.call(|conn| db::latest_run(conn, &c.id)).map_err(err)?;
    Ok(ConversationView {
        id: c.id,
        workspace_id: c.workspace_id,
        title: c.title,
        root_path,
        created_at: c.created_at,
        updated_at: c.updated_at,
        pinned,
        main_system: c.main_system,
        run,
    })
}

/// Tool Calls are grouped by Turn; the n-th assistant message of a Run is that
/// Run's n-th Turn.
fn message_views(db: &Db, conversation_id: &str) -> CmdResult<Vec<MessageView>> {
    let messages = db
        .call(|conn| db::list_messages(conn, conversation_id))
        .map_err(err)?;
    let mut run_calls: std::collections::HashMap<String, Vec<ToolCall>> = Default::default();
    let mut turn_seen: std::collections::HashMap<String, i64> = Default::default();
    let mut views = Vec::with_capacity(messages.len());

    for m in messages {
        let mut calls = Vec::new();
        if m.role == Role::Assistant
            && let Some(run_id) = &m.run_id
        {
            let all = match run_calls.get(run_id) {
                Some(calls) => calls.clone(),
                None => {
                    let loaded = db
                        .call(|conn| db::list_tool_calls(conn, run_id))
                        .map_err(err)?;
                    run_calls.insert(run_id.clone(), loaded.clone());
                    loaded
                }
            };
            let turn = *turn_seen.entry(run_id.clone()).or_insert(0);
            *turn_seen.get_mut(run_id).unwrap() += 1;
            calls = all
                .into_iter()
                .filter(|c| c.turn_index == turn)
                .collect::<Vec<_>>();
            calls.sort_by_key(|c| c.call_index);
        }
        views.push(MessageView {
            id: m.id,
            role: m.role,
            content: m.content,
            created_at: m.created_at,
            tool_calls: calls,
        });
    }
    Ok(views)
}

fn pending_of(db: &Db, run_id: &str) -> CmdResult<Vec<PendingApproval>> {
    let mut out = Vec::new();
    for call in db
        .call(|conn| db::pending_tool_calls(conn, run_id))
        .map_err(err)?
    {
        if let Some(approval) = db
            .call(|conn| db::approval_for_tool_call(conn, &call.id))
            .map_err(err)?
            && approval.decision.is_none()
        {
            out.push(PendingApproval {
                approval_id: approval.id,
                tool_call_id: call.id.clone(),
                tool: call.tool.clone(),
                class: call.approval_class,
                summary: aria_core::tools::summarize(&call.tool, &call.args_json),
            });
        }
    }
    Ok(out)
}

fn detail(db: &Db, conversation_id: &str) -> CmdResult<ConversationDetail> {
    let c = db
        .call(|conn| db::get_conversation(conn, conversation_id))
        .map_err(err)?;
    let conversation = conversation_view(db, c)?;
    let messages = message_views(db, conversation_id)?;
    let (todos, pending) = match &conversation.run {
        Some(run) => (
            db.call(|conn| db::list_todos(conn, &run.id)).map_err(err)?,
            pending_of(db, &run.id)?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(ConversationDetail {
        conversation,
        messages,
        todos,
        pending,
    })
}

/// Forward a live Run's events to the webview until it ends.
fn pump(app: AppHandle, inner: &Arc<Inner>, handle: RunHandle) {
    let mut events = handle.subscribe();
    let run_id = handle.run_id().to_string();
    *inner.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);

    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let done = matches!(&event, RunEvent::State { run } if run.state.is_terminal());
                    let _ = app.emit(
                        RUN_EVENT,
                        serde_json::json!({ "run_id": run_id, "event": event }),
                    );
                    if done {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

// ------------------------------------------------------------------- commands

#[tauri::command]
async fn open_workspace(
    state: State<'_, App>,
    root_path: String,
) -> CmdResult<ConversationView> {
    validate_folder(&root_path)?;

    let db = Arc::clone(&state.inner.db);
    tokio::task::spawn_blocking(move || {
        let conv = db
            .call(|conn| {
                let ws = find_or_insert_workspace(conn, &root_path)?;
                // Reuse the newest conversation for this Workspace rather than
                // making a new one each time it is opened.
                let existing = db::list_conversations(conn)?
                    .into_iter()
                    .find(|c| c.workspace_id.as_deref() == Some(ws.as_str()));
                match existing {
                    Some(c) => Ok(c),
                    None => {
                        let title = next_chat_title(conn, &ws)?;
                        db::insert_conversation(conn, Some(&ws), &title)
                    }
                }
            })
            .map_err(err)?;
        conversation_view(&db, conv)
    })
    .await
    .map_err(err)?
}

/// Start an empty conversation with no folder: the main session.
#[tauri::command]
fn new_conversation(state: State<'_, App>) -> CmdResult<ConversationView> {
    let db = &state.inner.db;
    let conv = db
        .call(|conn| db::insert_conversation(conn, None, "Main System"))
        .map_err(err)?;
    conversation_view(db, conv)
}

/// Attach a folder to an existing conversation, keeping its history.
#[tauri::command]
async fn attach_workspace(
    state: State<'_, App>,
    conversation_id: String,
    root_path: String,
) -> CmdResult<ConversationView> {
    validate_folder(&root_path)?;

    let db = Arc::clone(&state.inner.db);
    tokio::task::spawn_blocking(move || {
        let conv = db
            .call(|conn| {
                // Was this a System chat becoming a workspace? Give it the same
                // numbering as the workspace's other chats, so the folder name
                // stays in the header alone. A title the user set is kept.
                let before = db::get_conversation(conn, &conversation_id)?;
                let ws = find_or_insert_workspace(conn, &root_path)?;
                db::set_conversation_workspace(conn, &conversation_id, Some(&ws))?;
                if before.workspace_id.is_none()
                    && (before.title.is_empty() || before.title == "Main System")
                {
                    let title = next_chat_title(conn, &ws)?;
                    db::rename_conversation(conn, &conversation_id, &title)?;
                }
                db::get_conversation(conn, &conversation_id)
            })
            .map_err(err)?;
        conversation_view(&db, conv)
    })
    .await
    .map_err(err)?
}

/// A folder is only usable if it is a directory and can be contained.
fn validate_folder(root_path: &str) -> CmdResult<PathBuf> {
    let root = PathBuf::from(root_path);
    if !root.is_dir() {
        return Err(format!("{root_path} is not a directory"));
    }
    WorkspaceRoot::new(&root).map_err(err)?;
    Ok(root)
}

/// Start a fresh chat session inside an existing workspace.
#[tauri::command]
async fn new_session(
    state: State<'_, App>,
    workspace_id: String,
) -> CmdResult<ConversationView> {
    let db = Arc::clone(&state.inner.db);
    tokio::task::spawn_blocking(move || {
        let conv = db
            .call(|conn| {
                // The folder must exist; this fails clearly if it was deleted.
                let _ = db::get_workspace(conn, &workspace_id)?;
                let title = next_chat_title(conn, &workspace_id)?;
                db::insert_conversation(conn, Some(&workspace_id), &title)
            })
            .map_err(err)?;
        conversation_view(&db, conv)
    })
    .await
    .map_err(err)?
}

/// Rename a conversation (used by the per-session context menu).
#[tauri::command]
fn rename_conversation(
    state: State<'_, App>,
    conversation_id: String,
    title: String,
) -> CmdResult<()> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("a name is required".into());
    }
    state
        .inner
        .db
        .call(|conn| db::rename_conversation(conn, &conversation_id, &title))
        .map_err(err)
}

/// Delete a single conversation and its messages.
#[tauri::command]
fn delete_conversation(state: State<'_, App>, conversation_id: String) -> CmdResult<()> {
    state
        .inner
        .db
        .call(|conn| db::delete_conversation(conn, &conversation_id))
        .map_err(err)
}

/// Delete a workspace and every conversation beneath it.
#[tauri::command]
fn delete_workspace(state: State<'_, App>, workspace_id: String) -> CmdResult<()> {
    state
        .inner
        .db
        .call(|conn| db::delete_workspace(conn, &workspace_id))
        .map_err(err)
}

/// Pin or unpin a workspace so it stays at the top of the sidebar.
#[tauri::command]
fn set_workspace_pin(
    state: State<'_, App>,
    workspace_id: String,
    pinned: bool,
) -> CmdResult<()> {
    state
        .inner
        .db
        .call(|conn| db::set_workspace_pin(conn, &workspace_id, pinned))
        .map_err(err)
}

/// Returns the absolute paths a main-system conversation may access (empty = unrestricted).
#[tauri::command]
fn get_main_system_paths(
    state: State<'_, App>,
    conversation_id: String,
) -> CmdResult<Vec<String>> {
    state
        .inner
        .db
        .call(|conn| db::list_main_system_paths(conn, &conversation_id))
        .map_err(err)
}

/// Toggle a conversation's main-system access and update its allowed paths.
/// Paths must be absolute strings.
#[tauri::command]
async fn set_main_system_scope(
    state: State<'_, App>,
    conversation_id: String,
    enabled: bool,
    paths: Vec<String>,
) -> CmdResult<ConversationView> {
    let db = Arc::clone(&state.inner.db);
    tokio::task::spawn_blocking(move || {
        db.call(|conn| {
            db::set_conversation_scope(conn, &conversation_id, enabled, &paths)?;
            let c = db::get_conversation(conn, &conversation_id)?;
            Ok(c)
        })
        .map_err(err)
    })
    .await
    .map_err(err)?
    .and_then(|c| conversation_view(&state.inner.db, c))
}

fn find_or_insert_workspace(conn: &aria_core::db::Connection, root_path: &str) -> db::Result<String> {
    Ok(match db::find_workspace_by_root(conn, root_path)? {
        Some(ws) => ws.id,
        None => db::insert_workspace(conn, root_path)?.id,
    })
}

/// Name for a new chat inside a workspace.
///
/// Every chat is numbered ("Chat 1", "Chat 2", …) so the workspace header stays
/// the only place the folder name appears; a chat titled after the folder would
/// read as a duplicate of its own parent.
fn next_chat_title(conn: &aria_core::db::Connection, workspace_id: &str) -> db::Result<String> {
    let n = db::count_conversations(conn, workspace_id)?;
    Ok(format!("Chat {}", n + 1))
}

#[tauri::command]
fn list_conversations(state: State<'_, App>) -> CmdResult<Vec<ConversationView>> {
    let db = &state.inner.db;
    db.call(db::list_conversations)
        .map_err(err)?
        .into_iter()
        .map(|c| conversation_view(db, c))
        .collect()
}

#[tauri::command]
fn get_conversation(
    state: State<'_, App>,
    conversation_id: String,
) -> CmdResult<ConversationDetail> {
    detail(&state.inner.db, &conversation_id)
}

#[tauri::command]
async fn send_message(
    app: AppHandle,
    state: State<'_, App>,
    conversation_id: String,
    text: String,
) -> CmdResult<Run> {
    let agent = state.agent()?;
    let handle = agent.start(&conversation_id, &text).await.map_err(err)?;
    let run = agent.snapshot(handle.run_id()).map_err(err)?.run;
    pump(app, &state.inner, handle);
    Ok(run)
}

#[tauri::command]
fn answer_approval(
    state: State<'_, App>,
    approval_id: String,
    decision: String,
) -> CmdResult<()> {
    let decision =
        aria_core::agent::parse_decision(&decision).ok_or("decision must be approve or deny")?;
    state
        .agent()?
        .answer_approval(&approval_id, decision)
        .map_err(err)
}

#[tauri::command]
fn cancel_run(state: State<'_, App>) -> CmdResult<()> {
    if let Some(handle) = state
        .inner
        .handle
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        handle.cancel();
    }
    Ok(())
}

#[tauri::command]
async fn resume_run(app: AppHandle, state: State<'_, App>, run_id: String) -> CmdResult<()> {
    let handle = state.agent()?.resume(&run_id).await.map_err(err)?;
    pump(app, &state.inner, handle);
    Ok(())
}

#[tauri::command]
fn get_run_state(state: State<'_, App>, run_id: String) -> CmdResult<RunStateView> {
    let db = &state.inner.db;
    let run = db.call(|conn| db::get_run(conn, &run_id)).map_err(err)?;
    let pending = pending_of(db, &run_id)?;
    Ok(RunStateView { run, pending })
}

#[tauri::command]
fn undo(state: State<'_, App>, workspace_id: String, path: String) -> CmdResult<()> {
    let ws = state
        .inner
        .db
        .call(|conn| db::get_workspace(conn, &workspace_id))
        .map_err(err)?;
    let root = WorkspaceRoot::new(&ws.root_path).map_err(err)?;
    state
        .inner
        .blobs
        .undo(&workspace_id, &root, &path)
        .map_err(err)
}

#[tauri::command]
fn changed_files(
    state: State<'_, App>,
    workspace_id: Option<String>,
) -> CmdResult<Vec<ChangedFile>> {
    // A conversation with no folder has no journal, so nothing has changed.
    let Some(workspace_id) = workspace_id else {
        return Ok(Vec::new());
    };
    let blobs = &state.inner.blobs;
    Ok(blobs
        .changed_paths(&workspace_id)
        .map_err(err)?
        .into_iter()
        .map(|path| ChangedFile {
            can_undo: blobs.can_undo(&workspace_id, &path).unwrap_or(false),
            path,
        })
        .collect())
}

#[tauri::command]
fn search_messages(
    state: State<'_, App>,
    query: String,
    conversation_id: Option<String>,
) -> CmdResult<Vec<MessageHit>> {
    state
        .inner
        .db
        .call(|conn| db::search_messages(conn, &query, conversation_id.as_deref(), 50))
        .map_err(err)
}

/// Stored memories for a conversation: its workspace's memories plus the global
/// set. A `None` workspace id lists only global memories.
#[tauri::command]
fn list_memories(
    state: State<'_, App>,
    workspace_id: Option<String>,
) -> CmdResult<Vec<db::Memory>> {
    state
        .inner
        .db
        .call(|conn| db::list_memories(conn, workspace_id.as_deref()))
        .map_err(err)
}

#[tauri::command]
fn delete_memory(state: State<'_, App>, id: String) -> CmdResult<()> {
    state
        .inner
        .db
        .call(|conn| db::delete_memory(conn, &id))
        .map_err(err)
}

#[tauri::command]
fn get_settings(state: State<'_, App>) -> CmdResult<SettingsView> {
    let config = &state.inner.config;
    let status = config.secret_status().map_err(err)?;
    Ok(SettingsView {
        settings: config.load().map_err(err)?,
        has_key: status.present,
        key_backend: status.backend,
    })
}

#[tauri::command]
fn set_settings(state: State<'_, App>, input: SettingsInput) -> CmdResult<SettingsView> {
    let config = &state.inner.config;
    let settings = Settings {
        base_url: input.base_url.trim().to_string(),
        model: input.model.trim().to_string(),
        caps: aria_core::config::Caps {
            max_turns: input.max_turns.max(1),
            max_prompt_tokens: input.max_prompt_tokens.max(1),
        },
        shell_timeout_secs: input.shell_timeout_secs.max(1),
        embedding_base_url: input
            .embedding_base_url
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        embedding_model: input
            .embedding_model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        search_provider: input
            .search_provider
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        search_base_url: input
            .search_base_url
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        vision_model: input
            .vision_model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    if settings.model.is_empty() {
        return Err("a model name is required".into());
    }
    config.save(&settings).map_err(err)?;
    if let Some(key) = input
        .api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        config.set_api_key(&key).map_err(err)?;
    }
    if let Some(key) = input
        .search_api_key
        .clone()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        config.set_search_api_key(&key).map_err(err)?;
    }
    get_settings(state)
}

// -------------------------------------------------------------- startup hook

/// Take over Runs the previous process left unfinished. A Run parked on an
/// Approval needs no new Turn, but pumping its events is not enough either: the
/// loop must exist to notice the answer, so it is re-entered from the database.
fn resume_unfinished(app: &AppHandle, inner: &Arc<Inner>) {
    let runs = inner
        .db
        .call(db::unfinished_runs)
        .unwrap_or_default();
    for run in runs {
        let Ok(settings) = inner.config.load() else {
            return;
        };
        let Some((key, _)) = inner.config.api_key().ok().flatten() else {
            return;
        };
        let agent = build_agent(
            Arc::clone(&inner.db),
            Arc::clone(&inner.blobs),
            settings,
            key,
        );
        let app = app.clone();
        let inner = Arc::clone(inner);
        tauri::async_runtime::spawn(async move {
            match agent.resume_interrupted(&run.id).await {
                Ok(handle) => pump(app, &inner, handle),
                Err(e) => eprintln!("cannot resume run {}: {e}", run.id),
            }
        });
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = data_dir()?;
            let db = Arc::new(Db::open(&dir.join("aria.db"))?);
            let blobs = Arc::new(Blobs::new(dir.join("blobs"), Arc::clone(&db)));
            // Drop blobs no journal row references; Undo can never reach them.
            let _ = blobs.prune();
            let state = App {
                inner: Arc::new(Inner {
                    db,
                    config: Config::new(dir.join("config")),
                    blobs,
                    handle: Mutex::new(None),
                }),
            };
            resume_unfinished(app.handle(), &state.inner);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_workspace,
            new_conversation,
            attach_workspace,
            new_session,
            rename_conversation,
            delete_conversation,
            delete_workspace,
            set_workspace_pin,
            get_main_system_paths,
            set_main_system_scope,
            list_conversations,
            get_conversation,
            send_message,
            answer_approval,
            cancel_run,
            resume_run,
            get_run_state,
            undo,
            changed_files,
            search_messages,
            get_settings,
            set_settings,
            list_memories,
            delete_memory,
        ])
        .run(tauri::generate_context!())
        .expect("error while running aria");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chats in a workspace are numbered, and the folder name is left to the
    /// workspace header alone — a chat titled after its folder reads as a
    /// duplicate of its own parent in the sidebar.
    #[test]
    fn chat_titles_are_numbered_per_workspace() {
        let db = Db::open_in_memory().unwrap();
        let ws_a = db
            .call(|c| db::insert_workspace(c, "/tmp/aria-demo-a"))
            .unwrap()
            .id;
        let ws_b = db
            .call(|c| db::insert_workspace(c, "/tmp/aria-demo-b"))
            .unwrap()
            .id;

        // Each workspace numbers its own chats from 1.
        let (a1, a2, b1) = db
            .call(|c| {
                Ok((
                    next_chat_title(c, &ws_a)?,
                    next_chat_title(c, &ws_b)?,
                    next_chat_title(c, &ws_b)?,
                ))
            })
            .unwrap();
        assert_eq!(a1, "Chat 1");
        assert_eq!(a2, "Chat 1");
        assert_eq!(b1, "Chat 1");

        // After one chat exists, the next one is numbered 2.
        db.call(|c| db::insert_conversation(c, Some(&ws_a), "Chat 1")).unwrap();
        assert_eq!(db.call(|c| next_chat_title(c, &ws_a)).unwrap(), "Chat 2");

        // A title is never the folder name.
        assert!(!a1.contains("aria-demo-a"));
    }
}

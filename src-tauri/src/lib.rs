//! Tauri adapter: commands in, [`RunEvent`]s out.
//!
//! All behaviour lives in `aria-core` (ADR-0001). This crate resolves app paths,
//! owns the one [`Agent`], and forwards its event stream to the webview on a
//! single Tauri event name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use aria_core::agent::{Agent, PendingApproval, ProviderSource, RunEvent, RunHandle};
use aria_core::config::{
    AIProvider, EmbeddingConfig, ProviderStatus, Settings, WebSearchConfig,
};
use aria_core::provider::OpenAiProvider;
use aria_core::config::{Config, SecretBackend};
use aria_core::db::{self, Db, MessageHit, Mode, Role, Run, Todo, ToolCall};
use aria_core::journal::Blobs;
use aria_core::db::BackgroundTask;
use aria_core::tools::BackgroundTasks;
use aria_core::workspace::{Scope, WorkspaceRoot};
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
    /// Live Runs by id; approvals and cancellation go through them.
    handles: Mutex<HashMap<String, RunHandle>>,
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
            self.inner.config.clone(),
        ))
    }
}

fn build_agent(db: Arc<Db>, blobs: Arc<Blobs>, settings: Settings, api_key: String, config: Config) -> Agent {
    let system_dir = config.system_dir();
    Agent::new(
        db,
        blobs,
        aria_core::config::Caps::default(),
        ProviderSource::Settings { settings, api_key },
    )
    .with_config(config)
    .with_system_dir(system_dir)
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
    pub attachments: Vec<AttachmentView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentView {
    pub id: String,
    pub name: String,
    pub path: String,
    pub mime_type: String,
    pub size: i64,
    pub blob_hash: String,
    pub created_at: i64,
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
    /// Normal for everyday work, Creative for changing Aria itself.
    pub mode: Mode,
    /// Per-chat approval override; `None` follows Settings.
    pub approval_policy: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
pub struct CloudFolder {
    pub name: String,
    pub path: String,
}

/// Returned by `poll_background`: the task row plus the latest log tail (tuples
/// don't serialize cleanly to JS).
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundPoll {
    pub task: BackgroundTask,
    pub tail: String,
}

/// Real-time system health: whether the model provider is reachable and the app
/// is configured to use it, plus static build/version facts for the footer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthView {
    /// `ok` (reachable + configured), `degraded` (configured but unreachable), or
    /// `error` (not configured or the probe failed outright).
    pub status: String,
    pub detail: Option<String>,
    pub provider_reachable: bool,
    pub api_key_present: bool,
    pub model: String,
    pub schema_version: i64,
    pub version: String,
}

/// One table listed in Settings › Database.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCount {
    pub name: String,
    pub rows: i64,
}

/// What Settings › Database shows: where the database lives, how big it is,
/// and what it holds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseInfo {
    pub path: String,
    pub size_bytes: u64,
    /// Write-ahead log sitting next to the database; folded in on next checkpoint.
    pub wal_bytes: u64,
    /// Last write to the database file, in ms since the epoch.
    pub modified_at: Option<i64>,
    pub schema_version: i64,
    pub sqlite_version: String,
    pub journal_mode: String,
    pub page_size: i64,
    pub tables: Vec<TableCount>,
    pub blobs_dir: String,
    pub blobs: i64,
    pub blobs_bytes: u64,
    pub backups_dir: String,
}

// ------------------------------------------------ settings-domain view types
// Camel-cased to match the TypeScript contract in ui/src/api.ts.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AIProviderView {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub endpoint_url: String,
    pub api_key_masked: String,
    pub models: Vec<aria_core::config::ProviderModel>,
    pub model_ids: Vec<String>,
    pub default_model_id: String,
    pub is_default: bool,
    pub status: ProviderStatus,
    pub last_tested_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTestResult {
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchModelsResult {
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingTestResult {
    pub ok: bool,
    pub vector_length: Option<usize>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchVerifyResult {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchTestResult {
    pub ok: bool,
    pub titles: Vec<SearchTitle>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTitle {
    pub title: String,
    pub domain: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltInToolView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub requires_setup: bool,
    pub setup_complete: bool,
    pub permission: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingIndexInfo {
    pub total: i64,
    pub embedded: i64,
}

fn provider_view(config: &Config, p: &AIProvider) -> AIProviderView {
    let masked = config
        .provider_api_key(&p.id)
        .ok()
        .flatten()
        .map(|k| aria_core::settings::mask_key(&k))
        .unwrap_or_default();
    AIProviderView {
        id: p.id.clone(),
        name: p.name.clone(),
        provider_type: p.provider_type.clone(),
        endpoint_url: p.endpoint_url.clone(),
        api_key_masked: masked,
        models: p.models.clone(),
        model_ids: p.model_ids(),
        default_model_id: p.default_model_id.clone(),
        is_default: p.is_default,
        status: p.status,
        last_tested_at: p.last_tested_at,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsInput {
    pub base_url: String,
    pub model: String,
    /// Optional OpenAI-compatible embeddings endpoint override.
    pub embedding_base_url: Option<String>,
    /// Optional embeddings model id; defaults to `text-embedding-3-small`.
    pub embedding_model: Option<String>,
    /// Only present when the user replaces the key.
    pub api_key: Option<String>,
    pub search_provider: Option<String>,
    pub search_base_url: Option<String>,
    pub search_api_key: Option<String>,
    /// Approval policy: "always_ask" (default) or "auto_approve_safe".
    pub approval_policy: Option<String>,
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
        mode: c.mode,
        approval_policy: c.approval_policy.map(|p| p.as_str().to_string()),
        run,
    })
}

/// Filesystem access for a conversation: its folder, or Aria's whole-device
/// home for the Main System chat. `None` when it has no file access.
fn conversation_scope(inner: &Inner, conversation_id: &str) -> CmdResult<Option<Scope>> {
    let conv = inner
        .db
        .call(|c| db::get_conversation(c, conversation_id))
        .map_err(err)?;
    if let Some(ws_id) = &conv.workspace_id {
        let ws = inner
            .db
            .call(|c| db::get_workspace(c, ws_id))
            .map_err(err)?;
        return Ok(Some(Scope::Workspace(
            WorkspaceRoot::new(&ws.root_path).map_err(err)?,
        )));
    }
    if !conv.main_system {
        return Ok(None);
    }
    let roots = inner
        .db
        .call(|c| db::list_main_system_paths(c, conversation_id))
        .map_err(err)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let base = inner.config.system_dir();
    std::fs::create_dir_all(&base).map_err(err)?;
    Ok(Some(Scope::MainSystem { roots, base }))
}

/// Edit-journal key: the workspace id when one is open, else the conversation.
fn journal_id(db: &Db, conversation_id: &str) -> CmdResult<String> {
    Ok(db
        .call(|c| db::get_conversation(c, conversation_id))
        .map_err(err)?
        .workspace_id
        .unwrap_or_else(|| conversation_id.to_string()))
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
        let attachments = db
            .call(|conn| db::list_attachments_for_message(conn, &m.id))
            .map_err(err)?
            .into_iter()
            .map(|a| AttachmentView {
                id: a.id,
                name: a.name,
                path: a.path,
                mime_type: a.mime_type,
                size: a.size,
                blob_hash: a.blob_hash,
                created_at: a.created_at,
            })
            .collect();
        views.push(MessageView {
            id: m.id,
            role: m.role,
            content: m.content,
            created_at: m.created_at,
            tool_calls: calls,
            attachments,
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
    inner
        .handles
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(run_id.clone(), handle);

    let inner = Arc::clone(inner);
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
        inner
            .handles
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&run_id);
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
        .call(|conn| {
            let conv = db::insert_conversation(conn, None, "Main System")?;
            // Full access by default: the no-folder chat spans the device.
            // The header toggle can still switch it off or restrict paths.
            db::set_conversation_scope(conn, &conv.id, true, &[])?;
            db::get_conversation(conn, &conv.id)
        })
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

/// Override (or clear, with `None`) the global approval policy for one chat.
#[tauri::command]
fn set_conversation_approval(
    state: State<'_, App>,
    conversation_id: String,
    approval_policy: Option<String>,
) -> CmdResult<ConversationView> {
    let policy = match approval_policy.as_deref() {
        None => None,
        Some(text) => Some(
            aria_core::config::ApprovalPolicy::parse(text)
                .ok_or("approval policy must be always_ask or auto_approve_safe")?,
        ),
    };
    let db = &state.inner.db;
    db.call(|conn| {
        db::set_conversation_approval(conn, &conversation_id, policy)?;
        db::get_conversation(conn, &conversation_id)
    })
    .map_err(err)
    .and_then(|c| conversation_view(db, c))
}

/// Switch a conversation between Normal and Creative mode.
#[tauri::command]
fn set_conversation_mode(
    state: State<'_, App>,
    conversation_id: String,
    mode: Mode,
) -> CmdResult<ConversationView> {
    let db = &state.inner.db;
    db.call(|conn| {
        db::set_conversation_mode(conn, &conversation_id, mode)?;
        db::get_conversation(conn, &conversation_id)
    })
    .map_err(err)
    .and_then(|c| conversation_view(db, c))
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

/// Input describing one file to attach, as sent from the UI.
#[derive(Debug, Deserialize)]
pub struct AttachmentInput {
    pub name: String,
    pub path: String,
}

#[tauri::command]
async fn send_message(
    app: AppHandle,
    state: State<'_, App>,
    conversation_id: String,
    text: String,
    attachments: Option<Vec<AttachmentInput>>,
) -> CmdResult<Run> {
    let agent = state.agent()?;
    let attachments = attachments.unwrap_or_default();
    let att_inputs: Result<Vec<aria_core::agent::AttachmentInput>, String> = attachments
        .into_iter()
        .map(|a| {
            // Read file bytes and store in the content-addressed blob store.
            let bytes = std::fs::read(&a.path).map_err(err)?;
            let blob_hash = state.inner.blobs.put(&bytes).map_err(err)?;
            let mime_type = mime_guess::from_path(&a.path)
                .first_raw()
                .unwrap_or("application/octet-stream")
                .to_string();
            let meta = std::fs::metadata(&a.path).map_err(err)?;
            Ok(aria_core::agent::AttachmentInput {
                name: a.name,
                path: a.path,
                mime_type,
                size: meta.len() as i64,
                blob_hash,
            })
        })
        .collect();
    let att_inputs = att_inputs.map_err(err)?;
    let handle = agent
        .start(&conversation_id, &text, att_inputs)
        .await
        .map_err(err)?;
    let run = agent.snapshot(handle.run_id()).map_err(err)?.run;
    pump(app, &state.inner, handle);
    Ok(run)
}

/// Return an attachment's stored bytes as a data URL (mime + base64) so the
/// frontend can render images or offer documents for download.
#[tauri::command]
fn read_attachment(
    state: State<'_, App>,
    blob_hash: String,
) -> CmdResult<String> {
    let bytes = state
        .inner
        .blobs
        .read(&blob_hash)
        .map_err(err)?;
    // Re-derive mime from the blob hash is impossible; store mime alongside.
    // For now return base64 with a generic mime; the frontend knows the mime
    // from AttachmentView.mime_type.
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:application/octet-stream;base64,{b64}"))
}

#[tauri::command]
fn answer_approval(
    state: State<'_, App>,
    approval_id: String,
    decision: String,
) -> CmdResult<()> {
    let decision =
        aria_core::agent::parse_decision(&decision).ok_or("decision must be approve, approve_all or deny")?;
    state
        .agent()?
        .answer_approval(&approval_id, decision)
        .map_err(err)
}

#[tauri::command]
fn cancel_run(state: State<'_, App>, run_id: String) -> CmdResult<()> {
    if let Some(handle) = state
        .inner
        .handles
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&run_id)
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
fn undo(state: State<'_, App>, conversation_id: String, path: String) -> CmdResult<()> {
    let inner = &state.inner;
    let scope = conversation_scope(inner, &conversation_id)?
        .ok_or("No file access in this conversation")?;
    let base = scope.root().ok_or("No file access in this conversation")?;
    let key = journal_id(&inner.db, &conversation_id)?;
    inner.blobs.undo(&key, &base, &path).map_err(err)
}

#[tauri::command]
fn changed_files(state: State<'_, App>, conversation_id: String) -> CmdResult<Vec<ChangedFile>> {
    let inner = &state.inner;
    // A conversation with no file access has no journal, so nothing changed.
    if conversation_scope(inner, &conversation_id)?.is_none() {
        return Ok(Vec::new());
    }
    let key = journal_id(&inner.db, &conversation_id)?;
    let blobs = &inner.blobs;
    Ok(blobs
        .changed_paths(&key)
        .map_err(err)?
        .into_iter()
        .map(|path| ChangedFile {
            can_undo: blobs.can_undo(&key, &path).unwrap_or(false),
            path,
        })
        .collect())
}

/// List a conversation's directory (relative paths), directories marked with a
/// trailing slash. Read-only; contained to the conversation's scope.
#[tauri::command]
async fn list_workspace_dir(
    state: State<'_, App>,
    conversation_id: String,
    rel: String,
) -> CmdResult<Vec<String>> {
    let inner = Arc::clone(&state.inner);
    tokio::task::spawn_blocking(move || -> CmdResult<Vec<String>> {
        let scope = conversation_scope(&inner, &conversation_id)?
            .ok_or("No file access in this conversation")?;
        let resolved = scope.resolve_read(&rel).map_err(err)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&resolved).map_err(err)? {
            let entry = entry.map_err(err)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map_err(err)?.is_dir();
            out.push(if is_dir { format!("{name}/") } else { name });
        }
        out.sort();
        Ok(out)
    })
    .await
    .map_err(err)?
}

/// Read a conversation file as UTF-8. Refuses binary content.
#[tauri::command]
async fn read_workspace_file(
    state: State<'_, App>,
    conversation_id: String,
    rel: String,
) -> CmdResult<String> {
    let inner = Arc::clone(&state.inner);
    tokio::task::spawn_blocking(move || -> CmdResult<String> {
        let scope = conversation_scope(&inner, &conversation_id)?
            .ok_or("No file access in this conversation")?;
        let resolved = scope.resolve_read(&rel).map_err(err)?;
        std::fs::read_to_string(&resolved).map_err(err)
    })
    .await
    .map_err(err)?
}

/// Write a conversation file, journaling the prior version first (undoable, like
/// edit_file). Contained to the conversation's scope via resolve_create.
#[tauri::command]
async fn write_workspace_file(
    state: State<'_, App>,
    conversation_id: String,
    rel: String,
    content: String,
) -> CmdResult<()> {
    let inner = Arc::clone(&state.inner);
    tokio::task::spawn_blocking(move || -> CmdResult<()> {
        let scope = conversation_scope(&inner, &conversation_id)?
            .ok_or("No file access in this conversation")?;
        let resolved = scope.resolve_create(&rel).map_err(err)?;
        let base = scope.root().ok_or("No file access in this conversation")?;
        let rel_norm = scope.relativise(&resolved).map_err(err)?;
        let key = journal_id(&inner.db, &conversation_id)?;
        inner
            .blobs
            .write_file(&key, &base, &rel_norm, &content)
            .map_err(err)?;
        Ok(())
    })
    .await
    .map_err(err)?
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

/// Local sync folders a desktop client keeps on disk (Dropbox, Google Drive,
/// OneDrive, Nextcloud, Cloud, Sync) — purely local discovery, no network.
#[tauri::command]
fn list_cloud_folders() -> CmdResult<Vec<CloudFolder>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(Vec::new());
    };
    let folders = aria_core::workspace::cloud_sync_folders(&home);
    Ok(folders
        .into_iter()
        .map(|p| CloudFolder {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: p.to_string_lossy().into_owned(),
        })
        .collect())
}

/// Read an arbitrary file by absolute path for the attachment feature.
/// Capped at 512 KB; binary files return an error so the UI can fall back
/// to a path reference instead of embedding garbage.
#[tauri::command]
fn read_file_raw(path: String) -> CmdResult<String> {
    const MAX: u64 = 512 * 1024;
    let meta = std::fs::metadata(&path).map_err(err)?;
    if meta.len() > MAX {
        return Err(format!("File too large to attach ({} bytes; limit 512 KB)", meta.len()).into());
    }
    std::fs::read_to_string(&path).map_err(err)
}

/// Build a cheap [`BackgroundTasks`] for a conversation's workspace root, reusing
/// the shared `db`/`blobs`. Returns `Err` when the conversation has no folder.
fn background_tasks(inner: &Inner, conversation_id: &str) -> CmdResult<BackgroundTasks> {
    let scope = conversation_scope(inner, conversation_id)?
        .ok_or("No file access in this conversation")?;
    let root = scope
        .root()
        .ok_or("No file access in this conversation")?;
    Ok(BackgroundTasks {
        db: Arc::clone(&inner.db),
        blobs: Arc::clone(&inner.blobs),
        root,
        scope,
        shell_timeout_secs: 120,
    })
}

/// `poll`/`kill` key their log temp files under `blobs.dir()` and take only an
/// id, so no workspace resolution is needed — build from the blobs dir.
fn background_tasks_for_id(db: &Arc<Db>, blobs: &Arc<Blobs>) -> BackgroundTasks {
    BackgroundTasks {
        db: Arc::clone(db),
        blobs: Arc::clone(blobs),
        root: blobs.dir().to_path_buf(),
        scope: Scope::None,
        shell_timeout_secs: 120,
    }
}

#[tauri::command]
async fn start_background(
    state: State<'_, App>,
    cmd: String,
    conversation_id: String,
) -> CmdResult<BackgroundTask> {
    let bt = background_tasks(&state.inner, &conversation_id)?;
    bt.start(&conversation_id, &cmd).map_err(err)
}

#[tauri::command]
async fn poll_background(state: State<'_, App>, id: String) -> CmdResult<BackgroundPoll> {
    let (task, tail) = background_tasks_for_id(&state.inner.db, &state.inner.blobs)
        .poll(&id)
        .map_err(err)?;
    Ok(BackgroundPoll { task, tail })
}

#[tauri::command]
async fn list_background(
    state: State<'_, App>,
    conversation_id: String,
) -> CmdResult<Vec<BackgroundTask>> {
    let bt = background_tasks(&state.inner, &conversation_id)?;
    bt.list(&conversation_id).map_err(err)
}

#[tauri::command]
async fn kill_background(state: State<'_, App>, id: String) -> CmdResult<BackgroundTask> {
    background_tasks_for_id(&state.inner.db, &state.inner.blobs)
        .kill(&id)
        .await
        .map_err(err)
}

/// Real-time system health for the footer: is the model provider reachable now,
/// and is the app configured to use it? The probe is a live network request, so
/// the result reflects the current state, not a cached guess.
#[tauri::command]
async fn health(state: State<'_, App>) -> CmdResult<HealthView> {
    let config = &state.inner.config;
    let settings = config.load().map_err(err)?;
    let api_key_present = config.secret_status().map_err(err)?.present;
    let model = settings.model.clone();
    let schema_version = state.inner.db.schema_version().map_err(err)?;

    let provider = OpenAiProvider::from_config(&config);
    let (reachable, detail) = match provider {
        Ok(p) => match p.health_check().await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        },
        // No key or model configured: nothing to probe.
        Err(_) => (false, None),
    };

    let status = match (api_key_present && !model.trim().is_empty(), reachable) {
        (false, _) => "error",
        (true, true) => "ok",
        (true, false) => "degraded",
    };

    Ok(HealthView {
        status: status.to_string(),
        detail,
        provider_reachable: reachable,
        api_key_present,
        model,
        schema_version,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// ------------------------------------------------------------ database tools

/// Tables Settings › Database counts, in display order.
const COUNTED_TABLES: &[&str] = &[
    "workspaces",
    "conversations",
    "main_system_paths",
    "messages",
    "runs",
    "tool_calls",
    "approvals",
    "todos",
    "memories",
    "background_task",
    "edit_journal",
];

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Automatic copies written before a reset or import, so a destructive click
/// never loses the only copy of the data.
fn backups_dir() -> CmdResult<PathBuf> {
    let dir = data_dir()?.join("backups");
    std::fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

/// Write a consistent copy of the live database into the backups folder.
fn auto_snapshot(inner: &Inner, label: &str) -> CmdResult<()> {
    let dest = backups_dir()?.join(format!("{label}-{}.db", db::now_ms()));
    let sql = dest.to_string_lossy().to_string();
    inner
        .db
        .call(|c| {
            c.execute("VACUUM INTO ?1", [sql.as_str()])?;
            Ok(())
        })
        .map_err(err)
}

/// Reset and import replace every row; a live Run or task would keep writing
/// to a database it no longer owns.
fn ensure_idle(inner: &Inner) -> CmdResult<()> {
    let busy = inner
        .db
        .call(|c| {
            let runs: i64 = c.query_row(
                "SELECT COUNT(*) FROM runs WHERE state IN ('running', 'awaiting_approval')",
                [],
                |r| r.get(0),
            )?;
            let tasks: i64 = c.query_row(
                "SELECT COUNT(*) FROM background_task WHERE status = 'running'",
                [],
                |r| r.get(0),
            )?;
            Ok(runs + tasks)
        })
        .map_err(err)?;
    if busy > 0 {
        return Err("A run or background task is still active. Finish or cancel it first.".into());
    }
    Ok(())
}

fn database_info(inner: &Inner) -> CmdResult<DatabaseInfo> {
    let dir = data_dir()?;
    let path = dir.join("aria.db");
    let (schema_version, sqlite_version, journal_mode, page_size, tables) = inner
        .db
        .call(|c| {
            let mut tables = Vec::with_capacity(COUNTED_TABLES.len());
            for name in COUNTED_TABLES {
                let rows = c.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |r| r.get(0))?;
                tables.push(TableCount {
                    name: name.to_string(),
                    rows,
                });
            }
            Ok((
                c.query_row("PRAGMA user_version", [], |r| r.get(0))?,
                c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?,
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                c.query_row("PRAGMA page_size", [], |r| r.get(0))?,
                tables,
            ))
        })
        .map_err(err)?;
    let (blobs, blobs_bytes) = std::fs::read_dir(inner.blobs.dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .fold((0i64, 0u64), |(n, bytes), m| (n + 1, bytes + m.len()))
        })
        .unwrap_or((0, 0));
    let modified_at = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Ok(DatabaseInfo {
        path: path.to_string_lossy().to_string(),
        size_bytes: file_len(&path),
        wal_bytes: file_len(&dir.join("aria.db-wal")),
        modified_at,
        schema_version,
        sqlite_version,
        journal_mode,
        page_size,
        tables,
        blobs_dir: inner.blobs.dir().to_string_lossy().to_string(),
        blobs,
        blobs_bytes,
        backups_dir: backups_dir()?.to_string_lossy().to_string(),
    })
}

#[tauri::command]
fn get_database_info(state: State<'_, App>) -> CmdResult<DatabaseInfo> {
    database_info(&state.inner)
}

/// Save a consistent copy of the whole database to `dest` (a file the user
/// picked). Blobs (Undo history) are a separate store and are not included.
#[tauri::command]
fn backup_database(state: State<'_, App>, dest: String) -> CmdResult<()> {
    let path = PathBuf::from(&dest);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    // The save dialog already confirmed the overwrite; VACUUM INTO refuses an
    // existing file.
    if path.exists() {
        std::fs::remove_file(&path).map_err(err)?;
    }
    state
        .inner
        .db
        .call(|c| {
            c.execute("VACUUM INTO ?1", [dest.as_str()])?;
            Ok(())
        })
        .map_err(err)
}

/// Replace the whole database with a backup file. The current data is copied
/// into the backups folder first.
#[tauri::command]
fn import_database(state: State<'_, App>, path: String) -> CmdResult<DatabaseInfo> {
    let inner = &state.inner;
    ensure_idle(inner)?;
    let src = PathBuf::from(&path);
    db::validate_backup(&src).map_err(err)?;
    auto_snapshot(inner, "before-import")?;
    inner.db.restore(&src).map_err(err)?;
    // A backup file carries no blob store. Drop Undo steps whose saved content
    // is missing here, so the panel never offers a restore that cannot work.
    for hash in inner
        .db
        .call(db::referenced_blobs)
        .map_err(err)?
        .into_iter()
        .filter(|h| !h.is_empty())
    {
        if !inner.blobs.dir().join(&hash).exists() {
            inner
                .db
                .call(|c| {
                    c.execute("DELETE FROM edit_journal WHERE blob_hash = ?1", [hash.as_str()])?;
                    Ok(())
                })
                .map_err(err)?;
        }
    }
    // A backup taken mid-Run remembers a Run nothing is driving now.
    inner
        .db
        .call(|c| {
            c.execute(
                "UPDATE runs SET state = 'errored', stop_reason = 'interrupted',
                     ended_at = ?1 WHERE state IN ('running', 'awaiting_approval')",
                [db::now_ms()],
            )?;
            Ok(())
        })
        .map_err(err)?;
    let _ = inner.blobs.prune();
    database_info(inner)
}

/// Delete every chat, Run, memory and journal entry. Settings stay (they are
/// a file, not a row). The current data is copied into the backups folder
/// first.
#[tauri::command]
fn reset_database(state: State<'_, App>) -> CmdResult<DatabaseInfo> {
    let inner = &state.inner;
    ensure_idle(inner)?;
    auto_snapshot(inner, "before-reset")?;
    inner.db.reset().map_err(err)?;
    let _ = inner.blobs.prune();
    database_info(inner)
}

#[tauri::command]
fn set_settings(state: State<'_, App>, input: SettingsInput) -> CmdResult<SettingsView> {
    let config = &state.inner.config;
    // Load first so the new settings-domain fields (providers, embedding,
    // web search, disabled tools) are preserved rather than reset.
    let mut settings = config.load().map_err(err)?;
    settings.base_url = input.base_url.trim().to_string();
    settings.model = input.model.trim().to_string();
    // A General save does not carry embedding fields; keep the last
    // embedding setup instead of clearing the model and endpoint.
    if let Some(url) = input
        .embedding_base_url
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        settings.embedding_base_url = Some(url);
    }
    if let Some(model) = input
        .embedding_model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        settings.embedding_model = Some(model);
    }
    settings.search_provider = input
        .search_provider
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    settings.search_base_url = input
        .search_base_url
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    settings.approval_policy = match input.approval_policy.as_deref().unwrap_or("always_ask") {
        "auto_approve_safe" => aria_core::config::ApprovalPolicy::AutoApproveSafe,
        _ => aria_core::config::ApprovalPolicy::AlwaysAsk,
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

// ------------------------------------------------------ settings: AI providers

#[tauri::command]
fn list_ai_providers(state: State<'_, App>) -> CmdResult<Vec<AIProviderView>> {
    let config = &state.inner.config;
    let settings = config.load().map_err(err)?;
    Ok(settings
        .ai_providers
        .iter()
        .map(|p| provider_view(config, p))
        .collect())
}

#[tauri::command]
fn get_ai_provider(state: State<'_, App>, id: String) -> CmdResult<AIProviderView> {
    let config = &state.inner.config;
    let settings = config.load().map_err(err)?;
    let provider = settings
        .ai_providers
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("provider {id} not found"))?;
    Ok(provider_view(config, provider))
}

#[tauri::command]
fn create_ai_provider(
    state: State<'_, App>,
    name: String,
    endpoint_url: String,
    api_key: String,
    models: Option<Vec<aria_core::config::ProviderModel>>,
    model_ids: Option<Vec<String>>,
    default_model_id: String,
) -> CmdResult<AIProviderView> {
    use aria_core::settings::{self, SettingsError};
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;

    let raw_models = models.unwrap_or_else(|| {
        model_ids
            .unwrap_or_default()
            .into_iter()
            .map(|id| aria_core::config::ProviderModel::new(id.clone(), id))
            .collect()
    });
    let models = settings::dedupe_models(&raw_models);
    let default_model_id = if default_model_id.trim().is_empty() {
        models.first().map(|m| m.id.clone()).unwrap_or_default()
    } else {
        default_model_id.trim().to_string()
    };
    settings::validate_provider(&s, None, &name, &endpoint_url, &models, &default_model_id, &api_key)
        .map_err(|e: SettingsError| e.to_string())?;

    let id = aria_core::db::uuid_v7();
    let endpoint_url = settings::normalize_endpoint(&endpoint_url).map_err(err)?;
    config
        .set_provider_api_key(&id, api_key.trim())
        .map_err(err)?;

    let provider = AIProvider {
        id: id.clone(),
        name: name.trim().to_string(),
        provider_type: "openai-compatible".to_string(),
        endpoint_url,
        models,
        default_model_id,
        // The first provider becomes the default automatically.
        is_default: s.ai_providers.is_empty(),
        status: ProviderStatus::Untested,
        last_tested_at: None,
    };
    if provider.is_default {
        s.default_ai_provider_id = Some(id.clone());
    }
    s.ai_providers.push(provider.clone());
    s.sync_runtime_endpoint();
    config.save(&s).map_err(err)?;
    Ok(provider_view(config, &provider))
}

#[tauri::command]
fn update_ai_provider(
    state: State<'_, App>,
    id: String,
    name: Option<String>,
    endpoint_url: Option<String>,
    api_key: Option<String>,
    models: Option<Vec<aria_core::config::ProviderModel>>,
    model_ids: Option<Vec<String>>,
    default_model_id: Option<String>,
) -> CmdResult<AIProviderView> {
    use aria_core::settings::{self, SettingsError};
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;

    let idx = s
        .ai_providers
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| format!("provider {id} not found"))?;

    // Compute the post-update values, then validate the whole form once.
    let name = name.map(|n| n.trim().to_string()).unwrap_or_else(|| s.ai_providers[idx].name.clone());
    let endpoint_url = endpoint_url
        .map(|e| e.trim().to_string())
        .unwrap_or_else(|| s.ai_providers[idx].endpoint_url.clone());
    let models = if let Some(m) = models {
        settings::dedupe_models(&m)
    } else if let Some(m) = model_ids {
        let converted: Vec<aria_core::config::ProviderModel> = m
            .into_iter()
            .map(|id| aria_core::config::ProviderModel::new(id.clone(), id))
            .collect();
        settings::dedupe_models(&converted)
    } else {
        s.ai_providers[idx].models.clone()
    };
    let default_model_id = default_model_id
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| s.ai_providers[idx].default_model_id.clone());
    let new_key = api_key.unwrap_or_default();

    settings::validate_provider(
        &s,
        Some(&id),
        &name,
        &endpoint_url,
        &models,
        &default_model_id,
        // Only require a key when none is stored yet.
        if config.provider_api_key(&id).ok().flatten().is_some() {
            "present"
        } else {
            &new_key
        },
    )
    .map_err(|e: SettingsError| e.to_string())?;

    let endpoint_url = settings::normalize_endpoint(&endpoint_url).map_err(err)?;
    if !new_key.trim().is_empty() {
        config.set_provider_api_key(&id, new_key.trim()).map_err(err)?;
    }

    let p = &mut s.ai_providers[idx];
    p.name = name;
    p.endpoint_url = endpoint_url;
    p.models = models;
    p.default_model_id = default_model_id;
    let view = provider_view(config, p);
    s.sync_runtime_endpoint();
    config.save(&s).map_err(err)?;
    Ok(view)
}

#[tauri::command]
fn delete_ai_provider(state: State<'_, App>, id: String) -> CmdResult<()> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    s.ai_providers.retain(|p| p.id != id);
    config.clear_provider_api_key(&id).map_err(err)?;

    // Promote the next provider to default so one always remains when any exist.
    if s.default_ai_provider_id.as_deref() == Some(id.as_str()) {
        s.default_ai_provider_id = None;
        for p in &mut s.ai_providers {
            p.is_default = false;
        }
        if let Some(next) = s.ai_providers.first_mut() {
            next.is_default = true;
            s.default_ai_provider_id = Some(next.id.clone());
        }
    }
    s.sync_runtime_endpoint();
    config.save(&s).map_err(err)?;
    Ok(())
}

#[tauri::command]
fn set_default_ai_provider(state: State<'_, App>, id: String) -> CmdResult<()> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    if !s.ai_providers.iter().any(|p| p.id == id) {
        return Err(format!("provider {id} not found"));
    }
    for p in &mut s.ai_providers {
        p.is_default = p.id == id;
    }
    s.default_ai_provider_id = Some(id);
    s.sync_runtime_endpoint();
    config.save(&s).map_err(err)?;
    Ok(())
}

/// Select the active model (and optionally its provider) from the composer model selector.
#[tauri::command]
async fn select_model(
    state: State<'_, App>,
    provider_id: Option<String>,
    model_id: String,
) -> CmdResult<HealthView> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;

    let model_id = model_id.trim().to_string();
    if model_id.is_empty() {
        return Err("a model name is required".into());
    }

    if let Some(pid) = provider_id {
        let mut endpoint = None;
        for prov in &mut s.ai_providers {
            if prov.id == pid {
                prov.is_default = true;
                prov.default_model_id = model_id.clone();
                endpoint = Some(prov.endpoint_url.clone());
            } else {
                prov.is_default = false;
            }
        }
        if let Some(ep) = endpoint {
            s.default_ai_provider_id = Some(pid);
            s.base_url = ep;
        }
    }
    s.model = model_id;
    config.save(&s).map_err(err)?;
    health(state).await
}

#[tauri::command]
async fn test_ai_provider(
    state: State<'_, App>,
    id: Option<String>,
    endpoint_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> CmdResult<ProviderTestResult> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;

    let id = id.filter(|s| !s.trim().is_empty());
    let idx = if let Some(ref id) = id {
        Some(
            s.ai_providers
                .iter()
                .position(|p| p.id == *id)
                .ok_or_else(|| format!("provider {id} not found"))?,
        )
    } else {
        None
    };

    let endpoint = match endpoint_url {
        Some(url) => aria_core::settings::normalize_endpoint(&url).map_err(err)?,
        None => {
            let i = idx.ok_or("endpointUrl is required")?;
            s.ai_providers[i].endpoint_url.clone()
        }
    };

    let key = match api_key.filter(|k| !k.trim().is_empty()) {
        Some(k) => k,
        None => match id.as_deref() {
            Some(id) => config
                .provider_api_key(id)
                .map_err(err)?
                .ok_or("No API key stored for this provider")?,
            None => return Err("Enter the provider's API key.".into()),
        },
    };

    let outcome = aria_core::settings::probe_provider(
        &endpoint,
        &key,
        model.as_deref(),
        std::time::Duration::from_secs(8),
    )
    .await;
    // Only a saved provider has a status row to update.
    if let Some(idx) = idx {
        aria_core::settings::apply_probe_status(&mut s.ai_providers[idx], outcome.ok);
        config.save(&s).map_err(err)?;
    }
    Ok(ProviderTestResult {
        ok: outcome.ok,
        latency_ms: Some(outcome.latency_ms),
        error: outcome.error,
    })
}

#[tauri::command]
async fn fetch_ai_provider_models(
    state: State<'_, App>,
    id: String,
) -> CmdResult<FetchModelsResult> {
    let config = &state.inner.config;
    let s = config.load().map_err(err)?;
    let endpoint = s
        .ai_providers
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.endpoint_url.clone())
        .ok_or_else(|| format!("provider {id} not found"))?;
    let key = config
        .provider_api_key(&id)
        .map_err(err)?
        .ok_or("No API key stored for this provider")?;
    let models = aria_core::settings::fetch_models(
        &endpoint,
        &key,
        std::time::Duration::from_secs(10),
    )
    .await
    .map_err(err)?;
    Ok(FetchModelsResult { models })
}

// --------------------------------------------------------- settings: embedding

#[tauri::command]
fn get_embedding_config(state: State<'_, App>) -> CmdResult<EmbeddingConfig> {
    Ok(state.inner.config.load().map_err(err)?.embedding_config)
}

#[tauri::command]
fn set_embedding_config(state: State<'_, App>, config: EmbeddingConfig) -> CmdResult<EmbeddingConfig> {
    aria_core::settings::validate_embedding(&config.model, config.dimensions).map_err(err)?;
    let cfg = &state.inner.config;
    let mut s = cfg.load().map_err(err)?;
    // Auto-enable embeddings once the user has configured an embedding model:
    // semantic recall turns on by itself instead of requiring a separate toggle.
    let mut config = config;
    if !config.model.trim().is_empty() {
        config.enabled = true;
    }
    s.embedding_config = config.clone();
    // Mirror into the flat runtime fields the provider already reads.
    s.embedding_model = Some(config.model.clone());
    s.embedding_base_url = if config.provider_id == "default" {
        None
    } else {
        s.ai_providers
            .iter()
            .find(|p| p.id == config.provider_id)
            .map(|p| p.endpoint_url.clone())
    };
    cfg.save(&s).map_err(err)?;
    Ok(config)
}

#[tauri::command]
async fn test_embedding(
    state: State<'_, App>,
    model: Option<String>,
    provider_id: Option<String>,
) -> CmdResult<EmbeddingTestResult> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    // Test the values on screen, not the last saved ones: a model change is
    // exactly what the user wants to verify before saving.
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        s.embedding_config.model = m.trim().to_string();
        s.embedding_model = Some(s.embedding_config.model.clone());
    }
    if let Some(id) = provider_id.filter(|id| !id.trim().is_empty()) {
        s.embedding_config.provider_id = id;
        s.embedding_base_url = if s.embedding_config.provider_id == "default" {
            None
        } else {
            s.ai_providers
                .iter()
                .find(|p| p.id == s.embedding_config.provider_id)
                .map(|p| p.endpoint_url.clone())
        };
    }
    // The embedding source may be a non-default provider; read its key directly.
    let key = match config
        .provider_api_key(&s.embedding_config.provider_id)
        .map_err(err)?
    {
        Some(k) => k,
        None => match config.api_key().map_err(err)? {
            Some((k, _)) => k,
            None => {
                return Ok(EmbeddingTestResult {
                    ok: false,
                    vector_length: None,
                    latency_ms: None,
                    error: Some("No API key configured for this provider.".into()),
                });
            }
        },
    };
    let provider = match OpenAiProvider::new(s.clone(), key) {
        Ok(p) => p,
        Err(e) => {
            return Ok(EmbeddingTestResult {
                ok: false,
                vector_length: None,
                latency_ms: None,
                error: Some(e.to_string()),
            });
        }
    };
    // Embedding requests use the hardcoded recommended timeout (30s); not
    // user-tunable anymore.
    let timeout = std::time::Duration::from_secs(30);
    let outcome =
        aria_core::settings::probe_embedding(&provider, &s.embedding_config.model, timeout).await;
    Ok(EmbeddingTestResult {
        ok: outcome.ok,
        vector_length: outcome.vector_length,
        latency_ms: Some(outcome.latency_ms),
        error: outcome.error,
    })
}

/// How much of the memory index is embedded, so Settings can warn before a
/// model change and report progress after a re-index.
#[tauri::command]
fn get_embedding_index_info(state: State<'_, App>) -> CmdResult<EmbeddingIndexInfo> {
    let (total, embedded) = state
        .inner
        .db
        .call(aria_core::db::memory_embedding_counts)
        .map_err(err)?;
    Ok(EmbeddingIndexInfo { total, embedded })
}

/// Re-embed every stored memory with the current embedding config.
#[tauri::command]
async fn reindex_memories(state: State<'_, App>) -> CmdResult<usize> {
    let provider = OpenAiProvider::from_config(&state.inner.config).map_err(err)?;
    aria_core::settings::reindex_memories(&state.inner.db, &provider)
        .await
        .map_err(err)
}

// -------------------------------------------------------- settings: web search

#[tauri::command]
fn get_web_search_config(state: State<'_, App>) -> CmdResult<Option<WebSearchConfig>> {
    Ok(state.inner.config.load().map_err(err)?.web_search_config)
}

#[tauri::command]
fn set_web_search_config(
    state: State<'_, App>,
    provider_key: String,
    api_key: Option<String>,
    base_url: Option<String>,
) -> CmdResult<WebSearchConfig> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    let key = api_key.unwrap_or_default();
    let cfg = aria_core::settings::web_search_config(&provider_key, &key).map_err(err)?;
    if !key.trim().is_empty() {
        config.set_search_api_key(key.trim()).map_err(err)?;
    }
    // Mirror into the flat fields the agent already reads.
    s.search_provider = Some(cfg.provider_key.clone());
    s.search_base_url = base_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    s.web_search_config = Some(cfg.clone());
    config.save(&s).map_err(err)?;
    Ok(cfg)
}

#[tauri::command]
async fn verify_web_search(
    state: State<'_, App>,
    provider_key: String,
    api_key: Option<String>,
    base_url: Option<String>,
) -> CmdResult<WebSearchVerifyResult> {
    // Prove the key by running the smallest real query the agent would run, so
    // a green result means search actually works rather than "key looks fine".
    let config = &state.inner.config;
    let key = match api_key.filter(|k| !k.trim().is_empty()) {
        Some(k) => Some(k),
        None => config.search_api_key().map_err(err)?.map(|(k, _)| k),
    };
    if matches!(provider_key.as_str(), "exa" | "firecrawl") && key.is_none() {
        return Ok(WebSearchVerifyResult {
            ok: false,
            error: Some("Paste your API key first.".into()),
        });
    }
    let provider = match aria_core::tools::SearchProvider::parse(&provider_key) {
        Some(p) => p,
        None => {
            return Ok(WebSearchVerifyResult {
                ok: false,
                error: Some("Unknown search provider.".into()),
            });
        }
    };
    let ctx = aria_core::tools::context_for(
        std::path::Path::new("/tmp"),
        Arc::clone(&state.inner.db),
        Arc::clone(&state.inner.blobs),
        "",
        "web-search-verify",
    )
    .map_err(err)?;
    let mut ctx = ctx;
    ctx.search = Some(aria_core::tools::SearchConfig {
        provider,
        api_key: key,
        base_url: base_url.filter(|u| !u.trim().is_empty()),
    });
    let out = aria_core::tools::web_search(
        &ctx,
        aria_core::tools::search::SearchArgs {
            query: "aria".into(),
            max_results: Some(1),
        },
    )
    .await;
    Ok(match out {
        Ok(_) => WebSearchVerifyResult { ok: true, error: None },
        Err(e) => WebSearchVerifyResult {
            ok: false,
            error: Some(e.to_string()),
        },
    })
}

/// Forget the stored search key and provider, which also turns Web Search off.
#[tauri::command]
fn disconnect_web_search(state: State<'_, App>) -> CmdResult<()> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    config.clear_search_api_key().map_err(err)?;
    s.search_provider = None;
    s.search_base_url = None;
    s.web_search_config = None;
    config.save(&s).map_err(err)?;
    Ok(())
}

#[tauri::command]
async fn test_web_search(state: State<'_, App>) -> CmdResult<WebSearchTestResult> {
    let config = &state.inner.config;
    let s = config.load().map_err(err)?;
    let Some(provider) = s
        .search_provider
        .as_deref()
        .and_then(aria_core::tools::SearchProvider::parse)
    else {
        return Ok(WebSearchTestResult {
            ok: false,
            titles: Vec::new(),
            error: Some("Web search is not configured.".into()),
        });
    };
    let key = config.search_api_key().map_err(err)?.map(|(k, _)| k);
    let ctx = aria_core::tools::context_for(
        std::path::Path::new("/tmp"),
        Arc::clone(&state.inner.db),
        Arc::clone(&state.inner.blobs),
        "",
        "web-search-test",
    )
    .map_err(err)?;
    let mut ctx = ctx;
    ctx.search = Some(aria_core::tools::SearchConfig {
        provider,
        api_key: key,
        base_url: s.search_base_url.clone(),
    });
    let out = aria_core::tools::web_search(
        &ctx,
        aria_core::tools::search::SearchArgs {
            query: "aria desktop assistant".into(),
            max_results: Some(3),
        },
    )
    .await;
    match out {
        Ok(text) => {
            let titles = parse_search_titles(&text);
            Ok(WebSearchTestResult { ok: true, titles, error: None })
        }
        Err(e) => Ok(WebSearchTestResult {
            ok: false,
            titles: Vec::new(),
            error: Some(e.to_string()),
        }),
    }
}

/// The web_search tool returns numbered "title — url" lines; extract the first
/// three into title/domain pairs for the test panel.
fn parse_search_titles(text: &str) -> Vec<SearchTitle> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.split_once(". ").map(|(_, r)| r) else {
            continue;
        };
        if let Some((title, url)) = rest.split_once(" — ") {
            let domain = url
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            out.push(SearchTitle { title: title.trim().to_string(), domain });
            if out.len() == 3 {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------- settings: built-in tools

#[tauri::command]
fn list_built_in_tools(state: State<'_, App>) -> CmdResult<Vec<BuiltInToolView>> {
    let s = state.inner.config.load().map_err(err)?;
    let search_ready = s.web_search_config.is_some();
    Ok(aria_core::settings::tool_catalog()
        .into_iter()
        .map(|t| BuiltInToolView {
            id: t.id.to_string(),
            name: t.name.to_string(),
            description: t.description.to_string(),
            enabled: s.tool_enabled(t.id),
            requires_setup: t.requires_setup,
            setup_complete: !t.requires_setup || search_ready,
            permission: t.permission.to_string(),
        })
        .collect())
}

#[tauri::command]
fn set_built_in_tool(
    state: State<'_, App>,
    id: String,
    enabled: bool,
) -> CmdResult<BuiltInToolView> {
    let config = &state.inner.config;
    let mut s = config.load().map_err(err)?;
    if !aria_core::settings::tool_catalog().iter().any(|t| t.id == id) {
        return Err(format!("unknown tool {id}"));
    }
    s.disabled_tools.retain(|t| t != &id);
    if !enabled {
        s.disabled_tools.push(id.clone());
    }
    config.save(&s).map_err(err)?;
    let search_ready = s.web_search_config.is_some();
    let info = aria_core::settings::tool_catalog()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or("unknown tool")?;
    Ok(BuiltInToolView {
        id: info.id.to_string(),
        name: info.name.to_string(),
        description: info.description.to_string(),
        enabled,
        requires_setup: info.requires_setup,
        setup_complete: !info.requires_setup || search_ready,
        permission: info.permission.to_string(),
    })
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
            inner.config.clone(),
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

/// Any task still `running` when the app starts is an orphan: its process died
/// with the previous instance. Mark it `killed`, preserve its partial log as a
/// blob, and drop the stale temp files.
fn reap_orphan_background_tasks(inner: &Arc<Inner>) {
    let ids = inner
        .db
        .call(db::running_background_task_ids)
        .unwrap_or_default();
    for id in ids {
        let log = inner.blobs.dir().join(format!("bg-{id}.log"));
        let blob = std::fs::read(&log).ok().and_then(|b| inner.blobs.put(&b).ok());
        let _ = inner.db.call(|c| {
            db::update_background_task(
                c,
                &id,
                db::now_ms(),
                None,
                db::BackgroundStatus::Killed,
                blob.as_deref().unwrap_or(""),
            )
        });
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(inner.blobs.dir().join(format!("bg-{id}.pid")));
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
                    handles: Mutex::new(HashMap::new()),
                }),
            };
            resume_unfinished(app.handle(), &state.inner);
            reap_orphan_background_tasks(&state.inner);
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
            set_conversation_mode,
            set_conversation_approval,
            list_conversations,
            get_conversation,
            send_message,
            answer_approval,
            cancel_run,
            resume_run,
            get_run_state,
            undo,
            changed_files,
            list_workspace_dir,
            read_workspace_file,
            write_workspace_file,
            search_messages,
            get_settings,
            set_settings,
            list_memories,
            delete_memory,
            list_cloud_folders,
            start_background,
            poll_background,
            list_background,
            kill_background,
            health,
            get_database_info,
            backup_database,
            import_database,
            reset_database,
            list_ai_providers,
            get_ai_provider,
            create_ai_provider,
            update_ai_provider,
            delete_ai_provider,
            set_default_ai_provider,
            select_model,
            test_ai_provider,
            fetch_ai_provider_models,
            get_embedding_config,
            set_embedding_config,
            test_embedding,
            get_embedding_index_info,
            reindex_memories,
            get_web_search_config,
            set_web_search_config,
            verify_web_search,
            disconnect_web_search,
            test_web_search,
            list_built_in_tools,
            set_built_in_tool,
            read_file_raw,
            read_attachment,
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

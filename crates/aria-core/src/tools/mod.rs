//! The ten agent Tools, their approval classes and their dispatch.
//!
//! Each tool is a function from (validated args, [`ToolContext`]) to a
//! [`ToolOutcome`]. Nothing here touches the model; the Run loop calls
//! [`ToolRegistry::describe`] to advertise tools and [`ToolRegistry::execute`]
//! to run one (issue 07).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::{ApprovalClass, Db, ToolCallState};
use crate::journal::Blobs;
use crate::provider::{Embedder, ToolSpec};
use crate::workspace::{Scope, WorkspaceRoot};

mod files;
mod fetch;
mod shell;
mod memories;
pub mod search;

pub use fetch::{FetchOutcome, classify_ip, fetch_url};
pub use search::{SearchConfig, SearchProvider, SearchHit, web_search};
pub use shell::{ShellArgs, ShellOutcome, run_shell};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    Args(String),
    #[error("path: {0}")]
    Path(#[from] crate::workspace::PathError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal: {0}")]
    Journal(#[from] crate::journal::JournalError),
    #[error("db: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, ToolError>;

/// What a tool run produced. `is_error` only marks agent-recoverable failures
/// (bad arguments, missing files, refused fetches); it does not end the Run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Everything a Tool needs to act for one run.
///
/// `root` and `workspace_id` are `None` in the main session, which has no
/// folder: only the tools that never touch the filesystem are advertised, and
/// the rest refuse cleanly if the model asks for them anyway.
#[derive(Clone)]
pub struct ToolContext {
    pub scope: Scope,
    pub root: Option<WorkspaceRoot>,
    pub workspace_id: Option<String>,
    pub run_id: String,
    pub db: Arc<Db>,
    pub blobs: Arc<Blobs>,
    pub shell_timeout_secs: u64,
    /// Embedding capability, when the provider supports it. Enables similarity
    /// recall; absent for test fakes, which then fall back to recency.
    pub embed: Option<Arc<dyn Embedder>>,
    /// Web search configuration, when a provider is configured. Absent when
    /// search is disabled; `web_search` then refuses.
    pub search: Option<crate::tools::search::SearchConfig>,
}

impl ToolContext {}

/// Error text for a file Tool called in a conversation with no folder open.
pub const NO_FOLDER: &str =
    "No folder is open in this conversation. Ask the user to choose one, or answer \
     without file access.";

pub const READ_FILE: &str = "read_file";
pub const LIST_DIR: &str = "list_dir";
pub const SEARCH_FILES: &str = "search_files";
pub const EDIT_FILE: &str = "edit_file";
pub const WRITE_FILE: &str = "write_file";
pub const DELETE_FILE: &str = "delete_file";
pub const MOVE_FILE: &str = "move_file";
pub const RUN_SHELL: &str = "run_shell";
pub const WEB_FETCH: &str = "web_fetch";
pub const TODOS: &str = "todos";
pub const REMEMBER: &str = "remember";
pub const RECALL: &str = "recall";
pub const WEB_SEARCH: &str = "web_search";

/// The tool set with its approval classes.
pub const TOOLS: [(&str, ApprovalClass); 13] = [
    (READ_FILE, ApprovalClass::Read),
    (LIST_DIR, ApprovalClass::Read),
    (SEARCH_FILES, ApprovalClass::Read),
    (EDIT_FILE, ApprovalClass::Mutate),
    (WRITE_FILE, ApprovalClass::Mutate),
    (DELETE_FILE, ApprovalClass::Mutate),
    (MOVE_FILE, ApprovalClass::Mutate),
    (RUN_SHELL, ApprovalClass::Exec),
    (WEB_FETCH, ApprovalClass::Network),
    (WEB_SEARCH, ApprovalClass::Network),
    (TODOS, ApprovalClass::Read),
    (REMEMBER, ApprovalClass::Read),
    (RECALL, ApprovalClass::Read),
];

pub const SYSTEM_PROMPT: &str = "\
You are Aria, a desktop assistant. You help with everyday conversation and, when
a folder is open, with work on the files inside it.

Rules:
- Changes to files, shell commands and web fetches need the user's approval;
  if an approval is denied, explain and propose an alternative.
- Keep the todo list current for multi-step work.
- Use `remember` to persist durable facts the user shares (preferences, names,
  project context) so you can recall them in later conversations. Use `recall`
  when a past fact would help answer. Stored memories are global unless a folder
  is open, in which case they are scoped to that folder plus the global set.
- Report what you changed and how you verified it.";

/// Appended when the conversation has a folder open.
pub const FOLDER_PROMPT: &str = "\
A folder is open in this conversation. Its tools act on that folder.

- All paths are relative to the folder root. Never try to leave it.
- Prefer search_files and read_file before editing.
- edit_file replaces an exact substring that must occur exactly once; include
  enough context to make it unique. Use write_file only for new files.";

/// Appended when no folder is open, so the model does not promise file work it
/// cannot do.
pub const NO_FOLDER_PROMPT: &str = "\
No folder is open in this conversation, so you have no files or shell. Answer
questions and fetch web pages. If the user asks for work on files, offer to
open a folder from the conversation header and let them pick one.";

pub fn approval_class(tool: &str) -> Option<ApprovalClass> {
    TOOLS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, class)| *class)
}

/// JSON Schema descriptions handed to the model, plus dispatch.
pub struct ToolRegistry;

impl ToolRegistry {
    /// The tools to advertise. Without a folder only the filesystem-free ones
    /// are offered, so the model is never told about tools it cannot use.
    pub fn describe(scope: &Scope) -> Vec<ToolSpec> {
        let all = vec![
            spec(
                READ_FILE,
                "Read a UTF-8 text file from the project. Returns at most `max_bytes`.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative path" },
                        "max_bytes": { "type": "integer", "minimum": 1, "default": 200000 }
                    },
                    "required": ["path"]
                }),
            ),
            spec(
                LIST_DIR,
                "List a directory: names, sizes, directories first, sorted.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "default": "." }
                    }
                }),
            ),
            spec(
                SEARCH_FILES,
                "Search file contents by regular expression, optionally limited by a glob. Respects .gitignore.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust regex" },
                        "glob": { "type": "string", "description": "e.g. **/*.rs" },
                        "max_results": { "type": "integer", "minimum": 1, "default": 200 }
                    },
                    "required": ["pattern"]
                }),
            ),
            spec(
                EDIT_FILE,
                "Replace an exact substring in an existing file. The search text must occur exactly once.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "search": { "type": "string" },
                        "replace": { "type": "string" }
                    },
                    "required": ["path", "search", "replace"]
                }),
            ),
            spec(
                WRITE_FILE,
                "Create a new file. Fails if the path already exists; use edit_file to change one.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            ),
            spec(
                DELETE_FILE,
                "Delete a file inside the project.",
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            ),
            spec(
                MOVE_FILE,
                "Move or rename a file inside the project.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["from", "to"]
                }),
            ),
            spec(
                RUN_SHELL,
                "Run a shell command with `sh -c` in the project root. Non-interactive; returns exit code and output.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" },
                        "timeout_secs": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["cmd"]
                }),
            ),
            spec(
                WEB_FETCH,
                "Fetch an http(s) URL and return its text. Private and loopback addresses are refused.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "max_bytes": { "type": "integer", "minimum": 1, "default": 100000 }
                    },
                    "required": ["url"]
                }),
            ),
            spec(
                TODOS,
                "Replace the run's todo list. Use for multi-step work.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "done": { "type": "boolean" }
                                },
                                "required": ["text"]
                            }
                        }
                    },
                    "required": ["items"]
                }),
            ),
            spec(
                REMEMBER,
                "Store a durable fact under a key for later recall. Pass an empty \
                 content to forget it. Global unless a folder is open, then scoped \
                 to the folder plus the global set.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Stable label, e.g. user_name" },
                        "content": { "type": "string", "description": "The fact to retain" }
                    },
                    "required": ["key", "content"]
                }),
            ),
            spec(
                RECALL,
                "List stored memories, ranked by similarity to `query` when given. \
                 Returns the folder's memories plus global ones.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Optional relevance query" },
                        "limit": { "type": "integer", "minimum": 1, "default": 10 }
                    }
                }),
            ),
            spec(
                WEB_SEARCH,
                "Search the web for a query and return ranked results (title, url, snippet). Results are fetched over https only.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "max_results": { "type": "integer", "minimum": 1, "default": 5 }
                    },
                    "required": ["query"]
                }),
            ),
        ];
        if !matches!(scope, Scope::None) {
            return all;
        }
        let usable = [WEB_FETCH, WEB_SEARCH, TODOS, REMEMBER, RECALL];
        all.into_iter()
            .filter(|s| usable.contains(&s.name.as_str()))
            .collect()
    }

    /// Run one tool. Every failure is a [`ToolOutcome`] marked as an error, never
    /// a panic and never an aborted Run.
    pub async fn execute(tool: &str, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match Self::try_execute(tool, args, ctx).await {
            Ok(outcome) => outcome,
            Err(e) => ToolOutcome::error(e.to_string()),
        }
    }

    async fn try_execute(tool: &str, args: &str, ctx: &ToolContext) -> Result<ToolOutcome> {
        // Filesystem tools refuse rather than panic when no folder is open;
        // they are not advertised in that case, but the model may still ask.
        match tool {
            READ_FILE => files::read_file(ctx, parse_args(args)?).map(outcome),
            LIST_DIR => files::list_dir(ctx, parse_args(args)?).map(outcome),
            SEARCH_FILES => files::search_files(ctx, parse_args(args)?).map(outcome),
            EDIT_FILE => files::edit_file(ctx, parse_args(args)?).map(outcome),
            WRITE_FILE => files::write_file(ctx, parse_args(args)?).map(outcome),
            DELETE_FILE => files::delete_file(ctx, parse_args(args)?).map(outcome),
            MOVE_FILE => files::move_file(ctx, parse_args(args)?).map(outcome),
            RUN_SHELL => shell::run_shell(ctx, parse_args(args)?, None).await.map(outcome),
            WEB_FETCH => fetch::web_fetch(parse_args(args)?).await.map(outcome),
            WEB_SEARCH => search::web_search(ctx, parse_args(args)?).await.map(outcome),
            TODOS => files::todos(ctx, parse_args(args)?).map(outcome),
            REMEMBER => memories::remember(ctx, parse_args(args)?).await.map(outcome),
            RECALL => memories::recall(ctx, parse_args(args)?).await.map(outcome),
            other => Err(ToolError::Args(format!("unknown tool {other}"))),
        }
    }
}

fn outcome(text: String) -> ToolOutcome {
    ToolOutcome::ok(text)
}

fn spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

pub fn parse_args<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T> {
    serde_json::from_str(args).map_err(|e| ToolError::Args(format!("{e}: {args}")))
}

/// A side effect-free description of what a Tool Call will do, shown in the
/// Approval prompt.
pub fn summarize(tool: &str, args: &str) -> ApprovalSummary {
    let parsed: Option<serde_json::Value> = serde_json::from_str(args).ok();
    let get = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (title, body) = match tool {
        EDIT_FILE => (
            format!("Edit {}", get("path")),
            format!("--- search ---\n{}\n--- replace ---\n{}", get("search"), get("replace")),
        ),
        WRITE_FILE => (format!("Create {}", get("path")), get("content")),
        DELETE_FILE => (format!("Delete {}", get("path")), String::new()),
        MOVE_FILE => (
            format!("Move {} -> {}", get("from"), get("to")),
            String::new(),
        ),
        RUN_SHELL => ("Run shell command".to_string(), get("cmd")),
        WEB_FETCH => (format!("Fetch {}", get("url")), String::new()),
        WEB_SEARCH => (format!("Search the web for {}", get("query")), String::new()),
        other => (format!("Run {other}"), args.to_string()),
    };
    ApprovalSummary { title, body }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSummary {
    pub title: String,
    pub body: String,
}

/// Mark a tool call as `Approved` before execution. Kept here so the Run loop
/// and tests share one transition.
pub fn mark_approved(db: &Db, tool_call_id: &str) -> Result<()> {
    db.call(|c| crate::db::set_tool_call_state(c, tool_call_id, ToolCallState::Approved))?;
    Ok(())
}

/// Convenience for callers that need a context rooted at a directory.
pub fn context_for(
    root: &std::path::Path,
    db: Arc<Db>,
    blobs: Arc<Blobs>,
    workspace_id: &str,
    run_id: &str,
) -> Result<ToolContext> {
    let root = WorkspaceRoot::new(root)?;
    Ok(ToolContext {
        scope: Scope::Workspace(root.clone()),
        root: Some(root),
        workspace_id: Some(workspace_id.to_string()),
        run_id: run_id.to_string(),
        db,
        blobs,
        shell_timeout_secs: 120,
        embed: None,
        search: None,
    })
}

pub(crate) fn default_limit(limit: Option<usize>, default: usize) -> usize {
    limit.unwrap_or(default).max(1)
}

pub(crate) fn truncate(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated {} of {} bytes shown]", &text[..cut], cut, text.len())
}

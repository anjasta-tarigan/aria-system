//! The ten agent Tools, their approval classes and their dispatch.
//!
//! Each tool is a function from (validated args, [`ToolContext`]) to a
//! [`ToolOutcome`]. Nothing here touches the model; the Run loop calls
//! [`ToolRegistry::describe`] to advertise tools and [`ToolRegistry::execute`]
//! to run one (issue 07).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::db::{ApprovalClass, Db, ToolCallState};
use crate::journal::Blobs;
use crate::provider::{Embedder, Reranker, ToolSpec};
use crate::workspace::{Scope, WorkspaceRoot};

mod files;
mod fetch;
mod shell;
mod memories;
pub mod search;
pub mod vision;
pub mod background;

pub use background::BackgroundTasks;
pub use crate::db::{BackgroundStatus, BackgroundTask};
pub use fetch::{FetchOutcome, classify_ip, fetch_url};
pub use search::{SearchConfig, SearchProvider, SearchHit, web_search};
pub use shell::{ShellArgs, ShellOutcome, is_safe_command, run_shell};

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
/// folder. `journal_id` is what the edit journal is keyed by: the workspace id
/// when one is open, otherwise the conversation id, so Main System edits stay
/// undoable per chat.
#[derive(Clone)]
pub struct ToolContext {
    pub scope: Scope,
    pub root: Option<WorkspaceRoot>,
    pub workspace_id: Option<String>,
    /// Key for the edit journal (`workspace_id`, or the conversation id).
    pub journal_id: String,
    pub run_id: String,
    pub db: Arc<Db>,
    pub blobs: Arc<Blobs>,
    pub shell_timeout_secs: u64,
    /// Embedding capability, when the provider supports it. Enables similarity
    /// recall; absent for test fakes, which then fall back to recency.
    pub embed: Option<Arc<dyn Embedder>>,
    /// Reranker for the recall pipeline. Absent = no reranking (the hybrid
    /// score stands alone).
    pub reranker: Option<Arc<dyn Reranker>>,
    /// Web search configuration, when a provider is configured. Absent when
    /// search is disabled; `web_search` then refuses.
    pub search: Option<crate::tools::search::SearchConfig>,
    /// Vision model configuration for `view_image`. Absent when unconfigured;
    /// the tool then refuses.
    pub vision: Option<crate::tools::vision::VisionConfig>,
    /// Background task runner, set by the agent loop whenever the conversation
    /// has filesystem access (workspace or Main System). `None` without access.
    pub background: Option<Arc<BackgroundTasks>>,
    /// Tool ids the user turned off in Settings; they are neither advertised
    /// nor executed.
    pub disabled_tools: Vec<String>,
}

impl ToolContext {}

/// Error text for a file Tool called in a conversation with no folder open.
pub const NO_FOLDER: &str =
    "No folder is open in this conversation. Ask the user to choose one, or answer \
     without file access.";

pub const READ_FILE: &str = "read_file";
pub const VIEW_IMAGE: &str = "view_image";
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
pub const RUN_BACKGROUND: &str = "run_background";
pub const POLL_TASK: &str = "poll_task";
pub const KILL_TASK: &str = "kill_task";
pub const DELEGATE: &str = "delegate";

/// The tool set with its approval classes.
pub const TOOLS: [(&str, ApprovalClass); 18] = [
    (READ_FILE, ApprovalClass::Read),
    (VIEW_IMAGE, ApprovalClass::Read),
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
    (RUN_BACKGROUND, ApprovalClass::Exec),
    (POLL_TASK, ApprovalClass::Read),
    (KILL_TASK, ApprovalClass::Read),
    (DELEGATE, ApprovalClass::Network),
];

pub const SYSTEM_PROMPT: &str = "\
You are Aria, a desktop assistant. You help with everyday conversation and, when
a folder is open, with work on the files inside it.

Rules:
- You have a real terminal through `run_shell` whenever this chat has file
  access. Never tell the user you cannot access their terminal or computer:
  call the Tool and let the approval prompt decide. Commands are
  non-interactive (stdin is closed), so pass flags instead of answering prompts.
- Changes to files, shell commands and web fetches need the user's approval;
  if an approval is denied, explain and propose an alternative.
- Keep the todo list current for multi-step work.
- Use `remember` to persist durable facts the user shares (preferences, names,
  project context) so you can recall them in later conversations. Use `recall`
  when a past fact would help answer. Stored memories are global unless a folder
  is open, in which case they are scoped to that folder plus the global set.
- For broad read-only exploration (mapping a codebase, comparing sources), use
  `delegate` and keep this chat for the decisions and edits.
- Report what you changed and how you verified it.

<artifacts_info>
The assistant can create and reference standalone artifacts during conversations.
Artifacts are meant for substantial, self-contained content (>15 lines) that users
will inspect, iterate on, or use outside the conversation.

Good artifacts:
- Complete code programs, modules or self-contained scripts
- Interactive HTML/CSS/JS web applications, dashboards or widgets
- Standalone SVG drawings or diagrams
- Formatted markdown documents, reports, or articles
- Mermaid diagrams (architecture, flowcharts, sequence diagrams)

Do NOT use artifacts for:
- Brief snippets (<15 lines), explanations, or small examples
- Conversational or contextual replies

Usage:
Wrap artifact content in <antArtifact> tags with attributes:
<antArtifact identifier=\"unique-id\" type=\"MIME-TYPE\" title=\"Descriptive Title\" language=\"LANG\">
...content...
</antArtifact>

Supported types:
- \"application/vnd.ant.code\" (for code; specify language e.g. python, typescript, html, css, rust, etc.)
- \"text/html\" (standalone HTML with inline CSS/JS)
- \"image/svg+xml\" (raw SVG code)
- \"text/markdown\" (full markdown documents)
- \"application/vnd.ant.mermaid\" (Mermaid diagram definition)

When updating an artifact from earlier in the chat, reuse the exact same identifier.
</artifacts_info>";

/// Append when the conversation is the no-folder Main System chat: full device
/// access, so it must not assume a project folder.
pub const MAIN_SYSTEM_PROMPT: &str = "\
This is the Main System chat with access to the whole device.

- Absolute paths are allowed; relative paths resolve from Aria's base
  directory (named in the house block). Do not assume a project folder.
- Prefer read_file, list_dir, search_files and the mutating file tools over
  shell for file work. Pass `path` to search_files to search any directory.
- `run_shell` is a real terminal on this device: use it for anything the file
  tools do not cover (builds, tests, git, package managers, system commands).
- Long or unattended work (builds, servers, downloads) should use
  `run_background`, then `poll_task` to read its status and output. Stop one
  you no longer need with `kill_task`. Plain shell commands may still ask for
  approval.
- Shell commands start in Aria's base directory (empty by design), not the
  user's project or home; give them absolute paths or `cd` first.";

/// Appended when the conversation has a folder open.
pub const FOLDER_PROMPT: &str = "\
A folder is open in this conversation. Its tools act on that folder.

- All paths are relative to the folder root. Never try to leave it.
- `run_shell` is a real terminal in this folder: use it for builds, tests, git
  and anything the file tools do not cover.
- Prefer search_files and read_file before editing.
- edit_file replaces an exact substring that must occur exactly once; include
  enough context to make it unique. Use write_file only for new files.
- Long or unattended work (builds, servers, training) should use
  `run_background` instead of `run_shell`, then `poll_task` to read its
  status and output. Chain by feeding a task's output into the next tool:
  start it, do other work, then `poll_task` and act on the result. Stop one
  you no longer need with `kill_task`.";

/// Appended when no folder is open, so the model does not promise file work it
/// cannot do.
pub const NO_FOLDER_PROMPT: &str = "\
No folder is open in this conversation, so you have no files or shell. Answer
questions and fetch web pages. If the user asks for work on files, offer to
open a folder from the conversation header and let them pick one.";

/// Appended in Creative mode: the conversation is about building or changing
/// Aria itself, so the source checkout is the project.
pub const CREATIVE_PROMPT: &str = "\
You are in Creative mode: you help the user build and customize Aria itself.

- Treat Aria's source checkout as the project. Read GLOSSARY.md, AGENTS.md and
  docs/adr before changing behavior, and follow the structure already there.
- Prefer the file tools over shell. After changing code, run the checks the
  project uses (`cargo test -p aria-core`, `cargo check --workspace`, and the
  UI type check) and report what passed.
- A running app picks up core changes on restart; say what needs a restart or
  rebuild instead of implying a change is already live.
- If this build has no source checkout, say so and help the user get one
  rather than pretending to edit Aria.
- If a folder is open, change that folder; the source checkout is reachable
  from the Main System chat.
- Outside the user's request, leave Aria's files alone.";

/// External command-line tools worth naming to the model: aliases to probe and
/// a hint for when to reach for them. A program that shadows one of these names
/// with different behavior is still just a read-only shell command to us.
const CLIS: &[(&[&str], &str)] = &[
    (&["rg"], "fast regex content search (`rg -n PATTERN PATH`)"),
    (&["fd", "fdfind"], "fast file-name search (`fd PATTERN PATH`)"),
    (
        &["ast-grep"],
        "AST/structural code search (`ast-grep run -p PATTERN PATH`)",
    ),
    (&["jq"], "JSON query"),
    (&["bat", "batcat"], "syntax-highlighted file preview"),
    (
        &["fzf"],
        "interactive fuzzy picker; needs a terminal, so mostly unusable from run_shell",
    ),
    (&["zoxide"], "directory history (`zoxide query NAME`)"),
    (&["git"], "version control"),
];

fn on_path(name: &str) -> bool {
    // Same PATH shell commands get, so the prompt never advertises a CLI that
    // `run_shell` cannot find (or hides one it can).
    std::env::split_paths(&shell::shell_path()).any(|dir| {
        dir.join(name).is_file() || (cfg!(windows) && dir.join(format!("{name}.exe")).is_file())
    })
}

/// The CLIs from [`CLIS`] present on `PATH`, as (command to call, hint). Probed
/// once per process so the house prompt can point at faster tools than our own
/// walkers when the device has them.
pub fn available_clis() -> &'static [(&'static str, &'static str)] {
    static FOUND: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    FOUND.get_or_init(|| {
        CLIS.iter()
            .filter_map(|(names, hint)| {
                names
                    .iter()
                    .find(|name| on_path(name))
                    .map(|name| (*name, *hint))
            })
            .collect()
    })
}

/// The app's own version, data directory and (in a dev checkout) source root,
/// so the model can answer questions about Aria instead of guessing.
pub fn house_prompt(config_dir: &Path, system_dir: &Path) -> String {
    let data_dir = config_dir.parent().unwrap_or(config_dir);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .filter(|root| root.join("GLOSSARY.md").is_file())
        .map(|root| format!("- Source checkout: {}\n", root.display()))
        .unwrap_or_else(|| {
            "- Source checkout: not available in this build; do not claim to edit Aria's code.\n"
                .to_string()
        });
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let clis = available_clis();
    let cli_line = if clis.is_empty() {
        "- No extra command-line tools detected on PATH.".to_string()
    } else {
        let list = clis
            .iter()
            .map(|(name, hint)| format!("`{name}` ({hint})"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "- Available on PATH (use via `run_shell` instead of hand-rolling the \
             equivalent): {list}."
        )
    };
    format!(
        "\n\nAbout Aria, the app you run inside:\
        \n- Version {}, a local desktop assistant (Rust core, Tauri host, TypeScript UI).\
        \n- Platform: {} {}; shell commands run with `sh -c`.\
        \n- Home directory: {home}\
        \n- Aria's base directory: {} (shell cwd and where relative paths land).\
        \n- App data: {}\
        \n- Settings: {}/settings.json\
        \n- Database: {}/aria.db\
        \n{source}\
        \n{cli_line}\
        \n- Project docs, when a checkout exists: GLOSSARY.md, AGENTS.md, docs/adr.\
        \nWhen the user asks about Aria itself, read these paths instead of guessing. In a folder-scoped conversation you cannot read them; answer from these facts and point to the Main System chat for live detail.",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        system_dir.display(),
        data_dir.display(),
        config_dir.display(),
        data_dir.display(),
    )
}

pub fn approval_class(tool: &str) -> Option<ApprovalClass> {
    TOOLS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, class)| *class)
}

/// Whether an Exec tool call is a single read-only command that
/// `AutoApproveSafe` may run without asking. Anything that writes, chains or
/// is not recognised as read-only needs an Approval.
pub fn exec_is_safe(tool: &str, args_json: &str) -> bool {
    if !matches!(tool, RUN_SHELL | RUN_BACKGROUND) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_string))
        .is_some_and(|cmd| is_safe_command(&cmd))
}

/// Whether a file Tool call names a path outside a restricted Main System's
/// allowed roots. Such a call asks for approval even under `AutoApproveSafe`.
/// Paths that fail for other reasons (missing parent, unreadable) stay
/// ungated so the Tool reports the real error instead of prompting.
pub fn outside_scope(scope: &Scope, tool: &str, args_json: &str) -> bool {
    if !matches!(scope, Scope::MainSystem { roots, .. } if !roots.is_empty()) {
        return false;
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return false;
    };
    let arg = |key: &str| args.get(key).and_then(|v| v.as_str()).map(Path::new);
    let outside = |resolved: std::result::Result<std::path::PathBuf, crate::workspace::PathError>| {
        matches!(resolved, Err(crate::workspace::PathError::Escapes(_)))
    };
    let read = |p: &Path| outside(scope.resolve_read(p));
    let create = |p: &Path| outside(scope.resolve_create(p));
    match tool {
        READ_FILE | VIEW_IMAGE | DELETE_FILE => arg("path").is_some_and(|p| read(p)),
        // `list_dir` defaults to ".", which may itself be outside a root.
        LIST_DIR => read(arg("path").unwrap_or_else(|| Path::new("."))),
        EDIT_FILE | WRITE_FILE => arg("path").is_some_and(|p| create(p)),
        // A search may name any directory; the default searches allowed roots.
        SEARCH_FILES => arg("path").is_some_and(|p| read(p)),
        MOVE_FILE => {
            arg("from").is_some_and(|p| read(p)) || arg("to").is_some_and(|p| create(p))
        }
        _ => false,
    }
}

/// The resolved paths a file Tool call names, used to widen a restricted Main
/// System only as far as an approved call needs. Empty when the call names no
/// path; a path that cannot be resolved stays outside (the Tool errors).
pub fn approval_targets(scope: &Scope, tool: &str, args_json: &str) -> Vec<std::path::PathBuf> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return Vec::new();
    };
    // Resolve without the allow-list, which is exactly what made the call ask.
    let open = match scope {
        Scope::MainSystem { base, .. } => Scope::MainSystem {
            roots: Vec::new(),
            base: base.clone(),
        },
        other => other.clone(),
    };
    let arg = |key: &str| args.get(key).and_then(|v| v.as_str()).map(Path::new);
    let target = |resolved: std::result::Result<std::path::PathBuf, crate::workspace::PathError>| {
        resolved.ok()
    };
    let mut out = Vec::new();
    match tool {
        READ_FILE | VIEW_IMAGE | DELETE_FILE => {
            out.extend(arg("path").and_then(|p| target(open.resolve_read(p))));
        }
        LIST_DIR => out.extend(target(open.resolve_read(
            arg("path").unwrap_or_else(|| Path::new(".")),
        ))),
        EDIT_FILE | WRITE_FILE => {
            out.extend(arg("path").and_then(|p| target(open.resolve_create(p))));
        }
        SEARCH_FILES => out.extend(arg("path").and_then(|p| target(open.resolve_read(p)))),
        MOVE_FILE => {
            out.extend(arg("from").and_then(|p| target(open.resolve_read(p))));
            out.extend(arg("to").and_then(|p| target(open.resolve_create(p))));
        }
        _ => {}
    }
    out
}

/// JSON Schema descriptions handed to the model, plus dispatch.
pub struct ToolRegistry;

impl ToolRegistry {
    /// The tools to advertise. Without a folder only the filesystem-free ones
    /// are offered, so the model is never told about tools it cannot use.
    /// `disabled` lists tool ids the user turned off in Settings.
    pub fn describe(scope: &Scope, disabled: &[String]) -> Vec<ToolSpec> {
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
                VIEW_IMAGE,
                "Inspect and describe an image file using the vision model. Accepts workspace paths and an optional question.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative path" },
                        "question": { "type": "string", "description": "What to look for; defaults to a general description" }
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
                "Search file contents by regular expression, optionally limited by a glob. Respects .gitignore. Pass `path` to search a specific directory; otherwise the home directory (or the allowed roots) is searched.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust regex" },
                        "path": { "type": "string", "description": "Directory to search; absolute or relative to the base" },
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
                "List stored memories. With `query`, fuses keyword (FTS5) and \
                 semantic matches; without it, newest first. Returns the \
                 folder's memories plus global ones.",
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
            spec(
                RUN_BACKGROUND,
                "Start a shell command that runs in the background beyond this turn. Returns a task id to poll with `poll_task`.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string", "description": "Shell command to run with `sh -c`" }
                    },
                    "required": ["cmd"]
                }),
            ),
            spec(
                POLL_TASK,
                "Check a background task's status and recent output. Returns status (running|succeeded|failed|killed) and a tail of its log.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Background task id from `run_background`" }
                    },
                    "required": ["id"]
                }),
            ),
            spec(
                KILL_TASK,
                "Stop a background task started in this conversation and reap its process group.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Background task id from `run_background`" }
                    },
                    "required": ["id"]
                }),
            ),
            spec(
                DELEGATE,
                "Hand a self-contained research question to a read-only sub-agent and get back a concise report. It can read files, list folders, search, fetch pages and recall memories, but cannot change anything. Use it for broad exploration you do not want to fill this chat with; do the actual edits yourself.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "What to find out, and what the report should contain" }
                    },
                    "required": ["task"]
                }),
            ),
        ];
        if !matches!(scope, Scope::None) {
            return all.into_iter()
                .filter(|s| !disabled.contains(&s.name))
                .collect();
        }
        let usable = [WEB_FETCH, WEB_SEARCH, TODOS, REMEMBER, RECALL, DELEGATE];
        all.into_iter()
            .filter(|s| usable.contains(&s.name.as_str()))
            .filter(|s| !disabled.contains(&s.name))
            .collect()
    }

    /// Run one tool. Every failure is a [`ToolOutcome`] marked as an error, never
    /// a panic and never an aborted Run.
    pub async fn execute(tool: &str, args: &str, ctx: &ToolContext) -> ToolOutcome {
        if ctx.disabled_tools.iter().any(|t| t == tool) {
            return ToolOutcome::error(format!("tool `{tool}` is disabled in Settings"));
        }
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
            VIEW_IMAGE => vision::view_image(ctx, parse_args(args)?).await.map(outcome),
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
            RUN_BACKGROUND => run_background(ctx, parse_args(args)?).map(outcome),
            POLL_TASK => poll_task(ctx, parse_args(args)?).map(outcome),
            KILL_TASK => kill_task(ctx, parse_args(args)?).await.map(outcome),
            // Needs the live provider, so the Run loop intercepts it.
            DELEGATE => Err(ToolError::Refused("delegate needs a live Run".into())),
            other => Err(ToolError::Args(format!("unknown tool {other}"))),
        }
    }
}

fn outcome(text: String) -> ToolOutcome {
    ToolOutcome::ok(text)
}

/// `run_background`: detach a shell command. Returns the task id immediately;
/// the real work runs in the task's own tokio process, independent of this Run.
fn run_background(ctx: &ToolContext, args: RunBackgroundArgs) -> Result<String> {
    let bg = ctx
        .background
        .as_ref()
        .ok_or_else(|| ToolError::Refused("no background runner for this session".into()))?;
    // The task is filed under its Run's conversation; resolved from run_id so
    // the context need not carry a separate conversation_id field.
    let conversation_id = ctx
        .db
        .call(|c| crate::db::get_run(c, &ctx.run_id))?
        .conversation_id;
    let task = bg.start(&conversation_id, &args.cmd)?;
    Ok(format!("background task {} started", task.id))
}

fn poll_task(ctx: &ToolContext, args: PollTaskArgs) -> Result<String> {
    let bg = ctx
        .background
        .as_ref()
        .ok_or_else(|| ToolError::Refused("no background runner for this session".into()))?;
    let conversation_id = ctx
        .db
        .call(|c| crate::db::get_run(c, &ctx.run_id))?
        .conversation_id;
    let (task, tail) = bg.poll(&args.id)?;
    if task.conversation_id != conversation_id {
        return Err(ToolError::Refused(format!("no background task {}", args.id)));
    }
    Ok(format!(
        "status: {}\nexit_code: {}\n{}",
        task.status.as_str(),
        task.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
        tail,
    ))
}

/// Stop a background task that belongs to this conversation. Only tasks the
/// model itself started here are reachable, so this needs no approval of its
/// own even though it terminates a process.
async fn kill_task(ctx: &ToolContext, args: PollTaskArgs) -> Result<String> {
    let bg = ctx
        .background
        .as_ref()
        .ok_or_else(|| ToolError::Refused("no background runner for this session".into()))?;
    let conversation_id = ctx
        .db
        .call(|c| crate::db::get_run(c, &ctx.run_id))?
        .conversation_id;
    let (task, _) = bg.poll(&args.id)?;
    if task.conversation_id != conversation_id {
        return Err(ToolError::Refused(format!("no background task {}", args.id)));
    }
    let task = bg.kill(&args.id).await?;
    Ok(format!("background task {} {}", task.id, task.status.as_str()))
}

#[derive(Debug, serde::Deserialize)]
struct RunBackgroundArgs {
    cmd: String,
}

#[derive(Debug, serde::Deserialize)]
struct PollTaskArgs {
    id: String,
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
        VIEW_IMAGE => (
            format!("View image {}", get("path")),
            get("question"),
        ),
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
        DELEGATE => ("Delegate a research task".to_string(), get("task")),
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
        journal_id: workspace_id.to_string(),
        run_id: run_id.to_string(),
        db,
        blobs,
        shell_timeout_secs: 120,
        embed: None,
        reranker: None,
        search: None,
        vision: None,
        background: None,
        disabled_tools: Vec::new(),
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

//! The Run loop: Turns, Tool Calls, Approvals, Caps and cancellation.
//!
//! The database is the source of truth (ADR-0001). Every step of a Run changes
//! rows, so a crashed or closed window can resume by re-reading them; the
//! in-memory [`RunEvent`] broadcast is only a convenience for the UI.
//!
//! One [`Agent`] drives Runs; each started Run returns a [`RunHandle`] to watch
//! its events, answer Approvals and cancel it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::config::{Caps, Config, Settings};
use crate::db::{
    self, Approval, ApprovalClass, Db, DbError, Decision, Role, Run, RunState, ToolCall,
    ToolCallState,
};
use crate::journal::Blobs;
use crate::provider::{ChatMessage, Provider, ProviderEvent, ToolCallRequest};
use crate::tools::{self, ToolContext, ToolOutcome, ToolRegistry};
use crate::tools::search::{SearchConfig, SearchProvider};
use crate::workspace::{Scope, WorkspaceRoot};

/// How often a parked or streaming Run checks for cancellation and Approval
/// decisions. Small enough to feel instant, large enough to cost nothing.
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("workspace: {0}")]
    Workspace(#[from] crate::workspace::PathError),
    #[error("provider: {0}")]
    Provider(#[from] crate::provider::ProviderError),
    #[error("a run is already active in this conversation")]
    Busy,
    #[error("run {0} is not awaiting an approval: {1}")]
    NotAwaitingApproval(String, String),
    #[error("run {0} cannot be resumed from state {1}")]
    NotResumable(String, RunState),
    #[error("background task failed: {0}")]
    Task(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// A Tool Call waiting for the user, described without side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub tool_call_id: String,
    pub tool: String,
    pub class: ApprovalClass,
    pub summary: tools::ApprovalSummary,
}

/// Everything needed to render a Run without re-querying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run: Run,
    pub pending: Vec<PendingApproval>,
    pub todos: Vec<db::Todo>,
}

/// Progress of one Run, broadcast to every subscriber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The Run changed state (including its final state).
    State { run: Run },
    /// A Turn started; `turn_index` is 0-based within the Run.
    TurnStarted { turn_index: i64 },
    /// Streaming assistant text: the delta and the Turn's text so far.
    TextDelta { turn_index: i64, text: String, so_far: String },
    /// The Turn's assistant message is persisted.
    AssistantMessage { message_id: String, content: String },
    /// A Tool Call needs an Approval before it runs.
    ApprovalRequested { pending: PendingApproval },
    /// A Tool Call finished with a Tool Result.
    ToolResult {
        tool_call_id: String,
        tool: String,
        text: String,
        is_error: bool,
    },
    /// Live output lines from `run_shell`.
    ToolOutput { tool_call_id: String, line: String },
    /// The Run's todo list was replaced.
    Todos { todos: Vec<db::Todo> },
    /// The soft Cap threshold was crossed; the model was nudged to wrap up.
    SoftCap { turns: u32, prompt_tokens: u64 },
}

/// How to build a [`Provider`] for a Run: an injected fake in tests, the
/// settings-backed OpenAI client in the app.
pub enum ProviderSource {
    Fixed(Arc<dyn Provider>),
    Settings { settings: Settings, api_key: String },
}

impl ProviderSource {
    fn build(&self) -> std::result::Result<AnyProvider, crate::provider::ProviderError> {
        match self {
            ProviderSource::Fixed(p) => Ok(AnyProvider::Custom(Arc::clone(p))),
            ProviderSource::Settings { settings, api_key } => Ok(AnyProvider::OpenAi(
                crate::provider::OpenAiProvider::new(settings.clone(), api_key.clone())?,
            )),
        }
    }
}

/// The two concrete providers. Kept as an enum because `Provider` has an
/// `async fn` and so cannot be turned into a trait object.
enum AnyProvider {
    OpenAi(crate::provider::OpenAiProvider),
    Custom(Arc<dyn Provider>),
}

impl AnyProvider {
    /// Embedding capability, when the provider supports it. Enables memory
    /// recall to rank by similarity; `None` for test fakes.
    fn embedder(&self) -> Option<Arc<dyn crate::provider::Embedder>> {
        match self {
            AnyProvider::OpenAi(p) => Some(Arc::new(p.clone())),
            AnyProvider::Custom(_) => None,
        }
    }
}

/// Handle to a live Run: watch it, cancel it, answer its Approvals.
#[derive(Clone)]
pub struct RunHandle {
    run_id: String,
    inner: Arc<Mutex<HandleInner>>,
}

struct HandleInner {
    cancelled: bool,
    events: broadcast::Sender<RunEvent>,
}

impl RunHandle {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Subscribe to this Run's events. A late subscriber only sees events from
    /// now on; load the persisted state with [`Agent::snapshot`] to catch up.
    pub fn subscribe(&self) -> broadcast::Receiver<RunEvent> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).events.subscribe()
    }

    /// Ask the Run to stop; it ends `cancelled`. Effective during streaming and
    /// while parked on an Approval.
    pub fn cancel(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).cancelled
    }
}

pub struct Agent {
    db: Arc<Db>,
    blobs: Arc<Blobs>,
    caps: Caps,
    shell_timeout_secs: u64,
    source: ProviderSource,
    config: Option<Arc<Config>>,
}

impl Agent {
    pub fn new(db: Arc<Db>, blobs: Arc<Blobs>, caps: Caps, source: ProviderSource) -> Self {
        Self {
            db,
            blobs,
            caps,
            shell_timeout_secs: 120,
            source,
            config: None,
        }
    }

    /// Change the default shell timeout for this agent's Runs.
    pub fn with_shell_timeout(mut self, secs: u64) -> Self {
        self.shell_timeout_secs = secs;
        self
    }

    /// Carry the app config so search keys can be read at drive time.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// Append the user's message and start a Run for it.
    pub async fn start(&self, conversation_id: &str, user_text: &str) -> Result<RunHandle> {
        let db = Arc::clone(&self.db);
        let conv = conversation_id.to_string();
        let text = user_text.to_string();
        let run = spawn_blocking(move || {
            db.call(|c| {
                if let Some(run) = db::latest_run(c, &conv)?
                    && !run.state.is_terminal()
                {
                    return Err(DbError::InvalidValue {
                        field: "run",
                        value: format!("{} is still {}", run.id, run.state),
                    });
                }
                db::insert_message(c, &conv, None, Role::User, &text)?;
                db::insert_run(c, &conv)
            })
        })
        .await?;
        self.spawn_loop(run.id)
    }

    /// Reopen a `budget_exhausted` Run and keep going.
    pub async fn resume(&self, run_id: &str) -> Result<RunHandle> {
        let db = Arc::clone(&self.db);
        let id = run_id.to_string();
        let run = spawn_blocking(move || {
            db.call(|c| {
                let run = db::get_run(c, &id)?;
                if run.state != RunState::BudgetExhausted {
                    return Err(DbError::InvalidValue {
                        field: "run state",
                        value: run.state.to_string(),
                    });
                }
                db::reopen_run(c, &id)?;
                db::get_run(c, &id)
            })
        })
        .await?;
        if run.state == RunState::BudgetExhausted {
            return Err(AgentError::NotResumable(run.id, run.state));
        }
        self.spawn_loop(run.id)
    }

    /// Take over a Run that a previous process left `running` or
    /// `awaiting_approval`. The loop re-reads everything from the database, so
    /// this continues the same transcript. In-flight provider Turns did not
    /// finish, so they are simply asked for again.
    pub async fn resume_interrupted(&self, run_id: &str) -> Result<RunHandle> {
        let db = Arc::clone(&self.db);
        let id = run_id.to_string();
        let run = spawn_blocking(move || {
            db.call(|c| {
                let run = db::get_run(c, &id)?;
                match run.state {
                    RunState::Running | RunState::AwaitingApproval => Ok::<_, DbError>(run),
                    _ => Err(DbError::InvalidValue {
                        field: "run state",
                        value: format!("{} is not interrupted", run.state),
                    }),
                }
            })
        })
        .await?;
        self.spawn_loop(run.id)
    }

    /// Persist a decision on a pending Approval. The loop picks it up within
    /// [`POLL`]. Deciding twice is an error.
    pub fn answer_approval(&self, approval_id: &str, decision: Decision) -> Result<()> {
        let approval = self.db.call(|c| db::get_approval(c, approval_id))?;
        let call = self.db.call(|c| db::get_tool_call(c, &approval.tool_call_id))?;
        if call.state != ToolCallState::Pending {
            return Err(AgentError::NotAwaitingApproval(
                call.run_id,
                format!("{} is {}", call.id, call.state),
            ));
        }
        self.db.call(|c| db::decide_approval(c, approval_id, decision))?;
        Ok(())
    }

    /// Persisted state for a UI that just opened the conversation.
    pub fn snapshot(&self, run_id: &str) -> Result<RunSnapshot> {
        let run = self.db.call(|c| db::get_run(c, run_id))?;
        let mut pending = Vec::new();
        for call in self.db.call(|c| db::pending_tool_calls(c, run_id))? {
            if let Some(approval) = self.db.call(|c| db::approval_for_tool_call(c, &call.id))?
                && approval.decision.is_none()
            {
                pending.push(pending_approval(&approval, &call));
            }
        }
        let todos = self.db.call(|c| db::list_todos(c, run_id))?;
        Ok(RunSnapshot { run, pending, todos })
    }

    fn spawn_loop(&self, run_id: String) -> Result<RunHandle> {
        let (events, _) = broadcast::channel(1024);
        let handle = RunHandle {
            run_id: run_id.clone(),
            inner: Arc::new(Mutex::new(HandleInner {
                cancelled: false,
                events,
            })),
        };
        let provider = self.source.build()?;
        let search = match &self.source {
            ProviderSource::Settings { settings, .. } => {
                let provider = settings.search_provider.as_deref().and_then(SearchProvider::parse);
                provider.map(|p| {
                    let api_key = self
                        .config
                        .as_ref()
                        .and_then(|c| c.search_api_key().ok().flatten().map(|(k, _)| k));
                    SearchConfig {
                        provider: p,
                        api_key,
                        base_url: settings.search_base_url.clone(),
                    }
                })
            }
            ProviderSource::Fixed(_) => None,
        };
        let ctx = LoopCtx {
            db: Arc::clone(&self.db),
            blobs: Arc::clone(&self.blobs),
            caps: self.caps.clone(),
            shell_timeout_secs: self.shell_timeout_secs,
            provider,
            handle: handle.clone(),
            run_id,
            soft_warned: false,
            search,
        };
        tokio::spawn(ctx.run_to_end());
        Ok(handle)
    }
}

async fn spawn_blocking<T: Send + 'static>(
    f: impl FnOnce() -> db::Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AgentError::Task(e.to_string()))?
        .map_err(AgentError::Db)
}

fn pending_approval(approval: &Approval, call: &ToolCall) -> PendingApproval {
    PendingApproval {
        approval_id: approval.id.clone(),
        tool_call_id: call.id.clone(),
        tool: call.tool.clone(),
        class: call.approval_class,
        summary: tools::summarize(&call.tool, &call.args_json),
    }
}

/// State owned by one Run. Nothing here outlives the process except what the
/// loop has already written to the database.
struct LoopCtx {
    db: Arc<Db>,
    blobs: Arc<Blobs>,
    caps: Caps,
    shell_timeout_secs: u64,
    provider: AnyProvider,
    handle: RunHandle,
    run_id: String,
    soft_warned: bool,
    search: Option<SearchConfig>,
}

impl LoopCtx {
    fn emit(&self, event: RunEvent) {
        let inner = self.handle.inner.lock().unwrap_or_else(|p| p.into_inner());
        let _ = inner.events.send(event);
    }

    fn call<T>(&self, f: impl FnOnce(&Connection) -> db::Result<T>) -> Result<T> {
        Ok(self.db.call(f)?)
    }

    async fn run_to_end(mut self) {
        let terminal = match self.drive().await {
            Ok(state) => state,
            Err(AgentError::Provider(e)) => {
                let msg = e.to_string();
                let _ = self.set_state(RunState::Errored, Some("provider_error"), Some(&msg));
                RunState::Errored
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = self.set_state(RunState::Errored, Some("agent_error"), Some(&msg));
                RunState::Errored
            }
        };
        // Cancel can win a race with the end of a Turn; the row is the truth.
        if terminal != RunState::Cancelled && self.handle.is_cancelled() {
            let _ = self.set_state(RunState::Cancelled, Some("cancelled"), None);
        }
    }

    fn set_state(&self, state: RunState, reason: Option<&str>, error: Option<&str>) -> Result<()> {
        self.call(|c| db::set_run_state(c, &self.run_id, state, reason, error))?;
        let run = self.call(|c| db::get_run(c, &self.run_id))?;
        self.emit(RunEvent::State { run });
        Ok(())
    }

    fn finish(&self, state: RunState, reason: &str) -> Result<RunState> {
        self.set_state(state, Some(reason), None)?;
        Ok(state)
    }

    /// The body of the loop. Returns the terminal state that was persisted.
    async fn drive(&mut self) -> Result<RunState> {
        let run = self.call(|c| db::get_run(c, &self.run_id))?;
        let conversation = self.call(|c| db::get_conversation(c, &run.conversation_id))?;
        // The main session has no folder; only filesystem-free tools apply.
        let workspace = match &conversation.workspace_id {
            Some(id) => Some(self.call(|c| db::get_workspace(c, id))?),
            None => None,
        };
        let scope = if let Some(ws) = &workspace {
            Scope::Workspace(WorkspaceRoot::new(&ws.root_path)?)
        } else if conversation.main_system {
            let paths = self
                .call(|c| db::list_main_system_paths(c, &conversation.id))?
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            Scope::MainSystem(paths)
        } else {
            Scope::None
        };
        let ctx = ToolContext {
            root: workspace
                .as_ref()
                .map(|ws| WorkspaceRoot::new(&ws.root_path))
                .transpose()?,
            workspace_id: workspace.as_ref().map(|ws| ws.id.clone()),
            run_id: self.run_id.clone(),
            db: Arc::clone(&self.db),
            blobs: Arc::clone(&self.blobs),
            shell_timeout_secs: self.shell_timeout_secs,
            embed: self.provider.embedder(),
            scope: scope.clone(),
            search: self.search.clone(),
        };

        loop {
            if self.handle.is_cancelled() {
                return self.finish(RunState::Cancelled, "cancelled");
            }

            // Tool Calls from the previous Turn: wait for and run them.
            if self.work_left()? {
                if !self.work_off(&ctx).await? {
                    return self.finish(RunState::Cancelled, "cancelled");
                }
                if self.caps_reached()? {
                    return self.finish(RunState::BudgetExhausted, "budget_exhausted");
                }
                continue;
            }

            if self.caps_reached()? {
                return self.finish(RunState::BudgetExhausted, "budget_exhausted");
            }
            if !self.soft_warned && self.soft_limit()? {
                self.soft_warned = true;
                let run = self.call(|c| db::get_run(c, &self.run_id))?;
                self.emit(RunEvent::SoftCap {
                    turns: run.turn_count as u32,
                    prompt_tokens: run.prompt_tokens as u64,
                });
            }

            match self.next_turn(&run.conversation_id, &scope).await? {
                TurnOutcome::Final => return self.finish(RunState::Finished, "no_tool_calls"),
                TurnOutcome::ToolCalls => {}
                TurnOutcome::Cancelled => return self.finish(RunState::Cancelled, "cancelled"),
            }
        }
    }

    fn caps_reached(&self) -> Result<bool> {
        let run = self.call(|c| db::get_run(c, &self.run_id))?;
        Ok(self
            .caps
            .hard_limit_reached(run.turn_count as u32, run.prompt_tokens as u64))
    }

    fn soft_limit(&self) -> Result<bool> {
        let run = self.call(|c| db::get_run(c, &self.run_id))?;
        Ok(self
            .caps
            .soft_limit_reached(run.turn_count as u32, run.prompt_tokens as u64))
    }

    /// Whether there are Tool Calls still to answer or run.
    fn work_left(&self) -> Result<bool> {
        if !self.call(|c| db::executable_tool_calls(c, &self.run_id))?.is_empty() {
            return Ok(true);
        }
        Ok(!self.call(|c| db::pending_tool_calls(c, &self.run_id))?.is_empty())
    }

    /// Answer pending Approvals, then run every approved call. Returns `false`
    /// if the Run was cancelled while waiting.
    async fn work_off(&mut self, ctx: &ToolContext) -> Result<bool> {
        loop {
            if self.handle.is_cancelled() {
                return Ok(false);
            }
            let unanswered = self
                .call(|c| db::pending_tool_calls(c, &self.run_id))?
                .into_iter()
                .filter(|call| {
                    self.call(|c| db::approval_for_tool_call(c, &call.id))
                        .ok()
                        .flatten()
                        .is_some_and(|a| a.decision.is_none())
                })
                .collect::<Vec<_>>();

            if !unanswered.is_empty() {
                // Park. The row records the wait; the poll ends it.
                if self.call(|c| db::get_run(c, &self.run_id))?.state != RunState::AwaitingApproval {
                    self.set_state(RunState::AwaitingApproval, None, None)?;
                }
                tokio::time::sleep(POLL).await;
                continue;
            }

            let pending = self.call(|c| db::pending_tool_calls(c, &self.run_id))?;
            if !pending.is_empty() {
                for call in &pending {
                    self.record_decision(call)?;
                }
                self.set_state(RunState::Running, None, None)?;
                continue;
            }

            let ready = self.call(|c| db::executable_tool_calls(c, &self.run_id))?;
            if ready.is_empty() {
                return Ok(true);
            }
            for call in ready {
                if self.handle.is_cancelled() {
                    return Ok(false);
                }
                self.run_one(ctx, &call).await?;
            }
        }
    }

    /// Apply the approval decision of a pending call.
    fn record_decision(&self, call: &ToolCall) -> Result<()> {
        let decision = self
            .call(|c| db::approval_for_tool_call(c, &call.id))?
            .and_then(|a| a.decision);
        match decision {
            Some(Decision::Approve) => {
                self.call(|c| db::set_tool_call_state(c, &call.id, ToolCallState::Approved))?;
            }
            Some(Decision::Deny) => {
                let text = format!("Denied by the user; {} was not run.", call.tool);
                self.call(|c| db::finish_tool_call(c, &call.id, &text, true))?;
                self.call(|c| db::set_tool_call_state(c, &call.id, ToolCallState::Denied))?;
                self.emit(RunEvent::ToolResult {
                    tool_call_id: call.id.clone(),
                    tool: call.tool.clone(),
                    text,
                    is_error: true,
                });
            }
            None => {
                return Err(AgentError::NotAwaitingApproval(
                    self.run_id.clone(),
                    format!("{} has no decision", call.id),
                ));
            }
        }
        Ok(())
    }

    async fn run_one(&self, ctx: &ToolContext, call: &ToolCall) -> Result<()> {
        let outcome = if call.tool == tools::RUN_SHELL {
            self.run_shell(ctx, call).await
        } else {
            ToolRegistry::execute(&call.tool, &call.args_json, ctx).await
        };

        self.call(|c| db::finish_tool_call(c, &call.id, &outcome.text, outcome.is_error))?;
        self.emit(RunEvent::ToolResult {
            tool_call_id: call.id.clone(),
            tool: call.tool.clone(),
            text: outcome.text,
            is_error: outcome.is_error,
        });
        if call.tool == tools::TODOS {
            let todos = self.call(|c| db::list_todos(c, &self.run_id))?;
            self.emit(RunEvent::Todos { todos });
        }
        Ok(())
    }

    /// `run_shell` alone streams output to the UI as it is produced.
    async fn run_shell(&self, ctx: &ToolContext, call: &ToolCall) -> ToolOutcome {
        let args = match serde_json::from_str::<tools::ShellArgs>(&call.args_json) {
            Ok(args) => args,
            Err(e) => return ToolOutcome::error(format!("invalid arguments: {e}")),
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let forward = {
            let call_id = call.id.clone();
            let inner = Arc::clone(&self.handle.inner);
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    let guard = inner.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = guard.events.send(RunEvent::ToolOutput {
                        tool_call_id: call_id.clone(),
                        line,
                    });
                }
            })
        };
        let result = tools::run_shell(ctx, args, Some(tx)).await;
        let _ = forward.await;
        match result {
            Ok(text) => ToolOutcome::ok(text),
            Err(e) => ToolOutcome::error(e.to_string()),
        }
    }

    /// Request one Turn and persist its outcome.
    async fn next_turn(&mut self, conversation_id: &str, scope: &Scope) -> Result<TurnOutcome> {
        let run = self.call(|c| db::get_run(c, &self.run_id))?;
        // Resuming resets the counters, so the next Turn's index continues
        // from the Turns already recorded rather than from `turn_count`.
        let turn_index = self.call(|c| db::list_tool_calls(c, &self.run_id))?
            .iter()
            .map(|c| c.turn_index + 1)
            .max()
            .unwrap_or(0)
            .max(self.assistant_turns(&run)?);
        let messages = self.request_messages(&run, scope.is_folder())?;
        let specs = ToolRegistry::describe(scope);
        self.emit(RunEvent::TurnStarted { turn_index });

        let result = self.stream(&messages, &specs, turn_index).await?;
        let result = match result {
            Some(result) => result,
            None => return Ok(TurnOutcome::Cancelled),
        };

        let message = self.call(|c| {
            db::insert_message(
                c,
                conversation_id,
                Some(&self.run_id),
                Role::Assistant,
                &result.text,
            )
        })?;
        self.emit(RunEvent::AssistantMessage {
            message_id: message.id,
            content: result.text.clone(),
        });
        self.call(|c| {
            db::record_turn_usage(c, &self.run_id, result.prompt_tokens.unwrap_or(0) as i64)
        })?;

        if result.tool_calls.is_empty() {
            return Ok(TurnOutcome::Final);
        }
        for (index, request) in result.tool_calls.iter().enumerate() {
            self.persist_tool_call(turn_index, index as i64, request)?;
        }
        Ok(TurnOutcome::ToolCalls)
    }

    /// Stream one Turn, forwarding text to subscribers. `None` means the user
    /// cancelled mid-stream.
    async fn stream(
        &self,
        messages: &[ChatMessage],
        specs: &[crate::provider::ToolSpec],
        turn_index: i64,
    ) -> Result<Option<crate::provider::TurnResult>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<ProviderEvent>();
        let (messages, specs) = (messages.to_vec(), specs.to_vec());
        let future = match &self.provider {
            AnyProvider::OpenAi(p) => p.stream_chat(&messages, &specs, tx),
            AnyProvider::Custom(p) => p.stream_chat(&messages, &specs, tx),
        };
        tokio::pin!(future);

        let mut so_far = String::new();
        loop {
            tokio::select! {
                biased;
                event = rx.recv() => match event {
                    Some(ProviderEvent::TextDelta(text)) => {
                        so_far.push_str(&text);
                        self.emit(RunEvent::TextDelta {
                            turn_index,
                            text,
                            so_far: so_far.clone(),
                        });
                    }
                    Some(ProviderEvent::Usage { .. } | ProviderEvent::ToolCall(_)) => {}
                    None => return Ok(Some(future.await?)),
                },
                result = &mut future => return Ok(Some(result?)),
                // Lowest priority: only when nothing is ready, so a fast
                // provider still finishes every Turn it has queued.
                _ = tokio::time::sleep(POLL) => {
                    if self.handle.is_cancelled() {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// How many assistant messages this Run has, i.e. Turns already taken.
    fn assistant_turns(&self, run: &Run) -> Result<i64> {
        let count = self
            .call(|c| db::list_messages(c, &run.conversation_id))?
            .iter()
            .filter(|m| m.role == Role::Assistant && m.run_id.as_deref() == Some(run.id.as_str()))
            .count();
        Ok(count as i64)
    }

    fn persist_tool_call(
        &self,
        turn_index: i64,
        call_index: i64,
        request: &ToolCallRequest,
    ) -> Result<()> {
        let class = tools::approval_class(&request.function.name).unwrap_or(ApprovalClass::Read);
        let state = if class.requires_approval() {
            ToolCallState::Pending
        } else {
            ToolCallState::Approved
        };
        let call = self.call(|c| {
            db::insert_tool_call(
                c,
                &self.run_id,
                turn_index,
                call_index,
                &request.function.name,
                &request.function.arguments,
                class,
                state,
            )
        })?;
        if class.requires_approval() {
            let approval = self.call(|c| db::insert_approval(c, &call.id))?;
            self.emit(RunEvent::ApprovalRequested {
                pending: pending_approval(&approval, &call),
            });
        }
        Ok(())
    }

    /// A system-prompt suffix listing stored memories the model should keep in
    /// mind, so it does not have to recall on every turn. Global memories always
    /// accompany the folder's own, newest first.
    fn memory_block(&self, workspace_id: Option<&str>) -> Result<String> {
        let memories = self
            .call(|c| db::list_memories(c, workspace_id))?
            .into_iter()
            .take(20)
            .map(|m| format!("- [{}] {}", m.key, m.content))
            .collect::<Vec<_>>();
        Ok(if memories.is_empty() {
            String::new()
        } else {
            format!("\n\nStored memories:\n{}", memories.join("\n"))
        })
    }

    /// System prompt plus the conversation so far, with each Turn's Tool Calls
    /// (as the assistant asked them) and Tool Results (as the tools returned).
    fn request_messages(&self, run: &Run, has_folder: bool) -> Result<Vec<ChatMessage>> {
        let conversation = self.call(|c| db::get_conversation(c, &run.conversation_id))?;
        let memory_block = self.memory_block(conversation.workspace_id.as_deref())?;

        let prompt = format!(
            "{}\n\n{}{}",
            tools::SYSTEM_PROMPT,
            if has_folder {
                tools::FOLDER_PROMPT
            } else {
                tools::NO_FOLDER_PROMPT
            },
            memory_block,
        );
        let mut messages = vec![ChatMessage::system(prompt)];
        let history = self.call(|c| db::list_messages(c, &run.conversation_id))?;
        let calls = self.call(|c| db::list_tool_calls(c, &self.run_id))?;

        // Assistant messages of this Run, in order, are its Turns.
        let mut turn_of_message: HashMap<String, i64> = HashMap::new();
        let mut turn = 0;
        for msg in &history {
            if msg.role == Role::Assistant && msg.run_id.as_deref() == Some(run.id.as_str()) {
                turn_of_message.insert(msg.id.clone(), turn);
                turn += 1;
            }
        }

        for msg in history {
            match msg.role {
                Role::User => messages.push(ChatMessage::user(msg.content)),
                Role::Tool => {}
                Role::Assistant => {
                    let requests = turn_of_message
                        .get(&msg.id)
                        .map(|turn| {
                            calls
                                .iter()
                                .filter(|c| c.turn_index == *turn)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let asked = requests
                        .iter()
                        .map(|c| {
                            ToolCallRequest::new(c.id.clone(), c.tool.clone(), c.args_json.clone())
                        })
                        .collect::<Vec<_>>();
                    messages.push(ChatMessage::assistant(msg.content, asked));
                    for call in requests {
                        if let Some(result) = &call.result {
                            messages.push(ChatMessage::tool_result(call.id.clone(), result.clone()));
                        }
                    }
                }
            }
        }
        Ok(messages)
    }
}

enum TurnOutcome {
    Final,
    ToolCalls,
    Cancelled,
}

/// Parse a decision from a UI string.
pub fn parse_decision(s: &str) -> Option<Decision> {
    Decision::from_str(s).ok()
}

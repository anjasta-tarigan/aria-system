use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::DbError;

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            other => Err(DbError::InvalidValue {
                field: "role",
                value: other.to_string(),
            }),
        }
    }
}

/// Lifecycle of a [`Run`](crate::GLOSSARY).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    AwaitingApproval,
    Finished,
    BudgetExhausted,
    Cancelled,
    Errored,
}

impl RunState {
    pub const ALL: [RunState; 6] = [
        RunState::Running,
        RunState::AwaitingApproval,
        RunState::Finished,
        RunState::BudgetExhausted,
        RunState::Cancelled,
        RunState::Errored,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::AwaitingApproval => "awaiting_approval",
            RunState::Finished => "finished",
            RunState::BudgetExhausted => "budget_exhausted",
            RunState::Cancelled => "cancelled",
            RunState::Errored => "errored",
        }
    }

    /// Whether the run has ended and cannot progress without user action.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Finished | RunState::BudgetExhausted | RunState::Cancelled | RunState::Errored
        )
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunState {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RunState::ALL
            .into_iter()
            .find(|s2| s2.as_str() == s)
            .ok_or_else(|| DbError::InvalidValue {
                field: "run state",
                value: s.to_string(),
            })
    }
}

/// How much trust a [`Tool`](crate::GLOSSARY) call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass {
    Read,
    Mutate,
    Exec,
    Network,
}

impl ApprovalClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalClass::Read => "read",
            ApprovalClass::Mutate => "mutate",
            ApprovalClass::Exec => "exec",
            ApprovalClass::Network => "network",
        }
    }

    /// Reads run without asking; everything else waits for an [`Approval`].
    pub fn requires_approval(self) -> bool {
        !matches!(self, ApprovalClass::Read)
    }
}

impl fmt::Display for ApprovalClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalClass {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read" => Ok(ApprovalClass::Read),
            "mutate" => Ok(ApprovalClass::Mutate),
            "exec" => Ok(ApprovalClass::Exec),
            "network" => Ok(ApprovalClass::Network),
            other => Err(DbError::InvalidValue {
                field: "approval class",
                value: other.to_string(),
            }),
        }
    }
}

/// Where a Tool Call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    /// Waiting for the user to decide on its Approval.
    Pending,
    /// Approved, not yet executed.
    Approved,
    /// Finished; `result` holds the Tool Result text.
    Done,
    /// The user refused it.
    Denied,
}

impl ToolCallState {
    pub const ALL: [ToolCallState; 4] = [
        ToolCallState::Pending,
        ToolCallState::Approved,
        ToolCallState::Done,
        ToolCallState::Denied,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolCallState::Pending => "pending",
            ToolCallState::Approved => "approved",
            ToolCallState::Done => "done",
            ToolCallState::Denied => "denied",
        }
    }
}

impl fmt::Display for ToolCallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ToolCallState {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ToolCallState::ALL
            .into_iter()
            .find(|s2| s2.as_str() == s)
            .ok_or_else(|| DbError::InvalidValue {
                field: "tool call state",
                value: s.to_string(),
            })
    }
}

/// A persisted decision on one Tool Call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Decision {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approve" => Ok(Decision::Approve),
            "deny" => Ok(Decision::Deny),
            other => Err(DbError::InvalidValue {
                field: "decision",
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub root_path: String,
    pub created_at: i64,
    /// Kept at the top of the sidebar when true.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// `None` for the main session, which has no folder attached.
    pub workspace_id: Option<String>,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Full filesystem access (the "main system"); otherwise workspace-scoped.
    pub main_system: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub conversation_id: String,
    pub state: RunState,
    pub stop_reason: Option<String>,
    pub turn_count: i64,
    pub prompt_tokens: i64,
    pub error: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub run_id: Option<String>,
    pub role: Role,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub run_id: String,
    pub turn_index: i64,
    pub call_index: i64,
    pub tool: String,
    pub args_json: String,
    pub approval_class: ApprovalClass,
    pub state: ToolCallState,
    pub result: Option<String>,
    pub is_error: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub tool_call_id: String,
    pub decision: Option<Decision>,
    pub decided_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub run_id: String,
    pub position: i64,
    pub text: String,
    pub done: bool,
}

/// One FTS5 hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHit {
    pub message_id: String,
    pub conversation_id: String,
    pub role: Role,
    pub snippet: String,
    pub created_at: i64,
}

/// A persisted memory, scoped to a workspace or global (workspace_id NULL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub workspace_id: Option<String>,
    pub key: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<u8>>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Lifecycle of a background [`BackgroundTask`](crate::tools::BackgroundTask).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStatus {
    Running,
    Succeeded,
    Failed,
    Killed,
}

impl BackgroundStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BackgroundStatus::Running => "running",
            BackgroundStatus::Succeeded => "succeeded",
            BackgroundStatus::Failed => "failed",
            BackgroundStatus::Killed => "killed",
        }
    }

    /// Whether the task has ended and will not change again.
    pub fn is_terminal(self) -> bool {
        !matches!(self, BackgroundStatus::Running)
    }
}

impl fmt::Display for BackgroundStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackgroundStatus {
    type Err = DbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(BackgroundStatus::Running),
            "succeeded" => Ok(BackgroundStatus::Succeeded),
            "failed" => Ok(BackgroundStatus::Failed),
            "killed" => Ok(BackgroundStatus::Killed),
            other => Err(DbError::InvalidValue {
                field: "background status",
                value: other.to_string(),
            }),
        }
    }
}

/// A long-running shell command started via the `run_background` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub cmd: String,
    pub conversation_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub status: BackgroundStatus,
    pub log_blob: String,
}

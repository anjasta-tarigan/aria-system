// Typed wrappers around the Tauri commands and the run-event stream.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Role = "user" | "assistant" | "tool";
export type RunState =
  | "running"
  | "awaiting_approval"
  | "finished"
  | "budget_exhausted"
  | "cancelled"
  | "errored";
export type ApprovalClass = "read" | "mutate" | "exec" | "network";
export type ToolCallState = "pending" | "approved" | "done" | "denied";

export interface Run {
  id: string;
  conversation_id: string;
  state: RunState;
  stop_reason: string | null;
  turn_count: number;
  prompt_tokens: number;
  error: string | null;
  started_at: number;
  ended_at: number | null;
}

export interface ToolCall {
  id: string;
  run_id: string;
  turn_index: number;
  call_index: number;
  tool: string;
  args_json: string;
  approval_class: ApprovalClass;
  state: ToolCallState;
  result: string | null;
  is_error: boolean;
  created_at: number;
}

export interface Todo {
  id: string;
  run_id: string;
  position: number;
  text: string;
  done: boolean;
}

export interface MessageView {
  id: string;
  role: Role;
  content: string;
  created_at: number;
  tool_calls: ToolCall[];
}

export interface ConversationView {
  id: string;
  /// `null` for the main session, which has no folder attached.
  workspace_id: string | null;
  title: string;
  root_path: string | null;
  created_at: number;
  updated_at: number;
  /// True when the owning workspace is pinned to the top of the sidebar.
  pinned?: boolean;
  /// True when Aria may read/edit/exec outside any single folder (main session).
  main_system: boolean;
  run: Run | null;
}

export interface ConversationDetail {
  conversation: ConversationView;
  messages: MessageView[];
  todos: Todo[];
  pending: PendingApproval[];
}

export interface PendingApproval {
  approval_id: string;
  tool_call_id: string;
  tool: string;
  class: ApprovalClass;
  summary: { title: string; body: string };
}

export interface MessageHit {
  message_id: string;
  conversation_id: string;
  role: Role;
  snippet: string;
  created_at: number;
}

export interface ChangedFile {
  path: string;
  can_undo: boolean;
}

export interface Settings {
  base_url: string;
  model: string;
  caps: { max_turns: number; max_prompt_tokens: number };
  shell_timeout_secs: number;
  embedding_base_url: string | null;
  embedding_model: string | null;
}

export interface SettingsView {
  settings: Settings;
  has_key: boolean;
  key_backend: "keyring" | "file";
}

export interface RunStateView {
  run: Run;
  pending: PendingApproval[];
}

/** Mirrors `aria_core::agent::RunEvent`. */
export type RunEvent =
  | { type: "state"; run: Run }
  | { type: "turn_started"; turn_index: number }
  | { type: "text_delta"; turn_index: number; text: string; so_far: string }
  | { type: "assistant_message"; message_id: string; content: string }
  | { type: "approval_requested"; pending: PendingApproval }
  | { type: "tool_result"; tool_call_id: string; tool: string; text: string; is_error: boolean }
  | { type: "tool_output"; tool_call_id: string; line: string }
  | { type: "todos"; todos: Todo[] }
  | { type: "soft_cap"; turns: number; prompt_tokens: number };

export interface RunEventEnvelope {
  run_id: string;
  event: RunEvent;
}

export const api = {
  openWorkspace: (rootPath: string) =>
    invoke<ConversationView>("open_workspace", { rootPath }),
  newConversation: () => invoke<ConversationView>("new_conversation"),
  attachWorkspace: (conversationId: string, rootPath: string) =>
    invoke<ConversationView>("attach_workspace", { conversationId, rootPath }),
  newSession: (workspaceId: string) =>
    invoke<ConversationView>("new_session", { workspaceId }),
  renameConversation: (conversationId: string, title: string) =>
    invoke<void>("rename_conversation", { conversationId, title }),
  deleteConversation: (conversationId: string) =>
    invoke<void>("delete_conversation", { conversationId }),
  deleteWorkspace: (workspaceId: string) =>
    invoke<void>("delete_workspace", { workspaceId }),
  setWorkspacePin: (workspaceId: string, pinned: boolean) =>
    invoke<void>("set_workspace_pin", { workspaceId, pinned }),
  listConversations: () => invoke<ConversationView[]>("list_conversations"),
  getConversation: (conversationId: string) =>
    invoke<ConversationDetail>("get_conversation", { conversationId }),
  sendMessage: (conversationId: string, text: string) =>
    invoke<Run>("send_message", { conversationId, text }),
  answerApproval: (approvalId: string, decision: "approve" | "deny") =>
    invoke<void>("answer_approval", { approvalId, decision }),
  cancelRun: () => invoke<void>("cancel_run"),
  resumeRun: (runId: string) => invoke<void>("resume_run", { runId }),
  getRunState: (runId: string) => invoke<RunStateView>("get_run_state", { runId }),
  undo: (workspaceId: string, path: string) =>
    invoke<void>("undo", { workspaceId, path }),
  changedFiles: (workspaceId: string) =>
    invoke<ChangedFile[]>("changed_files", { workspaceId }),
  searchMessages: (query: string, conversationId?: string) =>
    invoke<MessageHit[]>("search_messages", {
      query,
      conversationId: conversationId ?? null,
    }),
  // Settings must be written somewhere private; the Rust side decides where.
  getSettings: () => invoke<SettingsView>("get_settings"),
  setSettings: (input: {
    base_url: string;
    model: string;
    max_turns: number;
    max_prompt_tokens: number;
    shell_timeout_secs: number;
    embedding_base_url?: string | null;
    embedding_model?: string | null;
    api_key?: string | null;
  }) => invoke<SettingsView>("set_settings", { input }),
  listMemories: (workspaceId?: string | null) =>
    invoke<Memory[]>("list_memories", { workspaceId: workspaceId ?? null }),
  deleteMemory: (id: string) => invoke<void>("delete_memory", { id }),
  getMainSystemPaths: (conversationId: string) =>
    invoke<string[]>("get_main_system_paths", { conversationId }),
  setMainSystemScope: (conversationId: string, enabled: boolean, paths: string[]) =>
    invoke<ConversationView>("set_main_system_scope", {
      conversationId,
      enabled,
      paths,
    }),
};

export interface Memory {
  id: string;
  workspace_id: string | null;
  key: string;
  content: string;
  embedding: string | null;
  created_at: number;
  updated_at: number;
}

export function onRunEvent(cb: (envelope: RunEventEnvelope) => void): Promise<UnlistenFn> {
  return listen<RunEventEnvelope>("run-event", (e) => cb(e.payload));
}

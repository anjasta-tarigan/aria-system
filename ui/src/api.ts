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

export type ConversationMode = "normal" | "creative";

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
  /// Normal for everyday work, Creative for changing Aria itself.
  mode: ConversationMode;
  /// Per-chat approval override; `null` follows Settings.
  approval_policy: string | null;
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
  embedding_base_url: string | null;
  embedding_model: string | null;
  search_provider: string | null;
  search_base_url: string | null;
  approval_policy: string;
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

export type BackgroundStatus = "running" | "succeeded" | "failed" | "killed";

export interface BackgroundTask {
  id: string;
  cmd: string;
  conversationId: string;
  startedAt: number;
  endedAt?: number;
  exitCode?: number;
  status: BackgroundStatus;
  logBlob: string;
}

export const api = {
  openWorkspace: (rootPath: string) =>
    invoke<ConversationView>("open_workspace", { rootPath }),
  listCloudFolders: () =>
    invoke<{ name: string; path: string }[]>("list_cloud_folders"),
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
  answerApproval: (
    approvalId: string,
    decision: "approve" | "deny" | "approve_all",
  ) => invoke<void>("answer_approval", { approvalId, decision }),
  cancelRun: (runId: string) => invoke<void>("cancel_run", { runId }),
  resumeRun: (runId: string) => invoke<void>("resume_run", { runId }),
  getRunState: (runId: string) => invoke<RunStateView>("get_run_state", { runId }),
  undo: (conversationId: string, path: string) =>
    invoke<void>("undo", { conversationId, path }),
  changedFiles: (conversationId: string) =>
    invoke<ChangedFile[]>("changed_files", { conversationId }),
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
    embedding_base_url?: string | null;
    embedding_model?: string | null;
    search_provider?: string | null;
    search_base_url?: string | null;
    search_api_key?: string | null;
    api_key?: string | null;
    approval_policy?: string | null;
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
  setConversationMode: (conversationId: string, mode: ConversationMode) =>
    invoke<ConversationView>("set_conversation_mode", { conversationId, mode }),
  setConversationApproval: (conversationId: string, approvalPolicy: string | null) =>
    invoke<ConversationView>("set_conversation_approval", {
      conversationId,
      approvalPolicy,
    }),
  listWorkspaceDir: (conversationId: string, rel: string) =>
    invoke<string[]>("list_workspace_dir", { conversationId, rel }),
  readWorkspaceFile: (conversationId: string, rel: string) =>
    invoke<string>("read_workspace_file", { conversationId, rel }),
  writeWorkspaceFile: (conversationId: string, rel: string, content: string) =>
    invoke<void>("write_workspace_file", { conversationId, rel, content }),
  startBackground: (cmd: string, conversationId: string) =>
    invoke<BackgroundTask>("start_background", { cmd, conversationId }),
  pollBackground: (id: string) =>
    invoke<{ task: BackgroundTask; tail: string }>("poll_background", { id }),
  listBackground: (conversationId: string) =>
    invoke<BackgroundTask[]>("list_background", { conversationId }),
  killBackground: (id: string) =>
    invoke<BackgroundTask>("kill_background", { id }),
  health: () => invoke<HealthView>("health"),

  // ------------------------------------------------- Settings domain endpoints
  listAiProviders: () => invoke<AIProvider[]>("list_ai_providers"),
  getAiProvider: (id: string) => invoke<AIProvider>("get_ai_provider", { id }),
  createAiProvider: (p: {
    name: string;
    endpointUrl: string;
    apiKey: string;
    models?: ProviderModel[];
    modelIds?: string[];
    defaultModelId: string;
  }) => invoke<AIProvider>("create_ai_provider", p),
  updateAiProvider: (id: string, p: {
    name?: string;
    endpointUrl?: string;
    apiKey?: string; // omit to keep current
    models?: ProviderModel[];
    modelIds?: string[];
    defaultModelId?: string;
  }) => invoke<AIProvider>("update_ai_provider", { id, ...p }),
  deleteAiProvider: (id: string) => invoke<void>("delete_ai_provider", { id }),
  setDefaultAiProvider: (id: string) => invoke<void>("set_default_ai_provider", { id }),
  selectModel: (providerId: string | null, modelId: string) =>
    invoke<HealthView>("select_model", { providerId, modelId }),
  /// Test a saved provider, or the unsaved Add/Edit form (no id). A blank
  /// apiKey on the draft falls back to the stored key.
  testAiProvider: (p: {
    id?: string;
    endpointUrl?: string;
    apiKey?: string;
    model?: string;
  }) => invoke<ProviderTestResult>("test_ai_provider", p),
  fetchAiProviderModels: (id: string) => invoke<FetchModelsResult>("fetch_ai_provider_models", { id }),

  getEmbeddingConfig: () => invoke<EmbeddingConfig>("get_embedding_config"),
  setEmbeddingConfig: (cfg: EmbeddingConfig) => invoke<EmbeddingConfig>("set_embedding_config", { config: cfg }),
  testEmbedding: () => invoke<EmbeddingTestResult>("test_embedding"),
  getEmbeddingIndexInfo: () => invoke<EmbeddingIndexInfo>("get_embedding_index_info"),
  reindexMemories: () => invoke<number>("reindex_memories"),

  getWebSearchConfig: () => invoke<WebSearchConfig | null>("get_web_search_config"),
  setWebSearchConfig: (p: {
    providerKey: string;
    apiKey?: string;
    baseUrl?: string;
  }) => invoke<WebSearchConfig>("set_web_search_config", p),
  verifyWebSearch: (p: {
    providerKey: string;
    apiKey?: string;
    baseUrl?: string;
  }) => invoke<WebSearchVerifyResult>("verify_web_search", p),
  disconnectWebSearch: () => invoke<void>("disconnect_web_search"),
  testWebSearch: () => invoke<WebSearchTestResult>("test_web_search"),

  listBuiltInTools: () => invoke<BuiltInTool[]>("list_built_in_tools"),
  setBuiltInTool: (id: string, enabled: boolean) => invoke<BuiltInTool>("set_built_in_tool", { id, enabled }),
};

export type HealthStatus = "ok" | "degraded" | "error";

export interface HealthView {
  status: HealthStatus;
  detail: string | null;
  providerReachable: boolean;
  apiKeyPresent: boolean;
  model: string;
  schemaVersion: number;
  version: string;
}

/// One table counted in Settings › Database.
export interface TableCount {
  name: string;
  rows: number;
}

/// Where the database lives and what it holds (Settings › Database).
export interface DatabaseInfo {
  path: string;
  sizeBytes: number;
  walBytes: number;
  modifiedAt: number | null;
  schemaVersion: number;
  sqliteVersion: string;
  journalMode: string;
  pageSize: number;
  tables: TableCount[];
  blobsDir: string;
  blobs: number;
  blobsBytes: number;
  backupsDir: string;
}

export interface Memory {
  id: string;
  workspace_id: string | null;
  key: string;
  content: string;
  embedding: string | null;
  created_at: number;
  updated_at: number;
}

// ------------------------------------------------------- Settings domain

export type ProviderType = "openai-compatible";

export interface ProviderModel {
  id: string;
  name: string;
}

export interface AIProvider {
  id: string;
  name: string;
  type: ProviderType;
  endpointUrl: string; // base URL, e.g. https://api.openai.com/v1
  apiKeyMasked: string; // server returns masked only
  models?: ProviderModel[];
  modelIds: string[];
  defaultModelId: string;
  isDefault: boolean;
  status: "active" | "untested" | "error";
  lastTestedAt?: number; // epoch ms
}

export interface EmbeddingConfig {
  enabled: boolean;
  providerId: string | "default";
  model: string;
  dimensions?: number;
}

export interface WebSearchConfig {
  providerKey: string; // exa | firecrawl | searxng | duckduckgo
  apiKeyMasked: string;
  status: "connected" | "disconnected" | "error";
  lastVerifiedAt?: number;
}

export interface BuiltInTool {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  requiresSetup: boolean;
  setupComplete: boolean;
  permission: ApprovalClass;
}

export interface EmbeddingIndexInfo {
  total: number;
  embedded: number;
}

export interface ProviderTestResult {
  ok: boolean;
  latencyMs?: number;
  error?: string; // friendly: 401 invalid key, 404 wrong endpoint, timeout, CORS, network
}

export interface FetchModelsResult {
  models: string[];
}

export interface EmbeddingTestResult {
  ok: boolean;
  vectorLength?: number;
  latencyMs?: number;
  error?: string;
}

export interface WebSearchVerifyResult {
  ok: boolean;
  error?: string;
}

export interface WebSearchTestResult {
  ok: boolean;
  titles: { title: string; domain: string }[];
  error?: string;
}

export const KNOWN_EMBEDDING_MODELS: Record<string, number> = {
  "text-embedding-3-small": 1536,
  "text-embedding-3-large": 3072,
  "text-embedding-ada-002": 1536,
  "nomic-embed-text": 768,
  "bge-large-en-v1.5": 1024,
  "bge-small-en-v1.5": 384,
};

export const AI_PROVIDERS_KEY = "aria.settingsSection";

export function onRunEvent(cb: (envelope: RunEventEnvelope) => void): Promise<UnlistenFn> {
  return listen<RunEventEnvelope>("run-event", (e) => cb(e.payload));
}

// ---------------------------------------------------- Pure validation rules
// Kept framework-free and side-effect-free so they are unit-testable in isolation.

/** Strip trailing slash from a base URL (idempotent). */
export function normalizeEndpoint(url: string): string {
  return url.trim().replace(/\/+$/, "");
}

/** True if url is a valid https:// URL, or http(s):// for localhost/LAN addresses. */
export function isValidEndpoint(url: string): boolean {
  const v = url.trim();
  if (!/^https?:\/\//.test(v)) return false;
  try {
    const u = new URL(v);
    if (u.protocol !== "https:" && u.protocol !== "http:") return false;
    // Local and LAN addresses are allowed over plain http; external hosts must be https.
    const host = u.hostname;
    const isLocal =
      host === "localhost" ||
      host === "127.0.0.1" ||
      host === "[::1]" ||
      host.endsWith(".local") ||
      /^192\.168\./.test(host) ||
      /^10\./.test(host) ||
      /^172\.(1[6-9]|2[0-9]|3[0-1])\./.test(host);
    if (u.protocol !== "https:" && !isLocal) return false;
    return true;
  } catch {
    return false;
  }
}

/** Mask an API key for display: keep first 2 and last 4 chars, dots between. */
export function maskKey(key: string): string {
  const k = key.trim();
  if (k.length <= 6) return "••••";
  return `${k.slice(0, 2)}${"•".repeat(4)}${k.slice(-4)}`;
}

/** Dedupe a list of ProviderModels case-sensitively by ID, trim, drop empties. */
export function dedupeModels(models: ProviderModel[]): ProviderModel[] {
  const seen = new Set<string>();
  const out: ProviderModel[] = [];
  for (const m of models) {
    const id = m.id.trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const name = m.name?.trim() || id;
    out.push({ id, name });
  }
  return out;
}

/** Dedupe a list of model ids case-sensitively, trim, drop empties. */
export function dedupeModelIds(ids: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of ids) {
    const t = raw.trim();
    if (!t) continue;
    if (seen.has(t)) continue;
    seen.add(t);
    out.push(t);
  }
  return out;
}

/** Split a pasted/colon/comma list into candidate model ids. */
export function parseModelList(text: string): string[] {
  return text
    .split(/[\n,;]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Validate the Add/Edit provider form. Returns a field→message map (empty = valid). */
export function validateProviderForm(f: {
  name: string;
  endpointUrl: string;
  apiKey: string; // "" on edit means "keep current"
  models?: ProviderModel[];
  modelIds?: string[];
}): Record<string, string> {
  const errors: Record<string, string> = {};
  const name = f.name.trim();
  if (name.length < 2) errors.name = "Provider name must be at least 2 characters.";
  else if (name.length > 50) errors.name = "Provider name must be 50 characters or fewer.";
  if (!f.endpointUrl.trim()) errors.endpointUrl = "Enter the endpoint base URL.";
  else if (!isValidEndpoint(f.endpointUrl))
    errors.endpointUrl = "Enter a valid URL starting with https:// (http://localhost allowed).";
  const count = f.models !== undefined ? f.models.length : (f.modelIds?.length ?? 0);
  if (count === 0) errors.modelIds = "Add at least one model id.";
  if (count > 50) errors.modelIds = "A provider can have at most 50 models.";
  return errors;
}

/** Validate the embedding config. */
export function validateEmbeddingForm(f: {
  model: string;
  dimensions?: number;
}): Record<string, string> {
  const errors: Record<string, string> = {};
  if (!f.model.trim()) errors.model = "Choose or enter an embedding model.";
  if (f.dimensions !== undefined && f.dimensions !== null) {
    const d = Number(f.dimensions);
    if (!Number.isInteger(d) || d < 64 || d > 8192)
      errors.dimensions = "Dimensions must be a whole number between 64 and 8192.";
  }
  return errors;
}

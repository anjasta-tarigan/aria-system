// The Settings modal: a self-managed dialog so form state (nested provider
// form, tag input, dirty tracking) survives without the app shell re-rendering
// it away. All behaviour is reachable through the injected `Ipc`, so `?mock=1`
// exercises the same code as the real app.
//
// Sections: General, AI Provider, Embedding Provider, Built-in Tools,
// Web Configuration. Only General is form-and-Save; the rest persist as you
// act on them, which is what the underlying data model supports.

import {
  AI_PROVIDERS_KEY,
  KNOWN_EMBEDDING_MODELS,
  dedupeModelIds,
  isValidEndpoint,
  normalizeEndpoint,
  parseModelList,
  validateEmbeddingForm,
  validateProviderForm,
  type AIProvider,
  type BuiltInTool,
  type DatabaseInfo,
  type EmbeddingConfig,
  type EmbeddingIndexInfo,
  type Settings,
  type SettingsView,
  type WebSearchConfig,
} from "./api";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { focusables, h } from "./dom";
import type { Ipc } from "./app";

/** User-facing copy, kept in one place so it can be translated. */
const STR = {
  title: "Settings",
  close: "Close settings",
  save: "Save",
  saving: "Saving…",
  discard: "Discard",
  back: "Back",
  cancel: "Cancel",
  retry: "Retry",
  loading: "Loading…",
  saveFailed: "Could not save. Check your connection and try again.",
  saved: "Settings saved",
  discardTitle: "Discard unsaved changes?",
  discardBody: "Your edits to this section will be lost.",
  discardConfirm: "Discard changes",
  keepEditing: "Keep editing",
  sections: {
    general: "General",
    ai: "AI Provider",
    embedding: "Embedding Provider",
    reranker: "Reranker",
    tools: "Built-in Tools",
    web: "Web Configuration",
    database: "Database",
  },
  groups: { settings: "Settings", ai: "AI Configuration", tools: "Tools" },
  general: {
    intro: "How Aria asks for permission and spends its budget.",
    approval: "Approval policy",
    approvalHelp:
      "Always ask prompts for every file change, shell command and web call. Auto-approve safe runs reads without a prompt. In a folder, edits and web calls also run silently, and shell always asks because it can leave the folder. In the Main System chat, edits and web calls still ask, and only one safe read-only shell command runs silently (a Main System restricted to allowed paths asks for every command).",
    approvalAlways: "Always ask",
    approvalAuto: "Auto-approve safe tools",
  },
  ai: {
    intro: "Aria talks to any OpenAI-compatible endpoint. The default provider supplies the chat model.",
    add: "Add provider",
    emptyTitle: "No providers yet",
    emptyBody: "Add an OpenAI-compatible endpoint to start chatting.",
    default: "Default",
    setDefault: "Set as default",
    test: "Test",
    edit: "Edit",
    delete: "Delete",
    models: "models",
    active: "Active",
    untested: "Untested",
    error: "Error",
    neverTested: "Never tested",
    testedAt: (t: string) => `Tested ${t}`,
    deleteTitle: "Delete provider?",
    deleteBody: (n: string) =>
      `“${n}” and its stored API key will be removed. This cannot be undone.`,
    deleteConfirm: "Delete provider",
    deleted: "Provider deleted",
    savedToast: "Provider saved",
    addTitle: "Add provider",
    editTitle: "Edit provider",
    name: "Name",
    nameHelp: "2–50 characters, unique.",
    endpoint: "Endpoint URL",
    endpointHelp:
      "Base URL of the OpenAI-compatible API, for example https://api.openai.com/v1. HTTPS is required; http://localhost is allowed for local servers.",
    apiKey: "API key",
    apiKeyNew: "Paste the provider's API key.",
    apiKeyKeep: "Leave blank to keep the current key.",
    show: "Show",
    hide: "Hide",
    modelsLabel: "Model IDs",
    modelsHelp: "Press Enter or comma to add. Paste a comma-separated list to add several.",
    modelsPlaceholder: "Add a model id…",
    removeModel: (m: string) => `Remove ${m}`,
    defaultModel: "Default model",
    defaultModelHelp: "Used for new requests unless a conversation overrides it.",
    testConnection: "Test connection",
    testing: "Testing…",
    testOk: (ms: number) => `Connected in ${ms} ms`,
    testFailed: "Could not connect",
    saveAnyway: "You can still save; fix it later.",
    invalidName: "Provider name must be 2–50 characters.",
    invalidEndpoint: "Enter a valid URL starting with https:// (http://localhost allowed).",
    invalidModels: "Add at least one model id.",
    tooManyModels: "A provider can have at most 50 models.",
    duplicateName: "A provider with that name already exists.",
    requiredKey: "Enter the provider's API key.",
    defaultModelHint: "Add a model id first.",
  },
  embedding: {
    intro:
      "Optional semantic search for memories. When off, memories are still stored and recalled by keyword with FTS5; nothing else changes.",
    enabled: "Use embeddings for memory search",
    enabledHelp:
      "Calls the embeddings endpoint when saving and recalling memories. Leave off if this provider has no embeddings model; keyword recall keeps working.",
    provider: "Provider",
    providerDefault: "Use default AI provider",
    model: "Embedding model",
    modelHelp: "Pick a known model or type the id your endpoint serves.",
    dimensions: "Dimensions",
    dimensionsHelp: "Optional. Leave blank to use the model's default width.",
    batchSize: "Batch size",
    batchSizeHelp: "How many texts to embed per request (1–256).",
    timeout: "Timeout (seconds)",
    timeoutHelp: "How long one embedding request may take (5–120).",
    test: "Test embedding",
    testing: "Testing…",
    testOk: (n: number, ms: number) => `Vector length ${n}, ${ms} ms`,
    testFailed: "Embedding failed",
    reindexTitle: "Changing the embedding model needs a re-index",
    reindexBody: (n: number) =>
      `${n} stored ${n === 1 ? "memory has" : "memories have"} an embedding from the current model. ` +
      "Saving will re-embed them with the new one.",
    reindexAck: "I understand, re-embed my memories",
    saveReindex: "Save and re-index",
    saved: "Embedding settings saved",
    reindexed: (n: number) => `Re-indexed ${n} ${n === 1 ? "memory" : "memories"}`,
    invalidModel: "Choose or enter an embedding model.",
    invalidDimensions: "Dimensions must be a whole number between 64 and 8192.",
    invalidBatch: "Batch size must be between 1 and 256.",
    invalidTimeout: "Timeout must be between 5 and 120 seconds.",
    indexInfo: (embedded: number, total: number) =>
      `${embedded} of ${total} ${total === 1 ? "memory is" : "memories are"} embedded.`,
  },
  reranker: {
    intro:
      "Re-rank memory and search results before they reach the model. Placeholder only: no reranker is wired up yet.",
    placeholder: "Reranker providers and a test action will appear here in a later update.",
  },
  tools: {
    intro: "Turn individual tools on or off. A disabled tool is not offered to the model.",
    details: "Details",
    permission: "Permission",
    permissions: {
      read: "Read-only",
      mutate: "Changes files",
      exec: "Runs commands",
      network: "Uses the network",
    },
    configure: "Configure",
    setupRequired: "Setup required",
    setupHelp: "Configure Web Configuration first.",
    disableAll: "Disable all",
    enableRecommended: "Enable recommended",
    enabled: "Enabled",
    disabled: "Disabled",
    toggleFailed: (n: string) => `Could not update ${n}`,
    recommended: new Set([
      "read_file",
      "list_dir",
      "search_files",
      "edit_file",
      "write_file",
      "todos",
      "remember",
      "recall",
    ]),
  },
  web: {
    intro: "Let Aria search the web. The key is stored encrypted and never shown again.",
    apiKeyHelp: "Sent straight to the backend and stored encrypted. Aria never displays it again.",
    providers: {
      exa: {
        name: "Exa",
        note: "Neural search built for AI agents.",
        keyUrl: "https://dashboard.exa.ai/api-keys",
        keyLabel: "Exa API key",
        placeholder: "Paste your Exa API key",
      },
      firecrawl: {
        name: "Firecrawl",
        note: "Search plus structured page content.",
        keyUrl: "https://www.firecrawl.dev/app/api-keys",
        keyLabel: "Firecrawl API key",
        placeholder: "Paste your Firecrawl API key (fc-…)",
      },
      searxng: {
        name: "SearXNG",
        note: "Self-hosted meta-search. No key needed.",
        keyUrl: "https://docs.searxng.org/admin/installation.html",
        keyLabel: "",
        placeholder: "",
      },
      duckduckgo: {
        name: "DuckDuckGo",
        note: "Works out of the box. No key needed.",
        keyUrl: "",
        keyLabel: "",
        placeholder: "",
      },
    } as Record<string, { name: string; note: string; keyUrl: string; keyLabel: string; placeholder: string }>,
    getKey: "Get an API key",
    baseUrl: "SearXNG base URL",
    baseUrlHelp: "Your instance's URL, with JSON output enabled.",
    baseUrlPlaceholder: "https://searx.example.com",
    apiKey: "API key",
    connect: "Connect",
    verifying: "Verifying key…",
    connectFailed: "Could not connect",
    connected: "Connected",
    disconnected: "Not connected",
    status: "Status",
    lastVerified: "Last verified",
    maskedKey: "API key",
    testSearch: "Test search",
    testingSearch: "Searching…",
    testOk: "Search returned results",
    testFailed: "Search failed",
    replaceKey: "Replace key",
    disconnect: "Disconnect",
    switchProvider: "Switch provider",
    advanced: "Advanced",
    advancedHelp: "Read-only details. The key itself is never returned by the backend.",
    providerLabel: "Provider",
    endpoint: "Endpoint",
    disconnectTitle: "Disconnect web search?",
    disconnectBody: "The stored API key will be deleted and Web Search will be turned off.",
    disconnectConfirm: "Disconnect",
    disconnectedToast: "Web search disconnected",
    connectedToast: "Web search connected",
    verifyFailed: "Could not verify the key. Check it and try again.",
    invalidBaseUrl: "Enter a valid URL for your SearXNG instance (http://localhost or https://).",
    requiredKey: "Paste your API key first.",
  },
  database: {
    intro:
      "Everything Aria remembers lives in one SQLite file: chats, runs, memories and the list of Undo steps. Undo also keeps the old file contents in a separate folder, which a backup file does not include.",
    location: "File",
    size: "Size",
    modified: "Last changed",
    schema: "Schema",
    engine: "Engine",
    blobs: "Undo history",
    backups: "Automatic copies",
    contents: "Contents",
    tables: {
      workspaces: "Folders",
      conversations: "Chats",
      main_system_paths: "Main System path rules",
      messages: "Messages",
      runs: "Runs",
      tool_calls: "Tool calls",
      approvals: "Approvals",
      todos: "Plan items",
      memories: "Memories",
      background_task: "Background tasks",
      edit_journal: "Edits kept for Undo",
    } as Record<string, string>,
    backup: "Backup…",
    backupHelp: "Save a consistent copy of the whole database to a file you choose.",
    import: "Import…",
    importHelp: "Replace the whole database with a backup file.",
    reset: "Reset database",
    resetHelp: "Delete every chat, run, memory and Undo step. Settings stay.",
    refresh: "Refresh",
    backupDone: (path: string) => `Backup saved to ${path}`,
    importTitle: "Import a backup?",
    importBody:
      "Every chat, run, memory and Undo step is replaced by the file you pick. A copy of the current database is saved in the automatic-copies folder first.",
    importConfirm: "Import backup",
    importDone: "Database imported",
    resetTitle: "Reset the database?",
    resetBody:
      "Every chat, run, memory and Undo step is deleted. Settings are kept. A copy of the current database is saved in the automatic-copies folder first.",
    resetConfirm: "Delete everything",
    resetDone: "Database reset",
    failed: "That did not work",
  },
  errorTitle: "Could not load settings",
  errorBody: "The backend did not respond. Check that Aria is running, then retry.",
  offline: "You are offline. Provider tests and key verification need a connection; saved settings still work.",
} as const;

type SectionId = "general" | "database" | "ai" | "embedding" | "reranker" | "tools" | "web";

const SECTIONS: { group: keyof typeof STR.groups; items: { id: SectionId }[] }[] = [
  { group: "settings", items: [{ id: "general" }, { id: "database" }] },
  { group: "ai", items: [{ id: "ai" }, { id: "embedding" }, { id: "reranker" }] },
  { group: "tools", items: [{ id: "tools" }, { id: "web" }] },
];

const SECTION_IDS: SectionId[] = SECTIONS.flatMap((g) => g.items.map((i) => i.id));

function storedSection(): SectionId {
  try {
    const v = sessionStorage.getItem(AI_PROVIDERS_KEY);
    if (v && (SECTION_IDS as string[]).includes(v)) return v as SectionId;
  } catch {
    // Private mode or disabled storage: fall back to the default section.
  }
  return "general";
}

function rememberSection(id: SectionId) {
  try {
    sessionStorage.setItem(AI_PROVIDERS_KEY, id);
  } catch {
    // Non-fatal: the section simply is not remembered.
  }
}

const fmtTime = (ms: number) =>
  new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

// --------------------------------------------------------------- entry point

/**
 * Open the Settings modal. `ipc` is the app's invoke adapter so the same code
 * runs against the real backend and the in-browser mock.
 */
export function openSettings(ipc: Ipc, onDatabaseReplaced?: () => void): void {
  // One dialog at a time; re-opening would stack duplicate focus traps.
  const prevBackdrop = document.querySelector(".settings-backdrop") as (HTMLElement & { _close?: () => void }) | null;
  prevBackdrop?._close?.();
  prevBackdrop?.remove();
  const returnFocus = document.activeElement as HTMLElement | null;

  let closed = false;

  const state: State = {
    section: storedSection(),
    loading: true,
    loadError: false,
    general: null,
    generalBaseline: null,
    providers: [],
    providersLoading: true,
    providersError: false,
    form: null,
    embedding: null,
    embeddingBaseline: null,
    indexInfo: null,
    tools: [],
    web: null,
    webForm: null,
    database: null,
  };

  const overlay = h("div", {
    class: "modal-backdrop settings-backdrop",
    onclick: (e: MouseEvent) => {
      if (e.target === overlay && !document.querySelector(".confirm-backdrop")) void requestClose();
    },
  });
  (overlay as HTMLElement & { _close?: () => void })._close = () => {
    closed = true;
  };
  const live = h("div", { class: "toast-region", "aria-live": "polite", "aria-atomic": "false" });
  overlay.append(live);

  let dialog: HTMLElement;
  let onKey: (e: KeyboardEvent) => void;

  // The app talks to a local backend, but key verification and provider tests
  // need the network. Say so up front instead of failing every button.
  let offline = !navigator.onLine;
  const onOnline = () => {
    offline = !navigator.onLine;
    render();
  };
  window.addEventListener("online", onOnline);
  window.addEventListener("offline", onOnline);

  const close = () => {
    closed = true;
    document.removeEventListener("keydown", onKey, true);
    window.removeEventListener("online", onOnline);
    window.removeEventListener("offline", onOnline);
    overlay.remove();
    returnFocus?.focus?.();
  };

  const hasUnsaved = () =>
    isDirty(state.general, state.generalBaseline) ||
    isFormDirty(state.form) ||
    isEmbeddingDirty(state.embedding, state.embeddingBaseline) ||
    Boolean(state.webForm?.apiKey?.trim());

  const requestClose = async () => {
    if (hasUnsaved()) {
      const ok = await confirmDialog(STR.discardTitle, STR.discardBody, STR.discardConfirm, true);
      if (!ok) return;
    }
    close();
  };

  onKey = (e: KeyboardEvent) => {
    // A confirm dialog owns the keyboard while it is open.
    if (document.querySelector(".confirm-backdrop")) return;
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      void requestClose();
      return;
    }
    if (e.key !== "Tab" || !dialog) return;
    const items = focusables(dialog);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement as HTMLElement | null;
    if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    } else if (e.shiftKey && (active === first || !dialog.contains(active))) {
      e.preventDefault();
      last.focus();
    }
  };
  document.addEventListener("keydown", onKey, true);

  const toast = (message: string, kind: "ok" | "bad" = "ok") => {
    const el = h("div", { class: `toast ${kind}`, role: "status" }, message);
    live.append(el);
    setTimeout(() => el.remove(), 4000);
  };

  const setSection = (id: SectionId) => {
    state.section = id;
    state.focusTabId = id;
    rememberSection(id);
    // Leaving the provider list drops an unfinished form; the form itself
    // guards section switches through switchSection().
    if (id !== "ai") state.form = null;
    render();
  };

  const switchSection = async (id: SectionId) => {
    if (id === state.section) return;
    if (state.form && isFormDirty(state.form)) {
      const ok = await confirmDialog(STR.discardTitle, STR.discardBody, STR.discardConfirm, true);
      if (!ok) return;
    }
    setSection(id);
  };

  // ------------------------------------------------------------- data load

  const loadAll = async () => {
    state.loading = true;
    state.loadError = false;
    state.providersLoading = true;
    state.providersError = false;
    render();
    try {
      const [view, providers, embedding, tools, web, indexInfo, database] = await Promise.all([
        ipc.invoke<SettingsView>("get_settings"),
        ipc.invoke<AIProvider[]>("list_ai_providers"),
        ipc.invoke<EmbeddingConfig>("get_embedding_config"),
        ipc.invoke<BuiltInTool[]>("list_built_in_tools"),
        ipc.invoke<WebSearchConfig | null>("get_web_search_config"),
        ipc.invoke<EmbeddingIndexInfo>("get_embedding_index_info"),
        ipc.invoke<DatabaseInfo>("get_database_info"),
      ]);
      state.general = generalFrom(view);
      state.generalBaseline = generalFrom(view);
      state.providers = providers;
      state.embedding = embedding;
      state.embeddingBaseline = structuredClone(embedding);
      state.tools = tools;
      state.web = web;
      state.indexInfo = indexInfo;
      state.database = database;
      state.loading = false;
      state.providersLoading = false;
    } catch {
      state.loading = false;
      state.providersLoading = false;
      state.loadError = true;
    }
    render();
  };

  // --------------------------------------------------------------- actions

  const saveGeneral = async () => {
    const g = state.general;
    if (!g || !state.generalBaseline) return;
    const errors = generalErrors(g);
    if (Object.keys(errors).length > 0) {
      state.general = { ...g, errors };
      render();
      focusFirstError(dialog);
      return;
    }
    state.savingGeneral = true;
    render();
    try {
      // Keep the provider-managed endpoint/model: this form does not edit them.
      const view = await ipc.invoke<SettingsView>("set_settings", {
        input: {
          base_url: state.generalBaseline.baseUrl,
          model: state.generalBaseline.model,
          approval_policy: g.approvalPolicy,
        },
      });
      state.general = generalFrom(view);
      state.generalBaseline = generalFrom(view);
      toast(STR.saved);
    } catch {
      toast(STR.saveFailed, "bad");
    }
    state.savingGeneral = false;
    render();
  };

  const refreshProviders = async () => {
    try {
      state.providers = await ipc.invoke<AIProvider[]>("list_ai_providers");
      state.providersError = false;
    } catch {
      state.providersError = true;
    }
    state.providersLoading = false;
  };

  const saveProviderForm = async () => {
    const f = state.form;
    if (!f) return;
    const errors = providerErrors(f, state.providers);
    if (Object.keys(errors).length > 0) {
      state.form = { ...f, errors };
      render();
      focusFirstError(dialog);
      return;
    }
    state.form = { ...f, saving: true, errors: {} };
    render();
    try {
      const endpointUrl = normalizeEndpoint(f.endpointUrl);
      if (f.id) {
        await ipc.invoke("update_ai_provider", {
          id: f.id,
          name: f.name.trim(),
          endpointUrl,
          // Blank means "keep the stored key"; never send an empty override.
          ...(f.apiKey.trim() ? { apiKey: f.apiKey.trim() } : {}),
          modelIds: dedupeModelIds(f.modelIds),
          defaultModelId: f.defaultModelId,
        });
      } else {
        await ipc.invoke("create_ai_provider", {
          name: f.name.trim(),
          endpointUrl,
          apiKey: f.apiKey.trim(),
          modelIds: dedupeModelIds(f.modelIds),
          defaultModelId: f.defaultModelId,
        });
      }
      state.form = null;
      await refreshProviders();
      toast(STR.ai.savedToast);
      // The key is dropped with the form object; it is never rendered again.
      render();
    } catch (e) {
      const msg = errorText(e);
      state.form = {
        ...f,
        saving: false,
        errors: /already exists/i.test(msg) ? { name: STR.ai.duplicateName } : { name: msg },
      };
      render();
    }
  };

  const removeProvider = async (p: AIProvider) => {
    const ok = await confirmDialog(
      STR.ai.deleteTitle,
      STR.ai.deleteBody(p.name),
      STR.ai.deleteConfirm,
      true,
    );
    if (!ok) return;
    try {
      await ipc.invoke("delete_ai_provider", { id: p.id });
      await refreshProviders();
      toast(STR.ai.deleted);
    } catch {
      toast(STR.saveFailed, "bad");
    }
    render();
  };

  const makeDefault = async (p: AIProvider) => {
    try {
      await ipc.invoke("set_default_ai_provider", { id: p.id });
      await refreshProviders();
    } catch {
      toast(STR.saveFailed, "bad");
    }
    render();
  };

  const testProvider = async (p: AIProvider) => {
    state.testingProviderId = p.id;
    render();
    try {
      const r = await ipc.invoke<{ ok: boolean; latencyMs?: number; error?: string }>(
        "test_ai_provider",
        { id: p.id },
      );
      state.providerTest = { id: p.id, ...r };
      await refreshProviders();
    } catch (e) {
      state.providerTest = { id: p.id, ok: false, error: errorText(e) };
    }
    state.testingProviderId = null;
    render();
  };

  const saveEmbedding = async (reindex: boolean) => {
    const cfg = state.embedding;
    if (!cfg) return;
    const errors = embeddingErrors(cfg);
    if (Object.keys(errors).length > 0) {
      state.embeddingErrors = errors;
      render();
      focusFirstError(dialog);
      return;
    }
    state.embeddingErrors = {};
    state.savingEmbedding = true;
    render();
    try {
      state.embedding = await ipc.invoke<EmbeddingConfig>("set_embedding_config", {
        config: {
          ...cfg,
          dimensions: cfg.dimensions ?? undefined,
        },
      });
      state.embeddingBaseline = structuredClone(state.embedding);
      if (reindex) {
        const n = await ipc.invoke<number>("reindex_memories");
        toast(STR.embedding.reindexed(n));
      } else {
        toast(STR.embedding.saved);
      }
      state.indexInfo = await ipc.invoke<EmbeddingIndexInfo>("get_embedding_index_info");
    } catch (e) {
      toast(errorText(e), "bad");
    }
    state.savingEmbedding = false;
    render();
  };

  const testEmbedding = async () => {
    state.testingEmbedding = true;
    render();
    try {
      const r = await ipc.invoke<{
        ok: boolean;
        vectorLength?: number;
        latencyMs?: number;
        error?: string;
      }>("test_embedding", {
        model: state.embedding?.model,
        providerId: state.embedding?.providerId,
      });
      state.embeddingTest = r;
    } catch (e) {
      state.embeddingTest = { ok: false, error: errorText(e) };
    }
    state.testingEmbedding = false;
    render();
  };

  const toggleTool = async (tool: BuiltInTool, enabled: boolean) => {
    // Optimistic: flip in place, roll back if the backend refuses.
    state.tools = state.tools.map((t) => (t.id === tool.id ? { ...t, enabled } : t));
    render();
    try {
      const updated = await ipc.invoke<BuiltInTool>("set_built_in_tool", {
        id: tool.id,
        enabled,
      });
      state.tools = state.tools.map((t) => (t.id === updated.id ? updated : t));
    } catch {
      state.tools = state.tools.map((t) =>
        t.id === tool.id ? { ...t, enabled: tool.enabled } : t,
      );
      toast(STR.tools.toggleFailed(tool.name), "bad");
    }
    render();
  };

  const bulkTools = async (enable: boolean) => {
    const targets = state.tools.filter((t) =>
      enable ? STR.tools.recommended.has(t.id) : true,
    );
    // Optimistic, one round-trip per tool so a partial failure is visible.
    state.tools = state.tools.map((t) =>
      targets.some((x) => x.id === t.id) ? { ...t, enabled: enable } : t,
    );
    render();
    const results = await Promise.allSettled(
      targets.map((t) =>
        ipc.invoke<BuiltInTool>("set_built_in_tool", { id: t.id, enabled: enable }),
      ),
    );
    if (results.some((r) => r.status === "rejected")) {
      toast(STR.saveFailed, "bad");
      try {
        state.tools = await ipc.invoke<BuiltInTool[]>("list_built_in_tools");
      } catch {
        // Keep the optimistic view; the next open re-reads the truth.
      }
    }
    render();
  };

  const connectWeb = async () => {
    const f = state.webForm;
    if (!f) return;
    const provider = STR.web.providers[f.providerKey] ?? STR.web.providers.exa;
    if (provider.keyLabel && !f.apiKey.trim()) {
      state.webErrors = { apiKey: STR.web.requiredKey };
      render();
      focusFirstError(dialog);
      return;
    }
    if (f.providerKey === "searxng" && !isValidEndpoint(f.baseUrl)) {
      state.webErrors = { baseUrl: STR.web.invalidBaseUrl };
      render();
      focusFirstError(dialog);
      return;
    }
    state.webErrors = {};
    state.connecting = true;
    render();
    try {
      const verify = await ipc.invoke<{ ok: boolean; error?: string }>("verify_web_search", {
        providerKey: f.providerKey,
        apiKey: f.apiKey.trim() || undefined,
        baseUrl: f.baseUrl.trim() || undefined,
      });
      if (!verify.ok) {
        state.webErrors = { form: verify.error ?? STR.web.verifyFailed };
        state.connecting = false;
        render();
        return;
      }
      state.web = await ipc.invoke<WebSearchConfig>("set_web_search_config", {
        providerKey: f.providerKey,
        apiKey: f.apiKey.trim() || undefined,
        baseUrl: f.baseUrl.trim() || undefined,
      });
      state.webForm = null;
      state.webErrors = {};
      // Web Search becomes usable once a provider is connected.
      state.tools = await ipc.invoke<BuiltInTool[]>("list_built_in_tools");
      toast(STR.web.connectedToast);
    } catch (e) {
      state.webErrors = { form: errorText(e) };
    }
    state.connecting = false;
    render();
  };

  const testSearch = async () => {
    state.testingSearch = true;
    render();
    try {
      state.searchTest = await ipc.invoke<{
        ok: boolean;
        titles: { title: string; domain: string }[];
        error?: string;
      }>("test_web_search");
    } catch (e) {
      state.searchTest = { ok: false, titles: [], error: errorText(e) };
    }
    state.testingSearch = false;
    render();
  };

  const disconnectWeb = async () => {
    const ok = await confirmDialog(
      STR.web.disconnectTitle,
      STR.web.disconnectBody,
      STR.web.disconnectConfirm,
      true,
    );
    if (!ok) return;
    try {
      await ipc.invoke("disconnect_web_search");
      state.web = null;
      state.searchTest = null;
      state.webForm = null;
      state.tools = await ipc.invoke<BuiltInTool[]>("list_built_in_tools");
      toast(STR.web.disconnectedToast);
    } catch {
      toast(STR.saveFailed, "bad");
    }
    render();
  };

  // -------------------------------------------------------------- rendering

  function render() {
    if (closed) return;
    const nav = sidebarNav();
    const body = state.loading
      ? loadingPanel()
      : state.loadError
        ? errorPanel()
        : sectionPanel();

    dialog = h(
      "div",
      {
        class: "modal settings-modal",
        role: "dialog",
        "aria-modal": "true",
        "aria-labelledby": "settings-title",
        tabindex: "-1",
      },
      h(
        "header",
        { class: "settings-header" },
        h("h2", { id: "settings-title" }, STR.title),
        h(
          "button",
          {
            class: "ghost icon-btn",
            "aria-label": STR.close,
            title: STR.close,
            onclick: () => void requestClose(),
          },
          "×",
        ),
      ),
      offline
        ? h("div", { class: "offline-banner", role: "status" }, STR.offline)
        : null,
      h("div", { class: "settings-body" }, nav, h("div", { class: "settings-content" }, body)),
    );

    // Re-rendering replaces the dialog, which would otherwise drop focus and
    // scroll. Carry both across so typing and keyboard nav survive a repaint.
    const activeId = document.activeElement?.id;
    const scrollTop =
      overlay.querySelector<HTMLElement>(".settings-content")?.scrollTop ?? 0;
    const existing = overlay.querySelector(".settings-modal");
    if (existing) existing.replaceWith(dialog);
    else overlay.append(dialog);
    if (!overlay.isConnected) document.body.append(overlay);

    const content = dialog.querySelector<HTMLElement>(".settings-content");
    if (content && scrollTop) content.scrollTop = scrollTop;
    const target =
      (state.focusTabId && dialog.querySelector<HTMLElement>(`#settings-tab-${state.focusTabId}`)) ||
      (activeId ? dialog.querySelector<HTMLElement>(`#${CSS.escape(activeId)}`) : null);
    if (state.focusTabId || activeId) {
      state.focusTabId = undefined;
      if (target && document.activeElement !== target) target.focus();
    }
  }

  function sidebarNav(): HTMLElement {
    const nav = h("nav", {
      class: "settings-nav",
      role: "tablist",
      "aria-label": STR.title,
      "aria-orientation": "vertical",
    });
    for (const group of SECTIONS) {
      nav.append(h("div", { class: "settings-nav-group" }, STR.groups[group.group]));
      for (const item of group.items) {
        const selected = state.section === item.id;
        const btn = h(
          "button",
          {
            class: `settings-nav-item ${selected ? "active" : ""}`,
            role: "tab",
            id: `settings-tab-${item.id}`,
            "aria-selected": selected ? "true" : "false",
            "aria-controls": `settings-panel-${item.id}`,
            tabindex: selected ? "0" : "-1",
            onclick: () => void switchSection(item.id),
            onkeydown: (e: KeyboardEvent) => {
              const ids = SECTION_IDS;
              const i = ids.indexOf(item.id);
              if (e.key === "ArrowDown" || e.key === "ArrowRight") {
                e.preventDefault();
                void switchSection(ids[(i + 1) % ids.length]);
              } else if (e.key === "ArrowUp" || e.key === "ArrowLeft") {
                e.preventDefault();
                void switchSection(ids[(i - 1 + ids.length) % ids.length]);
              } else if (e.key === "Home") {
                e.preventDefault();
                void switchSection(ids[0]);
              } else if (e.key === "End") {
                e.preventDefault();
                void switchSection(ids[ids.length - 1]);
              }
            },
          },
          h("span", { class: "settings-nav-label" }, STR.sections[item.id]),
        ) as HTMLButtonElement;
        nav.append(btn);
      }
    }
    return nav;
  }

  const panel = (id: SectionId, ...children: (Node | string | null)[]) =>
    h(
      "section",
      {
        class: "settings-panel",
        role: "tabpanel",
        id: `settings-panel-${id}`,
        "aria-labelledby": `settings-tab-${id}`,
        tabindex: "0",
      },
      ...children,
    );

  function loadingPanel(): HTMLElement {
    return h(
      "div",
      { class: "settings-skeleton", "aria-busy": "true", "aria-label": STR.loading },
      h("div", { class: "skeleton-line w60" }),
      h("div", { class: "skeleton-line w80" }),
      h("div", { class: "skeleton-line w40" }),
      h("div", { class: "skeleton-line w70" }),
    );
  }

  function errorPanel(): HTMLElement {
    return h(
      "div",
      { class: "settings-error", role: "alert" },
      h("h3", {}, STR.errorTitle),
      h("p", { class: "muted" }, STR.errorBody),
      h("button", { class: "primary", onclick: () => void loadAll() }, STR.retry),
    );
  }

  function sectionPanel(): HTMLElement {
    switch (state.section) {
      case "general":
        return generalPanel();
      case "ai":
        return aiPanel();
      case "embedding":
        return embeddingPanel();
      case "reranker":
        return rerankerPanel();
      case "tools":
        return toolsPanel();
      case "web":
        return webPanel();
      case "database":
        return databasePanel();
    }
  }

  // ---------------------------------------------------------------- general

  function generalPanel(): HTMLElement {
    const g = state.general!;
    const errors = g.errors ?? {};
    const dirty = isDirty(g, state.generalBaseline);
    const valid = Object.keys(generalErrors(g)).length === 0;

    const field = (
      key: keyof typeof g & string,
      label: string,
      help: string,
      input: HTMLElement,
      error?: string,
    ) =>
      h(
        "div",
        { class: "field" },
        h("label", { for: `general-${key}` }, label),
        input,
        error
          ? h("p", { class: "field-error", id: `general-${key}-error`, role: "alert" }, error)
          : h("p", { class: "field-help" }, help),
      );

    const number = (
      id: string,
      value: number,
      oninput: (n: number) => void,
      invalid: boolean,
    ) =>
      h("input", {
        id: `general-${id}`,
        type: "number",
        min: "1",
        value: String(value),
        class: invalid ? "invalid" : "",
        "aria-invalid": invalid ? "true" : "false",
        "aria-describedby": invalid ? `general-${id}-error` : undefined,
        oninput: (e: Event) => {
          oninput(Number((e.target as HTMLInputElement).value));
          state.general = { ...state.general!, errors: {} };
          refreshGeneralFooter();
        },
      }) as HTMLInputElement;

    const policy = h(
      "select",
      {
        id: "general-approvalPolicy",
        onchange: (e: Event) => {
          state.general = {
            ...state.general!,
            approvalPolicy: (e.target as HTMLSelectElement).value,
            errors: {},
          };
          refreshGeneralFooter();
        },
      },
      h("option", { value: "always_ask" }, STR.general.approvalAlways),
      h("option", { value: "auto_approve_safe" }, STR.general.approvalAuto),
    ) as HTMLSelectElement;
    policy.value = g.approvalPolicy;

    const footer = h(
      "div",
      { class: "settings-footer" },
      h(
        "button",
        {
          class: "ghost",
          disabled: !dirty,
          onclick: () => {
            state.general = { ...state.generalBaseline! };
            render();
          },
        },
        STR.discard,
      ),
      h(
        "button",
        {
          class: "primary",
          id: "general-save",
          disabled: !dirty || !valid || !!state.savingGeneral,
          onclick: () => void saveGeneral(),
        },
        state.savingGeneral ? STR.saving : STR.save,
      ),
    );
    // Live enable/disable without a re-render, so focus is not lost mid-typing.
    const refreshGeneralFooter = () => {
      const saveBtn = footer.querySelector<HTMLButtonElement>("#general-save")!;
      const discardBtn = footer.querySelector<HTMLButtonElement>("button.ghost")!;
      const dirtyNow = isDirty(state.general!, state.generalBaseline);
      const validNow = Object.keys(generalErrors(state.general!)).length === 0;
      saveBtn.disabled = !dirtyNow || !validNow || !!state.savingGeneral;
      discardBtn.disabled = !dirtyNow;
    };

    return panel(
      "general",
      h("h3", { class: "settings-section-title" }, STR.sections.general),
      h("p", { class: "settings-intro" }, STR.general.intro),
      field(
        "approvalPolicy",
        STR.general.approval,
        STR.general.approvalHelp,
        policy,
      ),
      footer,
    );
  }

  // ------------------------------------------------------------- ai provider

  function aiPanel(): HTMLElement {
    if (state.form) return providerForm();
    if (state.providersLoading) return panel("ai", loadingPanel());

    const list = state.providers.length
      ? h(
          "div",
          { class: "provider-list" },
          ...state.providers.map(providerCard),
        )
      : h(
          "div",
          { class: "empty-state" },
          h("h3", {}, STR.ai.emptyTitle),
          h("p", { class: "muted" }, STR.ai.emptyBody),
        );

    return panel(
      "ai",
      h(
        "div",
        { class: "settings-section-head" },
        h("div", {},
          h("h3", { class: "settings-section-title" }, STR.sections.ai),
          h("p", { class: "settings-intro" }, STR.ai.intro),
        ),
        h(
          "button",
          {
            class: "primary",
            id: "add-provider",
            onclick: () => {
              state.form = blankForm();
              render();
            },
          },
          STR.ai.add,
        ),
      ),
      state.providersError
        ? h(
            "div",
            { class: "inline-error", role: "alert" },
            STR.saveFailed,
            h("button", { class: "link", onclick: () => void loadAll() }, STR.retry),
          )
        : null,
      list,
    );
  }

  function providerCard(p: AIProvider): HTMLElement {
    const test = state.providerTest?.id === p.id ? state.providerTest : null;
    const testing = state.testingProviderId === p.id;
    return h(
      "article",
      { class: `provider-item ${p.isDefault ? "is-default" : ""}`, dataset: { provider: p.id } },
      h(
        "div",
        { class: "provider-item-head" },
        h(
          "div",
          { class: "provider-item-title" },
          h("strong", {}, p.name),
          p.isDefault ? h("span", { class: "badge accent" }, STR.ai.default) : null,
          statusBadge(p),
        ),
        h(
          "div",
          { class: "provider-item-actions" },
          h(
            "button",
            {
              class: "ghost small-btn",
              disabled: testing,
              onclick: () => void testProvider(p),
            },
            testing ? STR.ai.testing : STR.ai.test,
          ),
          h(
            "button",
            {
              class: "ghost small-btn",
              onclick: () => {
                state.form = formFrom(p);
                render();
              },
            },
            STR.ai.edit,
          ),
          !p.isDefault
            ? h(
                "button",
                { class: "ghost small-btn", onclick: () => void makeDefault(p) },
                STR.ai.setDefault,
              )
            : null,
          h(
            "button",
            { class: "ghost small-btn danger", onclick: () => void removeProvider(p) },
            STR.ai.delete,
          ),
        ),
      ),
      h("p", { class: "provider-endpoint", title: p.endpointUrl }, p.endpointUrl),
      h(
        "p",
        { class: "provider-key muted small" },
        h("span", {}, `${p.apiKeyMasked || "••••"}`),
        h(
          "span",
          { class: "provider-tested" },
          p.lastTestedAt ? STR.ai.testedAt(fmtTime(p.lastTestedAt)) : STR.ai.neverTested,
        ),
      ),
      p.modelIds.length
        ? h(
            "div",
            { class: "model-chips" },
            ...p.modelIds.map((m) =>
              h(
                "span",
                { class: `model-chip ${m === p.defaultModelId ? "is-default" : ""}`, title: m },
                m,
              ),
            ),
          )
        : null,
      test
        ? h(
            "p",
            { class: `inline-result ${test.ok ? "ok" : "bad"}`, role: "status" },
            test.ok
              ? STR.ai.testOk(test.latencyMs ?? 0)
              : `${STR.ai.testFailed}: ${test.error ?? ""}`,
          )
        : null,
    );
  }

  function providerForm(): HTMLElement {
    const f = state.form!;
    const errors = f.errors;
    const editing = !!f.id;

    const nameInput = h("input", {
      id: "provider-name",
      type: "text",
      value: f.name,
      maxlength: "50",
      autocomplete: "off",
      spellcheck: "false",
      "aria-invalid": errors.name ? "true" : "false",
      "aria-describedby": errors.name ? "provider-name-error" : "provider-name-help",
      oninput: (e: Event) => {
        state.form = { ...state.form!, name: (e.target as HTMLInputElement).value, errors: {} };
        refreshProviderFooter();
      },
    }) as HTMLInputElement;

    const endpointInput = h("input", {
      id: "provider-endpoint",
      type: "url",
      value: f.endpointUrl,
      placeholder: "https://api.openai.com/v1",
      autocomplete: "off",
      spellcheck: "false",
      "aria-invalid": errors.endpointUrl ? "true" : "false",
      "aria-describedby": errors.endpointUrl
        ? "provider-endpoint-error"
        : "provider-endpoint-help",
      oninput: (e: Event) => {
        state.form = {
          ...state.form!,
          endpointUrl: (e.target as HTMLInputElement).value,
          errors: {},
          test: null,
        };
        refreshProviderFooter();
      },
    }) as HTMLInputElement;

    const keyInput = h("input", {
      id: "provider-key",
      type: f.showKey ? "text" : "password",
      value: f.apiKey,
      placeholder: editing ? "" : "sk-…",
      autocomplete: "new-password",
      spellcheck: "false",
      "aria-invalid": errors.apiKey ? "true" : "false",
      "aria-describedby": errors.apiKey ? "provider-key-error" : "provider-key-help",
      oninput: (e: Event) => {
        state.form = { ...state.form!, apiKey: (e.target as HTMLInputElement).value, errors: {} };
        refreshProviderFooter();
      },
    }) as HTMLInputElement;

    const showToggle = h(
      "button",
      {
        class: "ghost small-btn",
        type: "button",
        "aria-pressed": f.showKey ? "true" : "false",
        "aria-label": f.showKey ? STR.ai.hide : STR.ai.show,
        onclick: () => {
          state.form = { ...state.form!, showKey: !state.form!.showKey };
          render();
          dialog.querySelector<HTMLInputElement>("#provider-key")?.focus();
        },
      },
      f.showKey ? STR.ai.hide : STR.ai.show,
    );

    // Tag input: chips live in the DOM; the form state is the source of truth.
    const chips = h("div", { class: "tag-chips" });
    const defaultSelect = h("select", {
      id: "provider-default-model",
      onchange: (e: Event) => {
        state.form = {
          ...state.form!,
          defaultModelId: (e.target as HTMLSelectElement).value,
        };
        refreshProviderFooter();
      },
    }) as HTMLSelectElement;

    const redrawChips = () => {
      const ids = state.form!.modelIds;
      chips.replaceChildren(
        ...ids.map((m) =>
          h(
            "span",
            { class: "tag" },
            h("span", { class: "tag-label" }, m),
            h(
              "button",
              {
                type: "button",
                class: "tag-remove",
                "aria-label": STR.ai.removeModel(m),
                onclick: () => {
                  const next = state.form!.modelIds.filter((x) => x !== m);
                  const keepDefault = next.includes(state.form!.defaultModelId)
                    ? state.form!.defaultModelId
                    : (next[0] ?? "");
                  state.form = { ...state.form!, modelIds: next, defaultModelId: keepDefault };
                  redrawChips();
                  refreshProviderFooter();
                },
              },
              "×",
            ),
          ),
        ),
      );
      // Keep the default-model select in sync with the chips.
      defaultSelect.replaceChildren(
        ...(ids.length
          ? ids.map((m) => h("option", { value: m }, m))
          : [h("option", { value: "" }, STR.ai.defaultModelHint)]),
      );
      if (ids.includes(state.form!.defaultModelId)) {
        defaultSelect.value = state.form!.defaultModelId;
      } else if (ids[0]) {
        state.form!.defaultModelId = ids[0];
        defaultSelect.value = ids[0];
      }
    };

    const addModels = (raw: string[]) => {
      const next = dedupeModelIds([...state.form!.modelIds, ...raw]).slice(0, 50);
      const keepDefault = next.includes(state.form!.defaultModelId)
        ? state.form!.defaultModelId
        : (next[0] ?? "");
      state.form = { ...state.form!, modelIds: next, defaultModelId: keepDefault, errors: {} };
      redrawChips();
      refreshProviderFooter();
    };

    const tagInput = h("input", {
      id: "provider-model-input",
      type: "text",
      class: "tag-input",
      placeholder: STR.ai.modelsPlaceholder,
      autocomplete: "off",
      spellcheck: "false",
      "aria-describedby": errors.modelIds ? "provider-models-error" : "provider-models-help",
      onkeydown: (e: KeyboardEvent) => {
        if (e.key === "Enter" || e.key === ",") {
          e.preventDefault();
          const input = e.target as HTMLInputElement;
          addModels(parseModelList(input.value));
          input.value = "";
        } else if (e.key === "Backspace" && !(e.target as HTMLInputElement).value) {
          const ids = state.form!.modelIds;
          if (ids.length) {
            state.form = { ...state.form!, modelIds: ids.slice(0, -1) };
            redrawChips();
            refreshProviderFooter();
          }
        }
      },
      onpaste: (e: ClipboardEvent) => {
        const text = e.clipboardData?.getData("text") ?? "";
        if (!/[,;\n]/.test(text)) return;
        e.preventDefault();
        addModels(parseModelList(text));
        (e.target as HTMLInputElement).value = "";
      },
    }) as HTMLInputElement;

    const testResult = h("p", { class: "inline-result", role: "status" });
    const testButton = h(
      "button",
      {
        type: "button",
        class: "ghost",
        id: "provider-test",
        onclick: async () => {
          const cur = state.form!;
          const endpoint = normalizeEndpoint(cur.endpointUrl);
          if (!isValidEndpoint(endpoint)) {
            state.form = { ...cur, errors: { endpointUrl: STR.ai.invalidEndpoint } };
            render();
            return;
          }
          testButton.disabled = true;
          testButton.textContent = STR.ai.testing;
          testResult.className = "inline-result";
          testResult.textContent = "";
          try {
            const r = await ipc.invoke<{ ok: boolean; latencyMs?: number; error?: string }>(
              "test_ai_provider",
              {
                id: cur.id || undefined,
                endpointUrl: endpoint,
                apiKey: cur.apiKey.trim() || undefined,
                model: cur.defaultModelId || cur.modelIds[0],
              },
            );
            state.form = { ...state.form!, test: r };
            testResult.className = `inline-result ${r.ok ? "ok" : "bad"}`;
            testResult.textContent = r.ok
              ? STR.ai.testOk(r.latencyMs ?? 0)
              : `${STR.ai.testFailed}: ${r.error ?? ""} ${STR.ai.saveAnyway}`;
          } catch (e) {
            testResult.className = "inline-result bad";
            testResult.textContent = `${STR.ai.testFailed}: ${errorText(e)}`;
          }
          testButton.disabled = false;
          testButton.textContent = STR.ai.testConnection;
        },
      },
      STR.ai.testConnection,
    );

    const footer = h(
      "div",
      { class: "settings-footer" },
      h(
        "button",
        {
          class: "ghost",
          onclick: () => {
            if (isFormDirty(state.form)) {
              void confirmDialog(STR.discardTitle, STR.discardBody, STR.discardConfirm, true).then(
                (ok) => {
                  if (ok) {
                    state.form = null;
                    render();
                  }
                },
              );
            } else {
              state.form = null;
              render();
            }
          },
        },
        STR.back,
      ),
      h(
        "button",
        {
          class: "primary",
          id: "provider-save",
          disabled: !!f.saving || Object.keys(providerErrors(f, state.providers)).length > 0,
          onclick: () => void saveProviderForm(),
        },
        f.saving ? STR.saving : STR.save,
      ),
    );

    const refreshProviderFooter = () => {
      const saveBtn = footer.querySelector<HTMLButtonElement>("#provider-save");
      const errs = providerErrors(state.form!, state.providers);
      const valid = Object.keys(errs).length === 0;
      if (saveBtn) saveBtn.disabled = !!state.form!.saving || !valid;

      const f = state.form!;
      const keyEl = dialog.querySelector<HTMLInputElement>("#provider-key");
      const keyHelp = dialog.querySelector("#provider-key-help");
      const keyErr = dialog.querySelector("#provider-key-error");
      const keyField = keyEl?.closest(".field");

      if (keyEl && keyField) {
        const needsKey =
          !f.id &&
          !f.apiKey.trim() &&
          Boolean(f.name.trim() || f.endpointUrl.trim() || f.modelIds.length > 0);
        if (needsKey) {
          keyEl.setAttribute("aria-invalid", "true");
          keyEl.setAttribute("aria-describedby", "provider-key-error");
          if (!keyErr) {
            keyHelp?.remove();
            keyField.appendChild(
              h(
                "p",
                { class: "field-error", id: "provider-key-error", role: "alert" },
                STR.ai.requiredKey,
              ),
            );
          }
        } else if (f.apiKey.trim() || f.id) {
          keyEl.setAttribute("aria-invalid", "false");
          keyEl.setAttribute("aria-describedby", "provider-key-help");
          if (keyErr) {
            keyErr.remove();
            keyField.appendChild(
              h(
                "p",
                { class: "field-help", id: "provider-key-help" },
                editing ? STR.ai.apiKeyKeep : STR.ai.apiKeyNew,
              ),
            );
          }
        }
      }
    };

    redrawChips();

    return panel(
      "ai",
      h(
        "div",
        { class: "settings-section-head" },
        h("h3", { class: "settings-section-title" }, editing ? STR.ai.editTitle : STR.ai.addTitle),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "provider-name" }, STR.ai.name),
        nameInput,
        errors.name
          ? h("p", { class: "field-error", id: "provider-name-error", role: "alert" }, errors.name)
          : h("p", { class: "field-help", id: "provider-name-help" }, STR.ai.nameHelp),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "provider-endpoint" }, STR.ai.endpoint),
        endpointInput,
        errors.endpointUrl
          ? h(
              "p",
              { class: "field-error", id: "provider-endpoint-error", role: "alert" },
              errors.endpointUrl,
            )
          : h("p", { class: "field-help", id: "provider-endpoint-help" }, STR.ai.endpointHelp),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "provider-key" }, STR.ai.apiKey),
        h("div", { class: "input-row" }, keyInput, showToggle),
        errors.apiKey
          ? h("p", { class: "field-error", id: "provider-key-error", role: "alert" }, errors.apiKey)
          : h(
              "p",
              { class: "field-help", id: "provider-key-help" },
              editing ? STR.ai.apiKeyKeep : STR.ai.apiKeyNew,
            ),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "provider-model-input" }, STR.ai.modelsLabel),
        h("div", { class: "tag-field" }, chips, tagInput),
        errors.modelIds
          ? h("p", { class: "field-error", id: "provider-models-error", role: "alert" }, errors.modelIds)
          : h("p", { class: "field-help", id: "provider-models-help" }, STR.ai.modelsHelp),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "provider-default-model" }, STR.ai.defaultModel),
        defaultSelect,
        h("p", { class: "field-help" }, STR.ai.defaultModelHelp),
      ),
      h("div", { class: "test-row" }, testButton, testResult),
      footer,
    );
  }

  // --------------------------------------------------------------- reranker

  function rerankerPanel(): HTMLElement {
    return panel(
      "reranker",
      h("h3", { class: "settings-section-title" }, STR.sections.reranker),
      h("p", { class: "settings-intro" }, STR.reranker.intro),
      h("div", { class: "empty-state" }, h("p", {}, STR.reranker.placeholder)),
    );
  }

  // --------------------------------------------------------------- embedding

  function embeddingPanel(): HTMLElement {
    const cfg = state.embedding!;
    const errors = state.embeddingErrors ?? {};

    const enabledInput = h("input", {
      type: "checkbox",
      id: "embedding-enabled",
      checked: cfg.enabled,
      onchange: (e: Event) => {
        state.embedding = {
          ...state.embedding!,
          enabled: (e.target as HTMLInputElement).checked,
        };
        render();
      },
    }) as HTMLInputElement;

    const providerSelect = h(
      "select",
      {
        id: "embedding-provider",
        onchange: (e: Event) => {
          state.embedding = {
            ...state.embedding!,
            providerId: (e.target as HTMLSelectElement).value,
          };
          render();
        },
      },
      h("option", { value: "default" }, STR.embedding.providerDefault),
      ...state.providers.map((p) => h("option", { value: p.id }, p.name)),
    ) as HTMLSelectElement;
    providerSelect.value = cfg.providerId;

    const modelInput = h("input", {
      id: "embedding-model",
      type: "text",
      list: "embedding-models",
      value: cfg.model,
      autocomplete: "off",
      spellcheck: "false",
      "aria-invalid": errors.model ? "true" : "false",
      "aria-describedby": errors.model ? "embedding-model-error" : "embedding-model-help",
      oninput: (e: Event) => {
        const model = (e.target as HTMLInputElement).value;
        // Auto-fill the width for a known model, but never overwrite a manual value.
        const known = KNOWN_EMBEDDING_MODELS[model.trim()];
        state.embedding = {
          ...state.embedding!,
          model,
          dimensions: known ?? state.embedding!.dimensions,
        };
        refreshEmbeddingFooter();
        const dims = dialog.querySelector<HTMLInputElement>("#embedding-dimensions");
        if (dims && known) dims.value = String(known);
      },
    }) as HTMLInputElement;

    const dimensionsInput = h("input", {
      id: "embedding-dimensions",
      type: "number",
      min: "64",
      max: "8192",
      value: cfg.dimensions === undefined || cfg.dimensions === null ? "" : String(cfg.dimensions),
      placeholder: "auto",
      "aria-invalid": errors.dimensions ? "true" : "false",
      oninput: (e: Event) => {
        const raw = (e.target as HTMLInputElement).value.trim();
        state.embedding = {
          ...state.embedding!,
          dimensions: raw === "" ? undefined : Number(raw),
        };
        refreshEmbeddingFooter();
      },
    }) as HTMLInputElement;

    const numberInput = (
      id: string,
      value: number,
      oninput: (n: number) => void,
      invalid: boolean,
      help: string,
    ) =>
      h(
        "div",
        { class: "field" },
        h("label", { for: `embedding-${id}` }, id === "batchSize" ? STR.embedding.batchSize : STR.embedding.timeout),
        h("input", {
          id: `embedding-${id}`,
          type: "number",
          value: String(value),
          class: invalid ? "invalid" : "",
          "aria-invalid": invalid ? "true" : "false",
          oninput: (e: Event) => {
            oninput(Number((e.target as HTMLInputElement).value));
            refreshEmbeddingFooter();
          },
        }),
        h("p", { class: "field-help" }, help),
      );

    const ack = h("input", {
      type: "checkbox",
      id: "embedding-reindex-ack",
      checked: !!state.reindexAck,
      onchange: (e: Event) => {
        state.reindexAck = (e.target as HTMLInputElement).checked;
        refreshEmbeddingFooter();
      },
    }) as HTMLInputElement;

    const reindexBanner = h(
      "div",
      { class: "warning-banner", role: "alert", hidden: true },
      h("strong", {}, STR.embedding.reindexTitle),
      h("p", { class: "reindex-body" }),
      h("label", { class: "checkbox-row", for: "embedding-reindex-ack" }, ack, STR.embedding.reindexAck),
    );

    const testResult = h("p", { class: "inline-result", role: "status" });
    const testButton = h(
      "button",
      {
        type: "button",
        class: "ghost",
        id: "embedding-test",
        onclick: () => void testEmbedding(),
      },
      state.testingEmbedding ? STR.embedding.testing : STR.embedding.test,
    );

    const saveBtn = h("button", {
      class: "primary",
      id: "embedding-save",
      onclick: () => void saveEmbedding(needsReindexNow()),
    }) as HTMLButtonElement;

    const discardBtn = h(
      "button",
      {
        class: "ghost",
        onclick: () => {
          state.embedding = structuredClone(state.embeddingBaseline!);
          state.embeddingErrors = {};
          state.reindexAck = false;
          render();
        },
      },
      STR.discard,
    ) as HTMLButtonElement;

    // Recompute every gate on each keystroke so Save can never be pressed with
    // invalid values, a clean form, or an unacknowledged re-index.
    const needsReindexNow = () => {
      const b = state.embeddingBaseline!;
      const c = state.embedding!;
      return (
        !!state.indexInfo &&
        state.indexInfo.embedded > 0 &&
        (c.model !== b.model || c.providerId !== b.providerId)
      );
    };
    const refreshEmbeddingFooter = () => {
      const c = state.embedding!;
      const dirtyNow = JSON.stringify(c) !== JSON.stringify(state.embeddingBaseline);
      const valid = Object.keys(embeddingErrors(c)).length === 0;
      const reindex = needsReindexNow();
      saveBtn.disabled = !dirtyNow || !valid || !!state.savingEmbedding || (reindex && !state.reindexAck);
      saveBtn.textContent = state.savingEmbedding
        ? STR.saving
        : reindex && state.reindexAck
          ? STR.embedding.saveReindex
          : STR.save;
      discardBtn.disabled = !dirtyNow;
      reindexBanner.hidden = !reindex;
      const body = reindexBanner.querySelector(".reindex-body");
      if (body && state.indexInfo) body.textContent = STR.embedding.reindexBody(state.indexInfo.embedded);
      ack.disabled = !reindex;
      if (!reindex) state.reindexAck = false;
    };

    if (state.embeddingTest) {
      testResult.className = `inline-result ${state.embeddingTest.ok ? "ok" : "bad"}`;
      testResult.textContent = state.embeddingTest.ok
        ? STR.embedding.testOk(
            state.embeddingTest.vectorLength ?? 0,
            state.embeddingTest.latencyMs ?? 0,
          )
        : `${STR.embedding.testFailed}: ${state.embeddingTest.error ?? ""}`;
    }

    const panelEl = panel(
      "embedding",
      h("h3", { class: "settings-section-title" }, STR.sections.embedding),
      h("p", { class: "settings-intro" }, STR.embedding.intro),
      h(
        "div",
        { class: "field" },
        h(
          "label",
          { class: "checkbox-row", for: "embedding-enabled" },
          enabledInput,
          STR.embedding.enabled,
        ),
        h("p", { class: "field-help" }, STR.embedding.enabledHelp),
      ),
      state.indexInfo
        ? h(
            "p",
            { class: "muted small" },
            STR.embedding.indexInfo(state.indexInfo.embedded, state.indexInfo.total),
          )
        : null,
      h(
        "div",
        { class: "field" },
        h("label", { for: "embedding-provider" }, STR.embedding.provider),
        providerSelect,
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "embedding-model" }, STR.embedding.model),
        modelInput,
        h("datalist", { id: "embedding-models" },
          ...Object.keys(KNOWN_EMBEDDING_MODELS).map((m) => h("option", { value: m })),
        ),
        errors.model
          ? h("p", { class: "field-error", id: "embedding-model-error", role: "alert" }, errors.model)
          : h("p", { class: "field-help", id: "embedding-model-help" }, STR.embedding.modelHelp),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "embedding-dimensions" }, STR.embedding.dimensions),
        dimensionsInput,
        errors.dimensions
          ? h("p", { class: "field-error", role: "alert" }, errors.dimensions)
          : h("p", { class: "field-help" }, STR.embedding.dimensionsHelp),
      ),
      numberInput(
        "batchSize",
        cfg.batchSize,
        (n) => {
          state.embedding = { ...state.embedding!, batchSize: n };
        },
        !!errors.batchSize,
        STR.embedding.batchSizeHelp,
      ),
      numberInput(
        "timeoutSeconds",
        cfg.timeoutSeconds,
        (n) => {
          state.embedding = { ...state.embedding!, timeoutSeconds: n };
        },
        !!errors.timeoutSeconds,
        STR.embedding.timeoutHelp,
      ),
      reindexBanner,
      h("div", { class: "test-row" }, testButton, testResult),
      h("div", { class: "settings-footer" }, discardBtn, saveBtn),
    );
    refreshEmbeddingFooter();
    return panelEl;
  }

  // ------------------------------------------------------------------- tools

  function toolsPanel(): HTMLElement {
    if (state.tools.length === 0) return panel("tools", loadingPanel());
    return panel(
      "tools",
      h(
        "div",
        { class: "settings-section-head" },
        h(
          "div",
          {},
          h("h3", { class: "settings-section-title" }, STR.sections.tools),
          h("p", { class: "settings-intro" }, STR.tools.intro),
        ),
        h(
          "div",
          { class: "bulk-actions" },
          h("button", { class: "ghost small-btn", onclick: () => void bulkTools(false) }, STR.tools.disableAll),
          h("button", { class: "ghost small-btn", onclick: () => void bulkTools(true) }, STR.tools.enableRecommended),
        ),
      ),
      h("div", { class: "tool-list" }, ...state.tools.map(toolCard)),
    );
  }

  function toolCard(tool: BuiltInTool): HTMLElement {
    const locked = tool.requiresSetup && !tool.setupComplete;
    const toggle = h("input", {
      type: "checkbox",
      class: "switch-input",
      role: "switch",
      id: `tool-${tool.id}`,
      checked: tool.enabled,
      disabled: locked,
      "aria-label": tool.name,
      onchange: (e: Event) => void toggleTool(tool, (e.target as HTMLInputElement).checked),
    }) as HTMLInputElement;

    return h(
      "article",
      { class: `tool-card ${tool.enabled ? "" : "is-off"}`, dataset: { tool: tool.id } },
      h(
        "div",
        { class: "tool-card-main" },
        h(
          "div",
          { class: "tool-card-title" },
          h("strong", {}, tool.name),
          locked ? h("span", { class: "badge warn" }, STR.tools.setupRequired) : null,
        ),
        h("p", { class: "tool-card-desc muted" }, tool.description),
        h(
          "details",
          { class: "tool-details" },
          h("summary", {}, STR.tools.details),
          h(
            "p",
            { class: "small muted" },
            `${STR.tools.permission}: ${STR.tools.permissions[tool.permission] ?? tool.permission}`,
          ),
        ),
        locked
          ? h(
              "p",
              { class: "small" },
              STR.tools.setupHelp,
              " ",
              h(
                "button",
                { class: "link", onclick: () => void switchSection("web") },
                STR.tools.configure,
              ),
            )
          : null,
      ),
      h(
        "div",
        { class: "tool-card-side" },
        h("label", { class: "switch", for: `tool-${tool.id}` }, toggle, h("span", { class: "switch-track" })),
        h("span", { class: "small muted" }, tool.enabled ? STR.tools.enabled : STR.tools.disabled),
      ),
    );
  }

  // ---------------------------------------------------------------- database

  const bytes = (n: number): string => {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  };

  async function refreshDatabase() {
    try {
      state.database = await ipc.invoke<DatabaseInfo>("get_database_info");
      render();
    } catch (e) {
      toast(`${STR.database.failed}: ${errorText(e)}`, "bad");
    }
  }

  async function backupDatabase() {
    try {
      const dest = await saveFileDialog({
        title: "Backup database",
        defaultPath: `aria-backup-${new Date().toISOString().slice(0, 10)}.db`,
        filters: [{ name: "SQLite database", extensions: ["db"] }],
      });
      if (typeof dest !== "string") return;
      await ipc.invoke("backup_database", { dest });
      toast(STR.database.backupDone(dest));
    } catch (e) {
      toast(`${STR.database.failed}: ${errorText(e)}`, "bad");
    }
  }

  async function importDatabase() {
    try {
      const path = await openFileDialog({
        title: "Import database",
        multiple: false,
        directory: false,
        filters: [{ name: "SQLite database", extensions: ["db"] }],
      });
      if (typeof path !== "string") return;
      const ok = await confirmDialog(
        STR.database.importTitle,
        STR.database.importBody,
        STR.database.importConfirm,
        true,
      );
      if (!ok) return;
      state.database = await ipc.invoke<DatabaseInfo>("import_database", { path });
      onDatabaseReplaced?.();
      toast(STR.database.importDone);
      render();
    } catch (e) {
      toast(`${STR.database.failed}: ${errorText(e)}`, "bad");
    }
  }

  async function resetDatabase() {
    try {
      const ok = await confirmDialog(
        STR.database.resetTitle,
        STR.database.resetBody,
        STR.database.resetConfirm,
        true,
      );
      if (!ok) return;
      state.database = await ipc.invoke<DatabaseInfo>("reset_database");
      onDatabaseReplaced?.();
      toast(STR.database.resetDone);
      render();
    } catch (e) {
      toast(`${STR.database.failed}: ${errorText(e)}`, "bad");
    }
  }

  function databasePanel(): HTMLElement {
    const db = state.database;
    if (!db) return panel("database", loadingPanel());
    const d = STR.database;
    const row = (label: string, value: string) => [h("dt", {}, label), h("dd", {}, value)];
    return panel(
      "database",
      h("h3", { class: "settings-section-title" }, STR.sections.database),
      h("p", { class: "settings-intro" }, d.intro),
      h(
        "dl",
        { class: "db-grid" },
        ...row(d.location, db.path),
        ...row(
          d.size,
          db.walBytes > 0
            ? `${bytes(db.sizeBytes)} + ${bytes(db.walBytes)} log`
            : bytes(db.sizeBytes),
        ),
        ...row(d.modified, db.modifiedAt ? fmtTime(db.modifiedAt) : "—"),
        ...row(
          d.engine,
          `SQLite ${db.sqliteVersion} · ${db.journalMode} · ${bytes(db.pageSize)} pages`,
        ),
        ...row(d.schema, String(db.schemaVersion)),
        ...row(d.blobs, `${db.blobs} · ${bytes(db.blobsBytes)} · ${db.blobsDir}`),
        ...row(d.backups, db.backupsDir),
      ),
      h("h4", { class: "db-subtitle" }, d.contents),
      h(
        "dl",
        { class: "db-grid" },
        ...db.tables.flatMap((t) => row(d.tables[t.name] ?? t.name, String(t.rows))),
      ),
      h(
        "div",
        { class: "db-actions" },
        h("button", { class: "primary", onclick: () => void backupDatabase() }, d.backup),
        h("button", { class: "ghost", onclick: () => void importDatabase() }, d.import),
        h("button", { class: "ghost", onclick: () => void refreshDatabase() }, d.refresh),
        h("button", { class: "danger", onclick: () => void resetDatabase() }, d.reset),
      ),
      h("p", { class: "field-help" }, `${d.backupHelp} ${d.importHelp} ${d.resetHelp}`),
    );
  }

  // --------------------------------------------------------------------- web

  function webPanel(): HTMLElement {
    if (state.web && !state.webForm) return webConnected(state.web);
    return webConnectForm();
  }

  function webConnected(web: WebSearchConfig): HTMLElement {
    const provider = STR.web.providers[web.providerKey];
    const test = state.searchTest;
    const advancedOpen = !!state.webAdvanced;
    return panel(
      "web",
      h("h3", { class: "settings-section-title" }, STR.sections.web),
      h("p", { class: "settings-intro" }, STR.web.intro),
      h(
        "div",
        { class: "web-connected", dataset: { provider: web.providerKey } },
        h(
          "div",
          { class: "web-connected-head" },
          h(
            "div",
            {},
            h("strong", { class: "web-provider-name" }, provider?.name ?? web.providerKey),
            h("span", { class: `badge ${web.status === "connected" ? "ok" : "bad"}` },
              web.status === "connected" ? STR.web.connected : STR.web.disconnected),
          ),
          h(
            "div",
            { class: "provider-item-actions" },
            h(
              "button",
              {
                class: "ghost small-btn",
                id: "web-test",
                disabled: !!state.testingSearch,
                onclick: () => void testSearch(),
              },
              state.testingSearch ? STR.web.testingSearch : STR.web.testSearch,
            ),
            h(
              "button",
              {
                class: "ghost small-btn",
                onclick: () => {
                  state.webForm = { providerKey: web.providerKey, apiKey: "", baseUrl: "" };
                  state.webErrors = {};
                  render();
                },
              },
              STR.web.replaceKey,
            ),
            h(
              "button",
              {
                class: "ghost small-btn",
                onclick: () => {
                  state.webForm = { providerKey: web.providerKey, apiKey: "", baseUrl: "" };
                  state.webErrors = {};
                  render();
                },
              },
              STR.web.switchProvider,
            ),
            h(
              "button",
              { class: "ghost small-btn danger", id: "web-disconnect", onclick: () => void disconnectWeb() },
              STR.web.disconnect,
            ),
          ),
        ),
        h(
          "dl",
          { class: "meta-list" },
          h("dt", {}, STR.web.maskedKey),
          h("dd", { class: "mono" }, web.apiKeyMasked || "••••"),
          h("dt", {}, STR.web.lastVerified),
          h("dd", {}, web.lastVerifiedAt ? fmtTime(web.lastVerifiedAt) : "—"),
        ),
        test
          ? h(
              "div",
              { class: `inline-result ${test.ok ? "ok" : "bad"}`, role: "status" },
              test.ok ? STR.web.testOk : `${STR.web.testFailed}: ${test.error ?? ""}`,
              test.ok && test.titles.length
                ? h(
                    "ul",
                    { class: "search-preview" },
                    ...test.titles.map((t) =>
                      h("li", {}, h("span", {}, t.title), h("span", { class: "muted" }, ` — ${t.domain}`)),
                    ),
                  )
                : null,
            )
          : null,
        h(
          "details",
          {
            class: "tool-details",
            ontoggle: (e: Event) => {
              state.webAdvanced = (e.target as HTMLDetailsElement).open;
            },
            open: advancedOpen,
          },
          h("summary", {}, STR.web.advanced),
          h("p", { class: "small muted" }, STR.web.advancedHelp),
          h(
            "dl",
            { class: "meta-list" },
            h("dt", {}, STR.web.providerLabel),
            h("dd", {}, provider?.name ?? web.providerKey),
            h("dt", {}, STR.web.status),
            h("dd", {}, web.status),
            h("dt", {}, STR.web.endpoint),
            h("dd", { class: "mono" }, provider?.keyUrl || "—"),
          ),
        ),
      ),
    );
  }

  function webConnectForm(): HTMLElement {
    if (!state.webForm) {
      state.webForm = { providerKey: "exa", apiKey: "", baseUrl: "" };
    }
    const f = state.webForm;
    const provider = STR.web.providers[f.providerKey] ?? STR.web.providers.exa;
    const errors = state.webErrors ?? {};

    const cards = h(
      "div",
      { class: "web-provider-grid", role: "radiogroup", "aria-label": STR.web.providerLabel },
      ...Object.entries(STR.web.providers).map(([key, info]) =>
        h(
          "button",
          {
            type: "button",
            role: "radio",
            "aria-checked": f.providerKey === key ? "true" : "false",
            class: `web-provider-card ${f.providerKey === key ? "selected" : ""}`,
            onclick: () => {
              state.webForm = { providerKey: key, apiKey: "", baseUrl: "" };
              state.webErrors = {};
              render();
            },
          },
          h("strong", {}, info.name),
          h("span", { class: "muted small" }, info.note),
        ),
      ),
    );

    const keyInput = h("input", {
      id: "web-api-key",
      type: "password",
      value: f.apiKey,
      placeholder: provider.placeholder,
      autocomplete: "new-password",
      spellcheck: "false",
      "aria-invalid": errors.apiKey ? "true" : "false",
      "aria-describedby": errors.apiKey ? "web-api-key-error" : "web-api-key-help",
      oninput: (e: Event) => {
        const cur = state.webForm ?? { providerKey: "exa", apiKey: "", baseUrl: "" };
        state.webForm = { ...cur, apiKey: (e.target as HTMLInputElement).value };
        state.webErrors = {};
      },
    }) as HTMLInputElement;

    const baseUrlInput = h("input", {
      id: "web-base-url",
      type: "url",
      value: f.baseUrl,
      placeholder: STR.web.baseUrlPlaceholder,
      autocomplete: "off",
      spellcheck: "false",
      "aria-invalid": errors.baseUrl ? "true" : "false",
      oninput: (e: Event) => {
        const cur = state.webForm ?? { providerKey: "exa", apiKey: "", baseUrl: "" };
        state.webForm = { ...cur, baseUrl: (e.target as HTMLInputElement).value };
        state.webErrors = {};
      },
    }) as HTMLInputElement;

    const needsKey = !!provider.keyLabel;
    const needsBase = f.providerKey === "searxng";

    return panel(
      "web",
      h("h3", { class: "settings-section-title" }, STR.sections.web),
      h("p", { class: "settings-intro" }, STR.web.intro),
      cards,
      needsKey
        ? h(
            "div",
            { class: "field" },
            h(
              "div",
              { class: "label-row" },
              h("label", { for: "web-api-key" }, provider.keyLabel),
              provider.keyUrl
                ? h(
                    "a",
                    {
                      class: "external-link",
                      href: provider.keyUrl,
                      target: "_blank",
                      rel: "noopener noreferrer",
                    },
                    STR.web.getKey,
                  )
                : null,
            ),
            keyInput,
            errors.apiKey
              ? h("p", { class: "field-error", id: "web-api-key-error", role: "alert" }, errors.apiKey)
              : h("p", { class: "field-help", id: "web-api-key-help" }, STR.web.apiKeyHelp),
          )
        : null,
      needsBase
        ? h(
            "div",
            { class: "field" },
            h("label", { for: "web-base-url" }, STR.web.baseUrl),
            baseUrlInput,
            errors.baseUrl
              ? h("p", { class: "field-error", role: "alert" }, errors.baseUrl)
              : h("p", { class: "field-help" }, STR.web.baseUrlHelp),
          )
        : null,
      errors.form ? h("p", { class: "field-error", role: "alert" }, errors.form) : null,
      h(
        "div",
        { class: "settings-footer" },
        state.web
          ? h(
              "button",
              {
                class: "ghost",
                onclick: () => {
                  state.webForm = null;
                  state.webErrors = {};
                  render();
                },
              },
              STR.cancel,
            )
          : null,
        h(
          "button",
          {
            class: "primary",
            id: "web-connect",
            disabled: !!state.connecting,
            onclick: () => void connectWeb(),
          },
          state.connecting ? STR.web.verifying : STR.web.connect,
        ),
      ),
    );
  }

  // Land on the active section's tab so the keyboard path starts in the nav.
  state.focusTabId = state.section;
  void render();
  void loadAll();
}

// -------------------------------------------------------------------- state

interface GeneralState {
  approvalPolicy: string;
  baseUrl: string;
  model: string;
  errors?: Record<string, string>;
}

interface ProviderFormState {
  id?: string;
  name: string;
  endpointUrl: string;
  apiKey: string;
  modelIds: string[];
  defaultModelId: string;
  showKey: boolean;
  saving: boolean;
  errors: Record<string, string>;
  test?: { ok: boolean; latencyMs?: number; error?: string } | null;
  /// Snapshot of the editable fields when the form opened, for dirty tracking.
  baseline: string;
}

interface WebFormState {
  providerKey: string;
  apiKey: string;
  baseUrl: string;
}

interface State {
  section: SectionId;
  loading: boolean;
  loadError: boolean;
  general: GeneralState | null;
  generalBaseline: GeneralState | null;
  providers: AIProvider[];
  providersLoading: boolean;
  providersError: boolean;
  form: ProviderFormState | null;
  embedding: EmbeddingConfig | null;
  embeddingBaseline: EmbeddingConfig | null;
  embeddingErrors?: Record<string, string>;
  embeddingTest?: { ok: boolean; vectorLength?: number; latencyMs?: number; error?: string };
  indexInfo: EmbeddingIndexInfo | null;
  tools: BuiltInTool[];
  web: WebSearchConfig | null;
  webForm: WebFormState | null;
  database: DatabaseInfo | null;
  webErrors?: Record<string, string>;
  webAdvanced?: boolean;
  /// Tab to focus after the next render (keyboard navigation).
  focusTabId?: SectionId;
  connecting?: boolean;
  testingSearch?: boolean;
  searchTest?: { ok: boolean; titles: { title: string; domain: string }[]; error?: string } | null;
  testingEmbedding?: boolean;
  savingEmbedding?: boolean;
  reindexAck?: boolean;
  savingGeneral?: boolean;
  testingProviderId?: string | null;
  providerTest?: { id: string; ok: boolean; latencyMs?: number; error?: string } | null;
}

function generalFrom(view: SettingsView): GeneralState {
  const s: Settings = view.settings;
  return {
    approvalPolicy: s.approval_policy || "always_ask",
    baseUrl: s.base_url,
    model: s.model,
  };
}

function isDirty(g: GeneralState | null, baseline: GeneralState | null): boolean {
  if (!g || !baseline) return false;
  return (
    g.approvalPolicy !== baseline.approvalPolicy ||
    g.baseUrl !== baseline.baseUrl ||
    g.model !== baseline.model
  );
}

function generalErrors(g: GeneralState): Record<string, string> {
  const errors: Record<string, string> = {};
  return errors;
}

function blankForm(): ProviderFormState {
  const f: ProviderFormState = {
    name: "",
    endpointUrl: "",
    apiKey: "",
    modelIds: [],
    defaultModelId: "",
    showKey: false,
    saving: false,
    errors: {},
    baseline: "",
  };
  f.baseline = formSnapshot(f);
  return f;
}

function formFrom(p: AIProvider): ProviderFormState {
  const f: ProviderFormState = {
    id: p.id,
    name: p.name,
    endpointUrl: p.endpointUrl,
    // Never prefill a key: the backend only ever returns the mask.
    apiKey: "",
    modelIds: [...p.modelIds],
    defaultModelId: p.defaultModelId,
    showKey: false,
    saving: false,
    errors: {},
    baseline: "",
  };
  f.baseline = formSnapshot(f);
  return f;
}

/** The editable fields, serialized for dirty comparison. */
function formSnapshot(f: ProviderFormState): string {
  return JSON.stringify({
    name: f.name,
    endpointUrl: f.endpointUrl,
    apiKey: f.apiKey,
    modelIds: f.modelIds,
    defaultModelId: f.defaultModelId,
  });
}

function isFormDirty(f: ProviderFormState | null): boolean {
  return !!f && formSnapshot(f) !== f.baseline;
}

function isEmbeddingDirty(
  cfg: EmbeddingConfig | null,
  baseline: EmbeddingConfig | null,
): boolean {
  if (!cfg || !baseline) return false;
  return JSON.stringify(cfg) !== JSON.stringify(baseline);
}

function providerErrors(
  f: ProviderFormState,
  providers: AIProvider[],
): Record<string, string> {
  // The shared validator covers name/endpoint/modelIds; the duplicate check
  // needs the provider list, which it does not have.
  const base = validateProviderForm({
    name: f.name,
    endpointUrl: f.endpointUrl,
    apiKey: f.id ? "present" : f.apiKey,
    modelIds: f.modelIds,
  });
  const errors: Record<string, string> = { ...base };
  const duplicate = providers.some(
    (p) => p.name.trim().toLowerCase() === f.name.trim().toLowerCase() && p.id !== f.id,
  );
  if (duplicate) errors.name = STR.ai.duplicateName;
  if (!f.id && !f.apiKey.trim()) errors.apiKey = STR.ai.requiredKey;
  return errors;
}

function embeddingErrors(cfg: EmbeddingConfig): Record<string, string> {
  return validateEmbeddingForm({
    model: cfg.model,
    dimensions: cfg.dimensions,
    batchSize: cfg.batchSize,
    timeoutSeconds: cfg.timeoutSeconds,
  });
}

function statusBadge(p: AIProvider): HTMLElement {
  const label =
    p.status === "active" ? STR.ai.active : p.status === "error" ? STR.ai.error : STR.ai.untested;
  return h("span", { class: `badge ${p.status === "active" ? "ok" : p.status === "error" ? "bad" : "neutral"}` }, label);
}

function focusFirstError(root: HTMLElement) {
  const el = root.querySelector<HTMLElement>('[aria-invalid="true"], .invalid');
  el?.focus();
}

// ---------------------------------------------------------- confirm dialog

function confirmDialog(
  title: string,
  body: string,
  confirmLabel: string,
  danger = false,
): Promise<boolean> {
  return new Promise((resolve) => {
    const previous = document.activeElement as HTMLElement | null;
    const overlay = h("div", { class: "modal-backdrop confirm-backdrop" });
    const dialog = h(
      "div",
      {
        class: "modal confirm-dialog",
        role: "alertdialog",
        "aria-modal": "true",
        "aria-labelledby": "confirm-title",
        "aria-describedby": "confirm-body",
        tabindex: "-1",
      },
      h("h2", { id: "confirm-title" }, title),
      h("p", { id: "confirm-body", class: "muted" }, body),
      h(
        "div",
        { class: "confirm-actions" },
        h("button", { class: "ghost", id: "confirm-cancel" }, STR.keepEditing),
        h(
          "button",
          { class: `primary ${danger ? "danger" : ""}`, id: "confirm-ok" },
          confirmLabel,
        ),
      ),
    );
    overlay.append(dialog);

    const done = (value: boolean) => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previous?.focus?.();
      resolve(value);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        done(false);
      } else if (e.key === "Tab") {
        e.stopPropagation();
        const items = focusables(dialog);
        if (!items.length) return;
        const first = items[0];
        const last = items[items.length - 1];
        if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        } else if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        }
      }
    };
    document.addEventListener("keydown", onKey, true);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) done(false);
    });
    dialog.querySelector("#confirm-cancel")!.addEventListener("click", () => done(false));
    dialog.querySelector("#confirm-ok")!.addEventListener("click", () => done(true));
    document.body.append(overlay);
    requestAnimationFrame(() => dialog.querySelector<HTMLElement>("#confirm-cancel")?.focus());
  });
}

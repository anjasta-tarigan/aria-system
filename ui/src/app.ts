import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  type AIProvider,
  type ApprovalClass,
  type BackgroundTask,
  type ChangedFile,
  type ConversationDetail,
  type ConversationMode,
  type ConversationView,
  type HealthView,
  type MessageHit,
  type Memory,
  type RunEventEnvelope,
  type MessageView,
  type AttachmentView,
  type PendingApproval,
  type Run,
  type RunEvent,
  type Todo,
  type ToolCall,
} from "./api";
import { mockFaults, mockInvoke, mockListen, seed } from "./mock";
import { h } from "./dom";
import { openSettings as openSettingsModal } from "./settings";
import { renderMarkdown } from "./format";
import {
  type ArtifactVersion,
  type ConversationArtifactIndex,
  buildArtifactIndex,
  segmentMessage,
  renderArtifactCard,
  renderArtifactPanel,
  isArtifactPreviewable,
} from "./artifacts";

/// Thin adapters the UI goes through, so `?mock=1` can swap the whole Rust
/// side for the in-browser fake.
export interface Ipc {
  invoke: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
  listen: (cb: (e: { payload: RunEventEnvelope }) => void) => Promise<() => void>;
  listCloudFolders: () => Promise<{ name: string; path: string }[]>;
}

// The real app talks to the Rust commands.
export const call: Ipc = {
  invoke: (cmd, args) => invoke(cmd, args) as Promise<never>,
  listen: (cb) => listen<RunEventEnvelope>("run-event", (e) => cb({ payload: e.payload })),
  listCloudFolders: () =>
    invoke<{ name: string; path: string }[]>("list_cloud_folders"),
};

export function useMock() {
  call.invoke = mockInvoke;
  call.listen = mockListen;
  call.listCloudFolders = () => mockInvoke("list_cloud_folders");
  seed();
}

// ------------------------------------------------------------------ helpers

const fmt = (ms: number) =>
  new Date(ms).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });

function args(call: ToolCall): Record<string, unknown> {
  try {
    return JSON.parse(call.args_json) as Record<string, unknown>;
  } catch {
    return {};
  }
}

const stateLabel: Record<Run["state"], string> = {
  running: "Running",
  awaiting_approval: "Waiting for you",
  finished: "Finished",
  budget_exhausted: "Budget exhausted",
  cancelled: "Cancelled",
  errored: "Failed",
};

// --------------------------------------------------------------------- state

interface Ui {
  conversations: ConversationView[];
  detail: ConversationDetail | null;
  /// Live text of the Turn being streamed, keyed by run id.
  streaming: { runId: string; text: string; reasoning: string; turn: number } | null;
  /// Live shell output per tool call.
  toolOutput: Record<string, string[]>;
  changed: ChangedFile[];
  search: MessageHit[];
  notice: string | null;
  jumpTo: string | null;
  /// Pending-approval count at the last render, to notice one appearing.
  pendingCount: number;
  /// Tool-call ids whose cards have been expanded by the user.
  expanded: Set<string>;
  /// Conversation id currently being renamed inline in the sidebar, if any.
  renamingId: string | null;
  /// Workspace ids whose session list is collapsed in the sidebar tree.
  collapsed: Set<string>;
  /// Background tasks for the current workspace conversation (cached between polls).
  bgTasks: BackgroundTask[];
  /// Whether the stored-memories viewer is open.
  memoriesOpen: boolean;
  memories: Memory[];
  /// Conversation id whose Main System access modal is open, if any.
  systemAccessId: string | null;
  /// Latest system health snapshot for the footer (polled in the background).
  health: HealthView | null;
  /// Available AI providers (polled or fetched for model selection).
  providers: AIProvider[];
  /// Active artifact being displayed in the side panel: identifier + version.
  activeArtifact: { identifier: string; version: number } | null;
  /// Tab currently active in the artifact panel: "preview" | "code".
  artifactTab: "preview" | "code";
  /// Side panel tab: "artifacts" | "files".
  sidePanelTab: "artifacts" | "files";
  /// Whether the side panel is open.
  sidePanelOpen: boolean;
  /// Identifier@version of the last auto-opened artifact.
  autoOpenedArtifactId: string | null;
  /// Currently open image lightbox: src data URL + file name.
  lightbox: { src: string; name: string } | null;
}

const ui: Ui = {
  conversations: [],
  detail: null,
  streaming: null,
  toolOutput: {},
  changed: [],
  search: [],
  notice: null,
  jumpTo: null,
  pendingCount: 0,
  expanded: new Set(),
  renamingId: null,
  collapsed: new Set(),
  bgTasks: [],
  memoriesOpen: false,
  memories: [],
  systemAccessId: null,
  /// Latest system health snapshot for the footer (polled in the background).
  health: null as HealthView | null,
  providers: [] as AIProvider[],
  activeArtifact: null,
  artifactTab: "preview",
  sidePanelTab: "artifacts",
  sidePanelOpen: true,
  autoOpenedArtifactId: null,
  lightbox: null,
};

const root = document.querySelector<HTMLElement>("#app")!;

/// Live subscription to Run events, kept so HMR can tear the app down cleanly.
let unsubscribe: (() => void) | null = null;
/// Set by stop(); stale async work must not paint over the new instance.
let disposed = false;
let streamingArtifactActive = false;

// Files panel state — kept module-level so expansion/editing survive re-renders.
let filesOpen = true;
let activeFile: string | null = null;
// Loaded file contents, keyed by rel, restored into the freshly-built editor on
// every re-render so openFile/saveFile don't lose the text (render rebuilds it).
const editorBuffer: Map<string, string> = new Map();
const openDirs: Set<string> = new Set([""]);
/// Directory the Files panel is browsing: blank for the conversation root, an
/// absolute path to reach anywhere the scope allows.
let filesRoot = "";
let filesConvId: string | null = null;

// Auto-poll timer for background tasks; cleared on re-render so only one loop
// runs for the visible conversation.
let bgPoll: ReturnType<typeof setTimeout> | null = null;

// Single real-time health poll loop for the footer.
let healthPoll: ReturnType<typeof setInterval> | null = null;

async function pollHealth() {
  try {
    ui.health = await call.invoke<HealthView>("health");
  } catch {
    ui.health = null;
  }
  try {
    ui.providers = await call.invoke<AIProvider[]>("list_ai_providers");
  } catch {
    // Keep current providers
  }
  if (ui.health) renderFooterOnly();
}

function startHealthPoll() {
  if (healthPoll) return;
  void pollHealth();
  healthPoll = setInterval(() => void pollHealth(), 5000);
}

function activeRun(): Run | null {
  return ui.detail?.conversation.run ?? null;
}

function isBusy(run: Run | null): boolean {
  return !!run && (run.state === "running" || run.state === "awaiting_approval");
}

// -------------------------------------------------------------------- render

function render() {
  if (disposed) return;
  if (bgPoll) {
    clearTimeout(bgPoll);
    bgPoll = null;
  }
  const detail = ui.detail;
  const previous = document.querySelector<HTMLElement>(".transcript");
  const previousTop = previous?.scrollTop ?? 0;
  const follow = !previous || previous.scrollHeight - previousTop - previous.clientHeight < 120;

  const artifactsIndex = detail
    ? buildArtifactIndex(detail.messages, ui.streaming?.text)
    : null;

  // Auto-open newest artifact if streaming or if none selected yet
  if (artifactsIndex && artifactsIndex.all.length > 0) {
    const latestArt = artifactsIndex.all[artifactsIndex.all.length - 1];
    const artKey = `${latestArt.identifier}@${latestArt.version}`;
    if (ui.autoOpenedArtifactId !== artKey && (ui.streaming || !ui.activeArtifact)) {
      ui.autoOpenedArtifactId = artKey;
      ui.activeArtifact = { identifier: latestArt.identifier, version: latestArt.version };
      ui.sidePanelOpen = true;
      ui.sidePanelTab = "artifacts";
      if (latestArt.isStreaming) {
        streamingArtifactActive = true;
        ui.artifactTab = "code";
      } else {
        ui.artifactTab = isArtifactPreviewable(latestArt.type) ? "preview" : "code";
      }
    } else if (streamingArtifactActive && !latestArt.isStreaming && !ui.streaming) {
      streamingArtifactActive = false;
      if (isArtifactPreviewable(latestArt.type)) {
        ui.artifactTab = "preview";
      }
    }
  }

  let activeArtVersion: ArtifactVersion | null = null;
  let allVersionsForActive: ArtifactVersion[] = [];
  if (artifactsIndex && ui.activeArtifact) {
    allVersionsForActive = artifactsIndex.byIdentifier.get(ui.activeArtifact.identifier) ?? [];
    activeArtVersion =
      allVersionsForActive.find((v) => v.version === ui.activeArtifact!.version) ??
      allVersionsForActive[allVersionsForActive.length - 1] ??
      null;
  }
  const showArtifact = activeArtVersion !== null;

  const body = h(
    "div",
    { class: `app-body ${showArtifact && ui.sidePanelOpen ? "has-artifact" : ""}${!ui.sidePanelOpen ? " side-panel-closed" : ""}` },
    sidebar(),
    h("main", { class: "pane" }, detail ? conversationPane(detail, artifactsIndex) : emptyPane()),
    sidePanelNode(activeArtVersion, allVersionsForActive),
  );
  const shell = h("div", { class: "app" }, body, footer());
  root.replaceChildren(
    shell,
    ...(ui.memoriesOpen ? [memoriesModal()] : []),
    ...(ui.systemAccessId ? [systemAccessModal(ui.systemAccessId)] : []),
    ...(ui.lightbox ? [lightboxNode()!] : []),
  );
  if (ui.jumpTo) {
    const el = document.getElementById(`msg-${ui.jumpTo}`);
    el?.scrollIntoView({ block: "center" });
    el?.classList.add("flash");
    ui.jumpTo = null;
  }
  const log = document.querySelector<HTMLElement>(".transcript");
  if (log) {
    // The DOM is rebuilt on every render, so restore the reader's position and
    // follow the newest content unless they had scrolled up. A newly arrived
    // Approval always scrolls into view so it cannot be missed.
    const approvals = detail?.pending.length ?? 0;
    log.scrollTop =
      ui.streaming || approvals > ui.pendingCount || follow ? log.scrollHeight : previousTop;
    ui.pendingCount = approvals;
  }
}

// Throttled render for high-frequency streaming events (text_delta,
// reasoning_delta, tool_output). Rebuilding the whole DOM on every token
// keeps the main thread busy and can make the UI feel frozen; cap it at one
// render per animation frame (~60 fps). User-initiated actions still call
// render() directly for instant response.
let renderFrame: number | null = null;
function scheduleRender() {
  if (renderFrame !== null) return;
  renderFrame = requestAnimationFrame(() => {
    renderFrame = null;
    render();
  });
}

// --------------------------------------------------------------- sidebar

interface WsGroup {
  wsId: string;
  name: string;
  pinned: boolean;
  rootPath: string | null;
  maxUpdated: number;
  sessions: ConversationView[];
}

/// Basename of a folder path, used as the workspace's display name.
function dirName(p: string | null): string {
  if (!p) return "Workspace";
  const cleaned = p.replace(/\/+$/, "");
  const base = cleaned.split("/").pop() ?? cleaned;
  return base || "Workspace";
}

/// Label for a chat row: an empty title (older rows stored none, or the
/// default main session before it was named) falls back to "Main System".
function convTitle(c: ConversationView): string {
  return c.title.trim() || "Main System";
}

/// Compact, 24-hour timestamp for a sidebar entry.
function fmtShort(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) {
    return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });
  }
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function sidebar(): HTMLElement {
  const systems = ui.conversations.filter((c) => !c.workspace_id);
  const groups = groupWorkspaces(ui.conversations.filter((c) => !!c.workspace_id));

  const searchResults = ui.search.length
    ? h(
        "div",
        { class: "search-results" },
        ...ui.search.map((hit) =>
          h(
            "button",
            { class: "hit", onclick: () => void jumpToHit(hit) },
            h("span", { class: "hit-role" }, hit.role),
            h("span", { class: "hit-text" }, hit.snippet),
          ),
        ),
      )
    : null;

  const emptySearch =
    ui.search.length === 0 && lastSearchNonEmpty
      ? h("p", { class: "muted small search-empty" }, "No matches")
      : null;

  const systemGroup = h(
    "div",
    { class: "conv-group" },
    h("div", { class: "section-row" }, h("span", { class: "section-title" }, "System")),
    h(
      "div",
      { class: "conv-list" },
      ...systems.map((c) =>
        h(
          "button",
          {
            class: `conv main${ui.detail?.conversation.id === c.id ? " active" : ""}`,
            title: "Main System — available everywhere",
            onclick: () => void selectConversation(c.id),
          },
          h("span", { class: "conv-title" }, convTitle(c)),
        ),
      ),
    ),
  );

  const workspaceGroup = h(
    "div",
    { class: "conv-group" },
    h(
      "div",
      { class: "section-row" },
      h("span", { class: "section-title" }, "Workspaces"),
      h(
        "button",
        {
          class: "icon",
          title: "Open a folder",
          onclick: (e: MouseEvent) => void openCloudOrBrowse(e.currentTarget as HTMLElement),
        },
        "+",
      ),
    ),
    ...(groups.length
      ? groups.map(renderWorkspace)
      : [
          h(
            "p",
            { class: "muted small ws-hint" },
            "Open a folder with + to start a workspace.",
          ),
        ]),
  );

  // Bottom menu list: a fixed set of icon rows (Extension is a placeholder for
  // future plugins; Settings opens the settings panel). Memory lives elsewhere,
  // not in this menu.
  const menu = h(
    "nav",
    { class: "sidebar-menu" },
    h(
      "button",
      { class: "menu-item", title: "Extensions", onclick: () => {} },
      h("span", { class: "menu-icon" }, "🧩"),
      h("span", { class: "menu-label" }, "Extensions"),
    ),
    h(
      "button",
      { class: "menu-item", title: "Settings", onclick: () => void openSettings() },
      h("span", { class: "menu-icon" }, "⚙"),
      h("span", { class: "menu-label" }, "Settings"),
    ),
  );

  return h(
    "aside",
    { class: "sidebar" },
    h(
      "div",
      { class: "sidebar-main" },
      h("div", { class: "brand" }, h("span", { class: "logo" }, "Aria")),
      h(
        "input",
        {
          class: "search",
          type: "search",
          placeholder: "Search messages…",
          oninput: (e: Event) => void runSearch((e.target as HTMLInputElement).value),
        },
      ),
      searchResults,
      emptySearch,
      systemGroup,
      workspaceGroup,
    ),
    menu,
  );
}

/// Full-width footer: left = live system-health indicator, center = app version,
/// right = reserved. Updated in place by `renderFooterOnly` on each poll so the
/// 5s health refresh never rebuilds the whole app.
function footer(): HTMLElement {
  const el = h(
    "footer",
    { class: "app-foot" },
    h("div", { class: "foot-foot", id: "foot-health" }, healthContent()),
    h("div", { class: "foot-center", id: "foot-version" }, versionContent()),
    h("div", { class: "foot-right" }),
  );
  return el;
}

function healthContent(): HTMLElement {
  const h0 = ui.health;
  const status = h0?.status ?? "error";
  const label =
    status === "ok"
      ? "Healthy"
      : status === "degraded"
        ? "Provider unreachable"
        : h0?.providerReachable
          ? "Not configured"
          : "Unhealthy";
  return h(
    "div",
    { class: `health health-${status}`, title: h0?.detail ?? "" },
    h("span", { class: "health-dot" }),
    h("span", { class: "health-label" }, label),
  );
}

function versionContent(): HTMLElement {
  const v = ui.health?.version ?? "dev";
  return h("span", { class: "foot-version-text" }, `Aria v${v}`);
}

function renderFooterOnly() {
  const health = document.getElementById("foot-health");
  if (health) health.replaceChildren(healthContent());
  const ver = document.getElementById("foot-version");
  if (ver) ver.replaceChildren(versionContent());
}

function groupWorkspaces(convs: ConversationView[]): WsGroup[] {
  const byWs = new Map<string, ConversationView[]>();
  for (const c of convs) {
    const id = c.workspace_id as string;
    const list = byWs.get(id) ?? [];
    list.push(c);
    byWs.set(id, list);
  }
  const groups: WsGroup[] = [];
  for (const [wsId, sessions] of byWs) {
    const sorted = [...sessions].sort((a, b) => b.updated_at - a.updated_at);
    const rootPath = sorted[0].root_path ?? null;
    groups.push({
      wsId,
      name: dirName(rootPath),
      pinned: sorted[0].pinned ?? false,
      rootPath,
      maxUpdated: Math.max(...sorted.map((s) => s.updated_at)),
      sessions: sorted,
    });
  }
  groups.sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    return b.maxUpdated - a.maxUpdated;
  });
  return groups;
}

function renderWorkspace(ws: WsGroup): HTMLElement {
  const collapsed = ui.collapsed.has(ws.wsId);
  const head = h(
    "div",
    { class: "workspace-head" },
    h(
      "button",
      {
        class: "tree-caret",
        title: collapsed ? "Expand" : "Collapse",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          if (ui.collapsed.has(ws.wsId)) ui.collapsed.delete(ws.wsId);
          else ui.collapsed.add(ws.wsId);
          render();
        },
      },
      "▸",
    ),
    h(
      "button",
      {
        class: "ws-name",
        title: ws.rootPath ?? ws.name,
        onclick: () => void selectConversation(ws.sessions[0].id),
      },
      ws.name,
    ),
    h(
      "button",
      {
        class: "icon ws-action",
        title: "Workspace menu",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          openMenuAt(e.currentTarget as HTMLElement, [
            {
              label: ws.pinned ? "Unpin" : "Pin",
              onClick: () => void pinWorkspace(ws.wsId, !ws.pinned),
            },
            {
              label: "Delete workspace",
              danger: true,
              onClick: () => void removeWorkspace(ws.wsId),
            },
          ]);
        },
      },
      "⋮",
    ),
    h(
      "button",
      {
        class: "icon ws-action",
        title: "New chat in this workspace",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          void newSessionIn(ws.wsId);
        },
      },
      "+",
    ),
  );
  const children = h(
    "div",
    { class: "tree-children" },
    ...ws.sessions.map(renderSession),
  );
  return h(
    "div",
    { class: `workspace${ws.pinned ? " pinned" : ""}${collapsed ? " collapsed" : ""}` },
    head,
    children,
  );
}

function renderSession(c: ConversationView): HTMLElement {
  if (ui.renamingId === c.id) {
    return h("div", { class: "conv session renaming" }, renameInput(c));
  }
  const meta = h(
    "span",
    { class: "conv-meta" },
    h("span", { class: "when" }, fmtShort(c.updated_at)),
    h(
      "button",
      {
        class: "icon sess-action",
        title: "Session menu",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          openMenuAt(e.currentTarget as HTMLElement, [
            { label: "Rename", onClick: () => void startRename(c.id) },
            { label: "Delete", danger: true, onClick: () => void removeSession(c.id) },
          ]);
        },
      },
      "⋮",
    ),
  );
  return h(
    "div",
    {
      class: `conv session${ui.detail?.conversation.id === c.id ? " active" : ""}`,
      onclick: () => void selectConversation(c.id),
    },
    h("span", { class: "conv-title" }, convTitle(c)),
    meta,
  );
}

function renameInput(c: ConversationView): HTMLElement {
  const input = h("input", {
    class: "rename-input",
    type: "text",
    value: c.title,
  }) as HTMLInputElement;
  input.value = c.title;
  const commit = async () => {
    const v = input.value.trim();
    ui.renamingId = null;
    if (v && v !== c.title) {
      try {
        await call.invoke("rename_conversation", { conversationId: c.id, title: v });
        await refreshConversations();
      } catch (e) {
        ui.notice = String(e);
      }
    }
    render();
  };
  input.onkeydown = (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void commit();
    } else if (e.key === "Escape") {
      ui.renamingId = null;
      render();
    }
  };
  input.onblur = () => void commit();
  setTimeout(() => {
    input.focus();
    input.select();
  }, 0);
  return input;
}

// -------------------------------------------------------- workspace actions

async function pinWorkspace(wsId: string, pinned: boolean) {
  try {
    await call.invoke("set_workspace_pin", { workspaceId: wsId, pinned });
    await refreshConversations();
    render();
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function newSessionIn(wsId: string) {
  try {
    const conv = await call.invoke<ConversationView>("new_session", { workspaceId: wsId });
    await refreshConversations();
    await selectConversation(conv.id);
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function removeWorkspace(wsId: string) {
  if (!window.confirm("Delete this workspace and all its chat sessions?")) return;
  try {
    await call.invoke("delete_workspace", { workspaceId: wsId });
    if (ui.detail && ui.detail.conversation.workspace_id === wsId) {
      ui.detail = null;
      await selectFirst();
    }
    await refreshConversations();
    render();
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function removeSession(id: string) {
  if (!window.confirm("Delete this chat session?")) return;
  try {
    await call.invoke("delete_conversation", { conversationId: id });
    if (ui.detail?.conversation.id === id) {
      ui.detail = null;
      await selectFirst();
    }
    await refreshConversations();
    render();
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

function startRename(id: string) {
  ui.renamingId = id;
  render();
}

async function selectFirst() {
  if (ui.conversations.length === 0) {
    const main = await call.invoke<ConversationView>("new_conversation");
    ui.conversations = [main];
  }
  await selectConversation(ui.conversations[0].id);
}

// ------------------------------------------------------------- context menu

interface MenuItem {
  label: string;
  danger?: boolean;
  onClick: () => void;
}

let menuEl: HTMLDivElement | null = null;
let menuAnchor: HTMLElement | null = null;

function openMenuAt(anchor: HTMLElement, items: MenuItem[]) {
  closeMenu();
  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  for (const item of items) {
    const btn = document.createElement("button");
    btn.className = "ctx-item" + (item.danger ? " danger" : "");
    btn.textContent = item.label;
    btn.onclick = () => {
      closeMenu();
      item.onClick();
    };
    menu.appendChild(btn);
  }
  document.body.appendChild(menu);
  menuEl = menu;
  menuAnchor = anchor;
  positionMenu();
  window.addEventListener("resize", positionMenu);
  // Close on any outside interaction or Escape.
  setTimeout(() => {
    window.addEventListener("click", onMenuOutside, true);
    window.addEventListener("keydown", onMenuKey, true);
  }, 0);
}

/// Keep the menu attached just below its trigger inside the sidebar.
function positionMenu() {
  if (!menuEl || !menuAnchor) return;
  const r = menuAnchor.getBoundingClientRect();
  const mw = menuEl.offsetWidth;
  const mh = menuEl.offsetHeight;
  let left = menuEl.classList.contains("model-tree-popover")
    ? Math.min(r.left, window.innerWidth - mw - 8)
    : Math.min(r.right - mw, window.innerWidth - mw - 8);
  let top = r.bottom + 4;
  if (top + mh > window.innerHeight - 8) top = r.top - mh - 4;
  menuEl.style.left = `${Math.max(8, left)}px`;
  menuEl.style.top = `${Math.max(8, top)}px`;
}

function onMenuOutside(e: Event) {
  if (menuEl && !menuEl.contains(e.target as Node)) closeMenu();
}
function onMenuKey(e: KeyboardEvent) {
  if (e.key === "Escape") closeMenu();
}
function closeMenu() {
  if (menuEl) {
    menuEl.remove();
    menuEl = null;
  }
  menuAnchor = null;
  window.removeEventListener("resize", positionMenu);
  window.removeEventListener("click", onMenuOutside, true);
  window.removeEventListener("keydown", onMenuKey, true);
}

function emptyPane(): HTMLElement {
  return h(
    "div",
    { class: "empty" },
    h("h1", {}, "Hi, I'm Aria"),
    h(
      "p",
      {},
      "Ask me anything, or open a folder to let me work on the files inside it.",
    ),
    h("button", { class: "ghost", onclick: () => void chooseFolder() }, "📁 Work in a folder…"),
  );
}

/// Greeting shown inside a conversation that has no messages yet.
function welcomeNode(conv: ConversationView): HTMLElement {
  const chip = (text: string) =>
    h(
      "button",
      {
        class: "chip",
        onclick: () => {
          const input = document.querySelector<HTMLTextAreaElement>(".composer-input");
          if (input) {
            input.value = text;
            void send();
          }
        },
      },
      text,
    );
  return h(
    "div",
    { class: "welcome" },
    h("h1", {}, "Hi, I'm Aria"),
    h(
      "p",
      {},
      conv.main_system
        ? "I can work anywhere on this device — read and edit files, run commands, " +
            "start background tasks. Just tell me what you need in plain words."
        : "Ask me anything — explain something, draft an email, or plan a task. " +
            "When you want me to work on files, open a folder from the header.",
    ),
    h(
      "div",
      { class: "chips" },
      chip(
        conv.main_system
          ? "Find the largest files in my home folder"
          : "Explain this error message",
      ),
      chip("Draft a short email about…"),
      chip("Summarize the pros and cons of…"),
    ),
  );
}

function conversationPane(
  detail: ConversationDetail,
  artifactsIndex?: ConversationArtifactIndex | null,
): HTMLElement {
  const conv = detail.conversation;
  const run = conv.run;
  const busy = isBusy(run);
  const pending = detail.pending;
  const hasFolder = !!conv.workspace_id;
  const hasFiles = hasFolder || conv.main_system;
  const runningBg = ui.bgTasks.filter((t) => t.status === "running");

  const folderSlot = hasFolder
    ? h("span", { class: "path", title: conv.root_path ?? "" }, conv.root_path)
    : conv.main_system
      ? h(
          "span",
          { class: "path", title: "Aria can work anywhere on this device" },
          "Whole device",
        )
      : h(
          "span",
          { class: "path no-folder" },
          "No folder · ",
          h(
            "button",
            { class: "link", onclick: () => void chooseFolder() },
            "Choose one…",
          ),
        );

  // Stripped-down header: title + path + run state + cancel/continue.
  // Mode and approval live in the composer; system-access badge removed
  // (the modal is still reachable via the system access modal triggered
  // from conversationPane above the composer if needed).
  // Side panel toggle icon: close (X) when open, menu (hamburger) when closed.
  const toggleIcon = ui.sidePanelOpen ? "✕" : "☰";

  const header = h(
    "header",
    { class: "conv-header" },
    h("div", { class: "conv-header-title" }, h("h2", {}, convTitle(conv)), folderSlot),
    run ? h("span", { class: `badge ${run.state}` }, stateLabel[run.state]) : null,
    run?.state === "cancelled" || run?.state === "errored"
      ? h("span", { class: "error-text" }, run.error ?? run.stop_reason ?? "")
      : null,
    busy && run
      ? h("button", { class: "ghost", onclick: () => void cancel(run.id) }, "Cancel")
      : null,
    runningBg.length > 0
      ? h(
          "button",
          {
            class: "ghost bg-task-badge",
            title: `${runningBg.length} background task running`,
            onclick: () => {
              ui.sidePanelTab = "files";
              render();
            },
          },
          h("span", { class: "dot pulse" }),
          h("span", {}, `${runningBg.length} bg`),
        )
      : null,
    run?.state === "budget_exhausted"
      ? h("button", { class: "primary", onclick: () => void resume(run.id) }, "Continue")
      : null,
    h("button", {
      class: `ghost side-panel-toggle${ui.sidePanelOpen ? " open" : ""}`,
      title: ui.sidePanelOpen ? "Hide side panel" : "Show side panel",
      onclick: () => {
        ui.sidePanelOpen = !ui.sidePanelOpen;
        render();
      },
    }, toggleIcon),
  );

  const transcript = h(
    "div",
    { class: "transcript", role: "log", "aria-live": "polite" },
    ...(detail.messages.length === 0 && !busy ? [welcomeNode(conv)] : []),
    ...detail.messages.map((m) => messageNode(m, artifactsIndex)),
    streamingNode(artifactsIndex),
    ...pending.map(approvalNode),
  );

  const composer = composerNode(busy, hasFolder, conv);

  return h(
    "section",
    { class: "conversation" },
    header,
    transcript,
    ui.changed.length
      ? h(
          "div",
          { class: "changed" },
          h("span", { class: "section-title" }, "Changed files"),
          ...ui.changed.map((f) =>
            h(
              "span",
              { class: "changed-file" },
              h("code", {}, f.path),
              h(
                "button",
                {
                  class: "link",
                  disabled: !f.can_undo,
                  onclick: () => void undo(f.path),
                },
                "Undo",
              ),
            ),
          ),
        )
      : null,
    composer,
    ui.notice
      ? h(
          "div",
          { class: "notice" },
          h("span", {}, ui.notice),
          h(
            "button",
            { class: "icon notice-close", title: "Dismiss", onclick: () => {
              ui.notice = null;
              render();
            } },
            "×",
          ),
        )
      : null,
  );
}

async function renderFileTree(
  container: HTMLElement,
  conversationId: string,
  editor: HTMLTextAreaElement,
  saved: HTMLElement,
) {
  container.replaceChildren();
  // Render each open dir, sorted with directories before files, indented by depth.
  const rels = [...openDirs].sort();
  for (const rel of rels) {
    const depth = rel === "" ? 0 : rel.split("/").length - 1;
    let entries: string[];
    try {
      entries = await call.invoke<string[]>("list_workspace_dir", { conversationId, rel });
    } catch (e) {
      container.append(
        h("div", { class: "file-row muted small" }, `Could not read ${rel || "."}: ${e}`),
      );
      continue;
    }
    entries.sort((a, b) => Number(b.endsWith("/")) - Number(a.endsWith("/")) || a.localeCompare(b));
    const childPrefix = rel === "" || rel.endsWith("/") ? rel : `${rel}/`;
    for (const name of entries) {
      const childRel = childPrefix + name;
      const isDir = name.endsWith("/");
      container.append(
        h(
          "div",
          {
            class: "file-row",
            style: `padding-left:${depth * 14 + 4}px`,
            onclick: () => {
              if (isDir) {
                if (openDirs.has(childRel)) openDirs.delete(childRel);
                else openDirs.add(childRel);
                renderFileTree(container, conversationId, editor, saved);
              } else {
                void openFile(conversationId, childRel, editor, saved);
              }
            },
          },
          isDir ? "▸ " : "  ", // shortcut: plain glyph tree, not recursive DOM
          name,
        ),
      );
    }
  }
}

async function openFile(
  conversationId: string,
  rel: string,
  editor: HTMLTextAreaElement,
  saved: HTMLElement,
) {
  activeFile = rel;
  saved.textContent = "";
  try {
    const text = await call.invoke<string>("read_workspace_file", { conversationId, rel });
    editorBuffer.set(rel, text);
    // Re-render so the Save row appears (filesPanelNode restores the editor
    // value from editorBuffer, so the just-loaded text survives the rebuild).
    render();
  } catch (e) {
    activeFile = null;
    editorBuffer.delete(rel);
    editor.value = "";
    editor.readOnly = true;
    ui.notice = `Could not open ${rel}: ${e}`;
    return;
  }
}

async function saveFile(
  conversationId: string,
  editor: HTMLTextAreaElement,
  saved: HTMLElement,
) {
  if (!activeFile) return;
  try {
    const text = editor.value;
    editorBuffer.set(activeFile, text);
    await call.invoke<void>("write_workspace_file", {
      conversationId,
      rel: activeFile,
      content: text,
    });
    saved.textContent = "Saved.";
    await refreshChanged();
  } catch (e) {
    ui.notice = `Could not save ${activeFile}: ${e}`;
  }
}

function systemAccessModal(conversationId: string): HTMLElement {
  const current = ui.detail?.conversation.main_system ?? false;
  const enabledCheckbox = h("input", {
    type: "checkbox",
    checked: current,
  }) as HTMLInputElement;
  const pathsArea = h("textarea", {
    class: "sys-paths",
    rows: "4",
    placeholder: "e.g. /Users/me/projects\n/Users/me/notes",
  }) as HTMLTextAreaElement;
  // Pre-fill allowed paths; failures leave the field blank rather than block.
  void call
    .invoke<string[]>("get_main_system_paths", { conversationId })
    .then((paths) => {
      pathsArea.value = paths.join("\n");
    })
    .catch(() => {});

  const close = () => {
    ui.systemAccessId = null;
    render();
  };
  const save = async () => {
    const enabled = enabledCheckbox.checked;
    const paths = pathsArea.value
      .split("\n")
      .map((p) => p.trim())
      .filter(Boolean);
    try {
      const updated = await call.invoke<ConversationView>("set_main_system_scope", {
        conversationId,
        enabled,
        paths,
      });
      if (ui.detail) ui.detail.conversation = updated;
      ui.systemAccessId = null;
      render();
    } catch (e) {
      ui.notice = String(e);
      render();
    }
  };

  return h(
    "div",
    {
      class: "modal-backdrop",
      onclick: (e: MouseEvent) => e.target === e.currentTarget && close(),
    },
    h(
      "div",
      { class: "modal" },
      h("h2", {}, "Main System File Access"),
      h(
        "p",
        { class: "muted small" },
        "Allows Aria to read, edit and execute commands outside of a single folder.",
      ),
      h(
        "label",
        { class: "sys-toggle" },
        enabledCheckbox,
        h("span", {}, "Enable system-wide file access"),
      ),
      h(
        "label",
        { class: "small" },
        "Allowed paths (one per line; paths outside ask for approval, empty means unrestricted)",
      ),
      pathsArea,
      h(
        "div",
        { class: "approval-actions" },
        h("button", { class: "primary", onclick: () => void save() }, "Save"),
        h("button", { class: "ghost", onclick: close }, "Cancel"),
      ),
    ),
  );
}

function messageNode(
  m: MessageView,
  artifactsIndex?: ConversationArtifactIndex | null,
): HTMLElement {
  const isUser = m.role === "user";
  // Only the assistant's output is markdown. The user's own text is shown
  // verbatim, so pasted code or a stray ** isn't silently reformatted.
  let bodyNodes: (HTMLElement | null)[] = [];

  if (isUser) {
    bodyNodes = m.content ? userContentNodes(m.content) : [];
  } else if (m.content) {
    if (
      artifactsIndex &&
      (m.content.includes("<antArtifact") || m.content.includes("<ariaArtifact"))
    ) {
      const segments = segmentMessage(m.content, m.id, artifactsIndex);
      bodyNodes = segments.map((seg) => {
        if (seg.type === "text") {
          return seg.text.trim()
            ? h("div", { class: "msg-body markdown", html: renderMarkdown(seg.text) })
            : null;
        } else {
          const isSelected =
            ui.activeArtifact?.identifier === seg.artifact.identifier &&
            ui.activeArtifact?.version === seg.artifact.version;
          return renderArtifactCard(seg.artifact, isSelected, () => {
            ui.activeArtifact = {
              identifier: seg.artifact.identifier,
              version: seg.artifact.version,
            };
            ui.artifactTab = isArtifactPreviewable(seg.artifact.type) ? "preview" : "code";
            // Opening an artifact should reveal the side panel if it is closed.
            ui.sidePanelOpen = true;
            ui.sidePanelTab = "artifacts";
            render();
          });
        }
      });
    } else {
      bodyNodes = [h("div", { class: "msg-body markdown", html: renderMarkdown(m.content) })];
    }
  }

  return h(
    "article",
    { class: `msg ${m.role}`, id: `msg-${m.id}` },
    h(
      "div",
      { class: "msg-head" },
      h("span", { class: "who" }, isUser ? "You" : "Aria"),
      h("time", { class: "when" }, fmt(m.created_at)),
    ),
    ...bodyNodes,
    ...(m.reasoning ? [reasoningNode(m.id, m.reasoning)] : []),
    ...m.tool_calls.map(toolCard),
    ...m.attachments.map((a) => attachmentNode(a)),
  );
}

/// Render one attachment beneath a message. Images become inline thumbnails
/// (fetched as data URLs from the blob store); other files become named links.
/// Format bytes as a human-readable size.
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/// Parse user message content for inline image markdown (`![alt](url)`) and
/// return a list of text nodes and image nodes. User text is otherwise shown
/// verbatim (no markdown), so only image syntax is treated specially.
function userContentNodes(content: string): (HTMLElement | null)[] {
  const nodes: (HTMLElement | null)[] = [];
  const imageRe = /!\[([^\]]*)\]\(([^)]+)\)/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = imageRe.exec(content)) !== null) {
    if (match.index > lastIndex) {
      nodes.push(
        h("div", { class: "msg-body" }, content.slice(lastIndex, match.index)),
      );
    }
    const alt = match[1];
    const url = match[2];
    nodes.push(
      h("img", {
        class: "user-image-inline",
        alt,
        src: url,
        title: alt,
        loading: "lazy",
        style: "max-width: 100%; height: auto; border-radius: var(--radius-sm); margin: var(--sp-1) 0;",
      }),
    );
    lastIndex = imageRe.lastIndex;
  }
  if (lastIndex < content.length) {
    nodes.push(h("div", { class: "msg-body" }, content.slice(lastIndex)));
  }
  if (nodes.length === 0) {
    nodes.push(h("div", { class: "msg-body" }, content));
  }
  return nodes;
}

/// Return an emoji icon for a MIME type so document attachments are
/// visually distinct in the transcript.
function mimeIcon(mime_type: string): string {
  if (mime_type.startsWith("image/")) return "🖼️";
  if (mime_type === "application/pdf") return "📄";
  if (mime_type.startsWith("text/")) return "📝";
  if (
    mime_type === "application/msword" ||
    mime_type === "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
  )
    return "📝";
  if (
    mime_type === "application/vnd.ms-excel" ||
    mime_type === "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
  )
    return "📊";
  if (
    mime_type === "application/vnd.ms-powerpoint" ||
    mime_type === "application/vnd.openxmlformats-officedocument.presentationml.presentation"
  )
    return "📽️";
  if (mime_type === "application/zip" || mime_type === "application/x-tar") return "📦";
  if (mime_type.startsWith("audio/")) return "🎵";
  if (mime_type.startsWith("video/")) return "🎬";
  return "📄";
}

/// Render one attachment beneath a message. Images become inline thumbnails
/// (fetched as data URLs from the blob store); other files become named links.
/// Clicking an image opens the lightbox.
function attachmentNode(a: AttachmentView): HTMLElement {
  const isImage = a.mime_type.startsWith("image/");
  const img = h("img", {
    class: "attachment-preview",
    alt: a.name,
    title: `${a.name} (${fmtSize(a.size)})`,
    loading: "lazy",
    onclick: () => {
      void call.invoke<string>("read_attachment", { blob_hash: a.blob_hash }).then((url) => {
        ui.lightbox = { src: url, name: a.name };
        render();
      });
    },
  });
  if (isImage) {
    void call.invoke<string>("read_attachment", { blob_hash: a.blob_hash }).then((url) => {
      img.src = url;
    });
  }
  return h(
    "div",
    { class: `attachment ${isImage ? "attachment-image" : "attachment-file"}` },
    isImage
      ? img
      : h("span", { class: "attachment-icon", "aria-hidden": "true" }, mimeIcon(a.mime_type)),
    h(
      "div",
      { class: "attachment-info" },
      h("a", {
        class: "attachment-link",
        href: isImage ? "" : "#",
        title: a.name,
        onclick: isImage
          ? undefined
          : async () => {
              const url = await call.invoke<string>("read_attachment", {
                blob_hash: a.blob_hash,
              });
              const aEl = document.createElement("a");
              aEl.href = url;
              aEl.download = a.name;
              aEl.style.display = "none";
              document.body.appendChild(aEl);
              aEl.click();
              document.body.removeChild(aEl);
            },
      }, a.name),
      h("span", { class: "attachment-meta" }, fmtSize(a.size)),
    ),
  );
}

/// Full-screen image lightbox. Clicking outside the image or pressing ESC closes it.
function lightboxNode(): HTMLElement | null {
  if (!ui.lightbox) return null;
  return h(
    "div",
    { class: "lightbox", onclick: (_e: MouseEvent) => closeLightbox },
    h("div", { class: "lightbox-backdrop" }),
    h(
      "div",
      { class: "lightbox-panel", onclick: (e: MouseEvent) => e.stopPropagation() },
      h("button", {
        class: "lightbox-close",
        "aria-label": "Close",
        onclick: closeLightbox,
      }, "✕"),
      h("img", { class: "lightbox-image", src: ui.lightbox.src, alt: ui.lightbox.name }),
      h("div", { class: "lightbox-caption" }, ui.lightbox.name),
    ),
  );
}

function closeLightbox() {
  ui.lightbox = null;
  render();
}

/// Collapsed-by-default reasoning block explaining the model's thought process
/// ahead of its tool calls or final answer. Toggled like tool-call cards.
function reasoningNode(id: string, reasoning: string): HTMLElement {
  const key = `reasoning-${id}`;
  const expanded = ui.expanded.has(key);
  return h(
    "div",
    { class: `reasoning-block ${expanded ? "expanded" : ""}` },
    h(
      "button",
      {
        class: "reasoning-head",
        type: "button",
        onclick: () => {
          if (expanded) ui.expanded.delete(key);
          else ui.expanded.add(key);
          render();
        },
        onkeydown: (e: KeyboardEvent) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            if (expanded) ui.expanded.delete(key);
            else ui.expanded.add(key);
            render();
          }
        },
      },
      h("span", { class: "reasoning-toggle" }, expanded ? "▼" : "▶"),
      h("span", { class: "reasoning-label" }, "Reasoning"),
      h("span", { class: "reasoning-hint" }, expanded ? "Hide" : "Show"),
    ),
    expanded
      ? h("div", { class: "reasoning-body markdown", html: renderMarkdown(reasoning) })
      : null,
  );
}

/// One-line summary of a tool call, shown when its card is collapsed.
function toolSummary(call: ToolCall): string {
  const a = args(call);
  const s = (k: string) => String(a[k] ?? "");
  switch (call.tool) {
    case "edit_file":
    case "write_file":
    case "delete_file":
    case "read_file":
    case "list_dir":
      return `${call.tool}  ${s("path")}`;
    case "search_files":
      return `search_files  ${s("pattern")}`;
    case "move_file":
      return `move_file  ${s("from")} → ${s("to")}`;
    case "run_shell":
      return `run_shell  ${s("cmd")}`;
    case "view_image":
      return `Inspect ${s("path") || "image"}`;
    default:
      return call.tool;
  }
}

function toolCard(call: ToolCall): HTMLElement {
  const a = args(call);
  const output = ui.toolOutput[call.id];
  const state =
    call.state === "pending"
      ? "waiting"
      : call.state === "denied"
        ? "denied"
        : call.is_error
          ? "error"
          : "done";
  const expanded = ui.expanded.has(call.id);
  const detail = expanded
    ? [
        h("pre", { class: "tool-args" }, JSON.stringify(a, null, 2)),
        output?.length ? h("pre", { class: "tool-output" }, output.join("\n")) : null,
        call.result
          ? h("pre", { class: `tool-result ${call.is_error ? "bad" : ""}` }, call.result)
          : null,
      ]
    : [];
  // Collapsed cards keep only one line of output so the transcript stays calm.
  const collapsedOutput =
    !expanded && output?.length
      ? h("pre", { class: "tool-output short" }, output.join("\n"))
      : null;

  return h(
    "div",
    {
      class: `tool ${state}${expanded ? " open" : ""}`,
      id: `msg-${call.id}`,
      role: "button",
      tabindex: "0",
      onclick: () => {
        if (expanded) ui.expanded.delete(call.id);
        else ui.expanded.add(call.id);
        render();
      },
      onkeydown: (e: KeyboardEvent) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          if (expanded) ui.expanded.delete(call.id);
          else ui.expanded.add(call.id);
          render();
        }
      },
    },
    h(
      "div",
      { class: "tool-head" },
      h("span", { class: `dot ${state}` }),
      h("code", { class: "tool-name" }, toolSummary(call)),
      h("span", { class: "pill state" }, state),
    ),
    collapsedOutput,
    ...detail,
  );
}

function streamingNode(artifactsIndex?: ConversationArtifactIndex | null): HTMLElement | null {
  if (!ui.streaming) return null;
  let bodyNodes: (HTMLElement | null)[] = [];

  if (ui.streaming.text.length > 0) {
    if (
      artifactsIndex &&
      (ui.streaming.text.includes("<antArtifact") || ui.streaming.text.includes("<ariaArtifact"))
    ) {
      const segments = segmentMessage(ui.streaming.text, "streaming", artifactsIndex);
      bodyNodes = segments.map((seg) => {
        if (seg.type === "text") {
          return seg.text.trim()
            ? h("div", { class: "msg-body markdown", html: renderMarkdown(seg.text) })
            : null;
        } else {
          const isSelected =
            ui.activeArtifact?.identifier === seg.artifact.identifier &&
            ui.activeArtifact?.version === seg.artifact.version;
          return renderArtifactCard(seg.artifact, isSelected, () => {
            ui.activeArtifact = {
              identifier: seg.artifact.identifier,
              version: seg.artifact.version,
            };
            ui.artifactTab = isArtifactPreviewable(seg.artifact.type) ? "preview" : "code";
            // Opening an artifact should reveal the side panel if it is closed.
            ui.sidePanelOpen = true;
            ui.sidePanelTab = "artifacts";
            render();
          });
        }
      });
    } else {
      bodyNodes = [h("div", { class: "msg-body markdown", html: renderMarkdown(ui.streaming.text) })];
    }
  } else {
    bodyNodes = [h("div", { class: "msg-body muted" }, "Thinking…")];
  }

  const nodes: (HTMLElement | null)[] = [];
  if (ui.streaming.reasoning.length > 0) {
    nodes.push(reasoningNode(ui.streaming.runId, ui.streaming.reasoning));
  }
  nodes.push(...bodyNodes);

  return h("article", { class: "msg assistant streaming" }, ...nodes);
}

function diffOf(body: string): HTMLElement {
  // The Rust side formats Edits as "--- search ---\n…\n--- replace ---\n…".
  const marker = "\n--- replace ---\n";
  const at = body.indexOf(marker);
  if (at < 0) return h("pre", {}, body);
  const search = body.slice(body.indexOf("\n") + 1, at);
  const replace = body.slice(at + marker.length);
  return h(
    "div",
    { class: "diff" },
    h("div", { class: "diff-line del" }, ...search.split("\n").map((l) => h("div", {}, `- ${l}`))),
    h("div", { class: "diff-line add" }, ...replace.split("\n").map((l) => h("div", {}, `+ ${l}`))),
  );
}

function approvalNode(p: PendingApproval): HTMLElement {
  const body =
    p.tool === "edit_file"
      ? diffOf(p.summary.body)
      : p.tool === "run_shell"
        ? h("pre", { class: "command" }, `$ ${p.summary.body}`)
        : h("pre", {}, p.summary.body);
  return h(
    "div",
    { class: "approval" },
    h(
      "div",
      { class: "approval-head" },
      h("span", { class: "warn-icon", "aria-hidden": "true" }, "⚠"),
      h(
        "span",
        { class: "approval-title" },
        "Aria wants to ",
        h("strong", {}, p.summary.title),
      ),
      h("span", { class: `pill class-${p.class}`, title: `${p.class} action` }, p.class),
    ),
    body,
    h(
      "div",
      { class: "approval-actions" },
      h(
        "button",
        { class: "primary", onclick: () => void answer(p.approval_id, "approve") },
        "Approve",
      ),
      h(
        "button",
        { class: "ghost", onclick: () => void answer(p.approval_id, "deny") },
        "Deny",
      ),
      h(
        "button",
        {
          class: "ghost",
          title: "Approve this and stop asking for the rest of this run",
          onclick: () => void answer(p.approval_id, "approve_all"),
        },
        "Allow all this run",
      ),
    ),
  );
}

// Attachments queued in the composer but not yet sent. Cleared on send/switch.
interface Attachment { name: string; path: string }
let pendingAttachments: Attachment[] = [];

function composerNode(
  busy: boolean,
  hasFolder: boolean,
  conv: ConversationView,
): HTMLElement {
  const mainSystem = conv.main_system;

  // ── textarea ──────────────────────────────────────────────────────────────
  const input = h("textarea", {
    class: "composer-input",
    rows: "1",
    placeholder: busy
      ? "Aria is working…"
      : hasFolder
        ? "Ask Aria about this folder"
        : mainSystem
          ? "Ask Aria to do anything on this device"
          : "Message Aria",
    disabled: busy,
    oninput: (e: Event) => {
      const t = e.target as HTMLTextAreaElement;
      t.style.height = "auto";
      t.style.height = `${Math.min(t.scrollHeight, 200)}px`;
    },
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void send(); }
      if (e.key === "Escape") (e.target as HTMLElement).blur();
    },
    onfocus: (e: Event) => {
      (e.target as HTMLElement).closest(".composer-card")?.classList.add("focused");
    },
    onblur: (e: Event) => {
      (e.target as HTMLElement).closest(".composer-card")?.classList.remove("focused");
    },
  });
  input.dataset.role = "composer";

  // ── attachment tags (shown above textarea when files are queued) ───────────
  const attachmentRow = pendingAttachments.length
    ? h(
        "div",
        { class: "composer-attachments" },
        ...pendingAttachments.map((att) =>
          h(
            "span",
            { class: "attachment-tag" },
            h("span", { class: "attachment-name" }, att.name),
            h("button", {
              class: "attachment-remove",
              title: `Remove ${att.name}`,
              "aria-label": `Remove ${att.name}`,
              onclick: () => {
                pendingAttachments = pendingAttachments.filter((a) => a.path !== att.path);
                render();
              },
            }, "×"),
          ),
        ),
      )
    : null;

  // ── send button ───────────────────────────────────────────────────────────
  const sendIcon = `<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
    <path d="M2 8L14 8M14 8L9 3M14 8L9 13" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/>
  </svg>`;
  const sendBtn = h("button", {
    class: `composer-send${busy ? " disabled" : ""}`,
    disabled: busy,
    title: "Send (Enter)",
    "aria-label": "Send message",
    html: sendIcon,
    onclick: () => void send(),
  });

  // ── attachment button (paperclip) ─────────────────────────────────────────
  const clipIcon = `<svg width="15" height="15" viewBox="0 0 15 15" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
    <path d="M13.5 6.5L7 13C5.6 14.4 3.4 14.4 2 13C0.6 11.6 0.6 9.4 2 8L8.5 1.5C9.5 0.5 11.1 0.5 12.1 1.5C13.1 2.5 13.1 4.1 12.1 5.1L5.6 11.6C5.1 12.1 4.3 12.1 3.8 11.6C3.3 11.1 3.3 10.3 3.8 9.8L10.3 3.3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
  </svg>`;
  const attachBtn = h("button", {
    class: "composer-attach",
    title: "Attach a file",
    "aria-label": "Attach file",
    html: clipIcon,
    onclick: async () => {
      try {
        const { open } = await import("@tauri-apps/plugin-dialog");
        const picked = await open({ multiple: true, directory: false });
        if (!picked) return;
        const paths = Array.isArray(picked) ? picked : [picked];
        for (const p of paths) {
          if (pendingAttachments.some((a) => a.path === p)) continue;
          const name = p.split("/").pop() ?? p;
          pendingAttachments.push({ name, path: p });
        }
        render();
      } catch (e) {
        ui.notice = String(e);
        render();
      }
    },
  });

  // ── bottom toolbar chips ──────────────────────────────────────────────────

  // Model chip: shows model display name, clicking opens tree popover.
  const currentModel = ui.health?.model ?? "";
  const modelLabel = getModelDisplayName(currentModel, ui.providers);
  const modelTitle = currentModel
    ? (modelLabel !== currentModel
        ? `Model: ${modelLabel} (${currentModel})\nClick to change`
        : `Model: ${currentModel}\nClick to change`)
    : "No model configured — click to set";
  const modelChip = h("button", {
    class: `composer-chip model-chip${!currentModel ? " model-unset" : ""}`,
    title: modelTitle,
    "aria-label": "Change model",
    onclick: (e: MouseEvent) => {
      e.stopPropagation();
      void openModelSelector(e.currentTarget as HTMLElement, currentModel);
    },
  },
    h("span", { class: "model-chip-icon", "aria-hidden": "true" }, "◈"),
    h("span", { class: "model-chip-text" }, modelLabel),
    h("span", { class: "chip-chevron", "aria-hidden": "true" }, "▾"),
  );

  // Mode chip (Normal / Creative).
  const modeSelect = h(
    "select",
    {
      class: "composer-chip",
      title: "Creative mode helps build or change Aria itself.",
      "aria-label": "Conversation mode",
      onchange: async (e: Event) => {
        const mode = (e.target as HTMLSelectElement).value as ConversationMode;
        try {
          const updated = await call.invoke<ConversationView>("set_conversation_mode", {
            conversationId: conv.id, mode,
          });
          if (ui.detail) ui.detail.conversation = updated;
          render();
        } catch (err) { ui.notice = String(err); render(); }
      },
    },
    h("option", { value: "normal" }, "Normal ▾"),
    h("option", { value: "creative" }, "Creative ▾"),
  ) as HTMLSelectElement;
  modeSelect.value = conv.mode;

  // Approval chip: toggles between auto (default, edits free) and always-ask (locked).
  // auto_approve_safe is now the global default; "always_ask" is the locked opt-in.
  const isAuto = (conv.approval_policy ?? "auto_approve_safe") !== "always_ask";
  const approvalTooltip = isAuto
    ? hasFolder
      ? "Auto: reads & edits run silently. Shell and network still ask.\nClick to lock down."
      : mainSystem
        ? "Auto: reads run silently. Edits, shell, network still ask.\nClick to lock down."
        : "Auto: reads run silently. Click to lock down."
    : "Locked: every tool call asks for approval.\nClick to switch to Auto.";
  const approvalBtn = h("button", {
    class: `composer-chip approval-chip${isAuto ? " approval-auto" : " approval-locked"}`,
    title: approvalTooltip,
    "aria-label": "Toggle approval mode",
    onclick: async () => {
      const next = isAuto ? "always_ask" : null; // null → back to default (auto)
      try {
        const updated = await call.invoke<ConversationView>(
          "set_conversation_approval",
          { conversationId: conv.id, approvalPolicy: next },
        );
        if (ui.detail) ui.detail.conversation = updated;
        render();
      } catch (err) { ui.notice = String(err); render(); }
    },
  },
    isAuto ? h("span", { "aria-hidden": "true" }, "⚡") : h("span", { "aria-hidden": "true" }, "🔒"),
    isAuto
      ? (hasFolder ? " Auto · edits free" : mainSystem ? " Auto · reads free" : " Auto")
      : " Ask always",
  );

  // ── card assembly ─────────────────────────────────────────────────────────
  const card = h(
    "div",
    { class: "composer-card" },
    attachmentRow,
    input,
    h(
      "div",
      { class: "composer-footer" },
      h("div", { class: "composer-chips" }, modelChip, modeSelect, approvalBtn),
      h("div", { class: "composer-actions" }, attachBtn, sendBtn),
    ),
  );

  return h("div", { class: "composer" }, card);
}

function getModelDisplayName(modelId: string, providers: AIProvider[]): string {
  if (!modelId) return "No model";
  for (const p of providers) {
    const m = p.models?.find((x) => x.id === modelId);
    if (m && m.name) return m.name;
  }
  return modelId.includes("/") ? modelId.split("/").pop()! : modelId;
}

// Tree-style model selector dropdown grouped by provider.
async function openModelSelector(anchor: HTMLElement, current: string) {
  closeMenu();
  try {
    ui.providers = await call.invoke<AIProvider[]>("list_ai_providers");
  } catch {
    // Keep cached providers
  }

  const container = document.createElement("div");
  container.className = "model-tree-popover ctx-menu";

  const totalModels = ui.providers.reduce(
    (acc, p) => acc + (p.models?.length || p.modelIds?.length || 0),
    0,
  );

  const header = h(
    "div",
    { class: "model-tree-header" },
    h("span", {}, "Select Model"),
    h("span", { class: "small muted" }, `${totalModels} available`),
  );

  const scroll = h("div", { class: "model-tree-scroll" });

  let hasModels = false;
  for (const p of ui.providers) {
    const models = p.models?.length
      ? p.models
      : (p.modelIds || []).map((id) => ({ id, name: id }));
    if (!models.length) continue;
    hasModels = true;

    const group = h(
      "div",
      { class: "model-tree-group" },
      h(
        "div",
        { class: "model-tree-provider-label" },
        h("span", { class: "model-tree-provider-icon", "aria-hidden": "true" }, "⬡"),
        h("span", {}, p.name),
        p.isDefault ? h("span", { class: "model-tree-provider-badge" }, "Default") : null,
      ),
      h(
        "div",
        { class: "model-tree-items" },
        ...models.map((m) => {
          const isActive = m.id === current;
          return h(
            "button",
            {
              type: "button",
              class: `model-tree-item${isActive ? " is-active" : ""}`,
              title: `${m.name} (${m.id})`,
              onclick: async () => {
                try {
                  ui.health = await call.invoke<HealthView>("select_model", {
                    providerId: p.id,
                    modelId: m.id,
                  });
                  ui.providers = await call.invoke<AIProvider[]>("list_ai_providers");
                  closeMenu();
                  render();
                } catch (e) {
                  ui.notice = String(e);
                  closeMenu();
                  render();
                }
              },
            },
            h(
              "div",
              { class: "model-tree-item-info" },
              h("span", { class: "model-tree-item-name" }, m.name),
              m.name !== m.id ? h("span", { class: "model-tree-item-id" }, m.id) : null,
            ),
            isActive ? h("span", { class: "model-tree-item-check", "aria-hidden": "true" }, "✓") : null,
          );
        }),
      ),
    );
    scroll.appendChild(group);
  }

  if (!hasModels) {
    scroll.appendChild(
      h("div", { class: "model-tree-empty" }, "No models configured in Settings."),
    );
  }

  const customInp = h("input", {
    type: "text",
    class: "model-tree-custom-input",
    placeholder: "Custom model ID…",
    value: current,
    spellcheck: "false",
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void applyCustom();
      }
      if (e.key === "Escape") closeMenu();
    },
  }) as HTMLInputElement;

  const applyCustom = async () => {
    const model = customInp.value.trim();
    if (!model || model === current) {
      closeMenu();
      return;
    }
    try {
      ui.health = await call.invoke<HealthView>("select_model", {
        providerId: null,
        modelId: model,
      });
      ui.providers = await call.invoke<AIProvider[]>("list_ai_providers");
      closeMenu();
      render();
    } catch (e) {
      ui.notice = String(e);
      closeMenu();
      render();
    }
  };

  const applyBtn = h(
    "button",
    {
      type: "button",
      class: "primary model-tree-custom-btn",
      onclick: () => void applyCustom(),
    },
    "Set",
  );

  const settingsLink = h(
    "button",
    {
      type: "button",
      class: "model-tree-settings-link",
      onclick: () => {
        closeMenu();
        void openSettings();
      },
    },
    "⚙ Manage AI providers in Settings",
  );

  const footer = h(
    "div",
    { class: "model-tree-footer" },
    h("div", { class: "model-tree-custom-row" }, customInp, applyBtn),
    settingsLink,
  );

  container.append(header, scroll, footer);
  document.body.appendChild(container);
  menuEl = container as HTMLDivElement;
  menuAnchor = anchor;
  positionMenu();
  setTimeout(() => {
    window.addEventListener("click", onMenuOutside, true);
    window.addEventListener("keydown", onMenuKey, true);
  }, 0);
}

function todoPanel(detail: ConversationDetail): HTMLElement {
  const todos: Todo[] = detail.todos;
  return h(
    "aside",
    { class: "todos" },
    h("div", { class: "section-title" }, "Plan"),
    todos.length
      ? h(
          "ul",
          {},
          ...todos.map((t) =>
            h(
              "li",
              { class: t.done ? "done" : "" },
              h("span", { class: "todo-mark", "aria-hidden": "true" }, t.done ? "☑" : "☐"),
              h("span", { class: "todo-text" }, t.text),
            ),
          ),
        )
      : h("p", { class: "muted" }, "No plan yet."),
    detail.conversation.run
      ? h(
          "p",
          { class: "muted small" },
          `${detail.conversation.run.turn_count} turns · ${detail.conversation.run.prompt_tokens} tokens`,
        )
      : null,
  );
}

// ---------------------------------------------------------------- side panel

/// Side panel wrapper: tabbed Artifacts | Files. Shows the artifact panel when
/// an artifact is active, otherwise an empty state.
function renderSidePanelArtifacts(
  art: ArtifactVersion,
  allVersions: ArtifactVersion[],
  artifactTab: "preview" | "code",
  sidePanelTab: "artifacts" | "files",
  onTabChange: (t: "preview" | "code") => void,
  onVersionChange: (ver: number) => void,
  onClose: () => void,
): HTMLElement {
  const active = sidePanelTab === "artifacts";
  return h(
    "aside",
    { class: `side-panel ${sidePanelTab}` },
    h(
      "div",
      { class: "side-panel-tabs" },
      h(
        "button",
        {
          class: `side-panel-tab ${active ? "active" : ""}`,
          onclick: () => {
            ui.sidePanelTab = "artifacts";
            render();
          },
        },
        "Artifacts",
      ),
      h(
        "button",
        {
          class: `side-panel-tab ${!active ? "active" : ""}`,
          onclick: () => {
            ui.sidePanelTab = "files";
            render();
          },
        },
        "Files",
      ),
    ),
    active
      ? renderArtifactPanel(
          art,
          allVersions,
          artifactTab,
          onTabChange,
          onVersionChange,
          onClose,
        )
      : null,
  );
}

/// Side panel Files view: a file tree for the current workspace conversation.
function renderSidePanelFiles(conv: ConversationView): HTMLElement {
  const conversationId = conv.id;
  if (filesConvId !== conv.id) {
    filesConvId = conv.id;
    filesRoot = "";
    openDirs.clear();
    openDirs.add("");
    activeFile = null;
  }
  const tree = h("div", { class: "file-tree" });
  const editor = h("textarea", {
    class: "editor",
    rows: "12",
    placeholder: "Select a file to edit",
    readonly: activeFile === null,
  }) as HTMLTextAreaElement;
  if (activeFile) editor.value = editorBuffer.get(activeFile) ?? "";
  const saved = h("span", { class: "muted small" });

  const pathInput = h("input", {
    class: "file-path",
    type: "text",
    value: filesRoot,
    placeholder: "Path (blank = conversation root)",
    title: "Absolute path, or relative to the conversation root",
    onchange: (e: Event) => {
      filesRoot = (e.target as HTMLInputElement).value.trim();
      openDirs.clear();
      openDirs.add(filesRoot);
      activeFile = null;
      render();
    },
  });

  const panel = h(
    "div",
    { class: "side-panel-files" },
    h(
      "div",
      { class: "section-row" },
      h(
        "button",
        {
          class: "tree-caret",
          title: filesOpen ? "Collapse" : "Expand",
          onclick: () => {
            filesOpen = !filesOpen;
            render();
          },
        },
        filesOpen ? "▾" : "▸",
      ),
      h("span", { class: "section-title" }, "Files"),
      h(
        "button",
        {
          class: "icon panel-dismiss",
          title: "Hide Files panel",
          onclick: () => {
            ui.sidePanelOpen = false;
            render();
          },
        },
        "×",
      ),
    ),
    filesOpen
      ? h(
          "div",
          {},
          pathInput,
          tree,
          activeFile
            ? h(
                "div",
                { class: "section-row" },
                h("span", { class: "muted small" }, activeFile),
                h(
                  "button",
                  {
                    class: "primary",
                    onclick: () => void saveFile(conversationId, editor, saved),
                  },
                  "Save",
                ),
              )
            : null,
        )
      : null,
  );
  renderFileTree(tree, conversationId, editor, saved);
  return panel;
}

/// Side panel node: rendered only when ui.sidePanelOpen is true.
function sidePanelNode(
  activeArtVersion: ArtifactVersion | null,
  allVersionsForActive: ArtifactVersion[],
): HTMLElement | null {
  if (!ui.sidePanelOpen) return null;
  const detail = ui.detail;
  if (!detail) return null;
  if (activeArtVersion) {
    return renderSidePanelArtifacts(
      activeArtVersion,
      allVersionsForActive,
      ui.artifactTab,
      ui.sidePanelTab,
      (tab) => {
        ui.artifactTab = tab;
        ui.sidePanelTab = "artifacts";
        render();
      },
      (ver) => {
        ui.activeArtifact = { identifier: activeArtVersion!.identifier, version: ver };
        render();
      },
      () => {
        ui.activeArtifact = null;
        render();
      },
    );
  }
  if (ui.sidePanelTab === "files") return renderSidePanelFiles(detail.conversation);
  if (detail.todos.length > 0) return todoPanel(detail);
  // Default: show files view when panel is open but no artifact/todos yet.
  return renderSidePanelFiles(detail.conversation);
}

// ---------------------------------------------------------------- behaviour

async function refreshConversations() {
  ui.conversations = await call.invoke<ConversationView[]>("list_conversations");
}

async function selectConversation(id: string) {
  const known = ui.detail?.conversation.id === id ? ui.detail.conversation.run : null;
  const fresh = await call.invoke<ConversationDetail>("get_conversation", {
    conversationId: id,
  });
  // A `state` event may have landed while this request was in flight. Never
  // regress to an older or missing Run.
  if (known && fresh.conversation.run?.id !== known.id) {
    fresh.conversation.run = known;
  }
  ui.detail = fresh;
  ui.streaming = null;
  ui.toolOutput = {};
  ui.notice = null;
  ui.activeArtifact = null;
  ui.autoOpenedArtifactId = null;
  pendingAttachments = [];
  await refreshChanged();
  await refreshBgTasks();
  render();
}

async function refreshBgTasks() {
  const detail = ui.detail;
  if (!detail) return;
  try {
    ui.bgTasks = await call.invoke<BackgroundTask[]>("list_background", {
      conversationId: detail.conversation.id,
    });
  } catch {
    ui.bgTasks = [];
  }
}

async function refreshChanged() {
  const detail = ui.detail;
  if (!detail) return;
  ui.changed = await call.invoke<ChangedFile[]>("changed_files", {
    conversationId: detail.conversation.id,
  });
}

async function openCloudOrBrowse(anchor: HTMLElement) {
  let folders: { name: string; path: string }[] = [];
  try {
    folders = await call.listCloudFolders();
  } catch {
    folders = [];
  }
  const items: MenuItem[] = folders.map((f) => ({
    label: f.name,
    onClick: () => void openWorkspacePath(f.path),
  }));
  items.push({ label: "Browse…", onClick: () => void chooseFolder() });
  if (items.length === 1) {
    await chooseFolder();
    return;
  }
  openMenuAt(anchor, items);
}

async function openWorkspacePath(rootPath: string) {
  const conv = await call.invoke<ConversationView>("open_workspace", { rootPath });
  await refreshConversations();
  await selectConversation(conv.id);
}

async function chooseFolder() {
  const picked = await openDialog({ directory: true, multiple: false });
  if (typeof picked !== "string") return;
  const conv = await call.invoke<ConversationView>("open_workspace", {
    rootPath: picked,
  });
  await refreshConversations();
  await selectConversation(conv.id);
}

async function send() {
  const detail = ui.detail;
  const input = document.querySelector<HTMLTextAreaElement>(".composer-input");
  const text = input?.value.trim();
  if (!detail || !text) return;
  input!.value = "";
  const atts = pendingAttachments;
  pendingAttachments = [];
  ui.notice = null;
  try {
    await call.invoke<Run>("send_message", {
      conversationId: detail.conversation.id,
      text,
      attachments: atts.map((a) => ({ name: a.name, path: a.path })),
    });
    await selectConversation(detail.conversation.id);
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function answer(
  approvalId: string,
  decision: "approve" | "deny" | "approve_all",
) {
  try {
    await call.invoke<void>("answer_approval", { approvalId, decision });
    const detail = ui.detail;
    if (detail) await selectConversation(detail.conversation.id);
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function cancel(runId: string) {
  await call.invoke<void>("cancel_run", { runId });
}

async function resume(runId: string) {
  try {
    await call.invoke<void>("resume_run", { runId });
    render();
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function undo(path: string) {
  const detail = ui.detail;
  if (!detail) return;
  try {
    await call.invoke<void>("undo", {
      conversationId: detail.conversation.id,
      path,
    });
    await refreshChanged();
    render();
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

let searchTimer: number | undefined;
let lastSearchNonEmpty = false;
async function runSearch(query: string) {
  const trimmed = query.trim();
  window.clearTimeout(searchTimer);
  if (!trimmed) {
    ui.search = [];
    lastSearchNonEmpty = false;
    render();
    return;
  }
  lastSearchNonEmpty = true;
  searchTimer = window.setTimeout(async () => {
    ui.search = await call.invoke<MessageHit[]>("search_messages", {
      query: trimmed,
      conversationId: null,
    });
    render();
  }, 150);
}

async function jumpToHit(hit: MessageHit) {
  if (ui.detail?.conversation.id !== hit.conversation_id) {
    await selectConversation(hit.conversation_id);
  }
  ui.search = [];
  ui.jumpTo = hit.message_id;
  render();
}

async function openSettings() {
  openSettingsModal(
    call,
    () => void reloadAfterDatabaseChange(),
    () => {
      void pollHealth().then(() => render());
    },
  );
}

/// A reset or import replaced every row: drop the open chat and start over on
/// the new database (creating the default Main System chat when it is empty).
async function reloadAfterDatabaseChange() {
  ui.detail = null;
  ui.search = [];
  ui.changed = [];
  ui.bgTasks = [];
  ui.memories = [];
  ui.notice = null;
  ui.streaming = null;
  ui.toolOutput = {};
  ui.activeArtifact = null;
  ui.autoOpenedArtifactId = null;
  await refreshConversations();
  await selectFirst();
}

async function openMemories(workspaceId: string | null) {
  ui.memories = await call.invoke<Memory[]>("list_memories", {
    workspaceId: workspaceId ?? null,
  });
  ui.memoriesOpen = true;
  render();
}

function memoriesModal(): HTMLElement {
  const close = () => {
    ui.memoriesOpen = false;
    render();
  };
  const list = ui.memories.length
    ? h(
        "ul",
        { class: "memory-list" },
        ...ui.memories.map((m) =>
          h(
            "li",
            {},
            h("div", { class: "memory-key" }, m.key),
            h("div", { class: "memory-content" }, m.content),
            h(
              "button",
              {
                class: "icon danger",
                title: "Forget",
                onclick: async () => {
                  await call.invoke("delete_memory", { id: m.id });
                  ui.memories = ui.memories.filter((x) => x.id !== m.id);
                  render();
                },
              },
              "🗑",
            ),
          ),
        ),
      )
    : h("p", { class: "muted small" }, "No memories stored yet.");
  return h(
    "div",
    { class: "modal-backdrop", onclick: (e: MouseEvent) => e.target === e.currentTarget && close() },
    h(
      "div",
      { class: "modal" },
      h("h2", {}, "Memories"),
      h(
        "p",
        { class: "muted small" },
        "Facts Aria has stored. Recalled automatically each turn; items here are global plus this workspace's.",
      ),
      list,
      h(
        "div",
        { class: "approval-actions" },
        h("button", { class: "ghost", onclick: close }, "Close"),
      ),
    ),
  );
}

// ------------------------------------------------------------------- events

function applyRunEvent(runId: string, event: RunEvent) {
  if (disposed) return;
  const detail = ui.detail;
  const run = detail?.conversation.run;
  const current = run?.id === runId;

  switch (event.type) {
    case "state":
      if (detail && current) detail.conversation.run = event.run;
      else if (detail && !run) detail.conversation.run = event.run;
      if (event.run.state !== "running") {
        ui.streaming = null;
      }
      void refreshConversations().then(() => render());
      return;
    case "turn_started":
      if (current) {
        ui.streaming = { runId, text: "", reasoning: "", turn: event.turn_index };
        render();
      }
      return;
    case "text_delta":
      if (current) {
        if (!ui.streaming) ui.streaming = { runId, text: "", reasoning: "", turn: event.turn_index };
        ui.streaming = { ...ui.streaming, text: event.so_far };
        scheduleRender();
      }
      return;
    case "reasoning_delta":
      if (current) {
        if (!ui.streaming) ui.streaming = { runId, text: "", reasoning: "", turn: event.turn_index };
        ui.streaming = { ...ui.streaming, reasoning: event.so_far };
        scheduleRender();
      }
      return;
    case "assistant_message":
      ui.streaming = null;
      if (detail) void reloadTranscript();
      return;
    case "tool_output":
      (ui.toolOutput[event.tool_call_id] ??= []).push(event.line);
      scheduleRender();
      return;
    case "tool_result":
      delete ui.toolOutput[event.tool_call_id];
      if (detail) void reloadTranscript();
      return;
    case "todos":
      if (detail) detail.todos = event.todos;
      render();
      return;
    case "approval_requested":
      if (detail) void reloadTranscript();
      return;
    case "soft_cap":
      ui.notice = `Heads up: ${event.turns} turns used. Aria will wrap up soon.`;
      render();
      return;
  }
}

let reloading = false;
let reloadAgain = false;

/// Terminal Run states; a snapshot that says "running" is older than one of
/// these, whatever the request order.
const isTerminal = (state: string) => state !== "running" && state !== "awaiting_approval";

async function reloadTranscript() {
  const detail = ui.detail;
  if (!detail) return;
  // A tool result and the assistant message can land together. Remember the
  // second request instead of dropping it, so the finished answer is shown.
  if (reloading) {
    reloadAgain = true;
    return;
  }
  reloading = true;
  try {
    const conversationId = detail.conversation.id;
    const fresh = await call.invoke<ConversationDetail>("get_conversation", {
      conversationId,
    });
    // The reader may have switched chats while this was in flight.
    if (ui.detail?.conversation.id !== conversationId) return;
    // A state event can land mid-request; never let the older snapshot put a
    // finished Run back into "running" (which hid the final answer).
    const current = ui.detail.conversation.run;
    if (
      current &&
      fresh.conversation.run?.id === current.id &&
      isTerminal(current.state) &&
      !isTerminal(fresh.conversation.run.state)
    ) {
      fresh.conversation.run = current;
    }
    // Keep a live stream that started after the snapshot.
    const live = ui.streaming;
    ui.detail = fresh;
    ui.streaming = live && live.runId === fresh.conversation.run?.id ? live : null;
    if (fresh.conversation.run?.state === "finished") {
      await refreshChanged();
    }
    render();
  } finally {
    reloading = false;
    if (reloadAgain) {
      reloadAgain = false;
      void reloadTranscript();
    }
  }
}

// --------------------------------------------------------------------- boot

export async function start(initialId?: string) {
  disposed = false;
  unsubscribe?.();
  unsubscribe = null;
  // Tauri hands listeners the whole event (`{ payload }`), so unwrap it.
  const unsub = await call.listen(({ payload }) => applyRunEvent(payload.run_id, payload.event));
  // stop() may have run while listen() was in flight.
  if (disposed) {
    unsub();
    return;
  }
  unsubscribe = unsub;
  startHealthPoll();
  await refreshConversations();
  // The default main session is the conversation with no folder. Create one on
  // first launch so the user can talk immediately; afterwards reuse the one we
  // already have.
  if (ui.conversations.length === 0) {
    const main = await call.invoke<ConversationView>("new_conversation");
    ui.conversations = [main];
  }
  // An HMR re-boot keeps the chat the reader was looking at, if it still exists.
  const target =
    initialId && ui.conversations.some((c) => c.id === initialId)
      ? initialId
      : ui.conversations[0].id;
  await selectConversation(target);
}

/// Tear down the live Run subscription before an HMR re-boot. The DOM is left
/// in place so the next instance can swap it without a blank frame.
export function stop() {
  disposed = true;
  unsubscribe?.();
  unsubscribe = null;
  // A leaked interval would double-poll after a hot reload; the guard in
  // startHealthPoll() lets the next boot recreate it cleanly.
  if (healthPoll) {
    clearInterval(healthPoll);
    healthPoll = null;
  }
}

export type { ApprovalClass, PendingApproval };

// Automation hooks for headless checks (`?mock=1`); the UI itself never uses
// these. They are the same actions the buttons call.
export const testHooks = {
  state: () => ui,
  /// Make the next call to `cmd` reject (mock mode only), for error-path tests.
  failNextCommand: (cmd: string) => {
    mockFaults.failNext = cmd;
  },
  selectConversation,
  send: async (text: string) => {
    const input = document.querySelector<HTMLTextAreaElement>(".composer-input");
    if (input) input.value = text;
    await send();
  },
  answer,
  attachWorkspace: async (rootPath: string) => {
    const conv = await call.invoke<ConversationView>("attach_workspace", {
      conversationId: ui.detail!.conversation.id,
      rootPath,
    });
    await refreshConversations();
    await selectConversation(conv.id);
  },
  startBackground: async (cmd: string) => {
    const task = await call.invoke<BackgroundTask>("start_background", {
      cmd,
      conversationId: ui.detail!.conversation.id,
    });
    render();
    return task;
  },
  killBackground: async (id: string) => {
    const task = await call.invoke<BackgroundTask>("kill_background", { id });
    render();
    return task;
  },
};

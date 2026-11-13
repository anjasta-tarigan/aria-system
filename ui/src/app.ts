import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  type ApprovalClass,
  type ChangedFile,
  type ConversationDetail,
  type ConversationView,
  type MessageHit,
  type Memory,
  type RunEventEnvelope,
  type MessageView,
  type PendingApproval,
  type Run,
  type RunEvent,
  type SettingsView,
  type Todo,
  type ToolCall,
} from "./api";
import { mockInvoke, mockListen, seed } from "./mock";
import { renderMarkdown } from "./format";

/// Thin adapters the UI goes through, so `?mock=1` can swap the whole Rust
/// side for the in-browser fake.
export interface Ipc {
  invoke: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
  listen: (cb: (e: { payload: RunEventEnvelope }) => void) => Promise<() => void>;
}

// The real app talks to the Rust commands.
export const call: Ipc = {
  invoke: (cmd, args) => invoke(cmd, args) as Promise<never>,
  listen: (cb) => listen<RunEventEnvelope>("run-event", (e) => cb({ payload: e.payload })),
};

export function useMock() {
  call.invoke = mockInvoke;
  call.listen = mockListen;
  seed();
}

// ------------------------------------------------------------------ helpers

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Record<string, unknown> = {},
  ...children: (Node | string | null | undefined)[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") el.className = String(v);
    else if (k === "html") el.innerHTML = String(v);
    else if (k === "dataset") Object.assign(el.dataset, v as object);
    else if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    } else if (v !== null && v !== undefined && v !== false) {
      el.setAttribute(k, v === true ? "" : String(v));
    }
  }
  for (const c of children) {
    if (c === null || c === undefined) continue;
    el.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return el;
}

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
  streaming: { runId: string; text: string; turn: number } | null;
  /// Live shell output per tool call.
  toolOutput: Record<string, string[]>;
  changed: ChangedFile[];
  search: MessageHit[];
  notice: string | null;
  settingsOpen: boolean;
  settings: SettingsView | null;
  jumpTo: string | null;
  /// Pending-approval count at the last render, to notice one appearing.
  pendingCount: number;
  /// Tool-call ids whose cards have been expanded by the user.
  expanded: Set<string>;
  /// Conversation id currently being renamed inline in the sidebar, if any.
  renamingId: string | null;
  /// Workspace ids whose session list is collapsed in the sidebar tree.
  collapsed: Set<string>;
  /// Whether the stored-memories viewer is open.
  memoriesOpen: boolean;
  memories: Memory[];
  /// Conversation id whose Main System access modal is open, if any.
  systemAccessId: string | null;
}

const ui: Ui = {
  conversations: [],
  detail: null,
  streaming: null,
  toolOutput: {},
  changed: [],
  search: [],
  notice: null,
  settingsOpen: false,
  settings: null,
  jumpTo: null,
  pendingCount: 0,
  expanded: new Set(),
  renamingId: null,
  collapsed: new Set(),
  memoriesOpen: false,
  memories: [],
  systemAccessId: null,
};

const root = document.querySelector<HTMLElement>("#app")!;

/// Live subscription to Run events, kept so HMR can tear the app down cleanly.
let unsubscribe: (() => void) | null = null;
/// Set by stop(); stale async work must not paint over the new instance.
let disposed = false;

function activeRun(): Run | null {
  return ui.detail?.conversation.run ?? null;
}

function isBusy(run: Run | null): boolean {
  return !!run && (run.state === "running" || run.state === "awaiting_approval");
}

// -------------------------------------------------------------------- render

function render() {
  if (disposed) return;
  const detail = ui.detail;
  const previous = document.querySelector<HTMLElement>(".transcript");
  const previousTop = previous?.scrollTop ?? 0;
  const follow = !previous || previous.scrollHeight - previousTop - previous.clientHeight < 120;
  const shell = h(
    "div",
    { class: "app" },
    sidebar(),
    h("main", { class: "pane" }, detail ? conversationPane(detail) : emptyPane()),
    detail && isBusy(detail.conversation.run) ? todoPanel(detail) : null,
  );
  root.replaceChildren(
    shell,
    ...(ui.settingsOpen && ui.settings ? [settingsModal(ui.settings)] : []),
    ...(ui.memoriesOpen ? [memoriesModal()] : []),
    ...(ui.systemAccessId ? [systemAccessModal(ui.systemAccessId)] : []),
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
        { class: "icon", title: "Open a folder", onclick: () => void chooseFolder() },
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

  const footer = h(
    "div",
    { class: "sidebar-foot" },
    h(
      "button",
      { class: "icon", title: "Settings", onclick: () => void openSettings() },
      "⚙",
    ),
    h("span", { class: "foot-label" }, "Settings"),
    h(
      "button",
      {
        class: "icon",
        title: "Memories",
        onclick: () => void openMemories(ui.detail?.conversation.workspace_id ?? null),
      },
      "🧠",
    ),
    h("span", { class: "foot-label" }, "Memories"),
  );

  return h(
    "aside",
    { class: "sidebar" },
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
    footer,
  );
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
  let left = Math.min(r.right - mw, window.innerWidth - mw - 8);
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
function welcomeNode(): HTMLElement {
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
      "Ask me anything — explain something, draft an email, or plan a task. " +
        "When you want me to work on files, open a folder from the header.",
    ),
    h(
      "div",
      { class: "chips" },
      chip("Explain this error message"),
      chip("Draft a short email about…"),
      chip("Summarize the pros and cons of…"),
    ),
  );
}

function conversationPane(detail: ConversationDetail): HTMLElement {
  const conv = detail.conversation;
  const run = conv.run;
  const busy = isBusy(run);
  const pending = detail.pending;
  const hasFolder = !!conv.workspace_id;

  const folderSlot = hasFolder
    ? h("span", { class: "path", title: conv.root_path ?? "" }, conv.root_path)
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

  // Main System chats (no folder) can grant Aria filesystem-wide access.
  const systemAccess = !hasFolder
    ? conv.main_system
      ? h(
          "button",
          {
            class: "badge-system-access",
            title: "Configure system-wide file access",
            onclick: () => {
              ui.systemAccessId = conv.id;
              render();
            },
          },
          "⚡ System File Access (active)",
        )
      : h(
          "button",
          {
            class: "link grant-access",
            onclick: () => {
              ui.systemAccessId = conv.id;
              render();
            },
          },
          "Grant File Access",
        )
    : null;

  const header = h(
    "header",
    { class: "conv-header" },
    h("div", {}, h("h2", {}, convTitle(conv)), folderSlot, systemAccess),
    run ? h("span", { class: `badge ${run.state}` }, stateLabel[run.state]) : null,
    run?.state === "cancelled" || run?.state === "errored"
      ? h("span", { class: "error-text" }, run.error ?? run.stop_reason ?? "")
      : null,
    busy ? h("button", { class: "ghost", onclick: () => void cancel() }, "Cancel") : null,
    run?.state === "budget_exhausted"
      ? h("button", { class: "primary", onclick: () => void resume(run.id) }, "Continue")
      : null,
  );

  const transcript = h(
    "div",
    { class: "transcript", role: "log", "aria-live": "polite" },
    ...(detail.messages.length === 0 && !busy ? [welcomeNode()] : []),
    ...detail.messages.map((m) => messageNode(m)),
    streamingNode(),
    ...pending.map(approvalNode),
  );

  const composer = composerNode(busy, hasFolder);

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
      h("label", { class: "small" }, "Allowed paths (one per line, leave empty for unrestricted access)"),
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

function messageNode(m: MessageView): HTMLElement {
  const isUser = m.role === "user";
  // Only the assistant's output is markdown. The user's own text is shown
  // verbatim, so pasted code or a stray ** isn't silently reformatted.
  const body = isUser
    ? m.content
      ? h("div", { class: "msg-body" }, m.content)
      : null
    : m.content
      ? h("div", { class: "msg-body markdown", html: renderMarkdown(m.content) })
      : null;
  return h(
    "article",
    { class: `msg ${m.role}`, id: `msg-${m.id}` },
    h(
      "div",
      { class: "msg-head" },
      h("span", { class: "who" }, isUser ? "You" : "Aria"),
      h("time", { class: "when" }, fmt(m.created_at)),
    ),
    body,
    ...m.tool_calls.map(toolCard),
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

function streamingNode(): HTMLElement | null {
  if (!ui.streaming) return null;
  const else_ =
    ui.streaming.text.length > 0
      ? h("div", { class: "msg-body markdown", html: renderMarkdown(ui.streaming.text) })
      : h("div", { class: "msg-body muted" }, "Thinking…");
  return h("article", { class: "msg assistant streaming" }, else_);
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
    ),
  );
}

function composerNode(busy: boolean, hasFolder: boolean): HTMLElement {
  const input = h("textarea", {
    class: "composer-input",
    rows: "1",
    placeholder: busy
      ? "Aria is working…"
      : hasFolder
        ? "Ask Aria about this folder"
        : "Message Aria",
    disabled: busy,
    oninput: (e: Event) => {
      const t = e.target as HTMLTextAreaElement;
      t.style.height = "auto";
      t.style.height = `${Math.min(t.scrollHeight, 160)}px`;
    },
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        void send();
      }
      if (e.key === "Escape") (e.target as HTMLElement).blur();
    },
  });
  input.dataset.role = "composer";
  return h(
    "div",
    { class: "composer" },
    input,
    h(
      "button",
      {
        class: "primary send",
        disabled: busy,
        title: "Send (Enter)",
        onclick: () => void send(),
      },
      "Send",
    ),
  );
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
  await refreshChanged();
  render();
}

async function refreshChanged() {
  const detail = ui.detail;
  if (!detail) return;
  ui.changed = await call.invoke<ChangedFile[]>("changed_files", {
    workspaceId: detail.conversation.workspace_id,
  });
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
  ui.notice = null;
  try {
    await call.invoke<Run>("send_message", {
      conversationId: detail.conversation.id,
      text,
    });
    await selectConversation(detail.conversation.id);
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function answer(approvalId: string, decision: "approve" | "deny") {
  try {
    await call.invoke<void>("answer_approval", { approvalId, decision });
    const detail = ui.detail;
    if (detail) await selectConversation(detail.conversation.id);
  } catch (e) {
    ui.notice = String(e);
    render();
  }
}

async function cancel() {
  await call.invoke<void>("cancel_run");
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
      workspaceId: detail.conversation.workspace_id,
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
  ui.settings = await call.invoke<SettingsView>("get_settings");
  ui.settingsOpen = true;
  render();
}

let activeSettingsTab: "models" | "web" | "system" | "limits" = "models";

interface ProviderPreset {
  name: string;
  baseUrl: string;
  models: string[];
  defaultEmbed?: string;
  note: string;
}

const PROVIDERS: ProviderPreset[] = [
  {
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    models: ["gpt-4o", "gpt-4o-mini", "o1", "o3-mini"],
    defaultEmbed: "text-embedding-3-small",
    note: "Official OpenAI API endpoint.",
  },
  {
    name: "Anthropic (Proxy)",
    baseUrl: "https://api.anthropic.com/v1",
    models: ["claude-3-5-sonnet", "claude-3-5-haiku", "claude-3-opus"],
    note: "Requires OpenAI-compatible gateway or reverse proxy.",
  },
  {
    name: "Ollama / Local",
    baseUrl: "http://localhost:11434/v1",
    models: ["llama3.2", "deepseek-r1", "qwen2.5", "mistral"],
    defaultEmbed: "nomic-embed-text",
    note: "Local OpenAI-compatible inference with Ollama.",
  },
  {
    name: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    models: ["anthropic/claude-3.5-sonnet", "openai/gpt-4o", "deepseek/deepseek-chat"],
    note: "Unified multi-provider API router.",
  },
  {
    name: "Custom OpenAI-Compatible",
    baseUrl: "http://localhost:8000/v1",
    models: [],
    note: "Any self-hosted or compatible LLM endpoint.",
  },
];

function settingsModal(view: SettingsView): HTMLElement {
  const s = view.settings;
  const field = (label: string, value: string | number, type = "text", placeholder = "") =>
    h(
      "label",
      {},
      h("span", {}, label),
      h("input", { type, value: String(value), placeholder, dataset: { field: label } }),
    );
  const close = () => {
    ui.settingsOpen = false;
    render();
  };

  const navTabs: { id: "models" | "web" | "system" | "limits"; label: string; icon: string }[] = [
    { id: "models", label: "AI Models", icon: "🧠" },
    { id: "web", label: "Web Access", icon: "🌐" },
    { id: "system", label: "System Access", icon: "💻" },
    { id: "limits", label: "Execution & Limits", icon: "⚡" },
  ];

  const navButtons = new Map<string, HTMLButtonElement>();
  const panelElements = new Map<string, HTMLElement>();

  const switchTab = (tabId: "models" | "web" | "system" | "limits") => {
    activeSettingsTab = tabId;
    for (const [id, btn] of navButtons.entries()) {
      const active = id === tabId;
      btn.classList.toggle("active", active);
      btn.setAttribute("aria-selected", String(active));
    }
    for (const [id, panel] of panelElements.entries()) {
      panel.style.display = id === tabId ? "flex" : "none";
    }
  };

  // 1. AI Models panel
  const baseUrlField = field("Base URL", s.base_url);
  const modelField = field("Model", s.model);
  const embedModelField = field("Embedding model", s.embedding_model ?? "");
  const embedBaseUrlField = field("Embedding base URL", s.embedding_base_url ?? "");

  const getBaseUrlInput = () => baseUrlField.querySelector("input") as HTMLInputElement;
  const getModelInput = () => modelField.querySelector("input") as HTMLInputElement;
  const getEmbedModelInput = () => embedModelField.querySelector("input") as HTMLInputElement;

  const providerList = h("div", { style: "display: flex; flex-direction: column; gap: var(--sp-2);" });
  for (const p of PROVIDERS) {
    const isActive = s.base_url.startsWith(p.baseUrl);
    const card = h(
      "div",
      {
        class: `provider-card ${isActive ? "selected" : ""}`,
        onclick: () => {
          getBaseUrlInput().value = p.baseUrl;
          if (p.models[0]) getModelInput().value = p.models[0];
          if (p.defaultEmbed) getEmbedModelInput().value = p.defaultEmbed;
          providerList.querySelectorAll(".provider-card").forEach((c) => c.classList.remove("selected"));
          card.classList.add("selected");
        },
      },
      h(
        "div",
        { class: "provider-header" },
        h("strong", {}, p.name),
        h(
          "span",
          { class: `provider-badge ${isActive ? "active" : ""}` },
          isActive ? "Active" : p.models.length ? `${p.models.length} models` : "Custom",
        ),
      ),
      h("p", { class: "muted small", style: "margin: 0 0 var(--sp-1) 0;" }, p.note),
      p.models.length
        ? h(
            "div",
            { class: "model-chips" },
            ...p.models.map((m) =>
              h(
                "span",
                {
                  class: "model-chip",
                  title: `Click to select ${m}`,
                  onclick: (e: MouseEvent) => {
                    e.stopPropagation();
                    getBaseUrlInput().value = p.baseUrl;
                    getModelInput().value = m;
                    providerList.querySelectorAll(".provider-card").forEach((c) => c.classList.remove("selected"));
                    card.classList.add("selected");
                  },
                },
                m,
              ),
            ),
          )
        : null,
    );
    providerList.append(card);
  }

  const modelsPanel = h(
    "div",
    {
      class: "settings-content",
      style: `display: ${activeSettingsTab === "models" ? "flex" : "none"}; flex-direction: column; gap: var(--sp-3);`,
    },
    h(
      "div",
      { class: "info-callout" },
      "Configure active AI inference providers and embeddings. Select a registered provider below or enter custom endpoint parameters.",
    ),
    h("h3", { class: "small muted" }, "Registered AI Providers"),
    providerList,
    h("hr", {}),
    h("h3", { class: "small muted" }, "Active Model Configuration"),
    baseUrlField,
    modelField,
    h(
      "label",
      {},
      h("span", {}, "API key"),
      h("input", {
        type: "password",
        placeholder: view.has_key ? `Stored in ${view.key_backend}` : "Not set",
        dataset: { field: "API key" },
      }),
    ),
    h("p", { class: "muted small" }, "Leave empty to keep the stored key."),
    h("hr", {}),
    h("h3", { class: "small muted" }, "Embeddings (Memory)"),
    embedModelField,
    embedBaseUrlField,
    h(
      "p",
      { class: "muted small" },
      "OpenAI-compatible embeddings endpoint. Leave blank to reuse the chat Base URL and default (text-embedding-3-small).",
    ),
  );
  panelElements.set("models", modelsPanel);

  // 2. Web Access panel
  const searchProviderSelect = h(
    "select",
    { dataset: { field: "Search provider" } },
    h("option", { value: "" }, "Disabled"),
    h("option", { value: "exa" }, "Exa"),
    h("option", { value: "firecrawl" }, "Firecrawl"),
    h("option", { value: "searxng" }, "searXNG"),
    h("option", { value: "duckduckgo" }, "DuckDuckGo"),
  ) as HTMLSelectElement;
  searchProviderSelect.value = s.search_provider ?? "";

  const webCallout = h("div", { class: "info-callout" });
  const searchBaseUrlField = field("Search base URL", s.search_base_url ?? "");
  const searchApiKeyField = h(
    "label",
    {},
    h("span", {}, "Search API key"),
    h("input", {
      type: "password",
      placeholder: "Empty = unchanged",
      dataset: { field: "Search API key" },
    }),
  );

  const updateSearchFields = (prov: string) => {
    switch (prov) {
      case "exa":
        webCallout.textContent =
          "Exa AI Search (api.exa.ai). Neural search engine optimized for AI agents with rich webpage snippets. Requires an Exa API key.";
        searchBaseUrlField.style.display = "none";
        searchApiKeyField.style.display = "flex";
        (searchApiKeyField.querySelector("input") as HTMLInputElement).placeholder = "Paste Exa API key";
        break;
      case "firecrawl":
        webCallout.textContent =
          "Firecrawl Search (api.firecrawl.dev). Web search and structured content extraction API. Requires a Firecrawl API key.";
        searchBaseUrlField.style.display = "none";
        searchApiKeyField.style.display = "flex";
        (searchApiKeyField.querySelector("input") as HTMLInputElement).placeholder = "Paste Firecrawl API key (fc-...)";
        break;
      case "searxng":
        webCallout.textContent =
          "Self-hosted searXNG meta-search instance. Requires the base URL of your searXNG service with JSON format enabled (format: json in settings.yml). No API key needed.";
        searchBaseUrlField.style.display = "flex";
        searchApiKeyField.style.display = "none";
        (searchBaseUrlField.querySelector("input") as HTMLInputElement).placeholder = "https://searx.example.com";
        break;
      case "duckduckgo":
        webCallout.textContent =
          "DuckDuckGo HTML Search. Works out of the box without an API key or base URL configuration.";
        searchBaseUrlField.style.display = "none";
        searchApiKeyField.style.display = "none";
        break;
      default:
        webCallout.textContent =
          "Web search is currently disabled. Choose a search provider above to enable web queries.";
        searchBaseUrlField.style.display = "none";
        searchApiKeyField.style.display = "none";
        break;
    }
  };

  searchProviderSelect.addEventListener("change", () => updateSearchFields(searchProviderSelect.value));
  updateSearchFields(searchProviderSelect.value);

  const webPanel = h(
    "div",
    {
      class: "settings-content",
      style: `display: ${activeSettingsTab === "web" ? "flex" : "none"}; flex-direction: column; gap: var(--sp-3);`,
    },
    h(
      "label",
      {},
      h("span", {}, "Search Provider"),
      searchProviderSelect,
    ),
    webCallout,
    searchBaseUrlField,
    searchApiKeyField,
  );
  panelElements.set("web", webPanel);

  // 3. System Access panel
  const sysAccessCheckbox = h("input", {
    type: "checkbox",
    class: "settings-sys-checkbox",
    checked: ui.detail?.conversation.main_system ?? false,
  }) as HTMLInputElement;

  const sysPathsTextarea = h("textarea", {
    class: "sys-paths settings-sys-paths",
    rows: "5",
    placeholder: "e.g. /home/user/projects\n/home/user/notes",
  }) as HTMLTextAreaElement;

  if (ui.detail) {
    void call
      .invoke<string[]>("get_main_system_paths", { conversationId: ui.detail.conversation.id })
      .then((paths) => {
        sysPathsTextarea.value = paths.join("\n");
      })
      .catch(() => {});
  }

  const systemPanel = h(
    "div",
    {
      class: "settings-content",
      style: `display: ${activeSettingsTab === "system" ? "flex" : "none"}; flex-direction: column; gap: var(--sp-3);`,
    },
    h(
      "div",
      { class: "info-callout" },
      "Main System File Access controls whether Aria can inspect, edit, and run shell commands on filesystem paths outside a single workspace directory (Scope::MainSystem).",
    ),
    ui.detail
      ? h(
          "div",
          { style: "display: flex; flex-direction: column; gap: var(--sp-3);" },
          h(
            "label",
            { class: "sys-toggle" },
            sysAccessCheckbox,
            h("span", {}, "Enable system-wide file access for current chat"),
          ),
          h("label", { class: "small" }, "Allowed paths (one per line, leave empty for unrestricted access)"),
          sysPathsTextarea,
          h("p", { class: "muted small" }, "Changes will apply to the currently open conversation upon saving."),
        )
      : h(
          "div",
          { style: "display: flex; flex-direction: column; gap: var(--sp-2);" },
          h("p", { class: "muted small" }, "No conversation is currently open."),
          h(
            "p",
            { class: "muted small" },
            "Main System scope is configured per-conversation. Open a conversation to view and manage its allowed filesystem paths.",
          ),
        ),
  );
  panelElements.set("system", systemPanel);

  // 4. Execution & Limits panel
  const limitsPanel = h(
    "div",
    {
      class: "settings-content",
      style: `display: ${activeSettingsTab === "limits" ? "flex" : "none"}; flex-direction: column; gap: var(--sp-3);`,
    },
    h(
      "div",
      { class: "info-callout" },
      "Safety limits protect against runaway loops, excessive token burn, and hanging shell commands.",
    ),
    field("Max turns", s.caps.max_turns, "number"),
    field("Max prompt tokens", s.caps.max_prompt_tokens, "number"),
    field("Shell timeout (s)", s.shell_timeout_secs, "number"),
    h(
      "p",
      { class: "muted small" },
      "Shell commands exceeding the timeout are forcibly terminated. Token limits stop execution before budget exhaustion.",
    ),
  );
  panelElements.set("limits", limitsPanel);

  // Nav list creation
  const navContainer = h("nav", { class: "settings-nav", role: "tablist", "aria-label": "Settings navigation" });
  for (const t of navTabs) {
    const btn = h(
      "button",
      {
        class: `settings-nav-item ${activeSettingsTab === t.id ? "active" : ""}`,
        role: "tab",
        "aria-selected": activeSettingsTab === t.id ? "true" : "false",
        onclick: () => switchTab(t.id),
      },
      h("span", {}, t.icon),
      h("span", {}, t.label),
    ) as HTMLButtonElement;
    navButtons.set(t.id, btn);
    navContainer.append(btn);
  }

  const save = async () => {
    const val = (name: string) =>
      (document.querySelector<HTMLInputElement>(`input[data-field="${name}"]`)?.value ?? "").trim();
    const sel = (name: string) =>
      (document.querySelector<HTMLSelectElement>(`select[data-field="${name}"]`)?.value ?? "").trim();
    try {
      ui.settings = await call.invoke<SettingsView>("set_settings", {
        input: {
          base_url: val("Base URL"),
          model: val("Model"),
          max_turns: Number(val("Max turns")) || 1,
          max_prompt_tokens: Number(val("Max prompt tokens")) || 1,
          shell_timeout_secs: Number(val("Shell timeout (s)")) || 1,
          embedding_model: val("Embedding model") || null,
          embedding_base_url: val("Embedding base URL") || null,
          search_provider: sel("Search provider") || null,
          search_base_url: val("Search base URL") || null,
          search_api_key: val("Search API key") || null,
          api_key: val("API key") || null,
        },
      });

      // Save conversation main-system scope if present
      if (ui.detail) {
        const sysCheck = document.querySelector<HTMLInputElement>(".settings-sys-checkbox");
        const sysArea = document.querySelector<HTMLTextAreaElement>(".settings-sys-paths");
        if (sysCheck && sysArea) {
          const enabled = sysCheck.checked;
          const paths = sysArea.value
            .split("\n")
            .map((p) => p.trim())
            .filter(Boolean);
          const updated = await call.invoke<ConversationView>("set_main_system_scope", {
            conversationId: ui.detail.conversation.id,
            enabled,
            paths,
          });
          ui.detail.conversation = updated;
        }
      }

      ui.settingsOpen = false;
      ui.notice = "Settings saved.";
      render();
    } catch (e) {
      ui.notice = String(e);
      render();
    }
  };

  return h(
    "div",
    { class: "modal-backdrop", onclick: (e: MouseEvent) => e.target === e.currentTarget && close() },
    h(
      "div",
      { class: "modal settings-modal" },
      h(
        "div",
        { class: "settings-header" },
        h("h2", {}, "Settings"),
        h("button", { class: "ghost", style: "padding: 2px 8px;", onclick: close }, "×"),
      ),
      h(
        "div",
        { class: "settings-body" },
        navContainer,
        modelsPanel,
        webPanel,
        systemPanel,
        limitsPanel,
      ),
      h(
        "div",
        { class: "settings-footer" },
        h("button", { class: "primary", onclick: () => void save() }, "Save"),
        h("button", { class: "ghost", onclick: close }, "Close"),
      ),
    ),
  );
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
        ui.streaming = { runId, text: "", turn: event.turn_index };
        render();
      }
      return;
    case "text_delta":
      if (current) {
        ui.streaming = { runId, text: event.so_far, turn: event.turn_index };
        render();
      }
      return;
    case "assistant_message":
      ui.streaming = null;
      if (detail) void reloadTranscript();
      return;
    case "tool_output":
      (ui.toolOutput[event.tool_call_id] ??= []).push(event.line);
      render();
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
async function reloadTranscript() {
  const detail = ui.detail;
  if (!detail || reloading) return;
  reloading = true;
  try {
    const fresh = await call.invoke<ConversationDetail>("get_conversation", {
      conversationId: detail.conversation.id,
    });
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
}

export type { ApprovalClass, PendingApproval };

// Automation hooks for headless checks (`?mock=1`); the UI itself never uses
// these. They are the same actions the buttons call.
export const testHooks = {
  state: () => ui,
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
};

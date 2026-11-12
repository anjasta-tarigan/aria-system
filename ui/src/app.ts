import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  api,
  onRunEvent,
  type ApprovalClass,
  type ChangedFile,
  type ConversationDetail,
  type ConversationView,
  type MessageHit,
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

/// Wired in main.ts so `?mock=1` can run the UI in a plain browser.
export const call = {
  invoke: ((cmd: string, args?: Record<string, unknown>) =>
    Promise.reject(new Error(`no ipc: ${cmd}`))) as <T>(
    cmd: string,
    args?: Record<string, unknown>,
  ) => Promise<T>,
  listen: ((_cb: (e: { payload: RunEventEnvelope }) => void) => Promise.resolve(() => {})) as (
    cb: (e: { payload: RunEventEnvelope }) => void,
  ) => Promise<() => void>,
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
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
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
};

const root = document.querySelector<HTMLElement>("#app")!;

function activeRun(): Run | null {
  return ui.detail?.conversation.run ?? null;
}

function isBusy(run: Run | null): boolean {
  return !!run && (run.state === "running" || run.state === "awaiting_approval");
}

// -------------------------------------------------------------------- render

function render() {
  const detail = ui.detail;
  const previous = document.querySelector<HTMLElement>(".transcript");
  const previousTop = previous?.scrollTop ?? 0;
  const follow = !previous || previous.scrollHeight - previousTop - previous.clientHeight < 120;
  const shell = h(
    "div",
    { class: "app" },
    sidebar(),
    h("main", { class: "pane" }, detail ? conversationPane(detail) : emptyPane()),
    detail && isBusy(detail.conversation.run) ? todoPanel(detail) : h("div"),
  );
  root.replaceChildren(
    shell,
    ...(ui.settingsOpen && ui.settings ? [settingsModal(ui.settings)] : []),
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

function sidebar(): HTMLElement {
  const list = h(
    "div",
    { class: "conv-list" },
    ...ui.conversations.map((c) =>
      h(
        "button",
        {
          class: `conv${ui.detail?.conversation.id === c.id ? " active" : ""}`,
          title: c.root_path,
          onclick: () => void selectConversation(c.id),
        },
        h("span", { class: "conv-title" }, c.title),
        h("span", { class: "conv-meta" }, c.run ? stateLabel[c.run.state] : "New"),
      ),
    ),
  );

  const searchResults = ui.search.length
    ? h(
        "div",
        { class: "search-results" },
        ...ui.search.map((hit) =>
          h(
            "button",
            {
              class: "hit",
              onclick: () => void jumpToHit(hit),
            },
            h("span", { class: "hit-role" }, hit.role),
            h("span", { class: "hit-text" }, hit.snippet),
          ),
        ),
      )
    : null;

  return h(
    "aside",
    { class: "sidebar" },
    h(
      "div",
      { class: "brand" },
      h("span", { class: "logo" }, "Aria"),
      h(
        "button",
        { class: "icon", title: "Settings", onclick: () => void openSettings() },
        "⚙",
      ),
    ),
    h(
      "button",
      {
        class: "primary",
        onclick: () => void chooseFolder(),
      },
      "Open folder…",
    ),
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
    h("div", { class: "section-title" }, "Conversations"),
    list,
  );
}

function emptyPane(): HTMLElement {
  return h(
    "div",
    { class: "empty" },
    h("h1", {}, "Pick a folder to work in"),
    h(
      "p",
      {},
      "Aria reads, edits and runs commands inside one folder, asking before it changes anything.",
    ),
    h("button", { class: "primary", onclick: () => void chooseFolder() }, "Open folder…"),
  );
}

function conversationPane(detail: ConversationDetail): HTMLElement {
  const run = detail.conversation.run;
  const busy = isBusy(run);
  const pending = detail.pending;

  const header = h(
    "header",
    { class: "conv-header" },
    h(
      "div",
      {},
      h("h2", {}, detail.conversation.title),
      h("span", { class: "path" }, detail.conversation.root_path),
    ),
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
    { class: "transcript" },
    ...detail.messages.map((m) => messageNode(m)),
    streamingNode(),
    ...pending.map(approvalNode),
    ui.notice ? h("div", { class: "notice" }, ui.notice) : null,
  );

  const composer = composerNode(busy);

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
  );
}

function messageNode(m: MessageView): HTMLElement {
  const isUser = m.role === "user";
  return h(
    "article",
    { class: `msg ${m.role}`, id: `msg-${m.id}` },
    h(
      "div",
      { class: "msg-head" },
      h("span", { class: "who" }, isUser ? "You" : "Aria"),
      h("time", { class: "when" }, fmt(m.created_at)),
    ),
    m.content ? h("div", { class: "msg-body" }, m.content) : null,
    ...m.tool_calls.map(toolCard),
  );
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
  return h(
    "div",
    { class: `tool ${state}`, id: `msg-${call.id}` },
    h(
      "div",
      { class: "tool-head" },
      h("code", { class: "tool-name" }, call.tool),
      h("span", { class: `pill class-${call.approval_class}` }, call.approval_class),
      h("span", { class: "pill state" }, state),
    ),
    h("pre", { class: "tool-args" }, JSON.stringify(a, null, 2)),
    output?.length ? h("pre", { class: "tool-output" }, output.join("\n")) : null,
    call.result ? h("pre", { class: `tool-result ${call.is_error ? "bad" : ""}` }, call.result) : null,
  );
}

function streamingNode(): HTMLElement | null {
  if (!ui.streaming) return null;
  const else_ =
    ui.streaming.text.length > 0
      ? h("div", { class: "msg-body" }, ui.streaming.text)
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
      h("strong", {}, p.summary.title),
      h("span", { class: `pill class-${p.class}` }, p.class),
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

function composerNode(busy: boolean): HTMLElement {
  const input = h("textarea", {
    class: "composer-input",
    rows: "2",
    placeholder: busy ? "Aria is working…" : "Ask Aria to do something in this folder",
    disabled: busy,
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        void send();
      }
    },
  });
  input.dataset.role = "composer";
  return h(
    "div",
    { class: "composer" },
    input,
    h(
      "button",
      { class: "primary", disabled: busy, onclick: () => void send() },
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
            h("li", { class: t.done ? "done" : "" }, t.done ? "☑ " : "☐ ", t.text),
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
async function runSearch(query: string) {
  window.clearTimeout(searchTimer);
  if (!query.trim()) {
    ui.search = [];
    render();
    return;
  }
  searchTimer = window.setTimeout(async () => {
    ui.search = await call.invoke<MessageHit[]>("search_messages", {
      query: query.trim(),
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

function settingsModal(view: SettingsView): HTMLElement {
  const s = view.settings;
  const field = (label: string, value: string | number, type = "text") =>
    h(
      "label",
      {},
      h("span", {}, label),
      h("input", { type, value: String(value), dataset: { field: label } }),
    );
  const close = () => {
    ui.settingsOpen = false;
    render();
  };
  return h(
    "div",
    { class: "modal-backdrop", onclick: (e: MouseEvent) => e.target === e.currentTarget && close() },
    h(
      "div",
      { class: "modal" },
      h("h2", {}, "Settings"),
      field("Base URL", s.base_url),
      field("Model", s.model),
      field("Max turns", s.caps.max_turns, "number"),
      field("Max prompt tokens", s.caps.max_prompt_tokens, "number"),
      field("Shell timeout (s)", s.shell_timeout_secs, "number"),
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
      h("p", { class: "muted small" }, "Leave the key empty to keep the stored one."),
      h(
        "div",
        { class: "approval-actions" },
        h(
          "button",
          {
            class: "primary",
            onclick: async () => {
              const val = (name: string) =>
                (document.querySelector<HTMLInputElement>(`input[data-field="${name}"]`)?.value ??
                  "").trim();
              try {
                ui.settings = await call.invoke<SettingsView>("set_settings", {
                  input: {
                    base_url: val("Base URL"),
                    model: val("Model"),
                    max_turns: Number(val("Max turns")) || 1,
                    max_prompt_tokens: Number(val("Max prompt tokens")) || 1,
                    shell_timeout_secs: Number(val("Shell timeout (s)")) || 1,
                    api_key: val("API key") || null,
                  },
                });
                ui.settingsOpen = false;
                ui.notice = "Settings saved.";
                render();
              } catch (e) {
                ui.notice = String(e);
                render();
              }
            },
          },
          "Save",
        ),
        h("button", { class: "ghost", onclick: close }, "Close"),
      ),
    ),
  );
}

// ------------------------------------------------------------------- events

function applyRunEvent(runId: string, event: RunEvent) {
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

export async function start() {
  // Tauri hands listeners the whole event (`{ payload }`), so unwrap it.
  await call.listen(({ payload }) => applyRunEvent(payload.run_id, payload.event));
  await refreshConversations();
  render();
  // Closing the window does not stop a Run: the Rust loop is detached and owns
  // the state, and this boot path rebuilds the view from the database.
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
};

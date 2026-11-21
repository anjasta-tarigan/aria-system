// An in-browser stand-in for the Rust side, used by `?mock=1`.
//
// It answers the same commands with the same shapes and replays a scripted
// Run, so the UI can be exercised (and screenshotted) without a model or a
// window. This file is not bundled into the real app path; the switch lives in
// main.ts.

import {
  KNOWN_EMBEDDING_MODELS,
  type AIProvider,
  type ProviderModel,
  type ApprovalClass,
  type BackgroundTask,
  type BuiltInTool,
  type ChangedFile,
  type ConversationDetail,
  type ConversationView,
  type ConversationMode,
  type DatabaseInfo,
  type EmbeddingConfig,
  type EmbeddingTestResult,
  type FetchModelsResult,
  type HealthView,
  type Memory,
  type MessageHit,
  type PendingApproval,
  type ProviderTestResult,
  type Run,
  type RunEvent,
  type RunEventEnvelope,
  type RunStateView,
  type SettingsView,
  type ToolCall,
  type WebSearchConfig,
  type WebSearchTestResult,
  type WebSearchVerifyResult,
} from "./api";

type Handler = (args: Record<string, unknown>) => unknown;

let seq = 0;
const id = (p: string) => `${p}-${++seq}`;
const now = () => Date.now();

// In-memory background tasks, keyed by id; survives across the Run that may
// have started them, so the UI can poll to a terminal state.
const backgroundTasks = new Map<string, BackgroundTask>();

const conversations = new Map<string, ConversationDetail>();
// One stable workspace id per folder path, so every session in a folder shares
// the same `workspace_id` and the UI groups them under one parent.
const workspaces = new Map<string, { id: string; pinned: boolean }>();

function findOrInsertWorkspace(root: string): { id: string; pinned: boolean } {
  const existing = workspaces.get(root);
  if (existing) return existing;
  const ws = { id: id("ws"), pinned: false };
  workspaces.set(root, ws);
  return ws;
}

/** Name for a new chat inside a workspace. Every chat is numbered so the
    workspace header stays the only place the folder name appears. */
function nextChatTitle(workspaceId: string): string {
  const n = [...conversations.values()].filter(
    (d) => d.conversation.workspace_id === workspaceId,
  ).length;
  return `Chat ${n + 1}`;
}
const settings: SettingsView = {
  settings: {
    base_url: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
    embedding_base_url: null,
    embedding_model: null,
    search_provider: null,
    search_base_url: null,
    approval_policy: "always_ask",
  },
  has_key: true,
  key_backend: "keyring",
};

const memories: Memory[] = [];

// --- Settings domain mock state (AI providers, embedding, web search, tools) ---
const aiProviders: AIProvider[] = [];
const embeddingConfig: EmbeddingConfig = {
  enabled: false,
  providerId: "default",
  model: "text-embedding-3-small",
};
let webSearchConfig: WebSearchConfig | null = null;
const toolState: Record<string, boolean> = {};

const listeners: Array<(e: { payload: RunEventEnvelope }) => void> = [];
function emit(event: RunEvent, runId: string) {
  for (const l of listeners) {
    l({ payload: { run_id: runId, event } });
  }
}

const stateNames: Record<string, string> = {
  Read: "read",
  Mutate: "mutate",
  Exec: "exec",
  Network: "network",
};

function toolCall(
  run: Run,
  turn: number,
  index: number,
  tool: string,
  cls: keyof typeof stateNames,
  args: Record<string, unknown>,
  state: ToolCall["state"] = "approved",
): ToolCall {
  return {
    id: id("call"),
    run_id: run.id,
    turn_index: turn,
    call_index: index,
    tool,
    args_json: JSON.stringify(args),
    approval_class: stateNames[cls] as ToolCall["approval_class"],
    state,
    result: null,
    is_error: false,
    created_at: now(),
  };
}

function summarize(tool: string, args: Record<string, unknown>) {
  const s = (k: string) => String(args[k] ?? "");
  switch (tool) {
    case "edit_file":
      return {
        title: `Edit ${s("path")}`,
        body: `--- search ---\n${s("search")}\n--- replace ---\n${s("replace")}`,
      };
    case "write_file":
      return { title: `Create ${s("path")}`, body: s("content") };
    case "run_shell":
      return { title: "Run shell command", body: s("cmd") };
    case "web_fetch":
      return { title: `Fetch ${s("url")}`, body: "" };
    default:
      return { title: `Run ${tool}`, body: JSON.stringify(args) };
  }
}

function pendingOf(calls: ToolCall[]): PendingApproval[] {
  return calls
    .filter((c) => c.state === "pending")
    .map((c) => {
      const args = JSON.parse(c.args_json) as Record<string, unknown>;
      return {
        approval_id: `ap-${c.id}`,
        tool_call_id: c.id,
        tool: c.tool,
        class: c.approval_class,
        summary: summarize(c.tool, args),
      };
    });
}

/** A plain chat reply, used when the main session has no folder open. */
async function runChat(detail: ConversationDetail, text: string) {
  const conv = detail.conversation;
  const run: Run = {
    id: id("run"),
    conversation_id: conv.id,
    state: "running",
    stop_reason: null,
    turn_count: 0,
    prompt_tokens: 0,
    error: null,
    started_at: now(),
    ended_at: null,
  };
  conv.run = run;
  detail.messages.push({
    id: id("msg"),
    role: "user",
    content: text,
    created_at: now(),
    attachments: [],
    tool_calls: [],
  });
  const setState = (state: Run["state"]) => {
    run.state = state;
    emit({ type: "state", run: { ...run } }, run.id);
  };

  emit({ type: "turn_started", turn_index: 0 }, run.id);
  let text2 = "";
  const say = async (words: string) => {
    for (const w of words.split(/(?<= )/)) {
      text2 += w;
      emit({ type: "text_delta", turn_index: 0, text: w, so_far: text2 }, run.id);
      await sleep(18);
    }
    detail.messages.push({
      id: id("msg"),
      role: "assistant",
      content: text2,
      created_at: now(),
      attachments: [],
      tool_calls: [],
    });
    emit({ type: "assistant_message", message_id: id("msg"), content: text2 }, run.id);  };

  if (/artifact|app|dashboard|calc|game/i.test(text)) {
    const streamReply = `I'll build an interactive calculator artifact for you:

<antArtifact identifier="calculator-app" type="text/html" title="Interactive Calculator">
<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { font-family: sans-serif; display: grid; place-items: center; height: 80vh; background: #0f1115; color: #fff; margin: 0; }
    .calc { background: #161a21; border: 1px solid #2a3039; border-radius: 12px; padding: 20px; width: 220px; }
    #screen { background: #0f1115; border: 1px solid #2a3039; border-radius: 6px; padding: 12px; font-size: 24px; text-align: right; margin-bottom: 16px; min-height: 28px; }
    .grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 8px; }
    button { padding: 12px; font-size: 16px; border: none; border-radius: 6px; background: #2a3039; color: #fff; cursor: pointer; }
    button.op { background: #6aa6ff; color: #0f1115; font-weight: bold; }
    button.eq { background: #4cc38a; color: #0f1115; font-weight: bold; grid-column: span 2; }
  </style>
</head>
<body>
  <div class="calc">
    <div id="screen">0</div>
    <div class="grid">
      <button onclick="press('7')">7</button>
      <button onclick="press('8')">8</button>
      <button onclick="press('9')">9</button>
      <button class="op" onclick="press('/')">/</button>
      <button onclick="press('4')">4</button>
      <button onclick="press('5')">5</button>
      <button onclick="press('6')">6</button>
      <button class="op" onclick="press('*')">*</button>
      <button onclick="press('1')">1</button>
      <button onclick="press('2')">2</button>
      <button onclick="press('3')">3</button>
      <button class="op" onclick="press('-')">-</button>
      <button onclick="press('0')">0</button>
      <button onclick="clr()">C</button>
      <button class="op" onclick="press('+')">+</button>
      <button class="eq" onclick="calc()">=</button>
    </div>
  </div>
  <script>
    let s = '';
    function press(v) { s += v; document.getElementById('screen').innerText = s; }
    function clr() { s = ''; document.getElementById('screen').innerText = '0'; }
    function calc() {
      try { s = String(Function('return ' + s)()); } catch { s = 'Err'; }
      document.getElementById('screen').innerText = s;
    }
  </script>
</body>
</html>
</antArtifact>

The calculator artifact is ready in the preview panel.`;
    await say(streamReply);
    run.stop_reason = "no_tool_calls";
    run.ended_at = now();
    setState("finished");
    return;
  }

  const reply =
    "I'm Aria, a desktop assistant. You can chat with me here, or open a folder " +
    "from the header to let me read, edit and run commands inside it. What would " +
    "you like to do?";
  await say(reply);
  run.stop_reason = "no_tool_calls";
  run.ended_at = now();
  setState("finished");
}

/** The scripted task the mock drives: find a bug, fix it, run the tests. */
async function runScript(detail: ConversationDetail, text: string) {
  if (!detail.conversation.workspace_id || /artifact|app|dashboard|calc|game/i.test(text)) {
    return runChat(detail, text);
  }
  const conv = detail.conversation;
  const run: Run = {
    id: id("run"),
    conversation_id: conv.id,
    state: "running",
    stop_reason: null,
    turn_count: 0,
    prompt_tokens: 0,
    error: null,
    started_at: now(),
    ended_at: null,
  };
  conv.run = run;
  detail.messages.push({
    id: id("msg"),
    role: "user",
    content: text,
    created_at: now(),
    attachments: [],
    tool_calls: [],
  });

  const setState = (state: Run["state"]) => {
    run.state = state;
    emit({ type: "state", run: { ...run } }, run.id);
  };

  const waitForAnswer = (calls: ToolCall[]) =>
    new Promise<void>((resolve) => {
      const tick = () => {
        if (calls.every((c) => c.state !== "pending")) resolve();
        else setTimeout(tick, 50);
      };
      tick();
    });

  const assistant = (turn: number) => {
    emit({ type: "turn_started", turn_index: turn }, run.id);
    const msgId = id("msg");
    return { turn, msgId, text: "", reasoning: "" };
  };
  const delta = (a: { turn: number; text: string }, piece: string) => {
    a.text += piece;
    emit(
      { type: "text_delta", turn_index: a.turn, text: piece, so_far: a.text },
      run.id,
    );
  };
  const reason = (a: { turn: number; reasoning: string }, piece: string) => {
    a.reasoning += piece;
    emit(
      { type: "reasoning_delta", turn_index: a.turn, text: piece, so_far: a.reasoning },
      run.id,
    );
  };
  const say = async (
    a: { turn: number; msgId: string; text: string; reasoning: string },
    words: string,
    reasoningWords?: string,
  ) => {
    if (reasoningWords) {
      for (const w of reasoningWords.split(/(?<= )/)) {
        reason(a, w);
        await sleep(18);
      }
    }
    for (const w of words.split(/(?<= )/)) {
      delta(a, w);
      await sleep(18);
    }
    run.turn_count = a.turn + 1;
    detail.messages.push({
      id: a.msgId,
      role: "assistant",
      content: a.text,
      reasoning: a.reasoning,
      created_at: now(),
      attachments: [],
      tool_calls: [],
    });
    emit({ type: "assistant_message", message_id: a.msgId, content: a.text }, run.id);
  };

  const finish = async (call: ToolCall, result: string, isError = false) => {
    call.state = "done";
    call.result = result;
    call.is_error = isError;
    emit(
      { type: "tool_result", tool_call_id: call.id, tool: call.tool, text: result, is_error: isError },
      run.id,
    );
  };

  // Turn 0: announce and look for the bug.
  const a0 = assistant(0);
  await say(a0, "Let me find where the total is computed. ", "I need to trace where the total is computed. Let me search for the total function. ");
  const search = toolCall(run, 0, 0, "search_files", "Read", { pattern: "fn total", glob: "**/*.rs" });
  detail.messages[detail.messages.length - 1].tool_calls.push(search);
  await sleep(120);
  await finish(search, "src/lib.rs:12:    pub fn total(items: &[Item]) -> u64 {");

  // Turn 1: propose a fix that needs approval.
  const a1 = assistant(1);
  await say(a1, "Line 12 skips the first item. I will fix the range. ");
  const edit = toolCall(run, 1, 0, "edit_file", "Mutate", {
    path: "src/lib.rs",
    search: "for i in 1..items.len() {",
    replace: "for i in 0..items.len() {",
  });
  edit.state = "pending";
  detail.messages[detail.messages.length - 1].tool_calls.push(edit);
  setState("awaiting_approval");
  emit({ type: "approval_requested", pending: pendingOf([edit])[0] }, run.id);
  await waitForAnswer([edit]);
  if ((edit.state as ToolCall["state"]) !== "approved") {
    await finish(edit, "Denied by the user; edit_file was not run.", true);
  } else {
    await finish(edit, "src/lib.rs: replaced 1 occurrence");
    changed.set(conv.workspace_id ?? "", ["src/lib.rs"]);
  }
  setState("running");

  // Turn 2: run the tests, which also needs approval.
  const a2 = assistant(2);
  await say(a2, "Now let me verify with the test suite. ");
  const sh = toolCall(run, 2, 0, "run_shell", "Exec", {
    cmd: "cargo test --quiet",
  });
  sh.state = "pending";
  detail.messages[detail.messages.length - 1].tool_calls.push(sh);
  setState("awaiting_approval");
  emit({ type: "approval_requested", pending: pendingOf([sh])[0] }, run.id);
  await waitForAnswer([sh]);
  if ((sh.state as ToolCall["state"]) !== "approved") {
    await finish(sh, "Denied by the user; run_shell was not run.", true);
  } else {
    for (const line of ["running 12 tests", "test result: ok. 12 passed; 0 failed"]) {
      emit({ type: "tool_output", tool_call_id: sh.id, line }, run.id);
      await sleep(140);
    }
    await finish(sh, `$ cargo test --quiet\nrunning 12 tests\ntest result: ok. 12 passed; 0 failed\nexit: 0`);
  }
  setState("running");

  // Turn 3: finish.
  const a3 = assistant(3);
  await say(a3, "Fixed: the loop started at 1, skipping the first item. All 12 tests pass. ");
  run.stop_reason = "no_tool_calls";
  run.ended_at = now();
  setState("finished");
}

function sleep(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}

const changed = new Map<string, string[]>();
const mainPaths = new Map<string, string[]>();

const handlers: Record<string, Handler> = {
  // The default main session: a conversation with no folder.
  new_conversation: () => {
    const conv: ConversationView = {
      id: id("conv"),
      workspace_id: null,
      title: "Main System",
      root_path: null,
      created_at: now(),
      updated_at: now(),
      pinned: false,
      main_system: true,
      mode: "normal",
      approval_policy: null,
      run: null,
    };
    conversations.set(conv.id, {
      conversation: conv,
      messages: [],
      todos: [],
      pending: [],
    });
    return conv;
  },
  // Open a folder by attaching it to an existing conversation, keeping history.
  attach_workspace: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    const root = String(a.rootPath);
    const ws = findOrInsertWorkspace(root);
    d.conversation.workspace_id = ws.id;
    d.conversation.root_path = root;
    d.conversation.pinned = ws.pinned;
    // Keep a title the user set; otherwise number it like the other chats.
    if (!d.conversation.title || d.conversation.title === "Main System") {
      d.conversation.title = nextChatTitle(ws.id);
    }
    return d.conversation;
  },
  list_cloud_folders: () => [
    { name: "Dropbox", path: "/home/user/Dropbox" },
    { name: "Google Drive", path: "/home/user/Google Drive" },
  ],
  open_workspace: (a) => {
    const root = String(a.rootPath);
    const ws = findOrInsertWorkspace(root);
    const existing = [...conversations.values()].find(
      (d) => d.conversation.workspace_id === ws.id,
    );
    if (existing) return existing.conversation;
    const conv: ConversationView = {
      id: id("conv"),
      workspace_id: ws.id,
      title: nextChatTitle(ws.id),
      root_path: root,
      created_at: now(),
      updated_at: now(),
      pinned: ws.pinned,
      main_system: false,
      mode: "normal",
      approval_policy: null,
      run: null,
    };
    conversations.set(conv.id, {
      conversation: conv,
      messages: [],
      todos: [],
      pending: [],
    });
    return conv;
  },
  // Start a fresh chat session inside an existing workspace.
  new_session: (a) => {
    const wsId = String(a.workspaceId);
    const siblings = [...conversations.values()].filter(
      (d) => d.conversation.workspace_id === wsId,
    );
    const existing = siblings[0];
    const root_path = existing?.conversation.root_path ?? null;
    const conv: ConversationView = {
      id: id("conv"),
      workspace_id: wsId,
      title: nextChatTitle(wsId),
      root_path,
      created_at: now(),
      updated_at: now(),
      pinned: existing?.conversation.pinned ?? false,
      main_system: false,
      mode: "normal",
      approval_policy: null,
      run: null,
    };
    conversations.set(conv.id, {
      conversation: conv,
      messages: [],
      todos: [],
      pending: [],
    });
    return conv;
  },
  rename_conversation: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    d.conversation.title = String(a.title);
    return null;
  },
  delete_conversation: (a) => {
    conversations.delete(String(a.conversationId));
    return null;
  },
  delete_workspace: (a) => {
    const wsId = String(a.workspaceId);
    for (const [k, d] of conversations) {
      if (d.conversation.workspace_id === wsId) conversations.delete(k);
    }
    for (const [root, ws] of workspaces) {
      if (ws.id === wsId) workspaces.delete(root);
    }
    return null;
  },
  set_workspace_pin: (a) => {
    const wsId = String(a.workspaceId);
    const pinned = Boolean(a.pinned);
    for (const ws of workspaces.values()) {
      if (ws.id === wsId) ws.pinned = pinned;
    }
    for (const d of conversations.values()) {
      if (d.conversation.workspace_id === wsId) d.conversation.pinned = pinned;
    }
    return null;
  },
  list_conversations: () => [...conversations.values()].map((d) => d.conversation),
  get_conversation: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    d.pending = d.conversation.run
      ? pendingOf(d.messages.flatMap((m) => m.tool_calls))
      : [];
    return structuredClone(d);
  },
  send_message: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    const text = String(a.text);
    // Announce the Run synchronously so the caller's first state read sees it.
    const run: Run = {
      id: id("run"),
      conversation_id: d.conversation.id,
      state: "running",
      stop_reason: null,
      turn_count: 0,
      prompt_tokens: 0,
      error: null,
      started_at: now(),
      ended_at: null,
    };
    d.conversation.run = run;
    setTimeout(() => void runScript(d, text), 0);
    return structuredClone(run);
  },
  answer_approval: (a) => {
    const approvalId = String(a.approvalId);
    for (const d of conversations.values()) {
      for (const m of d.messages) {
        for (const c of m.tool_calls) {
          if (`ap-${c.id}` === approvalId) {
            const allowAll = a.decision === "approve_all";
            c.state = a.decision === "deny" ? "denied" : "approved";
            for (const m of d.messages) {
              for (const other of m.tool_calls) {
                if (allowAll && other.state === "pending") other.state = "approved";
              }
            }
            d.pending = pendingOf(d.messages.flatMap((x) => x.tool_calls));
            return null;
          }
        }
      }
    }
    throw new Error("no such approval");
  },
  cancel_run: () => null,
  resume_run: () => null,
  get_run_state: (a): RunStateView => {
    for (const d of conversations.values()) {
      if (d.conversation.run?.id === a.runId) {
        return {
          run: d.conversation.run!,
          pending: pendingOf(d.messages.flatMap((m) => m.tool_calls)),
        };
      }
    }
    throw new Error("no such run");
  },
  undo: (a) => {
    const key = String(a.conversationId ?? "");
    const p = String(a.path);
    changed.set(key, (changed.get(key) ?? []).filter((x) => x !== p));
    return null;
  },
  changed_files: (a): ChangedFile[] =>
    (changed.get(String(a.conversationId ?? "")) ?? []).map((path) => ({
      path,
      can_undo: true,
    })),
  search_messages: (a): MessageHit[] => {
    const q = String(a.query).toLowerCase();
    if (!q) return [];
    const hits: MessageHit[] = [];
    for (const d of conversations.values()) {
      if (a.conversationId && d.conversation.id !== a.conversationId) continue;
      for (const m of d.messages) {
        const at = m.content.toLowerCase().indexOf(q);
        if (at < 0) continue;
        hits.push({
          message_id: m.id,
          conversation_id: d.conversation.id,
          role: m.role,
          snippet: `…${m.content.slice(Math.max(0, at - 20), at + 40)}…`,
          created_at: m.created_at,
        });
      }
    }
    return hits;
  },
  get_settings: () => settings,
  set_settings: (a) => {
    const input = a.input as Record<string, unknown>;
    settings.settings = {
      base_url: String(input.base_url),
      model: String(input.model),
      embedding_base_url: (input.embedding_base_url as string) ?? null,
      embedding_model: (input.embedding_model as string) ?? null,
      search_provider: (input.search_provider as string) ?? null,
      search_base_url: (input.search_base_url as string) ?? null,
      approval_policy: (input.approval_policy as string) ?? "always_ask",
    };
    if (input.api_key) settings.has_key = true;
    return settings;
  },
  list_memories: () => memories.slice(),
  delete_memory: (a) => {
    const i = memories.findIndex((m) => m.id === (a.id as string));
    if (i >= 0) memories.splice(i, 1);
  },
  get_main_system_paths: (a) => {
    const d = conversations.get(String(a.conversationId));
    return d ? (mainPaths.get(d.conversation.id) ?? []) : [];
  },
  list_workspace_dir: (a): string[] => {
    const rel = String(a.rel ?? "");
    if (rel === "") return ["src/", "README.md", "package.json"];
    if (rel === "src/") return ["main.ts", "app.ts"];
    return [];
  },
  read_workspace_file: (a): string => "// mock file: " + String(a.rel ?? ""),
  write_workspace_file: () => {
    // Mock backend is journaled externally; nothing to persist here.
  },
  start_background: (a): BackgroundTask => {
    const task: BackgroundTask = {
      id: id("bg"),
      cmd: String(a.cmd),
      conversationId: String(a.conversationId),
      startedAt: now(),
      status: "running",
      logBlob: "",
    };
    backgroundTasks.set(task.id, task);
    return task;
  },
  poll_background: (a): { task: BackgroundTask; tail: string } => {
    const task = backgroundTasks.get(String(a.id));
    if (!task) throw new Error("no such background task");
    // Flip a running task to its terminal state on the first poll so E2E can
    // observe the transition without a real child process.
    if (task.status === "running") {
      task.status = "succeeded";
      task.exitCode = 0;
      task.endedAt = now();
      task.logBlob = "mock-bg-log";
    }
    return { task, tail: "// mock background output\ndone" };
  },
  list_background: (a): BackgroundTask[] => {
    // Simulate completion: a task that has been running since the last list is
    // now done, so the UI's poll-to-terminal loop observes a transition.
    for (const t of backgroundTasks.values()) {
      if (t.status === "running") {
        t.status = "succeeded";
        t.exitCode = 0;
        t.endedAt = now();
        t.logBlob = "mock-bg-log";
      }
    }
    return [...backgroundTasks.values()].filter(
      (t) => t.conversationId === String(a.conversationId),
    );
  },
  kill_background: (a): BackgroundTask => {
    const task = backgroundTasks.get(String(a.id));
    if (!task) throw new Error("no such background task");
    task.status = "killed";
    task.endedAt = now();
    return task;
  },
  health: (): HealthView => ({
    status: "ok",
    detail: null,
    providerReachable: true,
    apiKeyPresent: true,
    model: settings.settings.model || "mock-model",
    schemaVersion: 6,
    version: "0.0.0-mock",
  }),
  get_database_info: (): DatabaseInfo => ({
    path: "/home/you/.local/share/dev.aria.app/aria.db",
    sizeBytes: 196608,
    walBytes: 32768,
    modifiedAt: now(),
    schemaVersion: 11,
    sqliteVersion: "3.46.0",
    journalMode: "wal",
    pageSize: 4096,
    tables: [
      { name: "workspaces", rows: workspaces.size },
      { name: "conversations", rows: conversations.size },
      { name: "main_system_paths", rows: mainPaths.size },
      {
        name: "messages",
        rows: [...conversations.values()].reduce((n, d) => n + d.messages.length, 0),
      },
      { name: "runs", rows: 4 },
      { name: "tool_calls", rows: 6 },
      { name: "approvals", rows: 3 },
      { name: "todos", rows: 3 },
      { name: "memories", rows: 2 },
      { name: "background_task", rows: backgroundTasks.size },
      { name: "edit_journal", rows: changed.size },
    ],
    blobsDir: "/home/you/.local/share/dev.aria.app/blobs",
    blobs: 4,
    blobsBytes: 8192,
    backupsDir: "/home/you/.local/share/dev.aria.app/backups",
  }),
  backup_database: () => undefined,
  import_database: () => handlers.get_database_info({}),
  reset_database: () => {
    conversations.clear();
    changed.clear();
    mainPaths.clear();
    return handlers.get_database_info({});
  },
  set_main_system_scope: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    const enabled = Boolean(a.enabled);
    const paths = (a.paths as string[]) ?? [];
    d.conversation.main_system = enabled;
    if (enabled) mainPaths.set(d.conversation.id, paths);
    else mainPaths.delete(d.conversation.id);
    return structuredClone(d.conversation);
  },
  set_conversation_mode: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    d.conversation.mode = a.mode as ConversationMode;
    return structuredClone(d.conversation);
  },
  set_conversation_approval: (a) => {
    const d = conversations.get(String(a.conversationId));
    if (!d) throw new Error("no such conversation");
    d.conversation.approval_policy = (a.approvalPolicy as string | null) ?? null;
    return structuredClone(d.conversation);
  },

  // --- Settings domain: AI providers ---
  list_ai_providers: () => structuredClone(aiProviders),
  get_ai_provider: (a) => {
    const p = aiProviders.find((p) => p.id === a.id);
    if (!p) throw new Error("no such provider");
    return structuredClone(p);
  },
  create_ai_provider: (a): AIProvider => {
    let models = (a.models as ProviderModel[]) ?? [];
    if (!models.length && a.modelIds) {
      models = (a.modelIds as string[]).map((id) => ({ id, name: id }));
    }
    const modelIds = models.map((m) => m.id);
    const p: AIProvider = {
      id: id("prov"),
      name: String(a.name),
      type: "openai-compatible",
      endpointUrl: String(a.endpointUrl),
      apiKeyMasked: "sk••••" + String(a.apiKey).slice(-4),
      models,
      modelIds,
      defaultModelId: String(a.defaultModelId || modelIds[0] || ""),
      isDefault: aiProviders.length === 0,
      status: "untested",
    };
    aiProviders.push(p);
    return structuredClone(p);
  },
  update_ai_provider: (a): AIProvider => {
    const p = aiProviders.find((p) => p.id === a.id);
    if (!p) throw new Error("no such provider");
    if (a.name !== undefined) p.name = String(a.name);
    if (a.endpointUrl !== undefined) p.endpointUrl = String(a.endpointUrl);
    if (a.apiKey) p.apiKeyMasked = "sk••••" + String(a.apiKey).slice(-4);
    if (a.models !== undefined) {
      p.models = a.models as ProviderModel[];
      p.modelIds = p.models.map((m) => m.id);
    } else if (a.modelIds !== undefined) {
      p.modelIds = a.modelIds as string[];
      p.models = p.modelIds.map((id) => ({ id, name: id }));
    }
    if (a.defaultModelId !== undefined) p.defaultModelId = String(a.defaultModelId);
    return structuredClone(p);
  },
  select_model: (a): HealthView => {
    const modelId = String(a.modelId || "");
    const providerId = a.providerId ? String(a.providerId) : null;
    if (providerId) {
      for (const p of aiProviders) {
        p.isDefault = p.id === providerId;
        if (p.id === providerId) p.defaultModelId = modelId;
      }
    }
    settings.settings.model = modelId;
    return handlers.health({}) as HealthView;
  },
  delete_ai_provider: (a) => {
    const i = aiProviders.findIndex((p) => p.id === a.id);
    if (i < 0) throw new Error("no such provider");
    const wasDefault = aiProviders[i].isDefault;
    aiProviders.splice(i, 1);
    if (wasDefault) {
      for (const p of aiProviders) p.isDefault = false;
      if (aiProviders[0]) {
        aiProviders[0].isDefault = true;
      }
    }
  },
  set_default_ai_provider: (a) => {
    for (const p of aiProviders) p.isDefault = p.id === a.id;
  },
  test_ai_provider: (a): ProviderTestResult => {
    // Draft form: no id, test the endpoint being typed.
    const p = a.id ? aiProviders.find((p) => p.id === a.id) : undefined;
    if (a.id && !p) throw new Error("no such provider");
    const endpoint = p ? p.endpointUrl : String(a.endpointUrl ?? "");
    // An endpoint containing "fail" simulates a broken connection so the E2E
    // can exercise the error path deterministically.
    const ok = !endpoint.includes("fail");
    if (p) {
      p.status = ok ? "active" : "error";
      p.lastTestedAt = now();
    }
    return { ok, latencyMs: 42, error: ok ? undefined : "Invalid API key" };
  },
  fetch_ai_provider_models: (): FetchModelsResult => ({
    models: ["gpt-4o", "gpt-4o-mini", "o3-mini"],
  }),

  // --- Settings domain: embedding ---
  get_embedding_config: () => structuredClone(embeddingConfig),
  set_embedding_config: (a): EmbeddingConfig => {
    const cfg = a.config as EmbeddingConfig;
    Object.assign(embeddingConfig, cfg);
    return structuredClone(embeddingConfig);
  },
  // Mirror the real probe: report the vector width of the model on screen.
  test_embedding: (a): EmbeddingTestResult => {
    const model = String(a?.model ?? embeddingConfig.model);
    return { ok: true, vectorLength: KNOWN_EMBEDDING_MODELS[model] ?? 1536, latencyMs: 30 };
  },

  // --- Settings domain: web search ---
  get_web_search_config: () => (webSearchConfig ? structuredClone(webSearchConfig) : null),
  set_web_search_config: (a): WebSearchConfig => {
    webSearchConfig = {
      providerKey: String(a.providerKey),
      apiKeyMasked: "••••" + String(a.apiKey ?? "").slice(-4),
      status: "connected",
      lastVerifiedAt: now(),
    };
    toolState.web_search = true;
    return structuredClone(webSearchConfig);
  },
  verify_web_search: (a): WebSearchVerifyResult => {
    // A provider key containing "bad" simulates a rejected key so the E2E can
    // exercise the failure path deterministically.
    if (String(a.apiKey ?? "").includes("bad")) {
      return { ok: false, error: "Invalid API key" };
    }
    return { ok: true };
  },
  disconnect_web_search: (): void => {
    webSearchConfig = null;
    toolState.web_search = false;
  },
  get_embedding_index_info: () => ({ total: 3, embedded: 2 }),
  reindex_memories: () => 3,
  test_web_search: (): WebSearchTestResult => ({
    ok: true,
    titles: [
      { title: "Aria — local-first AI assistant", domain: "aria.dev" },
      { title: "OpenAI-compatible APIs", domain: "platform.openai.com" },
      { title: "Tauri desktop apps", domain: "tauri.app" },
    ],
  }),

  // --- Settings domain: built-in tools ---
  list_built_in_tools: (): BuiltInTool[] =>
    TOOL_CATALOG.map((t) => ({
      id: t.id,
      name: t.name,
      description: t.description,
      enabled: toolState[t.id] ?? true,
      requiresSetup: t.requiresSetup,
      setupComplete: !t.requiresSetup || !!webSearchConfig,
      permission: t.permission,
    })),
  set_built_in_tool: (a): BuiltInTool => {
    const t = TOOL_CATALOG.find((t) => t.id === a.id);
    if (!t) throw new Error("unknown tool");
    toolState[t.id] = Boolean(a.enabled);
    return {
      id: t.id,
      name: t.name,
      description: t.description,
      enabled: Boolean(a.enabled),
      requiresSetup: t.requiresSetup,
      setupComplete: !t.requiresSetup || !!webSearchConfig,
      permission: t.permission,
    };
  },
};

/// Mirrors aria-core's tool catalog (ids must match the Rust registry).
const TOOL_CATALOG: { id: string; name: string; description: string; requiresSetup: boolean; permission: ApprovalClass }[] = [
  { id: "read_file", name: "File reader", description: "Reads a file inside the open workspace.", requiresSetup: false, permission: "read" },
  { id: "view_image", name: "Image viewer", description: "Views an image with the vision model.", requiresSetup: false, permission: "read" },
  { id: "list_dir", name: "Folder listing", description: "Lists the contents of a workspace folder.", requiresSetup: false, permission: "read" },
  { id: "search_files", name: "Workspace search", description: "Searches file contents across the workspace.", requiresSetup: false, permission: "read" },
  { id: "edit_file", name: "Edit file", description: "Replaces an exact substring in a workspace file.", requiresSetup: false, permission: "mutate" },
  { id: "write_file", name: "Write file", description: "Creates or overwrites a workspace file.", requiresSetup: false, permission: "mutate" },
  { id: "delete_file", name: "Delete file", description: "Deletes a file inside the workspace.", requiresSetup: false, permission: "mutate" },
  { id: "move_file", name: "Move file", description: "Moves or renames a workspace file.", requiresSetup: false, permission: "mutate" },
  { id: "run_shell", name: "Shell commands", description: "Runs a shell command in the workspace.", requiresSetup: false, permission: "exec" },
  { id: "web_fetch", name: "Web fetch", description: "Fetches and reads a web page.", requiresSetup: false, permission: "network" },
  { id: "web_search", name: "Web search", description: "Searches the web through a configured provider.", requiresSetup: true, permission: "network" },
  { id: "todos", name: "Todo list", description: "Maintains the run's todo list.", requiresSetup: false, permission: "read" },
  { id: "remember", name: "Remember facts", description: "Persists durable facts for later recall.", requiresSetup: false, permission: "read" },
  { id: "recall", name: "Recall memories", description: "Recalls previously stored facts.", requiresSetup: false, permission: "read" },
  { id: "run_background", name: "Background tasks", description: "Starts a long-running command in the background.", requiresSetup: false, permission: "exec" },
  { id: "poll_task", name: "Task polling", description: "Reads a background task's status and output.", requiresSetup: false, permission: "read" },
  { id: "kill_task", name: "Task control", description: "Stops a background task this chat started.", requiresSetup: false, permission: "read" },
  { id: "delegate", name: "Research sub-agent", description: "Runs a read-only research sub-agent and returns its report.", requiresSetup: false, permission: "network" },
];

/** Drop-in replacement for `invoke`. */
/// Test hook: make the next invocation of `cmd` reject, so the error paths
/// (failed save, dropped backend) can be exercised deterministically.
export const mockFaults: { failNext?: string } = {};

export async function mockInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const h = handlers[cmd];
  if (!h) throw new Error(`mock: unknown command ${cmd}`);
  await sleep(20);
  if (mockFaults.failNext === cmd) {
    mockFaults.failNext = undefined;
    throw new Error("network unreachable");
  }
  return h(args ?? {}) as T;
}

/** Same contract as Tauri's `listen`: call back with `{ payload }`. */
export function mockListen(
  cb: (e: { payload: RunEventEnvelope }) => void,
): Promise<() => void> {
  listeners.push(cb);
  return Promise.resolve(() => {
    const i = listeners.indexOf(cb);
    if (i >= 0) listeners.splice(i, 1);
  });
}

/** Seed a main session (and a sample folder conversation) for the demo page. */
export function seed(): void {
  // HMR re-boots call this again; keep the existing mock state instead of
  // stacking duplicate conversations in the sidebar.
  if (conversations.size > 0) return;
  const main = handlers.new_conversation({}) as ConversationView;
  const d = conversations.get(main.id);
  if (d) {
    d.todos = [
      { id: id("todo"), run_id: "", position: 0, text: "Reply to the user's greetings", done: true },
      { id: id("todo"), run_id: "", position: 1, text: "Open a folder when asked to work on files", done: false },
    ];
  }
  // A sample workspace with two chat sessions, to exercise the sidebar.
  const sample = handlers.open_workspace({
    rootPath: "/home/anjasta/Projects/sample-app",
  }) as ConversationView;
  handlers.new_session({ workspaceId: sample.workspace_id });

  // Dedicated Artifacts Showcase session demonstrating multi-version Claude-style artifacts
  const artSession = handlers.new_session({
    workspaceId: sample.workspace_id,
  }) as ConversationView;
  const artDetail = conversations.get(artSession.id);
  if (artDetail) {
    artSession.title = "Artifacts Showcase";
    artDetail.conversation.title = "Artifacts Showcase";
    artDetail.messages.push(
      {
        id: id("msg"),
        role: "user",
        content: "Can you create an interactive counter and dashboard web app?",
        created_at: now() - 120_000,
        attachments: [],
        tool_calls: [],
      },
      {
        id: id("msg"),
        role: "assistant",
        content: `I've created an interactive counter and metrics dashboard for you:

<antArtifact identifier="metrics-counter" type="text/html" title="Interactive Counter Widget">
<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { font-family: system-ui, sans-serif; padding: 24px; background: #0f1115; color: #e6e9ef; display: flex; flex-direction: column; align-items: center; justify-content: center; height: 80vh; margin: 0; }
    .card { background: #161a21; border: 1px solid #2a3039; border-radius: 12px; padding: 24px; text-align: center; box-shadow: 0 4px 12px rgba(0,0,0,0.3); }
    h2 { margin: 0 0 16px 0; color: #6aa6ff; font-size: 20px; }
    .count { font-size: 48px; font-weight: bold; color: #4cc38a; margin-bottom: 20px; font-variant-numeric: tabular-nums; }
    .btn-row { display: flex; gap: 12px; justify-content: center; }
    button { background: #6aa6ff; color: #0f1115; border: none; padding: 10px 18px; border-radius: 6px; font-weight: 600; cursor: pointer; transition: opacity 0.15s; }
    button:hover { opacity: 0.9; }
    button.reset { background: #2a3039; color: #8b94a3; }
  </style>
</head>
<body>
  <div class="card">
    <h2>Interactive Counter Widget</h2>
    <div class="count" id="val">0</div>
    <div class="btn-row">
      <button onclick="change(-1)">- Decrement</button>
      <button class="reset" onclick="reset()">Reset</button>
      <button onclick="change(1)">+ Increment</button>
    </div>
  </div>
  <script>
    let n = 0;
    function change(delta) {
      n += delta;
      document.getElementById('val').innerText = n;
    }
    function reset() {
      n = 0;
      document.getElementById('val').innerText = n;
    }
  </script>
</body>
</html>
</antArtifact>

You can interact with it in the Preview pane, view the code, or download it.`,
        created_at: now() - 60_000,
        reasoning: "The user wants a self-contained, interactive counter widget with a visible metrics display. I'll wrap it in a single HTML artifact with inline CSS and JavaScript so it runs entirely in the preview pane without external dependencies or build steps.",
        attachments: [],
        tool_calls: [],
      },
      {
        id: id("msg"),
        role: "user",
        content: "Can you update it with auto-increment speed controls?",
        created_at: now() - 30_000,
        attachments: [],
        tool_calls: [],
      },
      {
        id: id("msg"),
        role: "assistant",
        content: `Here is the updated version (v2) with auto-increment speed controls:

<antArtifact identifier="metrics-counter" type="text/html" title="Interactive Counter Widget with Auto-Timer">
<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { font-family: system-ui, sans-serif; padding: 24px; background: #0f1115; color: #e6e9ef; display: flex; flex-direction: column; align-items: center; justify-content: center; height: 80vh; margin: 0; }
    .card { background: #161a21; border: 1px solid #2a3039; border-radius: 12px; padding: 24px; text-align: center; box-shadow: 0 4px 12px rgba(0,0,0,0.3); min-width: 280px; }
    h2 { margin: 0 0 16px 0; color: #6aa6ff; font-size: 20px; }
    .count { font-size: 56px; font-weight: bold; color: #4cc38a; margin-bottom: 20px; font-variant-numeric: tabular-nums; }
    .btn-row { display: flex; gap: 8px; justify-content: center; margin-bottom: 12px; }
    button { background: #6aa6ff; color: #0f1115; border: none; padding: 10px 16px; border-radius: 6px; font-weight: 600; cursor: pointer; }
    button:hover { opacity: 0.9; }
    button.auto { background: #d98cff; }
    button.reset { background: #2a3039; color: #8b94a3; }
  </style>
</head>
<body>
  <div class="card">
    <h2>Interactive Counter (v2)</h2>
    <div class="count" id="val">0</div>
    <div class="btn-row">
      <button onclick="change(-1)">-1</button>
      <button class="reset" onclick="reset()">Reset</button>
      <button onclick="change(1)">+1</button>
    </div>
    <div class="btn-row">
      <button class="auto" id="toggle-btn" onclick="toggleAuto()">Start Auto-Increment</button>
    </div>
  </div>
  <script>
    let n = 0;
    let timer = null;
    function change(delta) {
      n += delta;
      document.getElementById('val').innerText = n;
    }
    function reset() {
      n = 0;
      document.getElementById('val').innerText = n;
    }
    function toggleAuto() {
      const btn = document.getElementById('toggle-btn');
      if (timer) {
        clearInterval(timer);
        timer = null;
        btn.innerText = 'Start Auto-Increment';
      } else {
        timer = setInterval(() => change(1), 300);
        btn.innerText = 'Stop Auto-Increment';
      }
    }
  </script>
</body>
</html>
</antArtifact>

I updated the widget with an automated increment timer. Use the version switcher at the top to toggle between v1 and v2!`,
        created_at: now() - 10_000,
        reasoning: "The user asked to add auto-increment speed controls, so this is a v2 update of the same artifact (reusing identifier metrics-counter). I kept the existing counter UI and added an interval-driven auto-increment toggle. Reusing the identifier lets the version switcher surface both v1 and v2.",
        attachments: [],
        tool_calls: [],
      },
    );
  }

  if (aiProviders.length === 0) {
    aiProviders.push(
      {
        id: "prov-openai",
        name: "OpenAI",
        type: "openai-compatible",
        endpointUrl: "https://api.openai.com/v1",
        apiKeyMasked: "sk••••abcd",
        models: [
          { id: "gpt-4o", name: "GPT-4o" },
          { id: "gpt-4o-mini", name: "GPT-4o mini" },
        ],
        modelIds: ["gpt-4o", "gpt-4o-mini"],
        defaultModelId: "gpt-4o",
        isDefault: true,
        status: "active",
        lastTestedAt: Date.now() - 3600_000,
      },
      {
        id: "prov-anthropic",
        name: "Anthropic (OpenRouter)",
        type: "openai-compatible",
        endpointUrl: "https://openrouter.ai/api/v1",
        apiKeyMasked: "sk••••efgh",
        models: [
          { id: "anthropic/claude-3.5-sonnet", name: "Claude 3.5 Sonnet" },
          { id: "anthropic/claude-3.5-haiku", name: "Claude 3.5 Haiku" },
        ],
        modelIds: ["anthropic/claude-3.5-sonnet", "anthropic/claude-3.5-haiku"],
        defaultModelId: "anthropic/claude-3.5-sonnet",
        isDefault: false,
        status: "active",
        lastTestedAt: Date.now() - 7200_000,
      },
    );
    settings.settings.model = "gpt-4o";
  }
}

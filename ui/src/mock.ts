// An in-browser stand-in for the Rust side, used by `?mock=1`.
//
// It answers the same commands with the same shapes and replays a scripted
// Run, so the UI can be exercised (and screenshotted) without a model or a
// window. This file is not bundled into the real app path; the switch lives in
// main.ts.

import type {
  ChangedFile,
  ConversationDetail,
  ConversationView,
  Memory,
  MessageHit,
  PendingApproval,
  Run,
  RunEvent,
  RunEventEnvelope,
  RunStateView,
  SettingsView,
  Todo,
  ToolCall,
} from "./api";

type Handler = (args: Record<string, unknown>) => unknown;

let seq = 0;
const id = (p: string) => `${p}-${++seq}`;
const now = () => Date.now();

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
    caps: { max_turns: 50, max_prompt_tokens: 200000 },
    shell_timeout_secs: 120,
    embedding_base_url: null,
    embedding_model: null,
    search_provider: null,
    search_base_url: null,
    vision_model: null,
    approval_policy: "always_ask",
  },
  has_key: true,
  key_backend: "keyring",
};

const memories: Memory[] = [];

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
      tool_calls: [],
    });
    emit({ type: "assistant_message", message_id: id("msg"), content: text2 }, run.id);
  };

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
  if (!detail.conversation.workspace_id) {
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
    return { turn, msgId, text: "" };
  };
  const delta = (a: { turn: number; text: string }, piece: string) => {
    a.text += piece;
    emit(
      { type: "text_delta", turn_index: a.turn, text: piece, so_far: a.text },
      run.id,
    );
  };
  const say = async (a: { turn: number; msgId: string; text: string }, words: string) => {
    for (const w of words.split(/(?<= )/)) {
      delta(a, w);
      await sleep(18);
    }
    run.turn_count = a.turn + 1;
    detail.messages.push({
      id: a.msgId,
      role: "assistant",
      content: a.text,
      created_at: now(),
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
  await say(a0, "Let me find where the total is computed. ");
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
      main_system: false,
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
            c.state = a.decision === "approve" ? "approved" : "denied";
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
    const ws = String(a.workspaceId ?? "");
    const p = String(a.path);
    changed.set(ws, (changed.get(ws) ?? []).filter((x) => x !== p));
    return null;
  },
  changed_files: (a): ChangedFile[] =>
    (changed.get(String(a.workspaceId ?? "")) ?? []).map((path) => ({
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
      caps: {
        max_turns: Number(input.max_turns),
        max_prompt_tokens: Number(input.max_prompt_tokens),
      },
      shell_timeout_secs: Number(input.shell_timeout_secs),
      embedding_base_url: (input.embedding_base_url as string) ?? null,
      embedding_model: (input.embedding_model as string) ?? null,
      search_provider: (input.search_provider as string) ?? null,
      search_base_url: (input.search_base_url as string) ?? null,
      vision_model: (input.vision_model as string) ?? null,
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
};

/** Drop-in replacement for `invoke`. */
export async function mockInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const h = handlers[cmd];
  if (!h) throw new Error(`mock: unknown command ${cmd}`);
  await sleep(20);
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
}

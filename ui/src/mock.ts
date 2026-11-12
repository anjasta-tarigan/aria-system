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
const settings: SettingsView = {
  settings: {
    base_url: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
    caps: { max_turns: 50, max_prompt_tokens: 200000 },
    shell_timeout_secs: 120,
  },
  has_key: true,
  key_backend: "keyring",
};

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

/** The scripted task the mock drives: find a bug, fix it, run the tests. */
async function runScript(detail: ConversationDetail, text: string) {
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
    changed.set(conv.workspace_id, ["src/lib.rs"]);
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

const handlers: Record<string, Handler> = {
  open_workspace: (a) => {
    const root = String(a.rootPath);
    const existing = [...conversations.values()].find(
      (d) => d.conversation.root_path === root,
    );
    if (existing) return existing.conversation;
    const conv: ConversationView = {
      id: id("conv"),
      workspace_id: id("ws"),
      title: root.split("/").filter(Boolean).pop() ?? root,
      root_path: root,
      created_at: now(),
      updated_at: now(),
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
    const ws = String(a.workspaceId);
    const p = String(a.path);
    changed.set(
      ws,
      (changed.get(ws) ?? []).filter((x) => x !== p),
    );
    return null;
  },
  changed_files: (a): ChangedFile[] =>
    (changed.get(String(a.workspaceId)) ?? []).map((path) => ({
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
    };
    if (input.api_key) settings.has_key = true;
    return settings;
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

/** Seed one conversation so the Mock page has something to show. */
export function seed(): void {
  const conv = handlers.open_workspace({ rootPath: "/home/you/projects/demo" }) as ConversationView;
  const d = conversations.get(conv.id);
  if (!d) return;
  d.todos = [
    { id: id("todo"), run_id: "", position: 0, text: "Find where the total is computed", done: true },
    { id: id("todo"), run_id: "", position: 1, text: "Fix the off-by-one", done: false },
    { id: id("todo"), run_id: "", position: 2, text: "Run the tests", done: false },
  ];
}

use std::sync::{Arc, Mutex};

use aria_core::agent::{Agent, ProviderSource, RunEvent, RunHandle};
use aria_core::config::{ApprovalPolicy, Caps, Config, Settings};
use aria_core::db::{self, Db, Decision, Mode, Role, RunState, ToolCallState};
use aria_core::journal::Blobs;
use aria_core::provider::{
    ChatMessage, Provider, ProviderEvent, ToolCallRequest, ToolSpec, TurnResult,
};
use tokio::sync::mpsc;

// ------------------------------------------------------------- fake provider

/// Each queued Turn is one scripted model response.
type Script = Vec<Box<dyn Fn(&[ChatMessage]) -> TurnResult + Send + Sync>>;

struct FakeProvider {
    /// Turns consumed so far, and what the model was asked each time.
    calls: Mutex<Vec<Vec<ChatMessage>>>,
    script: Mutex<Script>,
}

impl FakeProvider {
    fn new(turns: Vec<TurnResult>) -> Arc<Self> {
        let script: Script = turns
            .into_iter()
            .map(|t| {
                let t = Mutex::new(Some(t));
                Box::new(move |_: &[ChatMessage]| t.lock().unwrap().take().unwrap())
                    as Box<dyn Fn(&[ChatMessage]) -> TurnResult + Send + Sync>
            })
            .collect();
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(script),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn last_messages(&self) -> Vec<ChatMessage> {
        self.calls.lock().unwrap().last().cloned().unwrap_or_default()
    }
}

impl Provider for FakeProvider {
    fn stream_chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> std::pin::Pin<
        std::boxed::Box<
            dyn std::future::Future<Output = aria_core::provider::Result<TurnResult>> + Send + 'a,
        >,
    > {
        self.calls.lock().unwrap().push(messages.to_vec());
        let next = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                None
            } else {
                Some(script.remove(0))
            }
        };
        Box::pin(async move {
            let Some(make) = next else {
                return Ok(TurnResult::default());
            };
            let turn = make(messages);
            // Stream text as small deltas, as a real provider would.
            for word in turn.text.split_inclusive(' ') {
                let _ = events.send(ProviderEvent::TextDelta(word.to_string()));
            }
            for call in &turn.tool_calls {
                let _ = events.send(ProviderEvent::ToolCall(call.clone()));
            }
            if let Some(tokens) = turn.prompt_tokens {
                let _ = events.send(ProviderEvent::Usage { prompt_tokens: tokens });
            }
            Ok(turn)
        })
    }
}

fn text(s: &str) -> TurnResult {
    TurnResult {
        text: s.to_string(),
        tool_calls: Vec::new(),
        prompt_tokens: Some(10),
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCallRequest {
    ToolCallRequest::new(id, name, args.to_string())
}

fn tools(calls: Vec<ToolCallRequest>) -> TurnResult {
    TurnResult {
        text: String::new(),
        tool_calls: calls,
        prompt_tokens: Some(10),
    }
}

// ------------------------------------------------------------------ fixture

struct Fx {
    _ws: tempfile::TempDir,
    _blob: tempfile::TempDir,
    db: Arc<Db>,
    conversation_id: String,
}

fn fixture() -> Fx {
    let ws = tempfile::tempdir().unwrap();
    let blob = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let workspace_id = db
        .call(|c| db::insert_workspace(c, &ws.path().display().to_string()))
        .unwrap()
        .id;
    let conversation_id = db
        .call(|c| db::insert_conversation(c, Some(&workspace_id), "test"))
        .unwrap()
        .id;
    Fx {
        _ws: ws,
        _blob: blob,
        db,
        conversation_id,
    }
}

impl Fx {
    fn root(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(self._ws.path())
    }

    fn agent(&self, caps: Caps, provider: Arc<FakeProvider>) -> Agent {
        Agent::new(
            Arc::clone(&self.db),
            Arc::new(Blobs::new(self._blob.path(), Arc::clone(&self.db))),
            caps,
            ProviderSource::Fixed {
                provider,
                approval_policy: ApprovalPolicy::AlwaysAsk,
            },
        )
    }

    fn agent_with_settings(&self, caps: Caps, settings: Settings, provider: Arc<FakeProvider>) -> Agent {
        Agent::new(
            Arc::clone(&self.db),
            Arc::new(Blobs::new(self._blob.path(), Arc::clone(&self.db))),
            caps,
            ProviderSource::Fixed {
                provider,
                approval_policy: settings.approval_policy,
            },
        )
    }

    fn run_of(&self, handle: &RunHandle) -> db::Run {
        self.db.call(|c| db::get_run(c, handle.run_id())).unwrap()
    }

    fn tool_calls(&self, handle: &RunHandle) -> Vec<db::ToolCall> {
        self.db
            .call(|c| db::list_tool_calls(c, handle.run_id()))
            .unwrap()
    }

    fn messages(&self) -> Vec<db::Message> {
        self.db
            .call(|c| db::list_messages(c, &self.conversation_id))
            .unwrap()
    }
}

/// Poll the database until the Run reaches a terminal state.
async fn wait_terminal(fx: &Fx, handle: &RunHandle) -> RunState {
    for _ in 0..600 {
        let run = fx.run_of(handle);
        if run.state.is_terminal() {
            return run.state;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run did not finish: {:?}", fx.run_of(handle).state);
}

async fn wait_awaiting(fx: &Fx, handle: &RunHandle) {
    for _ in 0..600 {
        if fx.run_of(handle).state == RunState::AwaitingApproval {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run never parked on an approval");
}

/// The pending approval ids, in order.
fn pending(fx: &Fx, handle: &RunHandle) -> Vec<String> {
    fx.tool_calls(handle)
        .into_iter()
        .filter(|c| c.state == ToolCallState::Pending)
        .filter_map(|c| {
            fx.db
                .call(|conn| db::approval_for_tool_call(conn, &c.id))
                .unwrap()
                .filter(|a| a.decision.is_none())
                .map(|a| a.id)
        })
        .collect()
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn multi_turn_run_persists_messages_calls_and_todos() {
    let fx = fixture();
    std::fs::write(fx.root().join("note.txt"), "hello world").unwrap();

    let provider = FakeProvider::new(vec![
        tools(vec![
            call("c1", "read_file", serde_json::json!({"path": "note.txt"})),
            call(
                "c2",
                "todos",
                serde_json::json!({"items": [{"text": "read the note"}, {"text": "report"}]}),
            ),
        ]),
        text("The note says hello world. "),
    ]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "what does note.txt say?").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(provider.call_count(), 2);

    // Reads need no approval; both calls ran.
    let calls = fx.tool_calls(&handle);
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.state == ToolCallState::Done));
    assert!(calls[0].result.as_deref().unwrap().contains("hello world"));

    let todos = fx.db.call(|c| db::list_todos(c, handle.run_id())).unwrap();
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].text, "read the note");

    let msgs = fx.messages();
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs.iter().filter(|m| m.role == Role::Assistant).count(), 2);

    // The second request carried the Tool Results back to the model.
    let second = provider.last_messages();
    let tool_msgs: Vec<_> = second
        .iter()
        .filter(|m| matches!(m, ChatMessage::Tool { .. }))
        .collect();
    assert_eq!(tool_msgs.len(), 2, "{second:?}");

    let run = fx.run_of(&handle);
    assert_eq!(run.turn_count, 2);
    assert_eq!(run.prompt_tokens, 20);
    assert_eq!(run.stop_reason.as_deref(), Some("no_tool_calls"));
}

#[tokio::test]
async fn approval_parks_the_run_until_answered() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("w1", "write_file", serde_json::json!({"path": "new.txt", "content": "x"}))]),
        text("done "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "create new.txt").await.unwrap();

    wait_awaiting(&fx, &handle).await;
    assert!(!fx.root().join("new.txt").exists());

    let pending = pending(&fx, &handle);
    assert_eq!(pending.len(), 1);
    agent.answer_approval(&pending[0], Decision::Approve).unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(std::fs::read_to_string(fx.root().join("new.txt")).unwrap(), "x");
    assert_eq!(fx.tool_calls(&handle)[0].state, ToolCallState::Done);
}

#[tokio::test]
async fn allow_all_this_run_approves_remaining_calls() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![
            call(
                "w1",
                "write_file",
                serde_json::json!({"path": "a.txt", "content": "a"}),
            ),
            call(
                "w2",
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "b"}),
            ),
        ]),
        text("both written "),
    ]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "write two files").await.unwrap();
    wait_awaiting(&fx, &handle).await;
    let ids = pending(&fx, &handle);
    assert_eq!(ids.len(), 2);

    agent.answer_approval(&ids[0], Decision::ApproveAll).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(std::fs::read_to_string(fx.root().join("a.txt")).unwrap(), "a");
    assert_eq!(std::fs::read_to_string(fx.root().join("b.txt")).unwrap(), "b");
}

#[tokio::test]
async fn denying_an_approval_yields_an_error_result_and_the_run_continues() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("d1", "delete_file", serde_json::json!({"path": "keep.txt"}))]),
        text("I will leave it alone. "),
    ]);
    std::fs::write(fx.root().join("keep.txt"), "precious").unwrap();
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "delete keep.txt").await.unwrap();

    wait_awaiting(&fx, &handle).await;
    let id = pending(&fx, &handle).pop().unwrap();
    agent.answer_approval(&id, Decision::Deny).unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert!(fx.root().join("keep.txt").exists(), "denied tool must not run");
    let call = fx.tool_calls(&handle).remove(0);
    assert_eq!(call.state, ToolCallState::Denied);
    assert!(call.result.unwrap().contains("Denied"));
}

#[tokio::test]
async fn state_lives_in_rows_so_a_new_loop_continues_the_same_run() {
    let fx = fixture();
    // The first loop is abandoned (simulating a process restart) after it
    // parks on an approval; a fresh Agent resumes the same Run from the DB.
    let provider = FakeProvider::new(vec![
        tools(vec![call("w1", "write_file", serde_json::json!({"path": "a.txt", "content": "a"}))]),
        text("finished "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "make a.txt").await.unwrap();
    wait_awaiting(&fx, &handle).await;

    let id = pending(&fx, &handle).pop().unwrap();
    agent.answer_approval(&id, Decision::Approve).unwrap();
    // Wait for the first loop to finish the run.
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);

    // Restarting and reading state finds the same transcript.
    let snapshot = agent.snapshot(handle.run_id()).unwrap();
    assert_eq!(snapshot.run.state, RunState::Finished);
    assert!(snapshot.pending.is_empty());
    let calls = fx.tool_calls(&handle);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].result.as_deref().unwrap().contains("a.txt"));
}

#[tokio::test]
async fn turn_cap_ends_budget_exhausted_and_resume_continues() {
    let fx = fixture();
    // Every turn asks for a read, so the run would never stop on its own.
    let script: Vec<TurnResult> = (0..6)
        .map(|i| tools(vec![call(&format!("r{i}"), "list_dir", serde_json::json!({}))]))
        .collect();
    let provider = FakeProvider::new(script);
    let caps = Caps {
        max_turns: 3,
        max_prompt_tokens: 1_000_000,
    };
    let agent = fx.agent(caps, Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "list forever").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::BudgetExhausted);
    assert_eq!(fx.run_of(&handle).turn_count, 3);
    assert_eq!(provider.call_count(), 3);

    let resumed = agent.resume(handle.run_id()).await.unwrap();
    // Caps stay in force, so resuming lands in the same state again; what
    // matters is that the Run reopened and made more Turns.
    assert_eq!(wait_terminal(&fx, &resumed).await, RunState::BudgetExhausted);
    assert!(provider.call_count() > 3);
}

#[tokio::test]
async fn soft_cap_is_announced_at_seventy_percent() {
    let fx = fixture();
    let script: Vec<TurnResult> = (0..10)
        .map(|i| tools(vec![call(&format!("r{i}"), "list_dir", serde_json::json!({}))]))
        .collect();
    let provider = FakeProvider::new(script);
    let caps = Caps {
        max_turns: 10,
        max_prompt_tokens: 1_000_000,
    };
    let agent = fx.agent(caps, provider);
    let handle = agent.start(&fx.conversation_id, "go").await.unwrap();
    let mut events = handle.subscribe();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::BudgetExhausted);

    let mut soft = None;
    while let Ok(event) = events.try_recv() {
        if let RunEvent::SoftCap { turns, .. } = event {
            soft = Some(turns);
        }
    }
    let soft = soft.expect("no soft cap event");
    assert!((7..10).contains(&soft), "soft cap at {soft}");
}

#[tokio::test]
async fn cancel_during_streaming_ends_cancelled() {
    // A provider that never finishes streaming.
    struct Endless(Mutex<bool>);
    impl Provider for Endless {
        fn stream_chat<'a>(
            &'a self,
            _m: &'a [ChatMessage],
            _t: &'a [ToolSpec],
            events: mpsc::UnboundedSender<ProviderEvent>,
        ) -> std::pin::Pin<
            std::boxed::Box<
                dyn std::future::Future<Output = aria_core::provider::Result<TurnResult>> + Send + 'a,
            >,
        > {
            Box::pin(async move {
                loop {
                    if *self.0.lock().unwrap() {
                        std::future::pending::<()>().await;
                    }
                    let _ = events.send(ProviderEvent::TextDelta("tick ".into()));
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        }
    }
    let fx = fixture();
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider: Arc::new(Endless(Mutex::new(true))),
            approval_policy: ApprovalPolicy::AlwaysAsk,
        },
    );
    let handle = agent.start(&fx.conversation_id, "talk forever").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    handle.cancel();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Cancelled);
}

#[tokio::test]
async fn cancel_while_parked_on_an_approval_ends_cancelled() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![tools(vec![call(
        "w1",
        "write_file",
        serde_json::json!({"path": "b.txt", "content": "b"}),
    )])]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "write b").await.unwrap();
    wait_awaiting(&fx, &handle).await;

    handle.cancel();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Cancelled);
    assert!(!fx.root().join("b.txt").exists());
}

#[tokio::test]
async fn provider_error_ends_errored_with_a_reason() {
    struct Failing;
    impl Provider for Failing {
        fn stream_chat<'a>(
            &'a self,
            _m: &'a [ChatMessage],
            _t: &'a [ToolSpec],
            _e: mpsc::UnboundedSender<ProviderEvent>,
        ) -> std::pin::Pin<
            std::boxed::Box<
                dyn std::future::Future<Output = aria_core::provider::Result<TurnResult>> + Send + 'a,
            >,
        > {
            Box::pin(async {
                Err(aria_core::provider::ProviderError::Status {
                    status: 500,
                    body: "boom".into(),
                })
            })
        }
    }
    let fx = fixture();
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider: Arc::new(Failing),
            approval_policy: ApprovalPolicy::AlwaysAsk,
        },
    );
    let handle = agent.start(&fx.conversation_id, "hi").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Errored);
    let run = fx.run_of(&handle);
    assert!(run.error.unwrap().contains("500"));
}

#[tokio::test]
async fn a_failing_tool_is_a_result_not_a_run_failure() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("r1", "read_file", serde_json::json!({"path": "missing.txt"}))]),
        text("It is missing. "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "read missing").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let call = fx.tool_calls(&handle).remove(0);
    assert!(call.is_error);
    assert_eq!(call.state, ToolCallState::Done);
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_by_the_tool() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("r1", "read_file", serde_json::json!({"path": "../../etc/passwd"}))]),
        text("Refused. "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "read passwd").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let call = fx.tool_calls(&handle).remove(0);
    let result = call.result.unwrap();
    assert!(call.is_error, "{result}");
    assert!(!result.contains("root:"), "leaked file contents: {result}");
}

#[tokio::test]
async fn starting_while_a_run_is_active_is_refused() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![tools(vec![call(
        "w1",
        "write_file",
        serde_json::json!({"path": "c.txt", "content": "c"}),
    )])]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "one").await.unwrap();
    wait_awaiting(&fx, &handle).await;
    assert!(agent.start(&fx.conversation_id, "two").await.is_err());
    handle.cancel();
    wait_terminal(&fx, &handle).await;
}

#[tokio::test]
async fn transcript_turns_carry_their_tool_calls_and_results() {
    let fx = fixture();
    std::fs::write(fx.root().join("f.txt"), "data").unwrap();
    let provider = FakeProvider::new(vec![
        tools(vec![call("r1", "read_file", serde_json::json!({"path": "f.txt"}))]),
        text("ok "),
    ]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "read f").await.unwrap();
    wait_terminal(&fx, &handle).await;

    let sent = provider.last_messages();
    // system, user, assistant(with tool_calls), tool(result)
    assert_eq!(sent.len(), 4, "{sent:?}");
    match &sent[2] {
        ChatMessage::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name(), "read_file");
        }
        other => panic!("expected assistant with tool calls, got {other:?}"),
    }
    match &sent[3] {
        ChatMessage::Tool { content, .. } => assert!(content.contains("data")),
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn shell_tool_results_are_fed_back_to_the_model() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("s1", "run_shell", serde_json::json!({"cmd": "echo from-shell"}))]),
        text("Ran it. "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "run echo").await.unwrap();
    wait_awaiting(&fx, &handle).await;
    let id = pending(&fx, &handle).pop().unwrap();
    agent.answer_approval(&id, Decision::Approve).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);

    let result = fx.tool_calls(&handle).remove(0).result.unwrap();
    assert!(result.contains("from-shell"), "{result}");
}

#[tokio::test]
async fn memories_work_without_an_embedder() {
    let fx = fixture();
    // FakeProvider has no embedder, so this is the FTS5-only path.
    let provider = FakeProvider::new(vec![
        tools(vec![
            call(
                "m1",
                "remember",
                serde_json::json!({"key": "editor", "content": "prefers helix"}),
            ),
            call("m2", "recall", serde_json::json!({"query": "helix"})),
        ]),
        text("recalled "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "remember and recall").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);

    let calls = fx.tool_calls(&handle);
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].is_error, "{:?}", calls[0].result);
    let recalled = calls[1].result.as_deref().unwrap_or_default();
    assert!(recalled.contains("prefers helix"), "{recalled}");
}

#[tokio::test]
async fn prompts_carry_house_facts_and_creative_only_in_creative_mode() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![text("one"), text("two")]);
    let agent = fx
        .agent(Caps::default(), Arc::clone(&provider))
        .with_config(Config::new(fx._blob.path().join("config")));

    let handle = agent.start(&fx.conversation_id, "hi").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let normal = system_prompt(&provider.last_messages());
    assert!(normal.contains("About Aria"), "{normal}");
    assert!(normal.contains("settings.json"), "{normal}");
    assert!(!normal.contains("You are in Creative mode"), "{normal}");

    fx.db
        .call(|c| db::set_conversation_mode(c, &fx.conversation_id, Mode::Creative))
        .unwrap();
    let handle = agent.start(&fx.conversation_id, "make a tool").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let creative = system_prompt(&provider.last_messages());
    assert!(creative.contains("You are in Creative mode"), "{creative}");
}

fn system_prompt(messages: &[ChatMessage]) -> String {
    match &messages[0] {
        ChatMessage::System { content } => content.clone(),
        other => panic!("expected a system prompt first, got {other:?}"),
    }
}

#[tokio::test]
async fn workspace_auto_approve_safe_runs_mutate_without_parking() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![
            call("e1", "edit_file", serde_json::json!({"path": "f.txt", "search": "original", "replace": "edited"})),
        ]),
        text("done "),
    ]);
    std::fs::write(fx.root().join("f.txt"), "original").unwrap();
    let settings = Settings {
        approval_policy: ApprovalPolicy::AutoApproveSafe,
        model: "test-model".to_string(),
        ..Settings::default()
    };
    let agent = fx.agent_with_settings(Caps::default(), settings, Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "edit f.txt").await.unwrap();

    // The Mutate edit must run to completion without parking on an approval.
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(std::fs::read_to_string(fx.root().join("f.txt")).unwrap(), "edited");

    // A run_shell (Exec) call must still park under AutoApproveSafe.
    let provider2 = FakeProvider::new(vec![
        tools(vec![call("s1", "run_shell", serde_json::json!({"cmd": "echo hi"}))]),
        text("ran "),
    ]);
    let settings2 = Settings {
        approval_policy: ApprovalPolicy::AutoApproveSafe,
        model: "test-model".to_string(),
        ..Settings::default()
    };
    let agent2 = fx.agent_with_settings(Caps::default(), settings2, Arc::clone(&provider2));
    let handle2 = agent2.start(&fx.conversation_id, "run a command").await.unwrap();
    wait_awaiting(&fx, &handle2).await;
    assert!(fx.tool_calls(&handle2)[0].state == ToolCallState::Pending);
}

#[tokio::test]
async fn main_system_shell_approval_is_adaptive() {
    let fx = fixture();
    let conversation_id = fx
        .db
        .call(|c| db::insert_conversation(c, None, "main"))
        .unwrap()
        .id;
    fx.db
        .call(|c| db::set_conversation_scope(c, &conversation_id, true, &[]))
        .unwrap();

    // A single read-only command runs without parking.
    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "s1",
            "run_shell",
            serde_json::json!({"cmd": "echo safe-output"}),
        )]),
        text("ran it "),
    ]);
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: ApprovalPolicy::AutoApproveSafe,
        },
    );
    let handle = agent.start(&conversation_id, "run a read-only command").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let stored = fx
        .db
        .call(|c| db::list_tool_calls(c, handle.run_id()))
        .unwrap()
        .remove(0);
    assert_eq!(stored.state, ToolCallState::Done);
    assert!(stored.result.unwrap().contains("safe-output"));

    // A destructive command still parks.
    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "s2",
            "run_shell",
            serde_json::json!({"cmd": "rm -rf /tmp/aria-never-created"}),
        )]),
        text("asked "),
    ]);
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: ApprovalPolicy::AutoApproveSafe,
        },
    );
    let handle = agent.start(&conversation_id, "delete something").await.unwrap();
    wait_awaiting(&fx, &handle).await;
    assert_eq!(fx.tool_calls(&handle)[0].state, ToolCallState::Pending);
}

#[tokio::test]
async fn caps_are_hot_reloaded_between_turns() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());
    let initial = Settings {
        model: "test-model".to_string(),
        caps: Caps {
            max_turns: 50,
            ..Caps::default()
        },
        ..Settings::default()
    };
    config.save(&initial).unwrap();
    let mut lowered = initial.clone();
    lowered.caps.max_turns = 1;
    let write_settings = config.clone();

    let fake = Arc::new(FakeProvider {
        calls: Mutex::new(Vec::new()),
        script: Mutex::new(vec![Box::new(move |_: &[ChatMessage]| {
            // The user saves a tighter cap while the Run is live.
            write_settings.save(&lowered).unwrap();
            tools(vec![call("l1", "list_dir", serde_json::json!({"path": "."}))])
        })
            as Box<dyn Fn(&[ChatMessage]) -> TurnResult + Send + Sync>]),
    });
    let provider: Arc<dyn Provider> = fake.clone();
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: ApprovalPolicy::AlwaysAsk,
        },
    )
    .with_config(config);
    let handle = agent.start(&fx.conversation_id, "list").await.unwrap();

    // The next loop iteration must see max_turns = 1 and stop before Turn 2.
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::BudgetExhausted);
    assert_eq!(fake.call_count(), 1);
}

#[tokio::test]
async fn main_system_outside_path_parks_then_reads_after_approval() {
    let fx = fixture();
    let allowed = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret-data").unwrap();
    let conversation_id = fx
        .db
        .call(|c| db::insert_conversation(c, None, "main"))
        .unwrap()
        .id;
    fx.db
        .call(|c| {
            db::set_conversation_scope(
                c,
                &conversation_id,
                true,
                &[allowed.path().display().to_string()],
            )
        })
        .unwrap();

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "r1",
            "read_file",
            serde_json::json!({ "path": outside.path().join("secret.txt").display().to_string() }),
        )]),
        text("read it "),
    ]);
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: ApprovalPolicy::AutoApproveSafe,
        },
    );
    let handle = agent.start(&conversation_id, "read the secret").await.unwrap();

    // Outside the allowed root, even a Read must ask.
    wait_awaiting(&fx, &handle).await;
    let id = pending(&fx, &handle).pop().unwrap();
    agent.answer_approval(&id, Decision::Approve).unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let call = fx
        .db
        .call(|c| db::list_tool_calls(c, handle.run_id()))
        .unwrap()
        .remove(0);
    assert_eq!(call.state, ToolCallState::Done);
    assert!(!call.is_error, "{:?}", call.result);
    assert!(call.result.unwrap().contains("secret-data"));
}

#[tokio::test]
async fn main_system_inside_allowed_path_runs_without_parking() {
    let fx = fixture();
    let allowed = tempfile::tempdir().unwrap();
    std::fs::write(allowed.path().join("keep.txt"), "kept").unwrap();
    let conversation_id = fx
        .db
        .call(|c| db::insert_conversation(c, None, "main"))
        .unwrap()
        .id;
    fx.db
        .call(|c| {
            db::set_conversation_scope(
                c,
                &conversation_id,
                true,
                &[allowed.path().display().to_string()],
            )
        })
        .unwrap();

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "r1",
            "read_file",
            serde_json::json!({ "path": allowed.path().join("keep.txt").display().to_string() }),
        )]),
        text("read it "),
    ]);
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: ApprovalPolicy::AutoApproveSafe,
        },
    );
    let handle = agent.start(&conversation_id, "read the file").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let call = fx
        .db
        .call(|c| db::list_tool_calls(c, handle.run_id()))
        .unwrap()
        .remove(0);
    assert_eq!(call.state, ToolCallState::Done);
    assert!(!call.is_error, "{:?}", call.result);
    assert!(call.result.unwrap().contains("kept"));
}

/// Poll until a specific tool call of this Run is parked on an approval.
async fn wait_pending_tool(fx: &Fx, handle: &RunHandle, tool: &str) -> String {
    for _ in 0..600 {
        for c in fx.tool_calls(handle) {
            if c.tool == tool && c.state == ToolCallState::Pending
                && let Some(a) = fx
                    .db
                    .call(|conn| db::approval_for_tool_call(conn, &c.id))
                    .unwrap()
                && a.decision.is_none()
            {
                return a.id;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "{tool} never parked; run {:?}, calls {:?}",
        fx.run_of(handle).state,
        fx.tool_calls(handle)
            .into_iter()
            .map(|c| (c.tool, c.state))
            .collect::<Vec<_>>()
    );
}

fn main_system_agent(
    fx: &Fx,
    provider: Arc<FakeProvider>,
    policy: ApprovalPolicy,
) -> Agent {
    Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider,
            approval_policy: policy,
        },
    )
}

fn main_system(fx: &Fx, roots: &[&std::path::Path]) -> String {
    let conversation_id = fx
        .db
        .call(|c| db::insert_conversation(c, None, "main"))
        .unwrap()
        .id;
    let paths: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
    fx.db
        .call(|c| db::set_conversation_scope(c, &conversation_id, true, &paths))
        .unwrap();
    conversation_id
}

/// The policy contract: "Only risky" silently reads everywhere, but mutations
/// and egress reach the whole device in the Main System and must ask.
#[tokio::test]
async fn main_system_auto_approve_still_asks_for_mutations_and_network() {
    let fx = fixture();
    let conversation_id = main_system(&fx, &[]);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "before").unwrap();

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "e1",
            "edit_file",
            serde_json::json!({"path": file.display().to_string(), "search": "before", "replace": "after"}),
        )]),
        text("edited "),
    ]);
    let agent = main_system_agent(
        &fx,
        provider,
        ApprovalPolicy::AutoApproveSafe,
    );
    let handle = agent.start(&conversation_id, "edit the file").await.unwrap();
    let id = wait_pending_tool(&fx, &handle, "edit_file").await;
    agent.answer_approval(&id, Decision::Approve).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "after",
        "{:?}",
        fx.tool_calls(&handle)
    );

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "w1",
            "web_fetch",
            serde_json::json!({"url": "https://example.com"}),
        )]),
        text("fetched "),
    ]);
    let agent = main_system_agent(
        &fx,
        provider,
        ApprovalPolicy::AutoApproveSafe,
    );
    let handle = agent.start(&conversation_id, "fetch a page").await.unwrap();
    let id = wait_pending_tool(&fx, &handle, "web_fetch").await;
    agent.answer_approval(&id, Decision::Deny).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let call = fx.tool_calls(&handle).remove(0);
    assert_eq!(call.state, ToolCallState::Denied);
}

/// A restricted Main System asks for shell even when the command looks
/// read-only: shell cannot be confined to the allowed roots.
#[tokio::test]
async fn main_system_restricted_roots_ask_for_safe_shell() {
    let fx = fixture();
    let allowed = tempfile::tempdir().unwrap();
    let conversation_id = main_system(&fx, &[allowed.path()]);

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "s1",
            "run_shell",
            serde_json::json!({"cmd": "echo hi"}),
        )]),
        text("done "),
    ]);
    let agent = main_system_agent(
        &fx,
        provider,
        ApprovalPolicy::AutoApproveSafe,
    );
    let handle = agent.start(&conversation_id, "run echo").await.unwrap();
    let id = wait_pending_tool(&fx, &handle, "run_shell").await;
    agent.answer_approval(&id, Decision::Deny).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
}

/// Changing this chat's override while a Run is live applies from the next
/// tool call, not just the next Run.
#[tokio::test]
async fn conversation_policy_change_applies_mid_run() {
    let fx = fixture();
    std::fs::write(fx.root().join("f.txt"), "original").unwrap();
    fx.db
        .call(|c| {
            db::set_conversation_approval(
                c,
                &fx.conversation_id,
                Some(ApprovalPolicy::AutoApproveSafe),
            )
        })
        .unwrap();

    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "s1",
            "run_shell",
            serde_json::json!({"cmd": "echo hi"}),
        )]),
        tools(vec![call(
            "e1",
            "edit_file",
            serde_json::json!({"path": "f.txt", "search": "original", "replace": "edited"}),
        )]),
        text("done "),
    ]);
    let agent = fx.agent(Caps::default(), provider);
    let handle = agent.start(&fx.conversation_id, "run then edit").await.unwrap();

    // Workspace shell always asks; use it as the parking point.
    let id = wait_pending_tool(&fx, &handle, "run_shell").await;
    fx.db
        .call(|c| {
            db::set_conversation_approval(c, &fx.conversation_id, Some(ApprovalPolicy::AlwaysAsk))
        })
        .unwrap();
    agent.answer_approval(&id, Decision::Approve).unwrap();

    // Under the old captured policy this edit would run silently.
    let id = wait_pending_tool(&fx, &handle, "edit_file").await;
    agent.answer_approval(&id, Decision::Approve).unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(std::fs::read_to_string(fx.root().join("f.txt")).unwrap(), "edited");
}

#[tokio::test]
async fn ids_seen_by_the_model_match_persisted_calls() {
    // The provider's tool_call ids are replaced by row ids, which is what the
    // Tool Result must reference.
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("provider-id", "list_dir", serde_json::json!({"path": "."}))]),
        text("ok "),
    ]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "list").await.unwrap();
    wait_terminal(&fx, &handle).await;

    let stored = fx.tool_calls(&handle).remove(0);
    let sent = provider.last_messages();
    let assistant_id = sent
        .iter()
        .find_map(|m| match m {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.first().map(|c| c.id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(assistant_id, stored.id);
}

#[tokio::test]
async fn disabled_shell_tool_is_gated_and_returns_error_outcome() {
    let fx = fixture();
    let provider = FakeProvider::new(vec![
        tools(vec![call("s1", "run_shell", serde_json::json!({"cmd": "echo disabled"}))]),
        text("handled disabled shell "),
    ]);
    let agent = fx
        .agent(Caps::default(), provider)
        .with_disabled_tools(vec!["run_shell".to_string()]);
    let handle = agent.start(&fx.conversation_id, "run echo").await.unwrap();

    wait_awaiting(&fx, &handle).await;
    let id = pending(&fx, &handle).pop().unwrap();
    agent.answer_approval(&id, Decision::Approve).unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);

    let call = fx.tool_calls(&handle).remove(0);
    assert_eq!(call.state, ToolCallState::Done);
    assert!(call.is_error);
    let result = call.result.unwrap();
    assert!(result.contains("tool `run_shell` is disabled in Settings"), "{result}");
}

// ------------------------------------------------------------ orchestration

/// A provider that fails a few times before answering, to exercise retries.
struct FlakyProvider {
    calls: Mutex<usize>,
    fail_first: usize,
}

impl Provider for FlakyProvider {
    fn stream_chat<'a>(
        &'a self,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSpec],
        _events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> std::pin::Pin<
        std::boxed::Box<
            dyn std::future::Future<Output = aria_core::provider::Result<TurnResult>> + Send + 'a,
        >,
    > {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let attempt = *calls;
        let fail_first = self.fail_first;
        Box::pin(async move {
            if attempt <= fail_first {
                return Err(aria_core::provider::ProviderError::Status {
                    status: 503,
                    body: "busy".into(),
                });
            }
            Ok(text("recovered "))
        })
    }
}

#[tokio::test]
async fn transient_provider_errors_are_retried() {
    let fx = fixture();
    let provider = Arc::new(FlakyProvider {
        calls: Mutex::new(0),
        fail_first: 2,
    });
    let agent = Agent::new(
        Arc::clone(&fx.db),
        Arc::new(Blobs::new(fx._blob.path(), Arc::clone(&fx.db))),
        Caps::default(),
        ProviderSource::Fixed {
            provider: Arc::clone(&provider) as Arc<dyn Provider>,
            approval_policy: ApprovalPolicy::AlwaysAsk,
        },
    );
    let handle = agent.start(&fx.conversation_id, "hello").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    assert_eq!(*provider.calls.lock().unwrap(), 3);
    assert!(fx.messages().last().unwrap().content.contains("recovered"));
}

/// Two independent reads run in the same Turn and both land.
#[tokio::test]
async fn independent_reads_in_one_turn_all_complete() {
    let fx = fixture();
    std::fs::write(fx.root().join("a.txt"), "alpha").unwrap();
    std::fs::write(fx.root().join("b.txt"), "beta").unwrap();
    let provider = FakeProvider::new(vec![
        tools(vec![
            call("a", "read_file", serde_json::json!({"path": "a.txt"})),
            call("b", "read_file", serde_json::json!({"path": "b.txt"})),
        ]),
        text("both read "),
    ]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "read both").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let calls = fx.tool_calls(&handle);
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.state == ToolCallState::Done));
    let results = calls
        .iter()
        .map(|c| c.result.clone().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(results.contains("alpha") && results.contains("beta"), "{results}");
}

/// `delegate` runs a read-only sub-agent and returns its report as one Tool
/// Result; the sub-agent's own Turns never enter the transcript.
#[tokio::test]
async fn delegate_returns_a_report_without_touching_the_transcript() {
    let fx = fixture();
    std::fs::write(fx.root().join("note.txt"), "the secret is 42").unwrap();
    let provider = FakeProvider::new(vec![
        tools(vec![call(
            "d1",
            "delegate",
            serde_json::json!({"task": "what is in note.txt?"}),
        )]),
        // The sub-agent reads the file, then answers.
        tools(vec![call("r1", "read_file", serde_json::json!({"path": "note.txt"}))]),
        text("The note says 42. "),
        // Back in the parent Run.
        text("Delegation reported 42. "),
    ]);
    let settings = Settings {
        approval_policy: ApprovalPolicy::AutoApproveSafe,
        model: "test-model".to_string(),
        ..Settings::default()
    };
    let agent = fx.agent_with_settings(Caps::default(), settings, Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "find the secret").await.unwrap();

    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);
    let calls = fx.tool_calls(&handle);
    assert_eq!(calls.len(), 1, "sub-agent calls stay out of the transcript");
    assert_eq!(calls[0].tool, "delegate");
    let report = calls[0].result.clone().unwrap();
    assert!(report.contains("42"), "{report}");
    assert!(fx.messages().last().unwrap().content.contains("Delegation reported"));
}

/// A conversation longer than the context budget is trimmed at Turn
/// boundaries, so the provider never receives the whole history.
#[tokio::test]
async fn long_history_is_trimmed_to_fit_the_context() {
    let fx = fixture();
    for i in 0..40 {
        fx.db
            .call(|c| {
                db::insert_message(
                    c,
                    &fx.conversation_id,
                    None,
                    Role::User,
                    &format!("{i} {}", "u".repeat(20_000)),
                )
            })
            .unwrap();
        fx.db
            .call(|c| {
                db::insert_message(
                    c,
                    &fx.conversation_id,
                    None,
                    Role::Assistant,
                    &format!("{i} {}", "a".repeat(20_000)),
                )
            })
            .unwrap();
    }
    let provider = FakeProvider::new(vec![text("ok ")]);
    let agent = fx.agent(Caps::default(), Arc::clone(&provider));
    let handle = agent.start(&fx.conversation_id, "hi").await.unwrap();
    assert_eq!(wait_terminal(&fx, &handle).await, RunState::Finished);

    let sent = provider.last_messages();
    let chars: usize = sent
        .iter()
        .map(|m| match m {
            ChatMessage::System { content }
            | ChatMessage::User { content, .. }
            | ChatMessage::Assistant { content, .. }
            | ChatMessage::Tool { content, .. } => content.len(),
        })
        .sum();
    assert!(chars <= 400_000, "sent {chars} chars");
    assert!(chars > 0);
}

//! End-to-end acceptance for the four scenarios in `spec.md`.
//!
//! Everything real is real: SQLite on disk in WAL mode, the Tool layer, the
//! shell, the blob store, the FTS index and the Run loop. The only stand-in is
//! the Provider, which is a scripted OpenAI-compatible endpoint served over
//! loopback HTTP, so the SSE client and the wire format are exercised too.
//!
//! A real key and model are not required for these checks; run 13 also lists
//! the manual pass against a live Provider.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aria_core::agent::{Agent, ProviderSource, RunHandle};
use aria_core::config::{ApprovalPolicy, Caps};
use aria_core::db::{self, Db, Decision, RunState, ToolCallState};
use aria_core::journal::Blobs;
use aria_core::provider::{
    ChatMessage, OpenAiProvider, Provider, ProviderEvent, ToolCallRequest, ToolSpec, TurnResult,
};
use aria_core::workspace::WorkspaceRoot;
use serde_json::json;

// --------------------------------------------------- OpenAI-compatible stub

/// One scripted model reply. `content_before` streams as text, then the tool
/// calls, mirroring the shape a real provider sends.
#[derive(Clone, Default)]
struct Reply {
    text: String,
    tool_calls: Vec<ToolCallRequest>,
}

impl Reply {
    fn say(text: impl Into<String>) -> Self {
        Reply {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> Self {
        Reply {
            text: String::new(),
            tool_calls: vec![ToolCallRequest::new(id, name, args.to_string())],
        }
    }
}

/// A loopback HTTP server that answers `/chat/completions` with SSE built from
/// the queued replies. Requests are recorded so a test can inspect what the
/// model was sent.
struct Stub {
    base_url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Stub {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let queue = Arc::new(Mutex::new(replies));

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the whole request: headers, then exactly Content-Length
                // bytes of body.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(split) = text.find("\r\n\r\n") {
                        let body_at = text[..split].len() + 4;
                        let want = text[..split]
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= body_at + want {
                            break;
                        }
                    }
                }
                let full = String::from_utf8_lossy(&buf).to_string();
                let body = full.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    seen.lock().unwrap().push(v);
                }
                let reply = {
                    let mut q = queue.lock().unwrap();
                    if q.is_empty() {
                        Reply::say("done")
                    } else {
                        q.remove(0)
                    }
                };

                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                for chunk in reply.text.split_inclusive(' ') {
                    let frame = json!({
                        "choices": [{"index": 0, "delta": {"content": chunk}}]
                    });
                    let _ = write!(stream, "data: {frame}\n\n");
                }
                for (i, call) in reply.tool_calls.iter().enumerate() {
                    let frame = json!({
                        "choices": [{"index": 0, "delta": {"tool_calls": [{
                            "index": i,
                            "id": call.id,
                            "function": {"name": call.name(), "arguments": call.arguments()}
                        }]}}]
                    });
                    let _ = write!(stream, "data: {frame}\n\n");
                }
                let usage = json!({
                    "choices": [],
                    "usage": {"prompt_tokens": 42, "completion_tokens": 1}
                });
                let _ = write!(stream, "data: {usage}\n\n");
                let _ = write!(stream, "data: [DONE]\n\n");
                let _ = stream.flush();
            }
        });

        Stub {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
        }
    }

    /// The Provider the app would build from settings, pointed at the stub.
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(
            OpenAiProvider::new(
                aria_core::config::Settings {
                    base_url: self.base_url.clone(),
                    model: "stub-model".into(),
                    ..Default::default()
                },
                "test-key",
            )
            .unwrap(),
        )
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

// --------------------------------------------------------------- a toy repo

struct Repo {
    dir: tempfile::TempDir,
    data: tempfile::TempDir,
    db: Arc<Db>,
    blobs: Arc<Blobs>,
    workspace_id: String,
    conversation_id: String,
}

impl Repo {
    /// A throwaway project with a real off-by-one bug and a failing test.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"pub struct Item {
    pub qty: u64,
}

pub fn total(items: &[Item]) -> u64 {
    let mut sum = 0;
    for i in 1..items.len() {
        sum += items[i].qty;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_every_item() {
        let items = [Item { qty: 2 }, Item { qty: 3 }, Item { qty: 5 }];
        assert_eq!(total(&items), 10);
    }
}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"toy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        // Real on-disk database (WAL) in an isolated data dir.
        let db = Arc::new(Db::open(&data.path().join("aria.db")).unwrap());
        let root = dir.path().display().to_string();
        let workspace_id = db.call(|c| db::insert_workspace(c, &root)).unwrap().id;
        let conversation_id = db
            .call(|c| db::insert_conversation(c, Some(&workspace_id), "toy"))
            .unwrap()
            .id;
        let blobs = Arc::new(Blobs::new(data.path().join("blobs"), Arc::clone(&db)));
        Repo {
            dir,
            data,
            db,
            blobs,
            workspace_id,
            conversation_id,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn agent(&self, stub: &Stub, caps: Caps) -> Agent {
        Agent::new(
            Arc::clone(&self.db),
            Arc::clone(&self.blobs),
            caps,
            ProviderSource::Fixed {
                provider: stub.provider(),
                approval_policy: ApprovalPolicy::AlwaysAsk,
            },
        )
    }

    fn source(&self) -> String {
        std::fs::read_to_string(self.root().join("src/lib.rs")).unwrap()
    }

    fn calls(&self, handle: &RunHandle) -> Vec<db::ToolCall> {
        self.db
            .call(|c| db::list_tool_calls(c, handle.run_id()))
            .unwrap()
    }

    fn pending(&self, handle: &RunHandle) -> Vec<String> {
        self.calls(handle)
            .into_iter()
            .filter(|c| c.state == ToolCallState::Pending)
            .filter_map(|c| {
                self.db
                    .call(|conn| db::approval_for_tool_call(conn, &c.id))
                    .unwrap()
                    .filter(|a| a.decision.is_none())
                    .map(|a| a.id)
            })
            .collect()
    }
}

async fn wait_state(repo: &Repo, handle: &RunHandle, want: RunState) -> bool {
    for _ in 0..1200 {
        let run = repo.db.call(|c| db::get_run(c, handle.run_id())).unwrap();
        if run.state == want {
            return true;
        }
        if run.state.is_terminal() && run.state != want {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Approve (or deny) every Approval as it appears until the Run ends.
async fn settle(repo: &Repo, agent: &Agent, handle: &RunHandle, decision: Decision) -> RunState {
    for _ in 0..3000 {
        let run = repo.db.call(|c| db::get_run(c, handle.run_id())).unwrap();
        if run.state.is_terminal() {
            return run.state;
        }
        for id in repo.pending(handle) {
            agent.answer_approval(&id, decision).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run did not end");
}

// ------------------------------------------------------------- scenario one

/// spec.md: "the agent finds it (search_files), edits it (edit_file), runs the
/// tests (run_shell), each with an Approval, and the change can be undone."
#[tokio::test]
async fn fix_a_bug_then_undo_it() {
    let repo = Repo::new();
    let stub = Stub::start(vec![
        Reply::call(
            "find",
            "search_files",
            json!({"pattern": "for i in", "glob": "**/*.rs"}),
        ),
        Reply::call(
            "fix",
            "edit_file",
            json!({
                "path": "src/lib.rs",
                "search": "for i in 1..items.len() {",
                "replace": "for i in 0..items.len() {"
            }),
        ),
        Reply::call("test", "run_shell", json!({"cmd": "cargo test --quiet 2>&1"})),
        Reply::say("Fixed the off-by-one and the test passes. "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent
        .start(&repo.conversation_id, "the totals are wrong, fix the bug")
        .await
        .unwrap();

    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    let calls = repo.calls(&handle);
    let names: Vec<&str> = calls.iter().map(|c| c.tool.as_str()).collect();
    assert_eq!(names, ["search_files", "edit_file", "run_shell"]);
    assert!(calls[0].result.as_deref().unwrap().contains("for i in 1..items.len()"));
    assert!(calls[1].result.as_deref().unwrap().contains("1 replacement"));

    // The edit really happened and the tests really pass.
    assert!(repo.source().contains("for i in 0..items.len()"));
    let test_out = calls[2].result.as_deref().unwrap();
    assert!(
        test_out.contains("test result: ok") && !test_out.contains("FAILED"),
        "tests did not pass:\n{test_out}"
    );

    // A mutate call must have been gated on an Approval.
    assert_eq!(calls[1].approval_class, db::ApprovalClass::Mutate);
    assert!(
        repo.db
            .call(|c| db::approval_for_tool_call(c, &calls[1].id))
            .unwrap()
            .unwrap()
            .decided_at
            .is_some()
    );

    // Undo restores the file byte for byte.
    let before = "for i in 1..items.len() {";
    let root = WorkspaceRoot::new(repo.root()).unwrap();
    assert!(repo.blobs.can_undo(&repo.workspace_id, "src/lib.rs").unwrap());
    repo.blobs
        .undo(&repo.workspace_id, &root, "src/lib.rs")
        .unwrap();
    assert!(repo.source().contains(before), "undo did not restore the bug");
    assert!(!repo.blobs.can_undo(&repo.workspace_id, "src/lib.rs").unwrap());

    // The stub saw a proper OpenAI request: system prompt, user turn, tools.
    let first = &stub.requests()[0];
    assert_eq!(first["model"], "stub-model");
    assert_eq!(first["stream"], true);
    assert!(first["tools"].as_array().unwrap().len() >= 9);
    let roles: Vec<&str> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user"]);
}

#[tokio::test]
async fn the_model_sees_tool_results_in_openai_shape() {
    let repo = Repo::new();
    let stub = Stub::start(vec![
        Reply::call("a", "list_dir", json!({"path": "."})),
        Reply::say("done "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent.start(&repo.conversation_id, "list files").await.unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    let second = &stub.requests()[1];
    let messages = second["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn");
    assert_eq!(assistant["tool_calls"][0]["type"], "function");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "list_dir");
    let call_id = assistant["tool_calls"][0]["id"].as_str().unwrap();
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result");
    assert_eq!(result["tool_call_id"], call_id);
    assert!(result["content"].as_str().unwrap().contains("src"));
}

// ------------------------------------------------------------- scenario two

/// spec.md: "A request touching `../../etc` is refused by the Tool, not the
/// model."
#[tokio::test]
async fn path_escape_is_refused_by_the_tool() {
    let repo = Repo::new();
    let stub = Stub::start(vec![
        Reply::call("escape", "read_file", json!({"path": "../../etc/passwd"})),
        Reply::call(
            "escape2",
            "write_file",
            json!({"path": "../../../tmp/aria-escape.txt", "content": "x"}),
        ),
        Reply::say("Both were refused. "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent
        .start(&repo.conversation_id, "read ../../etc/passwd")
        .await
        .unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    let calls = repo.calls(&handle);
    let read = calls[0].result.as_deref().unwrap();
    assert!(calls[0].is_error, "{read}");
    assert!(!read.contains("root:"), "file contents leaked: {read}");

    // The write was approved by the user but still refused by the Tool.
    let write = calls[1].result.as_deref().unwrap();
    assert!(calls[1].is_error, "{write}");
    assert!(!Path::new("/tmp/aria-escape.txt").exists());
}

#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_refused() {
    let repo = Repo::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.root().join("escape-link")).unwrap();

    let stub = Stub::start(vec![
        Reply::call("s", "read_file", json!({"path": "escape-link/secret.txt"})),
        Reply::say("refused "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent.start(&repo.conversation_id, "read it").await.unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );
    let out = repo.calls(&handle)[0].result.clone().unwrap();
    assert!(!out.contains("top secret"), "symlink escaped: {out}");
}

// ----------------------------------------------------------- scenario three

/// spec.md: "Closing and reopening the UI mid-Run shows the same Run and lets
/// it complete."
#[tokio::test]
async fn reopening_mid_run_shows_the_same_run_and_completes() {
    let repo = Repo::new();
    let stub = Stub::start(vec![
        Reply::call(
            "fix",
            "edit_file",
            json!({
                "path": "src/lib.rs",
                "search": "for i in 1..items.len() {",
                "replace": "for i in 0..items.len() {"
            }),
        ),
        Reply::say("Fixed. "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent
        .start(&repo.conversation_id, "fix the off-by-one")
        .await
        .unwrap();
    assert!(wait_state(&repo, &handle, RunState::AwaitingApproval).await);

    // "Close the window": drop the UI's handle and reconnect like a new
    // window would, with a fresh Agent. State comes only from the database.
    let run_id = handle.run_id().to_string();
    drop(handle);
    let reopened = Agent::new(
        Arc::clone(&repo.db),
        Arc::clone(&repo.blobs),
        Caps::default(),
        ProviderSource::Fixed {
            provider: stub.provider(),
            approval_policy: ApprovalPolicy::AlwaysAsk,
        },
    );
    let snapshot = reopened.snapshot(&run_id).unwrap();
    assert_eq!(snapshot.run.state, RunState::AwaitingApproval);
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.pending[0].summary.title, "Edit src/lib.rs");

    // The interrupted Run is adopted, and the pending Approval is answered.
    let handle = reopened.resume_interrupted(&run_id).await.unwrap();
    assert_eq!(handle.run_id(), run_id);
    let mut events = handle.subscribe();
    let approval = repo.pending(&handle).pop().unwrap();
    reopened.answer_approval(&approval, Decision::Approve).unwrap();

    for _ in 0..1000 {
        let run = repo.db.call(|c| db::get_run(c, &run_id)).unwrap();
        if run.state.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let run = repo.db.call(|c| db::get_run(c, &run_id)).unwrap();
    assert_eq!(run.state, RunState::Finished);
    assert!(repo.source().contains("for i in 0..items.len()"));

    // The reconnected window saw the live events of the very same Run.
    let mut saw_finish = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, aria_core::agent::RunEvent::State { ref run } if run.state == RunState::Finished)
        {
            saw_finish = true;
        }
    }
    assert!(saw_finish, "no terminal event after reconnecting");
    assert_eq!(run.conversation_id, repo.conversation_id);
}

// ------------------------------------------------------------ scenario four

/// spec.md: "Past message text is findable through FTS5 immediately after it
/// is stored."
#[tokio::test]
async fn new_message_text_is_searchable_immediately() {
    let repo = Repo::new();
    let stub = Stub::start(vec![Reply::say("The xyzzy sentinel is worth remembering. ")]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent
        .start(&repo.conversation_id, "remember the wodka marker")
        .await
        .unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    let hits = repo
        .db
        .call(|c| db::search_messages(c, "xyzzy", None, 10))
        .unwrap();
    assert_eq!(hits.len(), 1, "assistant text not indexed: {hits:?}");
    assert_eq!(hits[0].conversation_id, repo.conversation_id);
    assert!(hits[0].snippet.contains("xyzzy"));

    let user_hit = repo
        .db
        .call(|c| db::search_messages(c, "wodka", None, 10))
        .unwrap();
    assert_eq!(user_hit.len(), 1);

    // Scoped search and prefix typing behave.
    let scoped = repo
        .db
        .call(|c| db::search_messages(c, "xyzzy", Some(&repo.conversation_id), 10))
        .unwrap();
    assert_eq!(scoped.len(), 1);
    let partial = repo
        .db
        .call(|c| db::search_messages(c, "xyz", None, 10))
        .unwrap();
    // Token matching, not substring matching: "xyz" is not the indexed token.
    assert_eq!(partial.len(), 0, "a partial word must not match");
    // Half-typed FTS syntax is "still typing", not an error.
    let typing = repo
        .db
        .call(|c| db::search_messages(c, "\"unclosed", None, 10))
        .unwrap();
    assert!(typing.is_empty());
}

// ------------------------------------------------------- restart durability

/// The database is the transcript of record: a second connection to the same
/// file sees everything the first wrote.
#[tokio::test]
async fn a_second_process_sees_the_same_transcript() {
    let repo = Repo::new();
    let stub = Stub::start(vec![Reply::say("All done. ")]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent.start(&repo.conversation_id, "hello").await.unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    let db_path = repo.data.path().join("aria.db");
    drop(stub);
    let reopened = Db::open(&db_path).unwrap();
    let run = reopened
        .call(|c| db::get_run(c, handle.run_id()))
        .unwrap();
    assert_eq!(run.state, RunState::Finished);
    let messages = reopened
        .call(|c| db::list_messages(c, &repo.conversation_id))
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, db::Role::User);
    // And it is still searchable from the new connection.
    let hits = reopened
        .call(|c| db::search_messages(c, "hello", None, 5))
        .unwrap();
    assert_eq!(hits.len(), 1);
}

/// `ChatMessage` serialisation is what a real endpoint receives.
#[test]
fn chat_messages_serialise_like_openai() {
    let assistant = ChatMessage::assistant(
        "",
        vec![ToolCallRequest::new("c1", "read_file", "{\"path\":\"a\"}")],
    );
    let v = serde_json::to_value(&assistant).unwrap();
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["tool_calls"][0]["id"], "c1");
    let tool = serde_json::to_value(ChatMessage::tool_result("c1", "ok")).unwrap();
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "c1");
    let user = serde_json::to_value(ChatMessage::user("hi")).unwrap();
    assert!(user.get("tool_calls").is_none());
}

#[allow(dead_code)]
fn _types(_: ToolSpec, _: TurnResult, _: ProviderEvent, _: &dyn Provider) {}

// ------------------------------------------------------------------- memory

/// spec.md: durable facts persist across runs and survive in the database, and
/// recall returns them without needing an embedding-capable provider.
#[tokio::test]
async fn memory_is_stored_and_recalled_across_runs() {
    let repo = Repo::new();
    let stub = Stub::start(vec![
        Reply::call("c1", "remember", json!({"key": "user_name", "content": "Sam"})),
        Reply::say("Got it, I'll remember that. "),
        Reply::call("c2", "recall", json!({})),
        Reply::say("Your name is Sam. "),
    ]);
    let agent = repo.agent(&stub, Caps::default());
    let handle = agent.start(&repo.conversation_id, "my name is Sam").await.unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle, Decision::Approve).await,
        RunState::Finished
    );

    // The fact is durable: it lives in the database, scoped to the workspace.
    let stored = repo
        .db
        .call(|c| db::list_memories(c, Some(&repo.workspace_id)))
        .unwrap();
    assert!(stored.iter().any(|m| m.key == "user_name" && m.content == "Sam"));

    // A fresh run in the same conversation recalls it from the store.
    let handle2 = agent
        .start(&repo.conversation_id, "what's my name?")
        .await
        .unwrap();
    assert_eq!(
        settle(&repo, &agent, &handle2, Decision::Approve).await,
        RunState::Finished
    );
    let recalled = repo
        .db
        .call(|c| db::list_tool_calls(c, handle2.run_id()))
        .unwrap()
        .into_iter()
        .find(|c| c.tool == "recall")
        .expect("recall was not called")
        .result
        .expect("recall produced no result");
    assert!(recalled.contains("user_name"), "recall missed the memory: {recalled}");
    assert!(recalled.contains("Sam"), "recall missed the memory: {recalled}");
}

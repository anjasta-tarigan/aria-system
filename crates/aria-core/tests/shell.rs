use std::sync::Arc;
use std::time::Duration;

use aria_core::db::{self, Db};
use aria_core::journal::Blobs;
use aria_core::tools::{self, ToolContext, ToolRegistry};
use aria_core::workspace::WorkspaceRoot;
use tokio::sync::mpsc;

struct Fx {
    _ws: tempfile::TempDir,
    ctx: ToolContext,
}

fn fx() -> Fx {
    let ws = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws_id = db
        .call(|c| db::insert_workspace(c, &ws.path().display().to_string()))
        .unwrap()
        .id;
    let conv = db.call(|c| db::insert_conversation(c, Some(&ws_id), "")).unwrap();
    let run = db.call(|c| db::insert_run(c, &conv.id)).unwrap();
    // Leak the blob dir so it lives as long as the context.
    let blob_dir = Box::leak(Box::new(blob_dir));
    let ctx = ToolContext {
        root: Some(WorkspaceRoot::new(ws.path()).unwrap()),
        workspace_id: Some(ws_id),
        run_id: run.id,
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(blob_dir.path(), Arc::clone(&db))),
        shell_timeout_secs: 10,
        embed: None,
    };
    Fx { _ws: ws, ctx }
}

async fn shell(f: &Fx, cmd: &str) -> String {
    let out = ToolRegistry::execute(
        "run_shell",
        &serde_json::json!({"cmd": cmd}).to_string(),
        &f.ctx,
    )
    .await;
    assert!(!out.is_error, "tool error: {}", out.text);
    out.text
}

#[tokio::test]
async fn captures_output_and_exit_code() {
    let f = fx();
    let out = shell(&f, "echo hello; echo oops >&2; exit 3").await;
    assert!(out.contains("hello"), "{out}");
    assert!(out.contains("stderr:\noops"), "{out}");
    assert!(out.contains("exit: 3"), "{out}");
}

#[tokio::test]
async fn success_exit_is_reported_and_not_an_error() {
    let f = fx();
    let outcome = ToolRegistry::execute(
        "run_shell",
        &serde_json::json!({"cmd": "true"}).to_string(),
        &f.ctx,
    )
    .await;
    assert!(!outcome.is_error);
    assert!(outcome.text.contains("exit: 0"));
}

#[tokio::test]
async fn runs_in_workspace_root() {
    let f = fx();
    std::fs::write(f.ctx.root.as_ref().unwrap().root().join("marker.txt"), "x").unwrap();
    let out = shell(&f, "ls").await;
    assert!(out.contains("marker.txt"), "{out}");
}

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let f = fx();
    let started = std::time::Instant::now();
    let out = ToolRegistry::execute(
        "run_shell",
        &serde_json::json!({"cmd": "sleep 30", "timeout_secs": 1}).to_string(),
        &f.ctx,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(out.text.contains("timed out"), "{}", out.text);
}

#[tokio::test]
async fn large_output_is_truncated_head_and_tail() {
    let f = fx();
    let out = shell(
        &f,
        "seq 1 20000; echo TAIL-MARKER",
    )
    .await;
    assert!(out.contains("bytes omitted"), "output was not truncated");
    assert!(out.contains("TAIL-MARKER"));
    assert!(out.contains("\n1\n"), "head should be preserved");
    assert!(out.len() < 40_000);
}

#[tokio::test]
async fn progress_is_streamed() {
    let f = fx();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let args = aria_core::tools::ShellArgs {
        cmd: "echo one; echo two".into(),
        timeout_secs: None,
    };
    tools::run_shell(&f.ctx, args, Some(tx)).await.unwrap();
    let mut lines = Vec::new();
    while let Ok(l) = rx.try_recv() {
        lines.push(l);
    }
    assert!(lines.iter().any(|l| l.contains("one")));
    assert!(lines.iter().any(|l| l.contains("two")));
}

#[tokio::test]
async fn interactive_stdin_is_closed() {
    let f = fx();
    let out = shell(&f, "cat; echo after").await;
    assert!(out.contains("after"), "{out}");
    assert!(!out.contains("timed out"));
}

#[tokio::test]
async fn shell_is_an_exec_class_tool() {
    assert_eq!(
        tools::approval_class(tools::RUN_SHELL).unwrap(),
        aria_core::db::ApprovalClass::Exec
    );
    assert!(
        tools::approval_class(tools::RUN_SHELL).unwrap().requires_approval()
    );
}

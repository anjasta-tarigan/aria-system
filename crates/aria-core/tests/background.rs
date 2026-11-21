//! Background task lifecycle: start, poll to terminal, failure, and kill.

use std::sync::Arc;
use std::time::Duration;

use aria_core::db::{self, Db};
use aria_core::journal::Blobs;
use aria_core::tools::{BackgroundStatus, BackgroundTasks};
use aria_core::workspace::{Scope, WorkspaceRoot};

struct Fx {
    _ws: tempfile::TempDir,
    _blobs: tempfile::TempDir,
    bg: BackgroundTasks,
}

fn fx() -> Fx {
    let ws = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws_id = db
        .call(|c| db::insert_workspace(c, &ws.path().display().to_string()))
        .unwrap()
        .id;
    let _conv = db.call(|c| db::insert_conversation(c, Some(&ws_id), "")).unwrap();
    let root = WorkspaceRoot::new(ws.path()).unwrap();
    let bg = BackgroundTasks {
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(blobs.path(), Arc::clone(&db))),
        root: root.root().to_path_buf(),
        scope: Scope::Workspace(root),
        shell_timeout_secs: 120,
    };
    Fx {
        _ws: ws,
        _blobs: blobs,
        bg,
    }
}

async fn poll_with(bg: &BackgroundTasks, id: &str) -> (aria_core::db::BackgroundTask, String) {
    for _ in 0..200 {
        let (task, tail) = bg.poll(id).unwrap();
        if task.status != BackgroundStatus::Running {
            return (task, tail);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} did not finish in time");
}

/// Poll until the task reaches a terminal state or we time out.
async fn poll_until_done(fx: &Fx, id: &str) -> (aria_core::db::BackgroundTask, String) {
    for _ in 0..200 {
        let (task, tail) = fx.bg.poll(id).unwrap();
        if task.status != BackgroundStatus::Running {
            return (task, tail);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} did not finish in time");
}

#[tokio::test]
async fn success_task_completes_with_output() {
    let fx = fx();
    let conv = fx.bg.db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    let task = fx.bg.start(&conv.id, "echo hello && sleep 0").unwrap();
    assert_eq!(task.status, BackgroundStatus::Running);
    let (task, tail) = poll_until_done(&fx, &task.id).await;
    assert_eq!(task.status, BackgroundStatus::Succeeded);
    assert_eq!(task.exit_code, Some(0));
    assert!(tail.contains("hello"), "tail: {tail}");
}

#[tokio::test]
async fn failing_task_reports_failure_code() {
    let fx = fx();
    let conv = fx.bg.db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    let task = fx.bg.start(&conv.id, "exit 3").unwrap();
    let (task, _tail) = poll_until_done(&fx, &task.id).await;
    assert_eq!(task.status, BackgroundStatus::Failed);
    assert_eq!(task.exit_code, Some(3));
}

#[tokio::test]
async fn long_running_task_can_be_killed() {
    let fx = fx();
    let conv = fx.bg.db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    let task = fx.bg.start(&conv.id, "sleep 30").unwrap();
    // Wait until the pid file is written so the process group exists to kill.
    for _ in 0..200 {
        if !fx.bg.poll(&task.id).unwrap().0.status.is_terminal() {
            let pid = fx.bg.blobs.dir().join(format!("bg-{}.pid", task.id));
            if pid.exists() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let killed = fx.bg.kill(&task.id).await.unwrap();
    assert_eq!(killed.status, BackgroundStatus::Killed);
    // A second kill on a finished task is a no-op.
    let again = fx.bg.kill(&task.id).await.unwrap();
    assert_eq!(again.status, BackgroundStatus::Killed);
}

#[tokio::test]
async fn orphaned_running_task_is_reaped_on_startup() {
    let fx = fx();
    let conv = fx.bg.db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    // Simulate a task left `running` by a previous app instance.
    let task = aria_core::db::BackgroundTask {
        id: "bg-orphan".into(),
        cmd: "sleep 999".into(),
        conversation_id: conv.id.clone(),
        started_at: 1,
        ended_at: None,
        exit_code: None,
        status: aria_core::db::BackgroundStatus::Running,
        log_blob: String::new(),
    };
    fx.bg.db.call(|c| db::insert_background_task(c, &task)).unwrap();

    let ids = fx
        .bg
        .db
        .call(db::running_background_task_ids)
        .unwrap();
    assert!(ids.contains(&"bg-orphan".to_string()), "orphan should be listed");

    // The startup reaper marks it killed (preserving any partial log blob).
    fx.bg
        .db
        .call(|c| {
            db::update_background_task(
                c,
                "bg-orphan",
                db::now_ms(),
                None,
                aria_core::db::BackgroundStatus::Killed,
                "",
            )
        })
        .unwrap();
    let reaped = fx.bg.db.call(|c| db::get_background_task(c, "bg-orphan")).unwrap();
    assert_eq!(reaped.status, aria_core::db::BackgroundStatus::Killed);
    assert!(reaped.ended_at.is_some());

    // No other task is wrongly reported as running.
    let remaining: Vec<String> = fx.bg.db.call(db::running_background_task_ids).unwrap();
    assert_eq!(remaining, Vec::<String>::new());
}

#[tokio::test]
async fn main_system_scope_can_start_tasks() {
    let fx = fx();
    let db = Arc::clone(&fx.bg.db);
    let blobs = Arc::clone(&fx.bg.blobs);
    let conv = db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    let base = fx._ws.path().to_path_buf();
    let bg = BackgroundTasks {
        db,
        blobs,
        root: base.clone(),
        scope: Scope::MainSystem { roots: vec![], base },
        shell_timeout_secs: 120,
    };
    let task = bg.start(&conv.id, "echo system").unwrap();
    let (task, tail) = poll_with(&bg, &task.id).await;
    assert_eq!(task.status, BackgroundStatus::Succeeded);
    assert!(tail.contains("system"));
}

#[tokio::test]
async fn refuses_without_workspace_scope() {
    let fx = fx();
    // Rebuild with a non-workspace scope to prove the guard, not execute's.
    let db = Arc::clone(&fx.bg.db);
    let blobs = Arc::clone(&fx.bg.blobs);
    let conv = db.call(|c| db::insert_conversation(c, None, "")).unwrap();
    let non_ws = BackgroundTasks {
        db,
        blobs,
        root: std::path::PathBuf::new(),
        scope: Scope::None,
        shell_timeout_secs: 120,
    };
    assert!(non_ws.start(&conv.id, "echo nope").is_err());
}

/// Polling reads only the last chunk of a large log, not the whole file.
#[test]
fn poll_reads_only_the_log_tail() {
    let fx = fx();
    let id = "bg-tail";
    let conversation_id = fx
        .bg
        .db
        .call(|c| db::insert_conversation(c, None, "tail"))
        .unwrap()
        .id;
    fx.bg
        .db
        .call(|c| {
            db::insert_background_task(
                c,
                &db::BackgroundTask {
                    id: id.to_string(),
                    cmd: "true".to_string(),
                    conversation_id,
                    started_at: 0,
                    ended_at: None,
                    exit_code: None,
                    status: BackgroundStatus::Running,
                    log_blob: String::new(),
                },
            )
        })
        .unwrap();
    let path = fx.bg.blobs.dir().join(format!("bg-{id}.log"));
    std::fs::write(&path, format!("START{}END-MARK", "x".repeat(10_000))).unwrap();

    let (task, tail) = fx.bg.poll(id).unwrap();
    assert_eq!(task.status, BackgroundStatus::Running);
    assert!(tail.contains("END-MARK"));
    assert!(!tail.contains("START"), "only the tail should be read");
    assert!(tail.len() <= 2000, "{}", tail.len());
}

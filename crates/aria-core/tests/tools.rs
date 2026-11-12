use std::sync::Arc;

use aria_core::db::{self, Db, ToolCallState};
use aria_core::journal::Blobs;
use aria_core::db::ApprovalClass;
use aria_core::tools::{self, ToolContext, ToolOutcome, ToolRegistry};
use aria_core::workspace::WorkspaceRoot;

struct Fx {
    _ws: tempfile::TempDir,
    _blobs: tempfile::TempDir,
    db: Arc<Db>,
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
    let ctx = ToolContext {
        root: Some(WorkspaceRoot::new(ws.path()).unwrap()),
        workspace_id: Some(ws_id),
        run_id: run.id,
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(blob_dir.path(), Arc::clone(&db))),
        shell_timeout_secs: 10,
    };
    Fx {
        _ws: ws,
        _blobs: blob_dir,
        db,
        ctx,
    }
}

fn write(f: &Fx, rel: &str, content: &str) {
    let p = f.ctx.root.as_ref().unwrap().root().join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

async fn call(f: &Fx, tool: &str, args: serde_json::Value) -> ToolOutcome {
    ToolRegistry::execute(tool, &args.to_string(), &f.ctx).await
}

#[test]
fn registry_advertises_ten_tools_with_classes() {
    let specs = ToolRegistry::describe(true);
    assert_eq!(specs.len(), 10);
    assert_eq!(specs.len(), tools::TOOLS.len());
    assert_eq!(tools::approval_class(tools::READ_FILE), Some(ApprovalClass::Read));
    assert_eq!(tools::approval_class(tools::RUN_SHELL), Some(ApprovalClass::Exec));
    assert_eq!(tools::approval_class(tools::WEB_FETCH), Some(ApprovalClass::Network));
    assert_eq!(tools::approval_class(tools::EDIT_FILE), Some(ApprovalClass::Mutate));
    assert_eq!(tools::approval_class("nope"), None);
    for spec in specs {
        assert_eq!(spec.parameters["type"], "object");
        assert!(!spec.description.is_empty());
    }
}

#[tokio::test]
async fn read_file_success_and_errors() {
    let f = fx();
    write(&f, "a.txt", "hello\nworld");
    let out = call(&f, "read_file", serde_json::json!({"path": "a.txt"})).await;
    assert!(!out.is_error);
    assert_eq!(out.text, "hello\nworld");

    let missing = call(&f, "read_file", serde_json::json!({"path": "nope.txt"})).await;
    assert!(missing.is_error);

    let escape = call(&f, "read_file", serde_json::json!({"path": "../etc/passwd"})).await;
    assert!(escape.is_error);
    assert!(escape.text.contains("escapes"));

    let absolute = call(&f, "read_file", serde_json::json!({"path": "/etc/passwd"})).await;
    assert!(absolute.is_error);

    let dir = call(&f, "read_file", serde_json::json!({"path": "."})).await;
    assert!(dir.is_error);

    write(&f, "bin.dat", "\u{0}\u{1}");
    std::fs::write(f.ctx.root.as_ref().unwrap().root().join("bin.dat"), [0xff, 0xfe]).unwrap();
    let binary = call(&f, "read_file", serde_json::json!({"path": "bin.dat"})).await;
    assert!(binary.is_error);
    assert!(binary.text.contains("UTF-8"));
}

#[tokio::test]
async fn read_file_truncates_at_max_bytes() {
    let f = fx();
    write(&f, "big.txt", &"x".repeat(1000));
    let out = call(
        &f,
        "read_file",
        serde_json::json!({"path": "big.txt", "max_bytes": 100}),
    )
    .await;
    assert!(out.text.contains("truncated"));
    assert!(out.text.len() < 300);
}

#[tokio::test]
async fn list_dir_orders_dirs_first() {
    let f = fx();
    write(&f, "z.txt", "1");
    write(&f, "a.txt", "22");
    std::fs::create_dir(f.ctx.root.as_ref().unwrap().root().join("sub")).unwrap();
    let out = call(&f, "list_dir", serde_json::json!({})).await;
    let lines: Vec<_> = out.text.lines().collect();
    assert_eq!(lines[0], "sub/");
    assert!(lines[1].starts_with("a.txt\t2 bytes"));
    assert!(lines[2].starts_with("z.txt\t1 bytes"));
}

#[tokio::test]
async fn search_files_regex_and_glob() {
    let f = fx();
    write(&f, "src/main.rs", "fn main() { panic!() }");
    write(&f, "src/lib.rs", "pub fn ok() {}");
    write(&f, "notes.md", "panic");
    let all = call(&f, "search_files", serde_json::json!({"pattern": "panic"})).await;
    assert!(all.text.contains("src/main.rs:1"));
    assert!(all.text.contains("notes.md:1"));
    let rs = call(
        &f,
        "search_files",
        serde_json::json!({"pattern": "panic", "glob": "**/*.rs"}),
    )
    .await;
    assert!(rs.text.contains("main.rs"));
    assert!(!rs.text.contains("notes.md"));
    let none = call(&f, "search_files", serde_json::json!({"pattern": "absent"})).await;
    assert_eq!(none.text, "no matches");
    let bad = call(&f, "search_files", serde_json::json!({"pattern": "("})).await;
    assert!(bad.is_error);
}

#[tokio::test]
async fn edit_file_exactly_once() {
    let f = fx();
    write(&f, "c.txt", "alpha beta alpha");
    let twice = call(
        &f,
        "edit_file",
        serde_json::json!({"path": "c.txt", "search": "alpha", "replace": "gamma"}),
    )
    .await;
    assert!(twice.is_error);
    assert!(twice.text.contains("2 times"));

    let none = call(
        &f,
        "edit_file",
        serde_json::json!({"path": "c.txt", "search": "delta", "replace": "x"}),
    )
    .await;
    assert!(none.is_error);
    assert!(none.text.contains("0 times"));

    let ok = call(
        &f,
        "edit_file",
        serde_json::json!({"path": "c.txt", "search": "beta", "replace": "BETA"}),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.text);
    assert_eq!(
        std::fs::read_to_string(f.ctx.root.as_ref().unwrap().root().join("c.txt")).unwrap(),
        "alpha BETA alpha"
    );

    // The edit was journaled, so it can be undone.
    f.ctx
        .blobs
        .undo(f.ctx.workspace_id.as_deref().unwrap(), f.ctx.root.as_ref().unwrap(), "c.txt")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.ctx.root.as_ref().unwrap().root().join("c.txt")).unwrap(),
        "alpha beta alpha"
    );
}

#[tokio::test]
async fn edit_file_refuses_symlink_escape() {
    let f = fx();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), f.ctx.root.as_ref().unwrap().root().join("link"))
        .unwrap();
    let out = call(
        &f,
        "edit_file",
        serde_json::json!({"path": "link", "search": "secret", "replace": "leaked"}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "secret"
    );
}

#[tokio::test]
async fn write_file_only_creates() {
    let f = fx();
    let created = call(
        &f,
        "write_file",
        serde_json::json!({"path": "new/dir.txt", "content": "hi"}),
    )
    .await;
    assert!(created.is_error, "parent dir missing must fail");

    let created = call(
        &f,
        "write_file",
        serde_json::json!({"path": "new.txt", "content": "hi"}),
    )
    .await;
    assert!(!created.is_error);
    let again = call(
        &f,
        "write_file",
        serde_json::json!({"path": "new.txt", "content": "hi"}),
    )
    .await;
    assert!(again.is_error);
    assert!(again.text.contains("edit_file"));
}

#[tokio::test]
async fn delete_and_move_are_journaled() {
    let f = fx();
    write(&f, "d.txt", "data");
    let moved = call(
        &f,
        "move_file",
        serde_json::json!({"from": "d.txt", "to": "e.txt"}),
    )
    .await;
    assert!(!moved.is_error, "{}", moved.text);
    let deleted = call(&f, "delete_file", serde_json::json!({"path": "e.txt"})).await;
    assert!(!deleted.is_error, "{}", deleted.text);
    assert!(!f.ctx.root.as_ref().unwrap().root().join("e.txt").exists());

    f.ctx.blobs.undo(f.ctx.workspace_id.as_deref().unwrap(), f.ctx.root.as_ref().unwrap(), "e.txt").unwrap();
    assert_eq!(
        std::fs::read_to_string(f.ctx.root.as_ref().unwrap().root().join("e.txt")).unwrap(),
        "data"
    );
    f.ctx.blobs.undo(f.ctx.workspace_id.as_deref().unwrap(), f.ctx.root.as_ref().unwrap(), "d.txt").unwrap();
    assert_eq!(
        std::fs::read_to_string(f.ctx.root.as_ref().unwrap().root().join("d.txt")).unwrap(),
        "data"
    );
}

#[tokio::test]
async fn todos_replace_run_list() {
    let f = fx();
    let out = call(
        &f,
        "todos",
        serde_json::json!({"items": [{"text": "one"}, {"text": "two", "done": true}]}),
    )
    .await;
    assert!(!out.is_error);
    assert!(out.text.contains("[ ] one"));
    assert!(out.text.contains("[x] two"));
    let saved = f
        .db
        .call(|c| db::list_todos(c, &f.ctx.run_id))
        .unwrap();
    assert_eq!(saved.len(), 2);
}

#[tokio::test]
async fn unknown_tool_and_bad_args_are_errors_not_panics() {
    let f = fx();
    let unknown = call(&f, "teleport", serde_json::json!({})).await;
    assert!(unknown.is_error);
    let bad = call(&f, "read_file", serde_json::json!({"path": 5})).await;
    assert!(bad.is_error);
    let not_json = ToolRegistry::execute("read_file", "not json", &f.ctx).await;
    assert!(not_json.is_error);
}

#[tokio::test]
async fn web_fetch_refuses_private_and_odd_schemes() {
    let f = fx();
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "http://127.0.0.1/",
        "http://localhost:8080/",
        "http://10.0.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/",
    ] {
        let out = call(&f, "web_fetch", serde_json::json!({"url": url})).await;
        assert!(out.is_error, "{url} must be refused");
    }
    let bad = call(&f, "web_fetch", serde_json::json!({"url": "not a url"})).await;
    assert!(bad.is_error);
}

#[test]
fn ip_classification() {
    use aria_core::tools::classify_ip;
    for blocked in ["127.0.0.1", "10.1.1.1", "192.168.0.5", "::1", "fe80::1", "::ffff:127.0.0.1"] {
        let ip = blocked.parse().unwrap();
        assert!(!classify_ip(&ip), "{blocked}");
    }
    for allowed in ["1.1.1.1", "2606:4700:4700::1111"] {
        let ip = allowed.parse().unwrap();
        assert!(classify_ip(&ip), "{allowed}");
    }
}

#[test]
fn approvals_marked_by_tool_layer_persist() {
    let f = fx();
    let tc = f
        .db
        .call(|c| {
            db::insert_tool_call(
                c,
                &f.ctx.run_id,
                0,
                0,
                "run_shell",
                "{\"cmd\":\"ls\"}",
                ApprovalClass::Exec,
                ToolCallState::Pending,
            )
        })
        .unwrap();
    tools::mark_approved(&f.db, &tc.id).unwrap();
    let got = f.db.call(|c| db::get_tool_call(c, &tc.id)).unwrap();
    assert_eq!(got.state, ToolCallState::Approved);
}

#[test]
fn summary_shows_exact_content_for_edits_and_commands() {
    let s = tools::summarize(
        "edit_file",
        &serde_json::json!({"path": "a.rs", "search": "old code", "replace": "new code"}).to_string(),
    );
    assert!(s.title.contains("a.rs"));
    assert!(s.body.contains("old code") && s.body.contains("new code"));
    let s = tools::summarize("run_shell", &serde_json::json!({"cmd": "rm -rf build"}).to_string());
    assert_eq!(s.body, "rm -rf build");
}

#[tokio::test]
async fn without_a_folder_only_filesystem_free_tools_are_offered() {
    let specs = ToolRegistry::describe(false);
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec![tools::WEB_FETCH, tools::TODOS]);

    // The folder tools are not advertised, but asking for one is recoverable.
    let ws = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let conv = db.call(|c| db::insert_conversation(c, None, "main")).unwrap();
    let run = db.call(|c| db::insert_run(c, &conv.id)).unwrap();
    let ctx = ToolContext {
        root: None,
        workspace_id: None,
        run_id: run.id,
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(ws.path(), db)),
        shell_timeout_secs: 10,
    };

    let cases = [
        (tools::READ_FILE, r#"{"path":"x"}"#),
        (tools::LIST_DIR, r#"{}"#),
        (tools::EDIT_FILE, r#"{"path":"x","search":"a","replace":"b"}"#),
        (tools::RUN_SHELL, r#"{"cmd":"true"}"#),
    ];
    for (tool, args) in cases {
        let outcome = ToolRegistry::execute(tool, args, &ctx).await;
        assert!(outcome.is_error, "{tool} should refuse");
        assert!(outcome.text.contains("No folder"), "{tool}: {}", outcome.text);
    }

    // The todo list still works, since it never touches the filesystem.
    let outcome = ToolRegistry::execute(
        tools::TODOS,
        r#"{"items":[{"text":"say hi"}]}"#,
        &ctx,
    )
    .await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("say hi"));
}

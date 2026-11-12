use aria_core::db::{self, Db, Decision, Role, RunState, ToolCallState};
use aria_core::db::ApprovalClass;

fn seed(db: &Db) -> (String, String) {
    db.call(|c| {
        let ws = db::insert_workspace(c, "/tmp/ws")?;
        let conv = db::insert_conversation(c, Some(&ws.id), "test")?;
        Ok((ws.id, conv.id))
    })
    .unwrap()
}

#[test]
fn fts_finds_inserted_message_in_same_transaction() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        let tx = c.unchecked_transaction()?;
        db::insert_message(&tx, &conv, None, Role::User, "the quick brown fox")?;
        let hits = db::search_messages(&tx, "brown", None, 10)?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("[brown]"));
        tx.commit()?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn fts_update_and_delete_keep_index_consistent() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        let msg = db::insert_message(c, &conv, None, Role::User, "alpha beta")?;
        assert_eq!(db::search_messages(c, "beta", None, 10)?.len(), 1);
        c.execute(
            "UPDATE messages SET content = 'gamma delta' WHERE id = ?1",
            [&msg.id],
        )?;
        assert_eq!(db::search_messages(c, "beta", None, 10)?.len(), 0);
        assert_eq!(db::search_messages(c, "gamma", None, 10)?.len(), 1);
        c.execute("DELETE FROM messages WHERE id = ?1", [&msg.id])?;
        assert_eq!(db::search_messages(c, "gamma", None, 10)?.len(), 0);
        Ok(())
    })
    .unwrap();
}

#[test]
fn fts_bad_syntax_is_no_hits_not_error() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        db::insert_message(c, &conv, None, Role::User, "hello")?;
        assert_eq!(db::search_messages(c, "\"unterminated", None, 10)?.len(), 0);
        assert_eq!(db::search_messages(c, "NEAR(", None, 10)?.len(), 0);
        assert_eq!(db::search_messages(c, "   ", None, 10)?.len(), 0);
        Ok(())
    })
    .unwrap();
}

#[test]
fn fts_scoped_to_conversation() {
    let db = Db::open_in_memory().unwrap();
    let (ws, a) = seed(&db);
    let b = db
        .call(|c| db::insert_conversation(c, Some(&ws), "second"))
        .unwrap();
    db.call(|c| {
        db::insert_message(c, &a, None, Role::User, "shared needle")?;
        db::insert_message(c, &b.id, None, Role::User, "shared needle")?;
        assert_eq!(db::search_messages(c, "needle", None, 10)?.len(), 2);
        assert_eq!(db::search_messages(c, "needle", Some(&a), 10)?.len(), 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn migration_is_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aria.db");
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    let (_ws, conv) = seed(&db);
    db.call(|c| db::insert_message(c, &conv, None, Role::User, "persisted"))
        .unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    db.call(|c| {
        assert_eq!(db::list_messages(c, &conv)?.len(), 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn journal_mode_is_wal_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aria.db");
    let db = Db::open(&path).unwrap();
    let mode: String = db
        .call(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    assert!(dir.path().join("aria.db-wal").exists());
}

#[test]
fn run_state_transitions_persist() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        let run = db::insert_run(c, &conv)?;
        assert_eq!(run.state, RunState::Running);
        db::record_turn_usage(c, &run.id, 120)?;
        db::record_turn_usage(c, &run.id, 80)?;
        db::set_run_state(c, &run.id, RunState::BudgetExhausted, Some("turn_cap"), None)?;
        let got = db::get_run(c, &run.id)?;
        assert_eq!(got.state, RunState::BudgetExhausted);
        assert_eq!(got.turn_count, 2);
        assert_eq!(got.prompt_tokens, 200);
        assert!(got.ended_at.is_some());
        db::reopen_run(c, &run.id)?;
        let got = db::get_run(c, &run.id)?;
        assert_eq!(got.state, RunState::Running);
        assert!(got.ended_at.is_none());
        assert_eq!(db::unfinished_runs(c)?.len(), 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn approval_cannot_be_decided_twice() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        let run = db::insert_run(c, &conv)?;
        let tc = db::insert_tool_call(
            c,
            &run.id,
            0,
            0,
            "run_shell",
            "{}",
            ApprovalClass::Exec,
            ToolCallState::Pending,
        )?;
        let ap = db::insert_approval(c, &tc.id)?;
        db::decide_approval(c, &ap.id, Decision::Approve)?;
        assert!(db::decide_approval(c, &ap.id, Decision::Deny).is_err());
        assert_eq!(
            db::get_approval(c, &ap.id)?.decision,
            Some(Decision::Approve)
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn todos_replace_and_order() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    db.call(|c| {
        let run = db::insert_run(c, &conv)?;
        db::replace_todos(c, &run.id, &[("one".into(), false), ("two".into(), true)])?;
        db::replace_todos(c, &run.id, &[("only".into(), true)])?;
        let todos = db::list_todos(c, &run.id)?;
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].text, "only");
        assert!(todos[0].done);
        Ok(())
    })
    .unwrap();
}

#[test]
fn migrating_v1_keeps_rows_and_allows_a_folderness_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aria.db");
    // Build a v1 database by hand, with a workspace, conversation and message,
    // then let `Db::open` upgrade it.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(V1_MINIMAL).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces VALUES ('w1', '/tmp/w1', 1);
             INSERT INTO conversations VALUES ('c1', 'w1', 'old', 1, 1);
             INSERT INTO messages (id, conversation_id, role, content, created_at)
                 VALUES ('m1', 'c1', 'user', 'legacy needle', 1);",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    db.call(|c| {
        // Existing rows survived the rebuild, and FTS was not disturbed.
        let conv = db::get_conversation(c, "c1")?;
        assert_eq!(conv.workspace_id.as_deref(), Some("w1"));
        assert_eq!(db::search_messages(c, "needle", None, 10)?.len(), 1);

        // A folderness conversation is now insertable.
        let lonely = db::insert_conversation(c, None, "main")?;
        assert!(lonely.workspace_id.is_none());
        db::set_conversation_workspace(c, &lonely.id, Some("w1"))?;
        assert_eq!(
            db::get_conversation(c, &lonely.id)?.workspace_id.as_deref(),
            Some("w1")
        );
        db::set_conversation_workspace(c, &lonely.id, None)?;
        assert!(db::get_conversation(c, &lonely.id)?.workspace_id.is_none());
        Ok(())
    })
    .unwrap();
}

/// Just enough of schema v1 for the upgrade test: the tables the migration
/// touches, plus the FTS index over `messages` so we can prove it survives.
const V1_MINIMAL: &str = r#"
CREATE TABLE workspaces (
    id          TEXT PRIMARY KEY,
    root_path   TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE conversations (
    id            TEXT PRIMARY KEY,
    workspace_id  TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    title         TEXT NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX conversations_by_workspace ON conversations(workspace_id, updated_at DESC);
CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    run_id          TEXT,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL
);
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content, content='messages', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

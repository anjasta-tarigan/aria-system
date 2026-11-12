use aria_core::db::{self, Db, Decision, Role, RunState, ToolCallState};
use aria_core::db::ApprovalClass;

fn seed(db: &Db) -> (String, String) {
    db.call(|c| {
        let ws = db::insert_workspace(c, "/tmp/ws")?;
        let conv = db::insert_conversation(c, &ws.id, "test")?;
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
        .call(|c| db::insert_conversation(c, &ws, "second"))
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
    assert_eq!(db.schema_version().unwrap(), 1);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 1);
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

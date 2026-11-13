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
    assert_eq!(db.schema_version().unwrap(), 6);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 6);
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
    assert_eq!(db.schema_version().unwrap(), 6);
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

#[test]
fn v3_pins_and_deletes_workspaces_and_conversations() {
    let db = Db::open_in_memory().unwrap();
    let (ws_id, c1) = db
        .call(|c| {
            let ws = db::insert_workspace(c, "/tmp/ws")?;
            let conv = db::insert_conversation(c, Some(&ws.id), "one")?;
            Ok((ws.id, conv.id))
        })
        .unwrap();

    // New workspaces are not pinned by default.
    assert!(!db.call(|c| db::get_workspace(c, &ws_id)).unwrap().pinned);

    // Pinning flips the flag and survives a read-back.
    db.call(|c| db::set_workspace_pin(c, &ws_id, true)).unwrap();
    assert!(db.call(|c| db::get_workspace(c, &ws_id)).unwrap().pinned);

    // A second session in the same workspace is independently deletable.
    let c2 = db
        .call(|c| db::insert_conversation(c, Some(&ws_id), "two"))
        .unwrap()
        .id;
    assert_eq!(db.call(|c| db::count_conversations(c, &ws_id)).unwrap(), 2);
    db.call(|c| db::delete_conversation(c, &c2)).unwrap();
    assert_eq!(db.call(|c| db::count_conversations(c, &ws_id)).unwrap(), 1);

    // Deleting the workspace cascades to its remaining conversations.
    db.call(|c| db::delete_workspace(c, &ws_id)).unwrap();
    assert!(db.call(|c| db::get_workspace(c, &ws_id)).is_err());
    assert_eq!(db.call(|c| db::count_conversations(c, &ws_id)).unwrap(), 0);
    assert!(db.call(|c| db::get_conversation(c, &c1)).is_err());
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

// ----------------------------------------------------------------- memories

#[test]
fn migration_v4_creates_memories_table() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.schema_version().unwrap(), 6);
    db.call(|c| {
        c.execute_batch("INSERT INTO memories (id, key, content, created_at, updated_at)
                         VALUES ('m1', 'k', 'v', 1, 1)")?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn memory_upsert_updates_existing_key() {
    let db = Db::open_in_memory().unwrap();
    let ws = db.call(|c| db::insert_workspace(c, "/tmp/ws")).unwrap().id;
    let (a, b) = db
        .call(|c| {
            let m1 = db::insert_or_update_memory(c, Some(&ws), "project", "rust", None)?;
            let m2 = db::insert_or_update_memory(c, Some(&ws), "project", "python", None)?;
            Ok((m1, m2))
        })
        .unwrap();
    // Same id reused: this is an upsert, not an insert.
    assert_eq!(a.id, b.id);
    assert_eq!(b.content, "python");
    assert_eq!(db.call(|c| db::list_memories(c, Some(&ws))).unwrap().len(), 1);
}

#[test]
fn memory_scope_is_workspace_plus_global() {
    let db = Db::open_in_memory().unwrap();
    let w1 = db.call(|c| db::insert_workspace(c, "/tmp/w1")).unwrap().id;
    let w2 = db.call(|c| db::insert_workspace(c, "/tmp/w2")).unwrap().id;
    db.call(|c| {
        db::insert_or_update_memory(c, Some(&w1), "k1", "w1 only", None)?;
        db::insert_or_update_memory(c, Some(&w2), "k2", "w2 only", None)?;
        db::insert_or_update_memory(c, None, "kg", "global", None)?;
        Ok(())
    })
    .unwrap();

    let w1mem = db.call(|c| db::list_memories(c, Some(&w1))).unwrap();
    assert_eq!(w1mem.len(), 2); // w1's own + the global one
    assert!(w1mem.iter().any(|m| m.workspace_id.is_none()));

    let global = db.call(|c| db::list_memories(c, None)).unwrap();
    assert_eq!(global.len(), 1);
    assert_eq!(global[0].key, "kg");

    // Deleting a workspace-scoped memory leaves the global one untouched.
    let id = w1mem
        .iter()
        .find(|m| m.workspace_id.as_deref() == Some(w1.as_str()))
        .unwrap()
        .id
        .clone();
    db.call(|c| db::delete_memory(c, &id)).unwrap();
    assert_eq!(db.call(|c| db::list_memories(c, Some(&w1))).unwrap().len(), 1);
}

#[test]
fn memory_delete_by_key_and_get() {
    let db = Db::open_in_memory().unwrap();
    let m = db
        .call(|c| db::insert_or_update_memory(c, None, "k", "v", None))
        .unwrap();
    db.call(|c| db::delete_memory_by_key(c, None, "k")).unwrap();
    assert!(db.call(|c| db::get_memory(c, &m.id)).is_err());
}

#[test]
fn embedding_roundtrip_and_similarity_search() {
    let db = Db::open_in_memory().unwrap();
    let ws = db.call(|c| db::insert_workspace(c, "/tmp/ws")).unwrap().id;
    let emb = db::floats_to_bytes(&[1.0, 0.0, 0.0]);
    let emb2 = db::floats_to_bytes(&[0.0, 1.0, 0.0]);
    db.call(|c| {
        db::insert_or_update_memory(c, Some(&ws), "close", "a", Some(&emb))?;
        db::insert_or_update_memory(c, Some(&ws), "orth", "b", Some(&emb2))?;
        db::insert_or_update_memory(c, Some(&ws), "noemb", "c", None)?;
        Ok(())
    })
    .unwrap();

    let q = db::floats_to_bytes(&[1.0, 0.0, 0.0]);
    let floats = db::bytes_to_floats(&q);
    let hits = db
        .call(|c| db::search_memories_by_embedding(c, Some(&ws), &floats, 10))
        .unwrap();
    // The no-embedding one is excluded; "close" ranks first.
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.key, "close");
    assert!((hits[0].1 - 1.0).abs() < 1e-6);
    assert!(hits[1].1 < hits[0].1);
}

// ------------------------------------------------------------ main-system scope

#[test]
fn migration_v5_adds_main_system_and_paths_tables() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.schema_version().unwrap(), 6);
    let (ws, conv) = seed(&db);
    db.call(|c| {
        db::set_conversation_scope(c, &conv, true, &[])?;
        let got = db::get_conversation(c, &conv)?;
        assert!(got.main_system);
        db::set_conversation_scope(c, &conv, false, &[])?;
        assert!(db::list_main_system_paths(c, &conv)?.is_empty());
        // workspace id still round-trips; nothing else disturbed.
        let got = db::get_conversation(c, &conv)?;
        assert_eq!(got.workspace_id.as_deref(), Some(ws.as_str()));
        Ok(())
    })
    .unwrap();
}

#[test]
fn conversation_scope_sets_main_system_and_paths() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    let abs = std::env::temp_dir().join("aria_main_system_test");
    let abs = abs.to_string_lossy().to_string();
    db.call(|c| {
        db::set_conversation_scope(c, &conv, true, &[abs.clone()])?;
        let paths = db::list_main_system_paths(c, &conv)?;
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], abs);
        assert!(db::get_conversation(c, &conv)?.main_system);
        db::set_conversation_scope(c, &conv, true, &[])?;
        assert!(db::list_main_system_paths(c, &conv)?.is_empty());
        db::set_conversation_scope(c, &conv, false, &[])?;
        assert!(!db::get_conversation(c, &conv)?.main_system);
        Ok(())
    })
    .unwrap();
}

#[test]
fn main_system_path_must_be_absolute() {
    let db = Db::open_in_memory().unwrap();
    let (_ws, conv) = seed(&db);
    let err = db.call(|c| db::set_conversation_scope(c, &conv, true, &["relative/path".to_string()]));
    assert!(err.is_err());
}

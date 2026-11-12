use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use super::{DbError, Result};

/// Maximum schema version understood by this build.
const SCHEMA_VERSION: i64 = 1;

/// A handle to the application database.
///
/// Only one connection is ever opened per process: SQLite is used for small,
/// latency-sensitive writes and a mutex keeps the surface simple. Callers must
/// not hold the guard across an `.await`; use [`Db::call`] for that.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if needed) the database and apply pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbError::NotADirectory(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// Open an in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn guard(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a blocking closure against the connection on the current thread.
    pub fn call<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.guard();
        f(&guard)
    }

    /// Run a blocking closure off the async runtime.
    ///
    /// `Db` must be held in an `Arc`; the closure receives a borrow of it.
    pub async fn call_async<T, F>(self: &std::sync::Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let db = std::sync::Arc::clone(self);
        tokio::task::spawn_blocking(move || db.call(f)).await.map_err(|e| {
            DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?
    }

    /// Current `user_version`; equals the applied schema version.
    pub fn schema_version(&self) -> Result<i64> {
        self.call(user_version)
    }
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

fn migrate(conn: &Connection) -> Result<()> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(DbError::InvalidValue {
            field: "schema version",
            value: format!("{current} (newer than this build's {SCHEMA_VERSION})"),
        });
    }
    if current < 1 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(super::schema::V1)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Version 1 schema text, applied inside a transaction.
pub(crate) const V1: &str = V1_SCHEMA;

/// Version 1 schema.
///
/// Rules for later migrations (see ADR-0003):
/// - `messages_fts` is external-content over `messages`; any migration that
///   rebuilds `messages` must rebuild the FTS index in the same migration.
/// - Every table mutation is a row write; no run state lives only in memory.
const V1_SCHEMA: &str = r#"
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

CREATE TABLE runs (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    state           TEXT NOT NULL,
    stop_reason     TEXT,
    turn_count      INTEGER NOT NULL DEFAULT 0,
    prompt_tokens   INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    started_at      INTEGER NOT NULL,
    ended_at        INTEGER
);
CREATE INDEX runs_by_conversation ON runs(conversation_id, started_at);

CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    run_id          TEXT REFERENCES runs(id) ON DELETE SET NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL
);
CREATE INDEX messages_by_conversation ON messages(conversation_id, created_at);

CREATE TABLE tool_calls (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    turn_index      INTEGER NOT NULL,
    call_index      INTEGER NOT NULL,
    tool            TEXT NOT NULL,
    args_json       TEXT NOT NULL,
    approval_class  TEXT NOT NULL,
    state           TEXT NOT NULL,
    result          TEXT,
    is_error        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);
CREATE INDEX tool_calls_by_run ON tool_calls(run_id, turn_index, call_index);

CREATE TABLE approvals (
    id            TEXT PRIMARY KEY,
    tool_call_id  TEXT NOT NULL UNIQUE REFERENCES tool_calls(id) ON DELETE CASCADE,
    decision      TEXT,
    decided_at    INTEGER,
    created_at    INTEGER NOT NULL
);
CREATE INDEX approvals_pending ON approvals(decision) WHERE decision IS NULL;

CREATE TABLE todos (
    id        TEXT PRIMARY KEY,
    run_id    TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    text      TEXT NOT NULL,
    done      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX todos_by_run ON todos(run_id, position);

CREATE TABLE edit_journal (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id  TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    path          TEXT NOT NULL,
    blob_hash     TEXT NOT NULL,
    existed       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL
);
CREATE INDEX edit_journal_by_path ON edit_journal(workspace_id, path, id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER messages_au AFTER UPDATE OF content ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

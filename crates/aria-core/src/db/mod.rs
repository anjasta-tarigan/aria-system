//! SQLite persistence: schema migrations, typed repositories.
//!
//! The database is the transcript of record (ADR-0003). It runs in WAL mode and
//! exposes a small, fully typed surface so the rest of the core never writes SQL.

mod models;
mod repo;
mod schema;

pub use models::*;
pub use repo::*;
pub use schema::Db;


pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("invalid value for {field}: {value}")]
    InvalidValue { field: &'static str, value: String },
    #[error("not a directory: {0}")]
    NotADirectory(String),
}

//! Aria core: database, provider client, agent loop and tools.
//!
//! This crate must not depend on Tauri. See `docs/adr/0001-agent-loop-in-rust.md`.

pub mod agent;
pub mod config;
pub mod db;
pub mod journal;
pub mod provider;
pub mod tools;
pub mod workspace;

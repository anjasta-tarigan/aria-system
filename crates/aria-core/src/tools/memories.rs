//! Persistent memory Tools: `remember` and `recall`.
//!
//! Memories are keyed by `(workspace_id, key)`. A conversation with a folder
//! stores workspace-scoped memories; the main session stores global ones. Recall
//! returns the workspace's memories plus the global set, ranked by embedding
//! similarity when an embedder is available, otherwise by recency. Both tools
//! are filesystem-free and run without an approval prompt.

use serde::Deserialize;

use super::{Result, ToolContext};
use crate::db;

#[derive(Debug, Deserialize)]
pub struct RememberArgs {
    /// Stable label the model can later recall by, e.g. "user_name".
    pub key: String,
    /// The fact to retain verbatim.
    pub content: String,
}

pub async fn remember(ctx: &ToolContext, args: RememberArgs) -> Result<String> {
    let key = args.key.trim();
    if key.is_empty() {
        return Ok("Nothing stored: `key` is required.".to_string());
    }
    if args.content.trim().is_empty() {
        // An empty value is a delete request.
        ctx.db
            .call(|c| db::delete_memory_by_key(c, ctx.workspace_id.as_deref(), key))?;
        return Ok(format!("Forgot memory \"{key}\"."));
    }
    let embedding = match ctx.embed.as_ref() {
        Some(embed) => Some(db::floats_to_bytes(
            &embed.embed(&args.content).await.map_err(|e| super::ToolError::Refused(e.to_string()))?,
        )),
        None => None,
    };
    let m = ctx.db.call(|c| {
        db::insert_or_update_memory(
            c,
            ctx.workspace_id.as_deref(),
            key,
            &args.content,
            embedding.as_deref(),
        )
    })?;
    Ok(format!(
        "Stored memory \"{}\" ({} chars).",
        m.key,
        m.content.chars().count()
    ))
}

#[derive(Debug, Deserialize)]
pub struct RecallArgs {
    /// Optional free-text query; memories are ranked by similarity to it.
    #[serde(default)]
    pub query: Option<String>,
    /// How many memories to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

pub async fn recall(ctx: &ToolContext, args: RecallArgs) -> Result<String> {
    let limit = args.limit.unwrap_or(10).max(1);
    let memories = ctx
        .db
        .call(|c| db::list_memories(c, ctx.workspace_id.as_deref()))?;
    if memories.is_empty() {
        return Ok("No memories stored yet.".to_string());
    }

    let ranked: Vec<(String, String, f32)> = if let (Some(query), Some(embed)) =
        (args.query.as_deref(), ctx.embed.as_ref())
    {
        let q = embed
            .embed(query)
            .await
            .map_err(|e| super::ToolError::Refused(e.to_string()))?;
        ctx.db
            .call(|c| db::search_memories_by_embedding(c, ctx.workspace_id.as_deref(), &q, limit))?
            .into_iter()
            .map(|(m, score)| (m.key, m.content, score))
            .collect()
    } else {
        memories
            .into_iter()
            .take(limit)
            .map(|m| (m.key, m.content, 1.0))
            .collect()
    };

    let scope = ctx
        .workspace_id
        .as_deref()
        .map(|_| "this workspace and global")
        .unwrap_or("global");
    let body = ranked
        .iter()
        .map(|(k, c, _)| format!("- [{k}] {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Memories ({scope}), {scope} scope:\n{body}"))
}

//! Persistent memory Tools: `remember` and `recall`.
//!
//! Memories are keyed by `(workspace_id, key)`. A conversation with a folder
//! stores workspace-scoped memories; the main session stores global ones. Recall
//! returns the workspace's memories plus the global set: with a query it fuses
//! FTS5 keyword hits with embedding similarity, falling back to keyword-only
//! when no embedder is configured or the embed call fails; without a query it is
//! newest first. Both tools are filesystem-free and run without an approval
//! prompt.

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
        // A provider without embeddings (or a failed embed) still stores the
        // memory; it stays searchable by keyword through FTS5.
        Some(embed) => embed
            .embed(&args.content)
            .await
            .ok()
            .map(|v| db::floats_to_bytes(&v)),
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

    let ranked: Vec<db::Memory> = match args.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(query) => {
            let keyword = ctx.db.call(|c| {
                db::search_memories_by_text(c, ctx.workspace_id.as_deref(), query, limit)
            })?;
            let semantic = match ctx.embed.as_ref() {
                Some(embed) => match embed.embed(query).await {
                    Ok(vector) => ctx
                        .db
                        .call(|c| {
                            db::search_memories_by_embedding(
                                c,
                                ctx.workspace_id.as_deref(),
                                &vector,
                                limit,
                            )
                        })?
                        .into_iter()
                        .map(|(m, _)| m)
                        .collect(),
                    // FTS5 hits still answer when embeddings are unavailable.
                    Err(_) => Vec::new(),
                },
                None => Vec::new(),
            };
            fuse(keyword, semantic, limit)
        }
        None => memories.into_iter().take(limit).collect(),
    };

    if ranked.is_empty() {
        return Ok(format!(
            "No memories match \"{}\".",
            args.query.as_deref().unwrap_or_default()
        ));
    }

    let scope = ctx
        .workspace_id
        .as_deref()
        .map(|_| "this workspace and global")
        .unwrap_or("global");
    let body = ranked
        .iter()
        .map(|m| format!("- [{}] {}", m.key, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Memories ({scope}):\n{body}"))
}

/// Reciprocal-rank fusion: a memory found by both keyword and semantic search
/// outranks one found by either alone, and neither side needs a calibrated
/// score. Missing embeddings only cost the semantic half.
fn fuse(keyword: Vec<db::Memory>, semantic: Vec<db::Memory>, limit: usize) -> Vec<db::Memory> {
    const K: f32 = 60.0;
    fn weight(rank: usize) -> f32 {
        1.0 / (K + rank as f32 + 1.0)
    }
    let mut ranked: Vec<(db::Memory, f32)> = Vec::new();
    for (rank, memory) in keyword.into_iter().enumerate() {
        ranked.push((memory, weight(rank)));
    }
    for (rank, memory) in semantic.into_iter().enumerate() {
        match ranked.iter_mut().find(|(m, _)| m.id == memory.id) {
            Some((_, score)) => *score += weight(rank),
            None => ranked.push((memory, weight(rank))),
        }
    }
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);
    ranked.into_iter().map(|(m, _)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::fuse;
    use crate::db::Memory;

    fn mem(id: &str, key: &str) -> Memory {
        Memory {
            id: id.into(),
            workspace_id: None,
            key: key.into(),
            content: String::new(),
            embedding: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn fusion_prefers_memories_found_by_both_and_honours_limit() {
        let fused = fuse(
            vec![mem("a", "both"), mem("b", "keyword")],
            vec![mem("c", "semantic"), mem("a", "both")],
            10,
        );
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].id, "a");
        assert_eq!(fuse(vec![mem("a", "x")], vec![mem("b", "y")], 1).len(), 1);
    }
}

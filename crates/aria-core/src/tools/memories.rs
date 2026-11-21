//! Persistent memory Tools: `remember` and `recall`.
//!
//! Memories are keyed by `(workspace_id, key)`. A conversation with a folder
//! stores workspace-scoped memories; the main session stores global ones. Recall
//! returns the workspace's memories plus the global set: with a query it runs
//! weighted hybrid search (FTS5 keyword + cosine semantic) over the ANN index,
//! optionally re-scores with a reranker, filters by access policy, and logs the
//! operation to the audit trail. Without a query it returns newest first. Both
//! tools are filesystem-free and run without an approval prompt.

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

    let workspace_id = ctx.workspace_id.as_deref();
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
        // RBAC: refuse writes the caller is not permitted to make.
        if !db::memory_can_write(c, "user", workspace_id) {
            return Err(crate::db::DbError::InvalidValue {
                field: "memory",
                value: "write access denied".into(),
            });
        }
        let m = db::insert_or_update_memory(
            c,
            workspace_id,
            key,
            &args.content,
            embedding.as_deref(),
            None, // encryption_key_id: key management is out of scope for now
        )?;
        // Keep the pre-normalized ANN index in sync for semantic recall.
        db::upsert_memory_vector_index(c, &m.id, embedding.as_deref())?;
        // Audit trail: who stored what, when.
        db::audit_memory(c, workspace_id, "user", "remember", Some(key), 1, 0)?;
        Ok(m)
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
    /// Fusion strategy: "weighted" (weighted sum of BM25 + cosine) or "rrf"
    /// (reciprocal rank fusion). Defaults to "weighted".
    #[serde(default)]
    pub fusion: Option<String>,
}

fn fusion_is_rrf(fusion: Option<&str>) -> bool {
    matches!(fusion, Some(s) if s.eq_ignore_ascii_case("rrf"))
}

pub async fn recall(ctx: &ToolContext, args: RecallArgs) -> Result<String> {
    let limit = args.limit.unwrap_or(10).max(1);
    let workspace_id = ctx.workspace_id.as_deref();
    let started = db::now_ms();

    let query = args
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());

    // Phase 1 (async): embed the query when an embedder is available.
    let query_embedding = match (query, ctx.embed.as_ref()) {
        (Some(q), Some(embed)) => match embed.embed(q).await {
            Ok(vector) => Some(vector),
            // Embed failure falls back to keyword-only recall.
            Err(_) => None,
        },
        _ => None,
    };
    let query_str = query.map(|q| q.to_string()).unwrap_or_default();

    // Phase 2 (sync): hybrid candidates + access control.
    let candidates: Vec<db::Memory> = ctx.db.call(|c| {
        if query_str.is_empty() {
            let mut out: Vec<db::Memory> = Vec::new();
            for m in db::list_memories(c, workspace_id)? {
                if db::memory_can_read(c, "user", &m) {
                    out.push(m);
                }
            }
            out.truncate(limit);
            Ok(out)
        } else {
            let accessible: Vec<(db::Memory, f32)> = if fusion_is_rrf(args.fusion.as_deref()) {
                // Reciprocal rank fusion: no score calibration needed.
                let scored: Vec<(db::Memory, f32)> = db::search_memories_hybrid_rrf(
                    c,
                    workspace_id,
                    &query_str,
                    query_embedding.as_deref(),
                    limit * 4,
                    60.0, // rrf_k
                    limit * 4, // keyword_limit
                    limit * 4, // semantic_limit
                )?;
                scored
                    .into_iter()
                    .filter(|(m, _)| db::memory_can_read(c, "user", m))
                    .collect()
            } else {
                let scored: Vec<(db::Memory, f32)> = db::search_memories_hybrid_weighted(
                    c,
                    workspace_id,
                    &query_str,
                    query_embedding.as_deref(),
                    limit * 4,
                    0.5, // dense_weight (semantic)
                    0.5, // sparse_weight (keyword)
                )?;
                scored
                    .into_iter()
                    .filter(|(m, _)| db::memory_can_read(c, "user", m))
                    .collect()
            };
            Ok(accessible.into_iter().map(|(m, _)| m).collect())
        }
    })?;

    // Phase 3 (async): rerank candidates when a reranker is configured.
    let ranked: Vec<db::Memory> = if let Some(reranker) = &ctx.reranker {
        // Only the top_k candidates go to the (costly) reranker.
        let top_k = ctx.reranker_top_k;
        let mut scored: Vec<(db::Memory, f32)> = Vec::new();
        for m in candidates.into_iter().take(top_k) {
            let score = reranker.rerank(&query_str, &m.content).await.unwrap_or(0.0);
            scored.push((m, score));
        }
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.into_iter().map(|(m, _)| m).collect()
    } else {
        candidates.into_iter().take(limit).collect()
    };

    // Phase 4 (sync): audit trail.
    let latency = db::now_ms() - started;
    ctx.db.call(|c| {
        db::audit_memory(
            c,
            workspace_id,
            "user",
            "recall",
            if query_str.is_empty() {
                None
            } else {
                Some(query_str.as_str())
            },
            ranked.len() as i64,
            latency,
        )
    })?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Memory;

    fn mem(id: &str, key: &str) -> Memory {
        Memory {
            id: id.into(),
            workspace_id: None,
            key: key.into(),
            content: String::new(),
            embedding: None,
            encryption_key_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn global_memories_are_readable_by_default() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let m = mem("a", "k");
        assert!(db.call(|c| Ok(db::memory_can_read(c, "user", &m))).unwrap());
    }

    #[test]
    fn workspace_writes_are_allowed_by_default() {
        let db = crate::db::Db::open_in_memory().unwrap();
        assert!(db
            .call(|c| Ok(db::memory_can_write(c, "user", Some("ws"))))
            .unwrap());
    }
}

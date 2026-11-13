//! `web_search` dispatch, config gating and provider parsing. No network.

use std::sync::Arc;

use aria_core::db::{self, Db};
use aria_core::journal::Blobs;
use aria_core::tools::search::{SearchConfig, SearchProvider};
use aria_core::tools::{web_search, ToolContext};
use aria_core::workspace::Scope;

fn ctx_with(search: Option<SearchConfig>) -> ToolContext {
    let ws = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let conv = db.call(|c| db::insert_conversation(c, None, "main")).unwrap();
    let run = db.call(|c| db::insert_run(c, &conv.id)).unwrap();
    ToolContext {
        scope: Scope::None,
        root: None,
        workspace_id: None,
        run_id: run.id,
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(ws.path(), db)),
        shell_timeout_secs: 10,
        embed: None,
        search,
        vision: None,
    }
}

#[test]
fn search_provider_parse() {
    assert_eq!(SearchProvider::parse("Exa"), Some(SearchProvider::Exa));
    assert_eq!(SearchProvider::parse("nope"), None);
    assert_eq!(SearchProvider::parse(""), None);
    assert_eq!(
        SearchProvider::parse("DUCKDUCKGO"),
        Some(SearchProvider::Duckduckgo)
    );
}

#[tokio::test]
async fn web_search_refuses_without_config() {
    let ctx = ctx_with(None);
    let err = web_search(
        &ctx,
        aria_core::tools::search::SearchArgs {
            query: "rust".into(),
            max_results: None,
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not configured"));
}

#[tokio::test]
async fn web_search_resolves_config() {
    // Searxng with an unreachable base_url: proves dispatch + config path
    // without needing network success.
    let ctx = ctx_with(Some(SearchConfig {
        provider: SearchProvider::Searxng,
        api_key: None,
        base_url: Some("http://127.0.0.1:9".into()),
    }));
    let err = web_search(
        &ctx,
        aria_core::tools::search::SearchArgs {
            query: "rust".into(),
            max_results: Some(3),
        },
    )
    .await
    .unwrap_err();
    let msg = err.to_string().to_lowercase();
    // A refusal (SSRF guard) or a connection failure both prove the config
    // path was reached and dispatched to the provider; either is fine offline.
    assert!(
        msg.contains("refus")
            || msg.contains("connection")
            || msg.contains("sending request"),
        "expected a network failure error, got: {msg}"
    );
}

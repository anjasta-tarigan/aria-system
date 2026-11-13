//! `web_search`: query a configured provider and return ranked hits.
//!
//! The provider, key and optional base URL arrive on [`ToolContext::search`]
//! (mirrors how `embed` is carried for Phase 1 embeddings). No search config
//! means the tool is disabled.

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

use super::{default_limit, truncate, Result, ToolContext, ToolError};
use crate::tools::fetch::check_host_allowed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchProvider {
    Exa,
    Firecrawl,
    Searxng,
    Duckduckgo,
}

impl SearchProvider {
    /// Parse from the settings string. `None`/empty/unknown => None (disabled).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "exa" => Some(SearchProvider::Exa),
            "firecrawl" => Some(SearchProvider::Firecrawl),
            "searxng" => Some(SearchProvider::Searxng),
            "duckduckgo" => Some(SearchProvider::Duckduckgo),
            _ => None,
        }
    }
}

/// Resolved search configuration carried on `ToolContext`.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    pub api_key: Option<String>,
    /// Base URL override: a searXNG instance, or a custom Exa/Firecrawl host.
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub async fn web_search(ctx: &ToolContext, args: SearchArgs) -> Result<String> {
    let cfg = ctx
        .search
        .as_ref()
        .ok_or_else(|| ToolError::Refused("web search is not configured".into()))?;
    let max = default_limit(args.max_results, 5);
    let hits = run_search(cfg, &args.query, max).await?;
    if hits.is_empty() {
        return Ok("No results.".into());
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {}\n   {}\n",
            i + 1,
            h.title,
            h.url,
            h.snippet
        ));
    }
    Ok(truncate(out, 8_000))
}

async fn run_search(cfg: &SearchConfig, query: &str, max: usize) -> Result<Vec<SearchHit>> {
    match cfg.provider {
        SearchProvider::Exa => search_exa(cfg, query, max).await,
        SearchProvider::Firecrawl => search_firecrawl(cfg, query, max).await,
        SearchProvider::Searxng => search_searxng(cfg, query, max).await,
        SearchProvider::Duckduckgo => search_ddg(query, max).await,
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Refused(e.to_string()))
}

/// Refuse to reach a non-public host (loopback, private, cloud metadata, etc.),
/// mirroring the guard in `fetch.rs`. A user-supplied `base_url` (searXNG) is
/// fully attacker-controllable, so it must pass the same SSRF check.
fn guard_url(u: &url::Url) -> Result<()> {
    let host = u
        .host_str()
        .ok_or_else(|| ToolError::Refused("URL has no host".into()))?;
    let port = u.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| ToolError::Refused(format!("cannot resolve {host}: {e}")))?
        .map(|a| a.ip())
        .collect();
    check_host_allowed(&addrs)
}

/// Normalize result URLs: prefix `//` with `https:`, and unwrap duckduckgo
/// `uddg=` redirect links back to the real destination.
fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(stripped) = raw.strip_prefix("//") {
        return format!("https:{stripped}");
    }
    if let Ok(parsed) = url::Url::parse(raw) {
        // Only surface http(s) results; the model never fetches these, but a
        // `javascript:`/`data:`/`file:` hit is noise at best.
        if !matches!(parsed.scheme(), "http" | "https") {
            return String::new();
        }
        if parsed.host_str() == Some("duckduckgo.com") {
            if let Some((_, v)) = parsed
                .query_pairs()
                .find(|(k, _)| k == "uddg")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
            {
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    raw.to_string()
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Vec<ExaHit>,
}
#[derive(Deserialize)]
struct ExaHit {
    title: String,
    url: String,
    text: String,
}

async fn search_exa(cfg: &SearchConfig, query: &str, max: usize) -> Result<Vec<SearchHit>> {
    let key = cfg
        .api_key
        .as_deref()
        .ok_or_else(|| ToolError::Refused("exa requires an api_key".into()))?;
    let resp = client()?
        .post("https://api.exa.ai/search")
        .bearer_auth(key)
        .json(&serde_json::json!({"query": query, "numResults": max}))
        .send()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let data: ExaResponse = resp
        .json()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    Ok(data
        .results
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title,
            url: normalize_url(&h.url),
            snippet: h.text,
        })
        .collect())
}

#[derive(Deserialize)]
struct FirecrawlResponse {
    data: Vec<FirecrawlHit>,
}
#[derive(Deserialize)]
struct FirecrawlHit {
    title: String,
    url: String,
    description: String,
}

async fn search_firecrawl(cfg: &SearchConfig, query: &str, max: usize) -> Result<Vec<SearchHit>> {
    let key = cfg
        .api_key
        .as_deref()
        .ok_or_else(|| ToolError::Refused("firecrawl requires an api_key".into()))?;
    let resp = client()?
        .post("https://api.firecrawl.dev/v1/search")
        .bearer_auth(key)
        .json(&serde_json::json!({"query": query, "limit": max}))
        .send()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let data: FirecrawlResponse = resp
        .json()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    Ok(data
        .data
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title,
            url: normalize_url(&h.url),
            snippet: h.description,
        })
        .collect())
}

#[derive(Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngHit>,
}
#[derive(Deserialize)]
struct SearxngHit {
    title: String,
    url: String,
    content: String,
}

async fn search_searxng(cfg: &SearchConfig, query: &str, max: usize) -> Result<Vec<SearchHit>> {
    let base = cfg
        .base_url
        .as_deref()
        .ok_or_else(|| ToolError::Refused("searxng requires a base_url".into()))?;
    let mut u = url::Url::parse(base)
        .map_err(|e| ToolError::Refused(format!("invalid base_url: {e}")))?;
    u.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json");
    guard_url(&u)?;
    let resp = client()?
        .get(u)
        .send()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let data: SearxngResponse = resp
        .json()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    Ok(data
        .results
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title,
            url: normalize_url(&h.url),
            snippet: h.content,
        })
        .collect())
}

async fn search_ddg(query: &str, max: usize) -> Result<Vec<SearchHit>> {
    let mut u = url::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    u.query_pairs_mut().append_pair("q", query);
    let html = client()?
        .get(u)
        .send()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?
        .text()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;

    // shortcut: regex over HTML, no scraper dependency; DDG markup is stable.
    let title_re =
        regex::Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .map_err(|e| ToolError::Refused(e.to_string()))?;
    let snip_re = regex::Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#)
        .map_err(|e| ToolError::Refused(e.to_string()))?;

    let urls_titles: Vec<(String, String)> = title_re
        .captures_iter(&html)
        .map(|c| (c[1].to_string(), decode_entity(&c[2])))
        .collect();
    let snips: Vec<String> = snip_re
        .captures_iter(&html)
        .map(|c| decode_entity(&c[1]))
        .collect();

    let n = urls_titles.len().min(snips.len()).min(max);
    let mut hits = Vec::with_capacity(n);
    for i in 0..n {
        let (url, title) = &urls_titles[i];
        hits.push(SearchHit {
            title: title.clone(),
            url: normalize_url(url),
            snippet: snips[i].clone(),
        });
    }
    Ok(hits)
}

/// Minimal HTML-entity decode for the common cases DDG emits.
fn decode_entity(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

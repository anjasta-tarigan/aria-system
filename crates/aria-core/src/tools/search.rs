//! `web_search`: query a configured provider and return ranked hits.
//!
//! The provider, key and optional base URL arrive on [`ToolContext::search`]
//! (mirrors how `embed` is carried for Phase 1 embeddings). No search config
//! means the tool is disabled.

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

use super::{default_limit, truncate, Result, ToolContext, ToolError};

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
        .redirect(reqwest::redirect::Policy::default())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Refused(e.to_string()))
}

/// Refuse link-local and cloud metadata addresses (e.g. 169.254.169.254).
/// Loopback and private LAN addresses are allowed because users run SearXNG
/// locally or on their home network via user-configured base_url.
fn guard_url(u: &url::Url) -> Result<()> {
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ToolError::Refused(format!(
            "only http and https URLs are allowed, got {}",
            u.scheme()
        )));
    }
    let host = u
        .host_str()
        .ok_or_else(|| ToolError::Refused("URL has no host".into()))?;
    let port = u.port_or_known_default().unwrap_or(80);
    let addrs: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| ToolError::Refused(format!("cannot resolve {host}: {e}")))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        return Err(ToolError::Refused("host resolved to no addresses".into()));
    }
    for ip in &addrs {
        if is_link_local_or_metadata(ip) {
            return Err(ToolError::Refused(format!(
                "refusing to reach link-local/metadata address ({ip})"
            )));
        }
    }
    Ok(())
}

fn is_link_local_or_metadata(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_unicast_link_local()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
    }
}

fn extract_error_msg(body: &str, status: reqwest::StatusCode) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = val.get("error").or_else(|| val.get("message")) {
            if let Some(s) = err.as_str() {
                return s.to_string();
            }
            if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                return m.to_string();
            }
            return err.to_string();
        }
    }
    let trimmed = body.trim();
    if !trimmed.is_empty() {
        return truncate(trimmed.to_string(), 200);
    }
    status.canonical_reason().unwrap_or("error").to_string()
}

async fn check_response_status(resp: reqwest::Response, provider: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let msg = extract_error_msg(&body, status);
    Err(ToolError::Refused(format!("{provider} returned {status}: {msg}")))
}

/// Read the response body as text, then parse as JSON. On failure, surface the
/// raw body (truncated) so "error decode response body" is never cryptic.
async fn read_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    provider: &str,
) -> Result<T> {
    let body = resp.text().await.map_err(|e| {
        ToolError::Refused(format!("{provider} failed to read response body: {e}"))
    })?;
    serde_json::from_str::<T>(&body).map_err(|_e| {
        ToolError::Refused(format!(
            "{provider} error decode response body: {}",
            truncate(body, 200)
        ))
    })
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
    #[serde(default)]
    results: Vec<ExaHit>,
}
#[derive(Deserialize)]
struct ExaHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

async fn search_exa(cfg: &SearchConfig, query: &str, max: usize) -> Result<Vec<SearchHit>> {
    let key = cfg
        .api_key
        .as_deref()
        .ok_or_else(|| ToolError::Refused("exa requires an api_key".into()))?;
    let resp = client()?
        .post("https://api.exa.ai/search")
        .bearer_auth(key)
        .json(&serde_json::json!({
            "query": query,
            "numResults": max,
            "contents": {
                "text": {
                    "maxCharacters": 1000
                }
            }
        }))
        .send()
        .await
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let resp = check_response_status(resp, "exa").await?;
    let data: ExaResponse = read_json(resp, "exa").await?;
    Ok(data
        .results
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title.unwrap_or_default(),
            url: normalize_url(h.url.as_deref().unwrap_or_default()),
            snippet: h.text.unwrap_or_default(),
        })
        .collect())
}

#[derive(Deserialize)]
struct FirecrawlResponse {
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    data: Option<Vec<FirecrawlHit>>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
struct FirecrawlHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
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
    let resp = check_response_status(resp, "firecrawl").await?;
    let data: FirecrawlResponse = read_json(resp, "firecrawl").await?;
    if data.success == Some(false) || data.data.is_none() {
        if let Some(err) = data.error {
            return Err(ToolError::Refused(format!("firecrawl returned error: {err}")));
        }
    }
    let hits = data.data.unwrap_or_default();
    Ok(hits
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title.unwrap_or_default(),
            url: normalize_url(h.url.as_deref().unwrap_or_default()),
            snippet: h.description.unwrap_or_default(),
        })
        .collect())
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngHit>,
}
#[derive(Deserialize)]
struct SearxngHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content: Option<String>,
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
    let resp = check_response_status(resp, "searxng").await?;
    let data: SearxngResponse = read_json(resp, "searxng").await?;
    Ok(data
        .results
        .into_iter()
        .take(max)
        .map(|h| SearchHit {
            title: h.title.unwrap_or_default(),
            url: normalize_url(h.url.as_deref().unwrap_or_default()),
            snippet: h.content.unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_url_allows_loopback_and_lan() {
        assert!(guard_url(&url::Url::parse("http://127.0.0.1:8888").unwrap()).is_ok());
        assert!(guard_url(&url::Url::parse("http://localhost:2301").unwrap()).is_ok());
        assert!(guard_url(&url::Url::parse("http://192.168.1.1:8080").unwrap()).is_ok());
    }

    #[test]
    fn guard_url_blocks_metadata() {
        let err = guard_url(&url::Url::parse("http://169.254.169.254/latest/meta-data").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("link-local/metadata"));
    }

    #[test]
    fn extract_error_msg_formats_correctly() {
        use reqwest::StatusCode;

        // JSON with "error" string
        assert_eq!(
            extract_error_msg(r#"{"error": "invalid api key"}"#, StatusCode::UNAUTHORIZED),
            "invalid api key"
        );
        // JSON with "error" object containing "message"
        assert_eq!(
            extract_error_msg(
                r#"{"error": {"message": "bad token"}}"#,
                StatusCode::UNAUTHORIZED
            ),
            "bad token"
        );
        // JSON with "message" string
        assert_eq!(
            extract_error_msg(
                r#"{"message": "rate limited"}"#,
                StatusCode::TOO_MANY_REQUESTS
            ),
            "rate limited"
        );
        // Plain text fallback
        assert_eq!(
            extract_error_msg("Bad Gateway", StatusCode::BAD_GATEWAY),
            "Bad Gateway"
        );
        // Empty body fallback to status reason
        assert_eq!(
            extract_error_msg("", StatusCode::NOT_FOUND),
            "Not Found"
        );
    }

    #[test]
    fn exa_deserializes_missing_or_null_fields() {
        let json = r#"{"results": [{"url": "https://example.com", "title": null, "text": null}]}"#;
        let resp: ExaResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, None);
        assert_eq!(resp.results[0].text, None);
        assert_eq!(resp.results[0].url.as_deref(), Some("https://example.com"));

        let empty: ExaResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.results.is_empty());
    }

    #[test]
    fn firecrawl_deserializes_missing_or_null_fields() {
        let json = r#"{"success": true, "data": [{"url": "https://example.com", "title": null}]}"#;
        let resp: FirecrawlResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.as_ref().unwrap().len(), 1);
        assert_eq!(resp.data.as_ref().unwrap()[0].description, None);

        let err_json = r#"{"success": false, "error": "Unauthorized"}"#;
        let resp: FirecrawlResponse = serde_json::from_str(err_json).unwrap();
        assert_eq!(resp.success, Some(false));
        assert_eq!(resp.error.as_deref(), Some("Unauthorized"));
    }

    #[test]
    fn searxng_deserializes_missing_or_null_fields() {
        let json = r#"{"results": [{"url": "https://example.com", "title": null, "content": null}]}"#;
        let resp: SearxngResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, None);
        assert_eq!(resp.results[0].content, None);
    }
}

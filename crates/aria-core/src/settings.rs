//! Settings-domain behaviour: AI provider validation and probing, embedding
//! configuration, web-search verification, and the built-in tool catalog.
//!
//! Kept out of the Tauri adapter so it is testable without a window and so all
//! behaviour lives in `aria-core` (ADR-0001). The Tauri layer only maps these
//! to commands and masks secrets.

use std::time::{Duration, Instant};

use crate::config::{
    AIProvider, ProviderStatus, Settings, WebSearchConfig, WebSearchStatus,
};
use crate::provider::{OpenAiProvider, ProviderError};

pub type Result<T> = std::result::Result<T, SettingsError>;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(String),
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("db: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("network: {0}")]
    Network(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}

/// One entry in the built-in tool catalog shown in Settings.
pub struct ToolInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// True when the tool cannot run until some other section is configured
    /// (currently only web search).
    pub requires_setup: bool,
    /// Approval class the tool runs under ("read"/"mutate"/"exec"/"network"),
    /// shown in the card's Details disclosure.
    pub permission: &'static str,
}

/// The catalog, derived from the real tool registry so it cannot drift.
pub fn tool_catalog() -> Vec<ToolInfo> {
    use crate::tools::TOOLS;
    TOOLS
        .iter()
        .map(|(id, class)| ToolInfo {
            id,
            name: tool_display_name(id),
            description: tool_description(id),
            requires_setup: *id == "web_search",
            permission: class.as_str(),
        })
        .collect()
}

fn tool_display_name(id: &str) -> &'static str {
    match id {
        "read_file" => "File reader",
        "view_image" => "Image viewer",
        "list_dir" => "Folder listing",
        "search_files" => "Workspace search",
        "edit_file" => "Edit file",
        "write_file" => "Write file",
        "delete_file" => "Delete file",
        "move_file" => "Move file",
        "run_shell" => "Shell commands",
        "web_fetch" => "Web fetch",
        "web_search" => "Web search",
        "todos" => "Todo list",
        "remember" => "Remember facts",
        "recall" => "Recall memories",
        "run_background" => "Background tasks",
        "poll_task" => "Task polling",
        "kill_task" => "Task control",
        "delegate" => "Research sub-agent",
        _ => "Tool",
    }
}

fn tool_description(id: &str) -> &'static str {
    match id {
        "read_file" => "Reads a file inside the open workspace.",
        "view_image" => "Views an image with the vision model.",
        "list_dir" => "Lists the contents of a workspace folder.",
        "search_files" => "Searches file contents across the workspace.",
        "edit_file" => "Replaces an exact substring in a workspace file.",
        "write_file" => "Creates or overwrites a workspace file.",
        "delete_file" => "Deletes a file inside the workspace.",
        "move_file" => "Moves or renames a workspace file.",
        "run_shell" => "Runs a shell command in the workspace.",
        "web_fetch" => "Fetches and reads a web page.",
        "web_search" => "Searches the web through a configured provider.",
        "todos" => "Maintains the run's todo list.",
        "remember" => "Persists durable facts for later recall.",
        "recall" => "Recalls previously stored facts.",
        "run_background" => "Starts a long-running command in the background.",
        "poll_task" => "Reads a background task's status and output.",
        "kill_task" => "Stops a background task this chat started.",
        "delegate" => "Runs a read-only research sub-agent and returns its report.",
        _ => "",
    }
}

/// Whether a tool is enabled, given the settings' disabled list.
pub fn tool_enabled(settings: &Settings, id: &str) -> bool {
    settings.tool_enabled(id)
}

/// Trim and validate a base URL. https:// required except for localhost.
pub fn normalize_endpoint(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    let url = url::Url::parse(&trimmed)
        .map_err(|_| SettingsError::Invalid("Enter a valid URL.".into()))?;
    let local = matches!(url.host_str(), Some("localhost") | Some("127.0.0.1") | Some("::1"));
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => {
            return Err(SettingsError::Invalid(
                "Enter a valid URL starting with https:// (http://localhost allowed).".into(),
            ));
        }
    }
    Ok(trimmed)
}

/// Validate a whole provider form. `api_key` may be empty on edit (keep current).
pub fn validate_provider(
    settings: &Settings,
    editing_id: Option<&str>,
    name: &str,
    endpoint: &str,
    model_ids: &[String],
    default_model_id: &str,
    api_key: &str,
) -> Result<()> {
    let name = name.trim();
    if name.len() < 2 || name.len() > 50 {
        return Err(SettingsError::Invalid(
            "Provider name must be 2–50 characters.".into(),
        ));
    }
    let duplicate = settings
        .ai_providers
        .iter()
        .any(|p| p.name == name && Some(p.id.as_str()) != editing_id);
    if duplicate {
        return Err(SettingsError::Invalid(
            "A provider with that name already exists.".into(),
        ));
    }
    normalize_endpoint(endpoint)?;
    if model_ids.is_empty() {
        return Err(SettingsError::Invalid("Add at least one model id.".into()));
    }
    if model_ids.len() > 50 {
        return Err(SettingsError::Invalid(
            "A provider can have at most 50 models.".into(),
        ));
    }
    if model_ids.len() > 1 && !model_ids.iter().any(|m| m == default_model_id) {
        return Err(SettingsError::Invalid(
            "Choose a default model from the model list.".into(),
        ));
    }
    if editing_id.is_none() && api_key.trim().is_empty() {
        return Err(SettingsError::Invalid("Enter the provider's API key.".into()));
    }
    Ok(())
}

/// Trim, dedupe (case-sensitive), drop empties, and cap at 50.
pub fn dedupe_models(ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in ids {
        let t = raw.trim();
        if t.is_empty() || out.iter().any(|m| m == t) {
            continue;
        }
        out.push(t.to_string());
        if out.len() == 50 {
            break;
        }
    }
    out
}

/// Result of a provider connectivity probe.
pub struct ProbeOutcome {
    pub ok: bool,
    pub latency_ms: u64,
    /// Friendly message for the user on failure (401/404/timeout/network).
    pub error: Option<String>,
    /// Set by embedding probes so the UI can show the vector width.
    pub vector_length: Option<usize>,
}

/// Probe a provider by listing its models; gateways that do not implement
/// `/models` fall back to a one-token chat completion when a model is known.
pub async fn probe_provider(
    endpoint: &str,
    api_key: &str,
    model: Option<&str>,
    timeout: Duration,
) -> ProbeOutcome {
    let start = Instant::now();
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return ProbeOutcome {
                ok: false,
                latency_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Could not build HTTP client: {e}")),
                vector_length: None,
            };
        }
    };
    let base = endpoint.trim_end_matches('/');
    let url = format!("{base}/models");
    match client.get(&url).bearer_auth(api_key).send().await {
        Ok(resp) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            match status {
                401 | 403 => ProbeOutcome {
                    ok: false,
                    latency_ms,
                    error: Some("Invalid API key".into()),
                    vector_length: None,
                },
                404 => match model.filter(|m| !m.trim().is_empty()) {
                    // A gateway without /models: prove the chat route instead.
                    Some(m) => probe_chat(&client, base, api_key, m, start).await,
                    None => ProbeOutcome {
                        ok: false,
                        latency_ms,
                        error: Some("Endpoint not found (no /models route)".into()),
                        vector_length: None,
                    },
                },
                s if s < 500 => ProbeOutcome {
                    ok: true,
                    latency_ms,
                    error: None,
                    vector_length: None,
                },
                s => ProbeOutcome {
                    ok: false,
                    latency_ms,
                    error: Some(format!("Provider returned HTTP {s}")),
                    vector_length: None,
                },
            }
        }
        Err(e) => ProbeOutcome {
            ok: false,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(transport_error(&e)),
            vector_length: None,
        },
    }
}

/// One-token chat completion, used when `/models` is missing.
async fn probe_chat(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    model: &str,
    start: Instant,
) -> ProbeOutcome {
    let url = format!("{base}/chat/completions");
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
    });
    match client
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            match status {
                401 | 403 => ProbeOutcome {
                    ok: false,
                    latency_ms,
                    error: Some("Invalid API key".into()),
                    vector_length: None,
                },
                404 => ProbeOutcome {
                    ok: false,
                    latency_ms,
                    error: Some("Endpoint not found".into()),
                    vector_length: None,
                },
                s if s < 500 => ProbeOutcome {
                    ok: true,
                    latency_ms,
                    error: None,
                    vector_length: None,
                },
                s => ProbeOutcome {
                    ok: false,
                    latency_ms,
                    error: Some(format!("Provider returned HTTP {s}")),
                    vector_length: None,
                },
            }
        }
        Err(e) => ProbeOutcome {
            ok: false,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(transport_error(&e)),
            vector_length: None,
        },
    }
}

/// One user-facing sentence for a transport failure.
fn transport_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Timed out reaching the provider".into()
    } else if e.is_connect() {
        "Could not connect to the provider".into()
    } else {
        format!("Network error: {e}")
    }
}

/// Fetch the provider's model ids from `{endpoint}/models`.
pub async fn fetch_models(
    endpoint: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let url = format!("{}/models", endpoint.trim_end_matches('/'));
    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| SettingsError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SettingsError::Network(format!(
            "Provider returned {}",
            resp.status().as_u16()
        )));
    }
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| SettingsError::Network(e.to_string()))?;
    let models = value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

/// Embed a short sample string and report the vector length, for the
/// embedding "Test" button.
pub async fn probe_embedding(
    provider: &OpenAiProvider,
    model: &str,
    timeout: Duration,
) -> ProbeOutcome {
    let start = Instant::now();
    // `embed` uses the provider's configured model; a custom model override is
    // not supported by the client, so this validates the configured path.
    let _ = model;
    let result = tokio::time::timeout(timeout, provider.embed("aria embedding test")).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match result {
        Ok(Ok(vector)) => ProbeOutcome {
            ok: true,
            latency_ms,
            error: None,
            vector_length: Some(vector.len()),
        },
        Ok(Err(e)) => ProbeOutcome {
            ok: false,
            latency_ms,
            error: Some(friendly_provider_error(&e)),
            vector_length: None,
        },
        Err(_) => ProbeOutcome {
            ok: false,
            latency_ms,
            error: Some("Timed out embedding the sample".into()),
            vector_length: None,
        },
    }
}

fn friendly_provider_error(e: &ProviderError) -> String {
    match e {
        ProviderError::Status { status, .. } if *status == 401 => "Invalid API key".into(),
        ProviderError::Status { status, .. } if *status == 404 => "Endpoint not found".into(),
        ProviderError::Unreachable(_) => "Provider unreachable".into(),
        ProviderError::Http(_) => "Network error".into(),
        other => other.to_string(),
    }
}

/// Validate and store the web-search provider choice. The key itself is stored
/// by the caller (secret store); this only builds the persisted metadata.
pub fn web_search_config(provider_key: &str, key: &str) -> Result<WebSearchConfig> {
    let provider = provider_key.trim();
    if !matches!(provider, "exa" | "firecrawl" | "searxng" | "duckduckgo") {
        return Err(SettingsError::Invalid("Unknown search provider.".into()));
    }
    if provider != "searxng" && provider != "duckduckgo" && key.trim().is_empty() {
        return Err(SettingsError::Invalid("Paste your API key.".into()));
    }
    Ok(WebSearchConfig {
        provider_key: provider.to_string(),
        api_key_masked: mask_key(key),
        status: WebSearchStatus::Connected,
        last_verified_at: Some(now_ms()),
    })
}

/// Mask a key for display: first 2 and last 4 chars, dots between.
pub fn mask_key(key: &str) -> String {
    let k = key.trim();
    let chars: Vec<char> = k.chars().collect();
    if chars.len() <= 6 {
        return "••••".to_string();
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}••••{tail}")
}

/// Validate an embedding config against the spec's ranges.
pub fn validate_embedding(
    model: &str,
    dimensions: Option<u32>,
    batch_size: u32,
    timeout_seconds: u32,
) -> Result<()> {
    if model.trim().is_empty() {
        return Err(SettingsError::Invalid("Choose an embedding model.".into()));
    }
    if let Some(d) = dimensions
        && !(64..=8192).contains(&d)
    {
        return Err(SettingsError::Invalid(
            "Dimensions must be between 64 and 8192.".into(),
        ));
    }
    if !(1..=256).contains(&batch_size) {
        return Err(SettingsError::Invalid(
            "Batch size must be between 1 and 256.".into(),
        ));
    }
    if !(5..=120).contains(&timeout_seconds) {
        return Err(SettingsError::Invalid(
            "Timeout must be between 5 and 120 seconds.".into(),
        ));
    }
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mark a provider's status after a probe.
pub fn apply_probe_status(provider: &mut AIProvider, ok: bool) {
    provider.status = if ok {
        ProviderStatus::Active
    } else {
        ProviderStatus::Error
    };
    provider.last_tested_at = Some(now_ms());
}

/// Recompute every memory's embedding with the configured embedder. Returns how
/// many rows were updated; a failure on one row aborts so the index never ends
/// up half-migrated with no signal.
pub async fn reindex_memories(
    db: &crate::db::Db,
    embedder: &dyn crate::provider::Embedder,
) -> Result<usize> {
    let memories = db.call(crate::db::list_all_memories)?;
    let mut updated = 0;
    for m in memories {
        let vector = embedder.embed(&m.content).await?;
        let bytes = crate::db::floats_to_bytes(&vector);
        db.call(|c| crate::db::set_memory_embedding(c, &m.id, &bytes))?;
        updated += 1;
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn normalize_endpoint_trims_and_requires_https() {
        assert_eq!(
            normalize_endpoint(" https://api.example.com/v1/ ").unwrap(),
            "https://api.example.com/v1"
        );
        assert!(normalize_endpoint("http://localhost:11434/v1").is_ok());
        assert!(normalize_endpoint("http://example.com/v1").is_err());
        assert!(normalize_endpoint("not-a-url").is_err());
    }

    #[test]
    fn dedupe_models_is_case_sensitive_and_trims() {
        let out = dedupe_models(&[
            " a ".into(),
            "a".into(),
            "A".into(),
            "".into(),
            "b".into(),
        ]);
        assert_eq!(out, vec!["a", "A", "b"]);
    }

    #[test]
    fn provider_validation_rejects_bad_forms() {
        let s = base_settings();
        let models = vec!["m1".to_string()];
        assert!(validate_provider(&s, None, "a", "https://x.test", &models, "m1", "k").is_err());
        assert!(validate_provider(&s, None, "Good", "http://x.test", &models, "m1", "k").is_err());
        assert!(validate_provider(&s, None, "Good", "https://x.test", &[], "", "k").is_err());
        assert!(validate_provider(&s, None, "Good", "https://x.test", &models, "m1", "").is_err());
        assert!(validate_provider(&s, None, "Good", "https://x.test", &models, "m1", "k").is_ok());
    }

    #[test]
    fn mask_key_keeps_ends() {
        assert_eq!(mask_key("sk-1234567890abcdef"), "sk••••cdef");
        assert_eq!(mask_key("abc"), "••••");
        assert_eq!(mask_key("🔑-1234567890abcdef"), "🔑-••••cdef");
        assert_eq!(mask_key("日本語12345678"), "日本••••5678");
        assert_eq!(mask_key("🦀"), "••••");
    }

    #[test]
    fn embedding_validation_ranges() {
        assert!(validate_embedding("m", Some(1536), 64, 30).is_ok());
        assert!(validate_embedding("", Some(1536), 64, 30).is_err());
        assert!(validate_embedding("m", Some(63), 64, 30).is_err());
        assert!(validate_embedding("m", None, 0, 30).is_err());
        assert!(validate_embedding("m", None, 64, 4).is_err());
    }

    #[test]
    fn catalog_covers_registry_and_flags_web_search() {
        let cat = tool_catalog();
        assert_eq!(cat.len(), crate::tools::TOOLS.len());
        let ws = cat.iter().find(|t| t.id == "web_search").unwrap();
        assert!(ws.requires_setup);
    }

    #[test]
    fn sync_runtime_endpoint_uses_default_provider() {
        let mut s = base_settings();
        s.ai_providers.push(AIProvider {
            id: "p1".into(),
            name: "P".into(),
            provider_type: "openai-compatible".into(),
            endpoint_url: "https://api.example.com/v1".into(),
            model_ids: vec!["m1".into(), "m2".into()],
            default_model_id: "m2".into(),
            is_default: true,
            status: ProviderStatus::Untested,
            last_tested_at: None,
        });
        s.default_ai_provider_id = Some("p1".into());
        s.sync_runtime_endpoint();
        assert_eq!(s.base_url, "https://api.example.com/v1");
        assert_eq!(s.model, "m2");
    }
}

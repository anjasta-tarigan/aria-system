//! Settings and API-key storage.
//!
//! One Provider (base URL, model) lives in a TOML-free JSON file in the app data
//! directory; the API key never does (issue 06). The key goes to the OS secret
//! service when one is reachable, otherwise to a `0600` file beside the config,
//! and the settings report which one is in use.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

const KEYRING_SERVICE: &str = "dev.aria.app";
const KEYRING_USER: &str = "provider-api-key";
const SEARCH_KEYRING_USER: &str = "search-api-key";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("secret storage: {0}")]
    Secret(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Defaults for a [`Run`](crate::GLOSSARY)'s Caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    pub max_turns: u32,
    pub max_prompt_tokens: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_turns: 20,
            max_prompt_tokens: 200_000,
        }
    }
}

impl Caps {
    /// The run is nudged to wrap up once either cap is 70% consumed.
    pub fn soft_limit_reached(&self, turns: u32, prompt_tokens: u64) -> bool {
        let turns_hit = u64::from(turns) * 100 >= u64::from(self.max_turns) * 70;
        let tokens_hit = prompt_tokens * 100 >= self.max_prompt_tokens * 70;
        turns_hit || tokens_hit
    }

    pub fn hard_limit_reached(&self, turns: u32, prompt_tokens: u64) -> bool {
        turns >= self.max_turns || prompt_tokens >= self.max_prompt_tokens
    }
}

/// When the agent may run certain tool classes without a per-call approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Every mutate/exec/network call waits for the user (current behavior).
    #[default]
    AlwaysAsk,
    /// Read everywhere. File mutations (edit/write/delete/move) and network
    /// (web search/fetch) run without asking in every scope. Shell runs
    /// silently too unless the command is destructive (`rm -rf`, `sudo`,
    /// `npm install`, ...); only those harmful commands still ask. A
    /// restricted Main System still asks for file calls naming a path outside
    /// its allowed roots.
    AutoApproveSafe,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalPolicy::AlwaysAsk => "always_ask",
            ApprovalPolicy::AutoApproveSafe => "auto_approve_safe",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "always_ask" => Some(ApprovalPolicy::AlwaysAsk),
            "auto_approve_safe" => Some(ApprovalPolicy::AutoApproveSafe),
            _ => None,
        }
    }
}

/// A model available on an AI provider, with its technical API ID and user display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    pub name: String,
}

impl ProviderModel {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let id = id.into().trim().to_string();
        let n = name.into().trim().to_string();
        let name = if n.is_empty() { id.clone() } else { n };
        Self { id, name }
    }
}

impl From<&str> for ProviderModel {
    fn from(s: &str) -> Self {
        Self::new(s, s)
    }
}

impl From<String> for ProviderModel {
    fn from(s: String) -> Self {
        Self::new(s.clone(), s)
    }
}

impl<'de> Deserialize<'de> for ProviderModel {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Str(String),
            Obj {
                id: String,
                #[serde(default)]
                name: Option<String>,
            },
        }

        match Helper::deserialize(deserializer)? {
            Helper::Str(s) => {
                let id = s.trim().to_string();
                Ok(ProviderModel {
                    name: id.clone(),
                    id,
                })
            }
            Helper::Obj { id, name } => {
                let id = id.trim().to_string();
                let name = match name {
                    Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                    _ => id.clone(),
                };
                Ok(ProviderModel { id, name })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AIProvider {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default = "openai_type")]
    pub provider_type: String,
    pub endpoint_url: String,
    #[serde(default, alias = "model_ids", alias = "modelIds")]
    pub models: Vec<ProviderModel>,
    pub default_model_id: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub status: ProviderStatus,
    pub last_tested_at: Option<i64>,
}

impl AIProvider {
    pub fn model_ids(&self) -> Vec<String> {
        self.models.iter().map(|m| m.id.clone()).collect()
    }
}

fn openai_type() -> String {
    "openai-compatible".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    #[default]
    Untested,
    Active,
    Error,
}

/// Embedding (vector search) configuration. Not a provider list: Aria runs one
/// embedding model against a chosen provider (or the default AI provider).
///
/// `dimensions` is left `None` (auto): the provider reports the vector width,
/// so the user never has to guess it. `batch_size` and `timeout_seconds` were
/// removed: embeddings use the hardcoded recommended values (batch 64, timeout
/// 30s) and are not user-tunable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingConfig {
    /// Whether semantic memory search is on. Off means recall uses the FTS5
    /// keyword index only and `remember` stores no vector.
    pub enabled: bool,
    pub provider_id: String,
    pub model: String,
    /// `None` = use the model's native vector width (auto).
    pub dimensions: Option<u32>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider_id: "default".to_string(),
            model: "text-embedding-3-small".to_string(),
            dimensions: None,
        }
    }
}

/// Reranker (cross-encoder) configuration for the retrieval pipeline.
///
/// After the embedding stage retrieves a candidate pool, a reranker model
/// re-scores each (query, candidate) pair for precision. Reranking is
/// significantly more accurate than embedding similarity alone but costs more
/// per call, so it runs only on the top-k candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RerankerConfig {
    /// Whether cross-encoder reranking is on.
    pub enabled: bool,
    pub provider_id: String,
    pub model: String,
    /// How many candidates to pass to the reranker (from the embedding pool).
    pub top_k: usize,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider_id: "default".to_string(),
            model: "nvidia/Nemotron-3.5-Rerank-8B-BF16".to_string(),
            top_k: 50,
        }
    }
}

/// The one connected web-search provider. The key itself lives in the secret
/// store; only its mask and metadata are persisted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    pub provider_key: String,
    pub api_key_masked: String,
    #[serde(default)]
    pub status: WebSearchStatus,
    pub last_verified_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchStatus {
    #[default]
    Disconnected,
    Connected,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub search_provider: Option<String>,
    pub search_base_url: Option<String>,
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
    /// Configured OpenAI-compatible chat providers. The default one also drives
    /// the runtime `base_url`/`model` (kept in sync by the settings commands).
    #[serde(default)]
    pub ai_providers: Vec<AIProvider>,
    #[serde(default)]
    pub default_ai_provider_id: Option<String>,
    #[serde(default)]
    pub embedding_config: EmbeddingConfig,
    #[serde(default)]
    pub web_search_config: Option<WebSearchConfig>,
    /// Reranker (cross-encoder) for the retrieval pipeline.
    #[serde(default)]
    pub reranker_config: RerankerConfig,
    /// Ids of built-in tools the user has turned off. Absent = all enabled, so
    /// the tool registry stays the source of truth for the tool list.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Side panel UI state, persisted so a user's layout choice survives
    /// app restarts.
    #[serde(default)]
    pub side_panel_open: bool,
    #[serde(default)]
    pub side_panel_dismissed: bool,
    #[serde(default)]
    pub side_panel_width: u32,
    #[serde(default)]
    pub side_panel_tab: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            model: String::new(),
            embedding_base_url: None,
            embedding_model: None,
            search_provider: None,
            search_base_url: None,
            approval_policy: ApprovalPolicy::AutoApproveSafe,
            ai_providers: Vec::new(),
            default_ai_provider_id: None,
            embedding_config: EmbeddingConfig::default(),
            web_search_config: None,
            reranker_config: RerankerConfig::default(),
            disabled_tools: Vec::new(),
            side_panel_open: true,
            side_panel_dismissed: false,
            side_panel_width: 440,
            side_panel_tab: "artifacts".to_string(),
        }
    }
}

impl Settings {
    /// The provider that supplies the runtime endpoint, if one is configured.
    pub fn active_provider(&self) -> Option<&AIProvider> {
        self.ai_providers
            .iter()
            .find(|p| Some(&p.id) == self.default_ai_provider_id.as_ref())
            .or_else(|| self.ai_providers.iter().find(|p| p.is_default))
    }

    /// Keep the flat runtime endpoint in sync with the default provider, so the
    /// agent keeps working unchanged. A no-op when no provider is configured.
    pub fn sync_runtime_endpoint(&mut self) {
        let Some(p) = self.active_provider() else {
            return;
        };
        let base_url = p.endpoint_url.clone();
        let model = if p.default_model_id.is_empty() {
            p.models.first().map(|m| m.id.clone()).unwrap_or_default()
        } else {
            p.default_model_id.clone()
        };
        self.base_url = base_url;
        self.model = model;
    }

    /// Whether a built-in tool is enabled (absent from the disabled list).
    pub fn tool_enabled(&self, id: &str) -> bool {
        !self.disabled_tools.iter().any(|t| t == id)
    }
}

/// Where the API key is currently kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBackend {
    Keyring,
    FileFallback,
}

impl SecretBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretBackend::Keyring => "keyring",
            SecretBackend::FileFallback => "file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretStatus {
    pub present: bool,
    pub backend: SecretBackend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    dir: PathBuf,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Aria's own scratch/home directory for the Main System chat: shell cwd,
    /// background task root and the place files land when no absolute path is
    /// given. Sits beside the config directory in the app data dir.
    pub fn system_dir(&self) -> PathBuf {
        self.dir
            .parent()
            .unwrap_or(&self.dir)
            .join("system")
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("api-key")
    }

    fn search_key_path(&self) -> PathBuf {
        self.dir.join("search-api-key")
    }

    pub fn load(&self) -> Result<Settings> {
        match std::fs::read_to_string(self.settings_path()) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, settings: &Settings) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.settings_path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
        std::fs::rename(&tmp, self.settings_path())?;
        Ok(())
    }

    fn keyring(&self) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()
    }

    fn search_keyring(&self) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, SEARCH_KEYRING_USER).ok()
    }

    /// Store the key. Prefers the OS secret service.
    pub fn set_api_key(&self, key: &str) -> Result<SecretBackend> {
        if let Some(entry) = self.keyring()
            && entry.set_password(key).is_ok()
        {
            // Do not leave a stale copy behind in the fallback file.
            let _ = std::fs::remove_file(self.key_path());
            return Ok(SecretBackend::Keyring);
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.key_path(), key)?;
        restrict_permissions(&self.key_path())?;
        Ok(SecretBackend::FileFallback)
    }

    pub fn api_key(&self) -> Result<Option<(String, SecretBackend)>> {
        if let Ok(settings) = self.load() {
            let default_id = settings
                .default_ai_provider_id
                .as_deref()
                .or_else(|| settings.active_provider().map(|p| p.id.as_str()));
            if let Some(id) = default_id
                && let Some(pair) = self.provider_api_key_with_backend(id)?
            {
                return Ok(Some(pair));
            }
        }
        if let Some(entry) = self.keyring()
            && let Ok(key) = entry.get_password()
            && !key.is_empty()
        {
            return Ok(Some((key, SecretBackend::Keyring)));
        }
        match std::fs::read_to_string(self.key_path()) {
            Ok(key) if !key.trim().is_empty() => {
                Ok(Some((key.trim().to_string(), SecretBackend::FileFallback)))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn secret_status(&self) -> Result<SecretStatus> {
        match self.api_key()? {
            Some((_, backend)) => Ok(SecretStatus {
                present: true,
                backend,
            }),
            None => Ok(SecretStatus {
                present: false,
                backend: if self.keyring().is_some() {
                    SecretBackend::Keyring
                } else {
                    SecretBackend::FileFallback
                },
            }),
        }
    }

    pub fn clear_api_key(&self) -> Result<()> {
        if let Some(entry) = self.keyring() {
            let _ = entry.delete_credential();
        }
        match std::fs::remove_file(self.key_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Store the search key, preferring the OS secret service.
    pub fn set_search_api_key(&self, key: &str) -> Result<SecretBackend> {
        if let Some(entry) = self.search_keyring()
            && entry.set_password(key).is_ok()
        {
            let _ = std::fs::remove_file(self.search_key_path());
            return Ok(SecretBackend::Keyring);
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.search_key_path(), key)?;
        restrict_permissions(&self.search_key_path())?;
        Ok(SecretBackend::FileFallback)
    }

    pub fn search_api_key(&self) -> Result<Option<(String, SecretBackend)>> {
        if let Some(entry) = self.search_keyring()
            && let Ok(key) = entry.get_password()
            && !key.is_empty()
        {
            return Ok(Some((key, SecretBackend::Keyring)));
        }
        match std::fs::read_to_string(self.search_key_path()) {
            Ok(key) if !key.trim().is_empty() => {
                Ok(Some((key.trim().to_string(), SecretBackend::FileFallback)))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn clear_search_api_key(&self) -> Result<()> {
        if let Some(entry) = self.search_keyring() {
            let _ = entry.delete_credential();
        }
        match std::fs::remove_file(self.search_key_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ---------------------------------------------- per-provider API keys
    // Keys are stored under `provider:{id}`, never in settings.json.

    fn provider_keyring(&self, provider_id: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("provider:{provider_id}")).ok()
    }

    fn provider_key_path(&self, provider_id: &str) -> PathBuf {
        // Provider ids are UUID v7, so they are filesystem-safe as-is.
        self.dir.join(format!("provider-{provider_id}.key"))
    }

    pub fn set_provider_api_key(&self, provider_id: &str, key: &str) -> Result<SecretBackend> {
        if let Some(entry) = self.provider_keyring(provider_id)
            && entry.set_password(key).is_ok()
        {
            let _ = std::fs::remove_file(self.provider_key_path(provider_id));
            return Ok(SecretBackend::Keyring);
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.provider_key_path(provider_id);
        std::fs::write(&path, key)?;
        restrict_permissions(&path)?;
        Ok(SecretBackend::FileFallback)
    }

    pub fn provider_api_key_with_backend(
        &self,
        provider_id: &str,
    ) -> Result<Option<(String, SecretBackend)>> {
        if let Some(entry) = self.provider_keyring(provider_id)
            && let Ok(key) = entry.get_password()
            && !key.is_empty()
        {
            return Ok(Some((key, SecretBackend::Keyring)));
        }
        match std::fs::read_to_string(self.provider_key_path(provider_id)) {
            Ok(key) if !key.trim().is_empty() => {
                Ok(Some((key.trim().to_string(), SecretBackend::FileFallback)))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn provider_api_key(&self, provider_id: &str) -> Result<Option<String>> {
        Ok(self.provider_api_key_with_backend(provider_id)?.map(|(k, _)| k))
    }

    pub fn clear_provider_api_key(&self, provider_id: &str) -> Result<()> {
        if let Some(entry) = self.provider_keyring(provider_id) {
            let _ = entry.delete_credential();
        }
        match std::fs::remove_file(self.provider_key_path(provider_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

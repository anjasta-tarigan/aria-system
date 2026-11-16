//! Settings and API-key storage.
//!
//! One Provider (base URL, model) lives in a TOML-free JSON file in the app data
//! directory; the API key never does (issue 06). The key goes to the OS secret
//! service when one is reachable, otherwise to a `0600` file beside the config,
//! and the settings report which one is in use.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
            max_turns: 50,
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
    /// Read, file mutations (edit/write/delete/move) and network (web
    /// search/fetch) run automatically; only shell (Exec) stays gated.
    AutoApproveSafe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AIProvider {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default = "openai_type")]
    pub provider_type: String,
    pub endpoint_url: String,
    pub model_ids: Vec<String>,
    pub default_model_id: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub status: ProviderStatus,
    pub last_tested_at: Option<i64>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingConfig {
    /// Whether semantic memory search is on. Off means recall uses the FTS5
    /// keyword index only and `remember` stores no vector.
    pub enabled: bool,
    pub provider_id: String,
    pub model: String,
    pub dimensions: Option<u32>,
    pub batch_size: u32,
    pub timeout_seconds: u32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider_id: "default".to_string(),
            model: "text-embedding-3-small".to_string(),
            dimensions: None,
            batch_size: 64,
            timeout_seconds: 30,
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
    pub caps: Caps,
    pub shell_timeout_secs: u64,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub search_provider: Option<String>,
    pub search_base_url: Option<String>,
    /// Vision (image input) model id; falls back to `model` when absent.
    pub vision_model: Option<String>,
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
    /// Ids of built-in tools the user has turned off. Absent = all enabled, so
    /// the tool registry stays the source of truth for the tool list.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            model: String::new(),
            caps: Caps::default(),
            shell_timeout_secs: 120,
            embedding_base_url: None,
            embedding_model: None,
            search_provider: None,
            search_base_url: None,
            vision_model: None,
            approval_policy: ApprovalPolicy::AlwaysAsk,
            ai_providers: Vec::new(),
            default_ai_provider_id: None,
            embedding_config: EmbeddingConfig::default(),
            web_search_config: None,
            disabled_tools: Vec::new(),
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
            p.model_ids.first().cloned().unwrap_or_default()
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

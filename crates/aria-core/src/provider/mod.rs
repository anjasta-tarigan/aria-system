//! OpenAI-compatible chat completions client: streaming, tool calls, cancel.
//!
//! The client turns the SSE body into a small [`ProviderEvent`] vocabulary; the
//! Run loop accumulates tool-call fragments into complete calls (issue 04,
//! ADR-0001).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{Config, Settings};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider unreachable: {0}")]
    Unreachable(String),
    #[error("no API key configured")]
    MissingKey,
    #[error("no model configured")]
    MissingModel,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("stream ended before the message was complete")]
    Truncated,
    #[error("malformed stream data: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, ProviderError>;

impl ProviderError {
    /// Worth retrying: rate limits, server hiccups and transport failures.
    /// Anything the request itself got wrong (auth, bad model, malformed body)
    /// fails again the same way, so it is not retried.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unreachable(_) | Self::Truncated => true,
            Self::Http(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            Self::Status { status, .. } => {
                matches!(status, 408 | 409 | 425 | 429 | 500..=599)
            }
            _ => false,
        }
    }

    /// The provider refused the request because it is longer than the model's
    /// window. Every provider words this differently, so match the usual
    /// phrasings; an unrecognized rejection still fails the Run as before.
    pub fn is_context_overflow(&self) -> bool {
        let Self::Status { status, body } = self else {
            return false;
        };
        if !matches!(status, 400 | 413) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        [
            "context length",
            "context_length",
            "context window",
            "maximum context",
            "too many tokens",
            "reduce the length",
            "prompt is too long",
            "input is too long",
            "exceeds the maximum",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }
}

/// One message as the API sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        /// Multimodal content parts (text + images). When non-empty, the
        /// provider serializes `content` as an array of parts instead of a
        /// plain string.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_parts: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Assistant {
        content: String,
        /// Chain-of-thought / reasoning the model produced before the final
        /// answer. Echoed back in conversation history so providers that
        /// support it (e.g. OpenAI o1) can see their own prior reasoning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// Multimodal content parts (text + images).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_parts: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Tool {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
}

/// One part of a multimodal message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

/// An image URL part, matching the OpenAI `image_url` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            content_parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCallRequest>) -> Self {
        Self::Assistant {
            content: content.into(),
            content_parts: Vec::new(),
            reasoning: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Assistant message with chain-of-thought reasoning.
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning: impl Into<String>,
        tool_calls: Vec<ToolCallRequest>,
    ) -> Self {
        Self::Assistant {
            content: content.into(),
            content_parts: Vec::new(),
            reasoning: Some(reasoning.into()),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            content: content.into(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// User message with text plus image attachments (data URLs).
    pub fn user_with_images(
        content: impl Into<String>,
        images: Vec<String>,
    ) -> Self {
        let mut parts: Vec<ContentPart> = Vec::new();
        let text = content.into();
        if !text.is_empty() {
            parts.push(ContentPart::Text { text: text.clone() });
        }
        for url in images {
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl { url, detail: None },
            });
        }
        Self::User {
            content: text,
            content_parts: parts,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Convert to a `serde_json::Value` suitable for the OpenAI chat/completions
    /// body. When `content_parts` is non-empty, `content` is serialized as an
    /// array of parts; otherwise as a plain string (backward compatible).
    pub fn to_openai_value(&self) -> serde_json::Value {
        match self {
            ChatMessage::System { content } => {
                serde_json::json!({ "role": "system", "content": content })
            }
            ChatMessage::User {
                content,
                content_parts,
                tool_calls,
                tool_call_id,
                ..
            } => {
                let mut map = serde_json::Map::new();
                map.insert("role".into(), serde_json::Value::String("user".into()));
                if !content_parts.is_empty() {
                    map.insert(
                        "content".into(),
                        serde_json::to_value(content_parts).unwrap_or(serde_json::Value::String(
                            content.clone(),
                        )),
                    );
                } else {
                    map.insert("content".into(), serde_json::Value::String(content.clone()));
                }
                if !tool_calls.is_empty() {
                    map.insert(
                        "tool_calls".into(),
                        serde_json::to_value(tool_calls).unwrap_or(serde_json::Value::Null),
                    );
                }
                if let Some(id) = tool_call_id {
                    map.insert(
                        "tool_call_id".into(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                serde_json::Value::Object(map)
            }
            ChatMessage::Assistant {
                content,
                reasoning,
                content_parts,
                tool_calls,
                tool_call_id,
                ..
            } => {
                let mut map = serde_json::Map::new();
                map.insert("role".into(), serde_json::Value::String("assistant".into()));
                if !content_parts.is_empty() {
                    map.insert(
                        "content".into(),
                        serde_json::to_value(content_parts).unwrap_or(serde_json::Value::String(
                            content.clone(),
                        )),
                    );
                } else {
                    map.insert("content".into(), serde_json::Value::String(content.clone()));
                }
                if let Some(r) = reasoning {
                    map.insert("reasoning".into(), serde_json::Value::String(r.clone()));
                }
                if !tool_calls.is_empty() {
                    map.insert(
                        "tool_calls".into(),
                        serde_json::to_value(tool_calls).unwrap_or(serde_json::Value::Null),
                    );
                }
                if let Some(id) = tool_call_id {
                    map.insert(
                        "tool_call_id".into(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                serde_json::Value::Object(map)
            }
            ChatMessage::Tool {
                content,
                tool_call_id,
            } => {
                let mut map = serde_json::Map::new();
                map.insert("role".into(), serde_json::Value::String("tool".into()));
                map.insert("content".into(), serde_json::Value::String(content.clone()));
                if let Some(id) = tool_call_id {
                    map.insert(
                        "tool_call_id".into(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                serde_json::Value::Object(map)
            }
        }
    }
}

/// A complete tool call requested by the model.
///
/// Serialises in the OpenAI wire shape
/// `{"id": …, "type": "function", "function": {"name", "arguments"}}`, because
/// these are echoed back in the assistant message of the next request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub function: FunctionCall,
}

/// The `function` member of a [`ToolCallRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

impl Serialize for ToolCallRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ToolCallRequest", 3)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("type", "function")?;
        st.serialize_field("function", &self.function)?;
        st.end()
    }
}

impl ToolCallRequest {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.function.name
    }

    pub fn arguments(&self) -> &str {
        &self.function.arguments
    }
}

/// A tool the model may call, described in OpenAI function JSON Schema form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Incremental output of one Turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    /// Incremental chain-of-thought / reasoning text.
    ReasoningDelta(String),
    /// A tool call is complete and ready to execute.
    ToolCall(ToolCallRequest),
    /// Prompt tokens for this Turn, when the provider reports usage.
    Usage { prompt_tokens: u64 },
}

/// The model's full answer to one Turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub text: String,
    /// Reasoning produced before the final answer / tool calls.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCallRequest>,
    pub prompt_tokens: Option<u64>,
}

/// A provider-agnostic interface so the Run loop can be tested without network.
///
/// The return type is a boxed future rather than `async fn` so the trait stays
/// object-safe and the Run loop can hold an `Arc<dyn Provider>`.
pub trait Provider: Send + Sync {
    fn stream_chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResult>> + Send + 'a>>;
}

/// Turns text into an embedding vector. Implemented by [`OpenAiProvider`]; absent
/// for test fakes, in which case memory recall falls back to a textual listing.
pub trait Embedder: Send + Sync {
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>>> + Send + 'a>>;
}

impl Embedder for OpenAiProvider {
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>>> + Send + 'a>> {
        Box::pin(self.embed(text))
    }
}

/// Scores (query, candidate) pairs for retrieval precision. A reranker is
/// significantly more accurate than embedding similarity alone but costs more
/// per call, so it runs only on the top-k candidates from the embedding pool.
pub trait Reranker: Send + Sync {
    /// Score one candidate against the query. Higher = more relevant.
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        candidate: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<f32>> + Send + 'a>>;
}

/// Reranks candidates using the embedding model itself: embeds the query and
/// each candidate, then scores by cosine similarity. Cheaper than a dedicated
/// cross-encoder but less precise; used when no reranker model is configured.
pub struct CosineReranker {
    embed: Arc<dyn Embedder>,
}

impl CosineReranker {
    pub fn new(embed: Arc<dyn Embedder>) -> Self {
        Self { embed }
    }
}

impl Reranker for CosineReranker {
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        candidate: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<f32>> + Send + 'a>> {
        let embed = Arc::clone(&self.embed);
        Box::pin(async move {
            let q = embed.embed(query).await?;
            let c = embed.embed(candidate).await?;
            Ok(cosine_similarity(&q, &c))
        })
    }
}

impl Reranker for OpenAiProvider {
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        candidate: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<f32>> + Send + 'a>> {
        Box::pin(self.rerank(query, candidate))
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    settings: Settings,
    api_key: String,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("base_url", &self.settings.base_url)
            .field("model", &self.settings.model)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Build from settings and the stored key.
    pub fn from_config(config: &Config) -> Result<Self> {
        let settings = config.load()?;
        let (api_key, _) = config.api_key()?.ok_or(ProviderError::MissingKey)?;
        Self::new(settings, api_key)
    }

    pub fn new(settings: Settings, api_key: impl Into<String>) -> Result<Self> {
        if settings.model.trim().is_empty() {
            return Err(ProviderError::MissingModel);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            settings,
            api_key: api_key.into(),
        })
    }

    /// Whether freshly loaded settings would build the same client. Used by the
    /// Run loop to hot-reload only on a real endpoint/model/key change, keeping
    /// the connection pool across Turns.
    pub fn same_config(&self, settings: &Settings, api_key: &str) -> bool {
        self.settings.base_url == settings.base_url
            && self.settings.model == settings.model
            && self.settings.embedding_base_url == settings.embedding_base_url
            && self.settings.embedding_model == settings.embedding_model
            && self.settings.embedding_config.enabled == settings.embedding_config.enabled
            && self.api_key == api_key
    }

    /// Whether memory should use embeddings. Off keeps recall on FTS5 alone and
    /// avoids a doomed embeddings request on every `remember`.
    pub fn embedding_enabled(&self) -> bool {
        self.settings.embedding_config.enabled
    }

    /// Whether recall re-scores candidates with a reranker. Off keeps the raw
    /// hybrid score; the cosine fallback still needs an embedder.
    pub fn reranker_enabled(&self) -> bool {
        self.settings.reranker_config.enabled
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        )
    }

    fn body(&self, messages: &[ChatMessage], tools: &[ToolSpec]) -> serde_json::Value {
        let msgs: Vec<serde_json::Value> =
            messages.iter().map(ChatMessage::to_openai_value).collect();
        let mut body = serde_json::json!({
            "model": self.settings.model,
            "messages": msgs,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            let tools: Vec<_> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }

    /// Probe the provider's reachability. Any HTTP response (even an error status)
    /// means the host is up; only a transport failure (DNS, refused, timeout)
    /// means the endpoint is unreachable. The request is scoped to the base URL
    /// with a short timeout so the health check stays cheap and real-time.
    pub async fn health_check(&self) -> Result<()> {
        let base = self.settings.base_url.trim_end_matches('/');
        let resp = self
            .client
            .get(base)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match resp {
            // A response arrived: host reachable (status may still be 401/404).
            Ok(_) => Ok(()),
            // Transport failure only: unreachable.
            Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => Err(
                ProviderError::Unreachable(base.to_string()),
            ),
            // Other reqwest errors still mean we talked to something; surface it.
            Err(e) => Err(ProviderError::Unreachable(e.to_string())),
        }
    }

    /// Embeddings endpoint: per-`embedding_base_url` override or the chat base URL.
    pub fn embedding_endpoint(&self) -> String {
        let base = self
            .settings
            .embedding_base_url
            .as_deref()
            .unwrap_or(&self.settings.base_url)
            .trim_end_matches('/');
        format!("{base}/embeddings")
    }

    /// The configured embedding model, defaulting to `text-embedding-3-small`.
    pub fn embedding_model(&self) -> &str {
        self.settings
            .embedding_model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or("text-embedding-3-small")
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let body = serde_json::json!({
            "input": text,
            "model": self.embedding_model(),
        });
        let response = self
            .client
            .post(self.embedding_endpoint())
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status { status, body });
        }
        let value: serde_json::Value = response.json().await?;
        let embedding = value
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|d| d.first())
            .and_then(|d| d.get("embedding"))
            .and_then(|e| serde_json::from_value(e.clone()).ok())
            .ok_or_else(|| ProviderError::Decode("missing embedding vector".into()))?;
        Ok(embedding)
    }

    /// Rerank one candidate against the query using a cross-encoder endpoint.
    /// Posts {query, candidate} to {base}/rerank and reads back a score.
    pub async fn rerank(&self, query: &str, candidate: &str) -> Result<f32> {
        let body = serde_json::json!({
            "query": query,
            "passage": candidate,
            "model": self.reranker_model(),
        });
        let response = self
            .client
            .post(self.reranker_endpoint())
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status { status, body });
        }
        let value: serde_json::Value = response.json().await?;
        value
            .get("score")
            .and_then(|s| s.as_f64())
            .map(|s| s as f32)
            .ok_or_else(|| ProviderError::Decode("missing rerank score".into()))
    }

    /// Reranking endpoint: per-`embedding_base_url` override or the chat base URL.
    pub fn reranker_endpoint(&self) -> String {
        let base = self
            .settings
            .embedding_base_url
            .as_deref()
            .unwrap_or(&self.settings.base_url)
            .trim_end_matches('/');
        format!("{base}/rerank")
    }

    /// The configured reranker model.
    pub fn reranker_model(&self) -> &str {
        if self.settings.reranker_config.model.trim().is_empty() {
            "nvidia/Nemotron-3.5-Rerank-8B-BF16"
        } else {
            &self.settings.reranker_config.model
        }
    }
}

impl Provider for OpenAiProvider {
    fn stream_chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResult>> + Send + 'a>> {
        Box::pin(self.stream_chat_inner(messages, tools, events))
    }
}

impl OpenAiProvider {
    async fn stream_chat_inner(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<TurnResult> {
        let response = self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .header("accept", "text/event-stream")
            .json(&self.body(messages, tools))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status { status, body });
        }

        let mut stream = SseParser::new();
        let mut acc = Accumulator::default();
        let mut bytes = response.bytes_stream();

        use futures_util::StreamExt;
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk?;
            for frame in stream.push(&chunk)? {
                match frame {
                    Frame::Done => {
                        return acc.finish(&events);
                    }
                    Frame::Data(json) => acc.apply(&json, &events)?,
                }
            }
        }
        // A well-formed provider ends with `data: [DONE]`.
        Err(ProviderError::Truncated)
    }
}

// ------------------------------------------------------------------ SSE

#[derive(Debug, PartialEq)]
enum Frame {
    Data(String),
    Done,
}

/// Incremental `text/event-stream` decoder. Tolerates `\n` and `\r\n`.
struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(pos) = find_double_newline(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..pos).collect();
            // Drop the delimiter (two bytes, or three for \r\n\r\n).
            let skip = if self.buf.starts_with(b"\r\n") { 2 } else { 1 };
            if self.buf.len() > skip && self.buf[skip] == b'\n' {
                self.buf.drain(..skip + 1);
            } else {
                self.buf.drain(..skip);
            }

            let text = String::from_utf8_lossy(&block);
            let mut data = String::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                }
            }
            if data.is_empty() {
                continue;
            }
            if data.trim() == "[DONE]" {
                frames.push(Frame::Done);
            } else {
                frames.push(Frame::Data(data));
            }
        }
        Ok(frames)
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n").or_else(|| {
        buf.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 2)
    })
}

// --------------------------------------------------------- accumulation

#[derive(Default)]
struct Accumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialCall>,
    prompt_tokens: Option<u64>,
    finished: bool,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Accumulator {
    fn apply(
        &mut self,
        data: &str,
        events: &mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Decode(format!("{e}: {data}")))?;

        if let Some(usage) = value.get("usage").filter(|u| !u.is_null())
            && let Some(tokens) = usage.get("prompt_tokens").and_then(|t| t.as_u64())
        {
            self.prompt_tokens = Some(tokens);
            let _ = events.send(ProviderEvent::Usage {
                prompt_tokens: tokens,
            });
        }

        let Some(choices) = value.get("choices").and_then(|c| c.as_array()) else {
            return Ok(());
        };
        for choice in choices {
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                && !content.is_empty()
            {
                self.text.push_str(content);
                let _ = events.send(ProviderEvent::TextDelta(content.to_string()));
            }
            // OpenAI o1-series models stream reasoning in `reasoning_content`;
            // some providers use a top-level `reasoning` field on the delta.
            let reasoning = delta
                .get("reasoning_content")
                .and_then(|c| c.as_str())
                .or_else(|| delta.get("reasoning").and_then(|c| c.as_str()))
                .filter(|s| !s.is_empty());
            if let Some(reasoning) = reasoning {
                self.reasoning.push_str(reasoning);
                let _ = events.send(ProviderEvent::ReasoningDelta(reasoning.to_string()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                for call in calls {
                    let index = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let slot = self.tool_calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(|i| i.as_str())
                        && !id.is_empty()
                    {
                        slot.id = id.to_string();
                    }
                    if let Some(func) = call.get("function") {
                        if let Some(name) = func.get("name").and_then(|n| n.as_str())
                            && !name.is_empty()
                        {
                            slot.name.push_str(name);
                        }
                        if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                            slot.arguments.push_str(args);
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str())
                && !reason.is_empty()
            {
                self.finished = true;
            }
        }
        Ok(())
    }

    fn finish(self, events: &mpsc::UnboundedSender<ProviderEvent>) -> Result<TurnResult> {
        let mut tool_calls = Vec::new();
        for (_, call) in self.tool_calls {
            if call.name.is_empty() {
                return Err(ProviderError::Truncated);
            }
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                call.arguments
            };
            // An unparseable argument object is the model's mistake, not ours:
            // pass it through so the Tool layer reports a clean error.
            let complete = ToolCallRequest::new(
                if call.id.is_empty() {
                    format!("call_{}", tool_calls.len())
                } else {
                    call.id
                },
                call.name,
                arguments,
            );
            let _ = events.send(ProviderEvent::ToolCall(complete.clone()));
            tool_calls.push(complete);
        }
        Ok(TurnResult {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            prompt_tokens: self.prompt_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderError;

    #[test]
    fn status_400_with_context_length_is_context_overflow() {
        assert!(ProviderError::Status {
            status: 400,
            body: "This model's maximum context length is 8192 tokens".to_string(),
        }
        .is_context_overflow());
    }

    #[test]
    fn status_413_with_input_too_long_is_context_overflow() {
        assert!(ProviderError::Status {
            status: 413,
            body: "input is too long for the requested model".to_string(),
        }
        .is_context_overflow());
    }

    #[test]
    fn status_400_auth_error_is_not_context_overflow() {
        assert!(!ProviderError::Status {
            status: 400,
            body: "Invalid API key".to_string(),
        }
        .is_context_overflow());
    }

    #[test]
    fn non_status_error_is_not_context_overflow() {
        assert!(!ProviderError::Unreachable("dns".to_string()).is_context_overflow());
        assert!(!ProviderError::Status {
            status: 500,
            body: "context length exceeded".to_string(),
        }
        .is_context_overflow());
    }
}

/// Cosine similarity between two vectors. Returns 0.0 on length mismatch or
/// zero-norm vectors. Used by the cosine reranker fallback.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

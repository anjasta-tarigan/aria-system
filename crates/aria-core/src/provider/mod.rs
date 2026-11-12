//! OpenAI-compatible chat completions client: streaming, tool calls, cancel.
//!
//! The client turns the SSE body into a small [`ProviderEvent`] vocabulary; the
//! Run loop accumulates tool-call fragments into complete calls (issue 04,
//! ADR-0001).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{Config, Settings};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
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

/// One message as the API sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Assistant {
        content: String,
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

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCallRequest>) -> Self {
        Self::Assistant {
            content: content.into(),
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
    /// A tool call is complete and ready to execute.
    ToolCall(ToolCallRequest),
    /// Prompt tokens for this Turn, when the provider reports usage.
    Usage { prompt_tokens: u64 },
}

/// The model's full answer to one Turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub text: String,
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

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        )
    }

    fn body(&self, messages: &[ChatMessage], tools: &[ToolSpec]) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.settings.model,
            "messages": messages,
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
            tool_calls,
            prompt_tokens: self.prompt_tokens,
        })
    }
}

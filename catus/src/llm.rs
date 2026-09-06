//! OpenAI-compatible Chat Completions client.
//!
//! Supports both one-shot and streaming (SSE) completions, including
//! OpenAI-compatible function/tool calling.
//!
//! Requests are built from two abstractions:
//! - [`Provider`]: an API vendor endpoint (base URL, API key, optional
//!   per-session header name).
//! - [`Model`]: a concrete model configuration (API model id, display name,
//!   context window, owning provider).

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::message::{Message, Role};
use crate::tool::{ToolCall, ToolDefinition};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// An API vendor endpoint.
///
/// `session_header` names the request header used to carry one stable ID per
/// conversation (e.g. `"x-opencode-session"` for the opencode platform).
/// `None` (or empty) means the vendor does not need such a header.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Provider {
    /// Logical name, e.g. `"opencode"`. Referenced by models.
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub session_header: Option<String>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            session_header: None,
        }
    }
}

impl Provider {
    /// Whether every request should carry the per-conversation session ID.
    pub fn sends_session_id(&self) -> bool {
        self.session_header
            .as_deref()
            .map(|h| !h.trim().is_empty())
            .unwrap_or(false)
    }
}

/// A concrete model configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Model {
    /// Model id sent in the API request's `model` field.
    pub id: String,
    /// Name shown in the UI. Falls back to `id` when empty.
    pub name: String,
    /// Context window in tokens; 0 means unspecified.
    pub context_window: usize,
    /// The resolved vendor endpoint for this model.
    #[serde(skip)]
    pub provider: Provider,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            context_window: 0,
            provider: Provider::default(),
        }
    }
}

impl Model {
    /// Display name: `name` when set, otherwise the API `id`.
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// Errors returned by the LLM client.
#[derive(Debug)]
pub enum LlmError {
    Http(reqwest::Error),
    Api(String),
    Json(serde_json::Error),
    Stream(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Http(e) => write!(f, "http error: {}", e),
            LlmError::Api(msg) => write!(f, "api error: {}", msg),
            LlmError::Json(e) => write!(f, "json error: {}", e),
            LlmError::Stream(msg) => write!(f, "stream error: {}", msg),
        }
    }
}

impl std::error::Error for LlmError {}

impl From<reqwest::Error> for LlmError {
    fn from(e: reqwest::Error) -> Self {
        LlmError::Http(e)
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(e: serde_json::Error) -> Self {
        LlmError::Json(e)
    }
}

/// A single event delivered by a streaming completion.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of text content.
    Text(String),
    /// A chunk of reasoning content.
    Reasoning(String),
    /// A completed tool call.
    ToolCall(ToolCall),
    /// Token usage reported for the completed request.
    Usage(Usage),
}

/// Token usage reported by the API for one completion.
///
/// `cached_tokens` covers prompt tokens served from the provider cache; it is
/// taken from `prompt_tokens_details.cached_tokens` (OpenAI) or a top-level
/// `cached_tokens` field (some compatible providers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
}

impl Usage {
    /// The most recent prompt size, i.e. the current context consumption.
    pub fn context_tokens(&self) -> u64 {
        self.prompt_tokens
    }
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        struct Raw {
            #[serde(default)]
            prompt_tokens: u64,
            #[serde(default)]
            completion_tokens: u64,
            #[serde(default)]
            total_tokens: u64,
            #[serde(default)]
            cached_tokens: u64,
            #[serde(default)]
            prompt_tokens_details: Option<PromptTokensDetails>,
        }
        #[derive(Default, Deserialize)]
        struct PromptTokensDetails {
            #[serde(default)]
            cached_tokens: u64,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(Usage {
            prompt_tokens: raw.prompt_tokens,
            completion_tokens: raw.completion_tokens,
            total_tokens: raw.total_tokens,
            cached_tokens: raw
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(raw.cached_tokens),
        })
    }
}

/// Full assistant reply from a non-streaming completion.
#[derive(Debug, Clone, Default)]
pub struct ChatReply {
    pub content: String,
    pub reasoning_content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

impl ChatReply {
    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.reasoning_content.is_empty() && self.tool_calls.is_empty()
    }
}

/// A tool-call chunk used both for complete non-streaming `tool_calls` and
/// for streaming deltas. Optional fields are absent in deltas.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ToolCallChunk {
    index: Option<usize>,
    id: Option<String>,
    #[serde(rename = "type")]
    tool_type: Option<String>,
    function: Option<FunctionCallChunk>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FunctionCallChunk {
    name: Option<String>,
    arguments: Option<String>,
}

/// A single chat message for the API request.
#[derive(Debug, Serialize, Deserialize)]
struct ApiMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    role: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_as_default"
    )]
    content: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_as_default"
    )]
    reasoning_content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallChunk>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// Streaming options requesting usage in the final chunk.
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Chat completion request body.
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

/// Non-streaming chat completion response.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    #[serde(default)]
    usage: Option<Usage>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Option<ApiMessage>,
    delta: Option<ApiMessage>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default, deserialize_with = "null_as_default")]
    message: String,
}

/// Simple OpenAI-compatible client.
#[derive(Debug, Clone)]
pub struct LlmClient {
    client: reqwest::Client,
    provider: Provider,
    model: Model,
    session_id: String,
}

impl LlmClient {
    pub fn new(provider: Provider, model: Model, session_id: &str) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static("curl/8.0.0"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            provider,
            model,
            session_id: session_id.to_string(),
        }
    }

    /// The per-conversation session ID sent to providers that request one.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn auth_header(&self) -> HeaderValue {
        let value = format!("Bearer {}", self.provider.api_key);
        HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    /// Apply the provider-specific headers shared by both request paths.
    fn apply_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header(AUTHORIZATION, self.auth_header());
        if let Some(name) = self
            .provider
            .session_header
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_str(name),
                HeaderValue::from_str(&self.session_id),
            ) {
                return request.header(name, value);
            }
        }
        request
    }

    fn url(&self) -> String {
        let base = self.provider.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{}/chat/completions", base)
        }
    }

    fn build_request(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> ChatRequest {
        let tools = tools
            .iter()
            .map(|def| serde_json::to_value(def).unwrap())
            .collect();
        ChatRequest {
            model: self.model.id.clone(),
            messages: messages
                .iter()
                .filter(|m| m.role != Role::Event)
                .map(into_api_message)
                .collect(),
            stream,
            tools: Some(tools),
            tool_choice: Some("auto".to_string()),
            stream_options: stream.then_some(StreamOptions {
                include_usage: true,
            }),
        }
    }

    /// Send a non-streaming chat request and return the full assistant reply.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatReply, LlmError> {
        let mut last_error: Option<LlmError> = None;
        for attempt in 0..5 {
            match self.try_chat_once(messages, tools).await {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(2_u64.pow(attempt))).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| LlmError::Api("all retries exhausted".to_string())))
    }

    async fn try_chat_once(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatReply, LlmError> {
        let body = self.build_request(messages, tools, false);
        let response = self
            .apply_headers(self.client.post(&self.url()))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(LlmError::Api(format!("HTTP {}: {}", status, text)));
        }

        let parsed: ChatResponse = serde_json::from_str(&text)?;
        if let Some(err) = parsed.error {
            return Err(LlmError::Api(err.message));
        }

        let had_choices = parsed.choices.is_some();
        let choices = parsed.choices.unwrap_or_default();
        let message = choices.into_iter().next().and_then(|c| c.message);

        let mut reply = ChatReply::default();
        if let Some(msg) = message {
            reply.content = msg.content;
            reply.reasoning_content = msg.reasoning_content;
            reply.tool_calls = msg
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .filter_map(|tc| {
                    Some(ToolCall {
                        id: tc.id?,
                        name: tc.function.as_ref()?.name.clone()?,
                        arguments: tc.function?.arguments.unwrap_or_default(),
                    })
                })
                .collect();
        }

        if reply.is_empty() && !had_choices {
            return Err(LlmError::Api("provider returned empty choices".to_string()));
        }

        reply.usage = parsed.usage;

        Ok(reply)
    }

    /// Send a streaming chat request and forward events to `tx`.
    /// Returns when the stream is exhausted or an error occurs.
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<(), LlmError> {
        let body = self.build_request(messages, tools, true);
        let response = self
            .apply_headers(self.client.post(&self.url()))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await?;
            return Err(LlmError::Api(format!("HTTP {}: {}", status, text)));
        }

        let mut partial_calls: HashMap<usize, PartialToolCall> = HashMap::new();
        let mut stream = response.bytes_stream().eventsource();

        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| LlmError::Stream(e.to_string()))?;
            let data = event.data.trim();

            if data == "[DONE]" {
                log::debug!("sse [DONE]");
                break;
            }

            log::debug!("sse data: {}", data);

            let parsed: ChatResponse = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    if data.is_empty() {
                        continue;
                    }
                    return Err(LlmError::Json(e));
                }
            };

            if let Some(err) = parsed.error {
                return Err(LlmError::Api(err.message));
            }

            // Providers send a final chunk with empty `choices` carrying only
            // the token usage.
            if let Some(usage) = parsed.usage {
                log::info!(
                    "sse usage: prompt={} completion={} total={} cached={}",
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.cached_tokens
                );
                let _ = tx.send(StreamEvent::Usage(usage)).await;
            }

            let choices = parsed.choices.unwrap_or_default();
            let choice = choices.into_iter().next();
            let Some(choice) = choice else {
                continue;
            };

            if let Some(delta) = choice.delta {
                if !delta.content.is_empty() {
                    log::debug!("sse text delta: {}", delta.content);
                    let _ = tx.send(StreamEvent::Text(delta.content)).await;
                }

                if !delta.reasoning_content.is_empty() {
                    log::debug!("sse reasoning delta: {}", delta.reasoning_content);
                    let _ = tx
                        .send(StreamEvent::Reasoning(delta.reasoning_content))
                        .await;
                }

                if let Some(calls) = delta.tool_calls {
                    log::debug!("sse tool-call delta chunks: {}", calls.len());
                    for call in calls {
                        let idx = call.index.unwrap_or(0);
                        let entry = partial_calls.entry(idx).or_default();
                        if let Some(id) = call.id {
                            log::debug!("tool-call {} id = {}", idx, id);
                            entry.id = Some(id);
                        }
                        if let Some(name) = call.function.as_ref().and_then(|f| f.name.clone()) {
                            log::debug!("tool-call {} name = {}", idx, name);
                            entry.name = Some(name);
                        }
                        if let Some(args) = call.function.as_ref().and_then(|f| f.arguments.clone())
                        {
                            entry.arguments.push_str(&args);
                        }
                    }
                }
            }

            log::debug!(
                "sse finish_reason = {:?}, pending partial calls = {}",
                choice.finish_reason,
                partial_calls.len()
            );

            if choice.finish_reason.as_deref() == Some("tool_calls") {
                for (_, partial) in partial_calls.drain() {
                    if let Some(call) = partial.into_tool_call() {
                        log::info!("streamed tool call: {} -> {}", call.id, call.name);
                        let _ = tx.send(StreamEvent::ToolCall(call)).await;
                    }
                }
            }
        }

        // Some providers do not set finish_reason to "tool_calls" even when
        // tool-call deltas were streamed. Drain any remaining partial calls.
        if !partial_calls.is_empty() {
            log::warn!(
                "stream ended with {} partial tool-call(s) unsent; draining",
                partial_calls.len()
            );
            for (_, partial) in partial_calls.drain() {
                if let Some(call) = partial.into_tool_call() {
                    log::info!("drained tool call: {} -> {}", call.id, call.name);
                    let _ = tx.send(StreamEvent::ToolCall(call)).await;
                }
            }
        }

        Ok(())
    }
}

/// Partial tool call accumulated from streaming deltas.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PartialToolCall {
    fn into_tool_call(self) -> Option<ToolCall> {
        Some(ToolCall {
            id: self.id?,
            name: self.name?,
            arguments: self.arguments,
        })
    }
}

/// Deserialize helper that treats explicit JSON `null` as the default value.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

fn into_api_message(msg: &Message) -> ApiMessage {
    let tool_calls = if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
        Some(
            msg.tool_calls
                .iter()
                .map(|tc| ToolCallChunk {
                    index: None,
                    id: Some(tc.id.clone()),
                    tool_type: Some("function".to_string()),
                    function: Some(FunctionCallChunk {
                        name: Some(tc.name.clone()),
                        arguments: Some(tc.arguments.clone()),
                    }),
                })
                .collect(),
        )
    } else {
        None
    };

    ApiMessage {
        role: match msg.role {
            Role::System => "system".to_string(),
            Role::User => "user".to_string(),
            Role::Assistant => "assistant".to_string(),
            Role::Tool => "tool".to_string(),
            Role::Event => "event".to_string(),
        },
        content: msg.content.clone(),
        reasoning_content: msg.reasoning_content.clone(),
        tool_calls,
        tool_call_id: msg.tool_call_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_with_null_content_and_tool_call() {
        let json = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\":\"ls -la\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).expect("should parse null content");
        let choices = parsed.choices.unwrap();
        let msg = choices[0].message.as_ref().unwrap();
        assert_eq!(msg.content, "");
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn parse_streaming_delta_with_null_content() {
        let json = r#"{
            "choices": [{
                "delta": {
                    "role": "assistant",
                    "content": null
                },
                "finish_reason": null
            }]
        }"#;
        let parsed: ChatResponse =
            serde_json::from_str(json).expect("should parse null delta content");
        let choices = parsed.choices.unwrap();
        let delta = choices[0].delta.as_ref().unwrap();
        assert_eq!(delta.content, "");
    }

    #[test]
    fn tool_call_serializes_to_openai_format() {
        let call = ToolCall {
            id: "call_123".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls -la"}"#.to_string(),
        };
        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains(r#""id":"call_123""#));
        assert!(json.contains(r#""type":"function""#));
        assert!(json.contains(r#""name":"shell""#));
        assert!(json.contains(r#""arguments":"{\"command\":\"ls -la\"}""#));
    }

    #[test]
    fn into_api_message_includes_tool_calls_for_empty_content() {
        let mut msg = Message::assistant(String::new());
        msg.tool_calls.push(ToolCall {
            id: "call_abc".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"pwd"}"#.to_string(),
        });
        let api = into_api_message(&msg);
        assert_eq!(api.role, "assistant");
        assert_eq!(api.content, "");
        assert!(api.tool_calls.is_some());
        let calls = api.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, Some("call_abc".to_string()));
        assert_eq!(calls[0].tool_type, Some("function".to_string()));
        assert_eq!(
            calls[0].function.as_ref().unwrap().name,
            Some("shell".to_string())
        );
    }

    #[test]
    fn parse_response_with_reasoning_content() {
        let json = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Let me calculate..."
                },
                "finish_reason": "stop"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).expect("should parse reasoning");
        let choices = parsed.choices.unwrap();
        let msg = choices[0].message.as_ref().unwrap();
        assert_eq!(msg.content, "The answer is 42.");
        assert_eq!(msg.reasoning_content, "Let me calculate...");
    }

    #[test]
    fn parse_streaming_delta_with_reasoning_content() {
        let json = r#"{
            "choices": [{
                "delta": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "thinking..."
                },
                "finish_reason": null
            }]
        }"#;
        let parsed: ChatResponse =
            serde_json::from_str(json).expect("should parse reasoning delta");
        let choices = parsed.choices.unwrap();
        let delta = choices[0].delta.as_ref().unwrap();
        assert_eq!(delta.content, "");
        assert_eq!(delta.reasoning_content, "thinking...");
    }

    #[test]
    fn into_api_message_includes_reasoning_content() {
        let mut msg = Message::assistant("answer");
        msg.reasoning_content = "my reasoning".to_string();
        let api = into_api_message(&msg);
        assert_eq!(api.reasoning_content, "my reasoning");
        let json = serde_json::to_string(&api).unwrap();
        assert!(json.contains("reasoning_content"));
    }

    #[test]
    fn parse_usage_with_prompt_tokens_details() {
        let json = r#"{
            "choices": [],
            "usage": {
                "prompt_tokens": 19,
                "completion_tokens": 13,
                "total_tokens": 32,
                "prompt_tokens_details": { "cached_tokens": 12 }
            }
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 19);
        assert_eq!(usage.completion_tokens, 13);
        assert_eq!(usage.total_tokens, 32);
        assert_eq!(usage.cached_tokens, 12);
    }

    #[test]
    fn parse_usage_with_top_level_cached_tokens() {
        let json = r#"{
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "cached_tokens": 40
            }
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cached_tokens, 40);
    }

    #[test]
    fn response_without_usage_parses() {
        let json = r#"{ "choices": [{ "message": { "role": "assistant", "content": "hi" } }] }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.usage.is_none());
    }

    fn test_provider() -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: "https://example.com".to_string(),
            api_key: "key".to_string(),
            session_header: None,
        }
    }

    fn test_client() -> LlmClient {
        LlmClient::new(test_provider(), Model::default(), "sess-1")
    }

    #[test]
    fn streaming_request_includes_stream_options() {
        let client = test_client();
        let body = client.build_request(&[], &[], true);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""stream_options":{"include_usage":true}"#));

        let non_streaming = client.build_request(&[], &[], false);
        let json = serde_json::to_string(&non_streaming).unwrap();
        assert!(!json.contains("stream_options"));
    }

    #[test]
    fn build_request_includes_all_tools() {
        let client = test_client();
        let tools = vec![
            ToolDefinition {
                tool_type: "function".to_string(),
                function: crate::tool::FunctionDefinition {
                    name: "shell".to_string(),
                    description: "run".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            },
            ToolDefinition {
                tool_type: "function".to_string(),
                function: crate::tool::FunctionDefinition {
                    name: "fs__read".to_string(),
                    description: "read".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            },
        ];
        let body = client.build_request(&[], &tools, false);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("shell"));
        assert!(json.contains("fs__read"));
    }

    #[test]
    fn build_request_uses_model_id() {
        let mut model = Model::default();
        model.id = "kimi-k2-0711".to_string();
        model.name = "Kimi K2".to_string();
        let client = LlmClient::new(test_provider(), model, "sess-1");
        let body = client.build_request(&[], &[], false);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""model":"kimi-k2-0711""#));
        assert!(!json.contains("Kimi K2"));
    }

    #[test]
    fn provider_detects_session_header_config() {
        let mut provider = test_provider();
        assert!(!provider.sends_session_id());
        provider.session_header = Some(String::new());
        assert!(!provider.sends_session_id());
        provider.session_header = Some("   ".to_string());
        assert!(!provider.sends_session_id());
        provider.session_header = Some("x-opencode-session".to_string());
        assert!(provider.sends_session_id());
    }

    #[test]
    fn model_display_name_falls_back_to_id() {
        let mut model = Model::default();
        model.id = "gpt-4o".to_string();
        assert_eq!(model.display_name(), "gpt-4o");
        model.name = "GPT-4o".to_string();
        assert_eq!(model.display_name(), "GPT-4o");
    }
}

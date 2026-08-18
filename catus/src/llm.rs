//! OpenAI-compatible Chat Completions client.
//!
//! Supports both one-shot and streaming (SSE) completions, including
//! OpenAI-compatible function/tool calling.

use std::collections::HashMap;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::message::{Message, Role};
use crate::tool::{ToolCall, shell_tool_definition};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

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
    /// A completed tool call.
    ToolCall(ToolCall),
}

/// Full assistant reply from a non-streaming completion.
#[derive(Debug, Clone, Default)]
pub struct ChatReply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

impl ChatReply {
    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallChunk>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
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
}

/// Non-streaming chat completion response.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
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
    base_url: String,
    api_key: String,
    model: String,
}

impl LlmClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
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
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn auth_header(&self) -> HeaderValue {
        let value = format!("Bearer {}", self.api_key);
        HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{}/chat/completions", base)
        }
    }

    fn build_request(&self, messages: &[Message], stream: bool) -> ChatRequest {
        let tools = vec![serde_json::to_value(shell_tool_definition()).unwrap()];
        ChatRequest {
            model: self.model.clone(),
            messages: messages
                .iter()
                .filter(|m| m.role != Role::Event)
                .map(into_api_message)
                .collect(),
            stream,
            tools: Some(tools),
            tool_choice: Some("auto".to_string()),
        }
    }

    /// Send a non-streaming chat request and return the full assistant reply.
    pub async fn chat(&self, messages: &[Message]) -> Result<ChatReply, LlmError> {
        let mut last_error: Option<LlmError> = None;
        for attempt in 0..5 {
            match self.try_chat_once(messages).await {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(2_u64.pow(attempt))).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| LlmError::Api("all retries exhausted".to_string())))
    }

    async fn try_chat_once(&self, messages: &[Message]) -> Result<ChatReply, LlmError> {
        let body = self.build_request(messages, false);
        let response = self
            .client
            .post(&self.url())
            .header(AUTHORIZATION, self.auth_header())
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

        Ok(reply)
    }

    /// Send a streaming chat request and forward events to `tx`.
    /// Returns when the stream is exhausted or an error occurs.
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<(), LlmError> {
        let body = self.build_request(messages, true);
        let response = self
            .client
            .post(&self.url())
            .header(AUTHORIZATION, self.auth_header())
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
}

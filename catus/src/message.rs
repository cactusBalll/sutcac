//! Message types used in the Agent conversation.

use serde::{Deserialize, Serialize};

use crate::tool::ToolCall;

/// The role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// Display-only event message (errors, notifications). Not sent to the LLM.
    Event,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Event => "event",
        }
    }
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// For `Role::Tool` messages, the id of the tool call this message is
    /// responding to (required by the OpenAI tool-calling protocol).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    /// For `Role::Assistant` messages, true if this message triggered one or
    /// more tool calls. Used by the TUI to dim intermediate reasoning.
    #[serde(default)]
    pub had_tool_calls: bool,
    /// For `Role::Assistant` messages, the tool calls generated in this
    /// response. Required when re-sending the conversation back to the API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            had_tool_calls: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            had_tool_calls: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            had_tool_calls: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            had_tool_calls: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn event(content: impl Into<String>) -> Self {
        Self {
            role: Role::Event,
            content: content.into(),
            tool_call_id: None,
            had_tool_calls: false,
            tool_calls: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn assistant_with_tool_calls(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            had_tool_calls: true,
            tool_calls: Vec::new(),
        }
    }

    pub fn is_system(&self) -> bool {
        self.role == Role::System
    }

    pub fn is_user(&self) -> bool {
        self.role == Role::User
    }

    pub fn is_assistant(&self) -> bool {
        self.role == Role::Assistant
    }

    pub fn is_tool(&self) -> bool {
        self.role == Role::Tool
    }

    pub fn is_event(&self) -> bool {
        self.role == Role::Event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_message_roundtrip() {
        let msg = Message::assistant_with_tool_calls("thinking...");
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, Role::Assistant);
        assert_eq!(parsed.content, "thinking...");
        assert!(parsed.had_tool_calls);
        assert!(parsed.tool_calls.is_empty());
    }

    #[test]
    fn role_serializes_to_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }
}

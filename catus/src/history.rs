//! SQLite-backed session history subsystem.
//!
//! A session stores everything needed to fully resume an interrupted
//! conversation: the main agent's messages, every subagent's messages and
//! runtime record (so a still-running subagent can be restarted after the
//! process exits), token usage, the active model, and miscellaneous
//! application state (pending tool calls, an open `ask_user` interaction,
//! shell working directory and variables).
//!
//! The database lives at `<history_path>/sessions.db` where `<history_path>`
//! comes from `[agent].history_path` in the configuration. Persistence is
//! incremental: the app rewrites its snapshot after each LLM turn, tool
//! result, usage report, and subagent event, so exiting (even while a
//! subagent is running) always leaves a complete record on disk.

pub mod store;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::llm::Usage;
use crate::message::Message;
use crate::tool::ToolCall;

pub use store::SessionStore;

/// `agent_id` of the main agent's messages in the `messages` table.
pub const MAIN_AGENT_ID: &str = "main";

/// `session_state` key holding the JSON list of pending [`ToolCall`]s.
pub const STATE_PENDING_TOOL_CALLS: &str = "pending_tool_calls";
/// `session_state` key holding the JSON of a paused `ask_user` interaction.
pub const STATE_PENDING_INTERACTION: &str = "pending_interaction";
/// `session_state` key holding the shell's working directory.
pub const STATE_SHELL_CWD: &str = "shell_cwd";
/// `session_state` key holding the shell's variables (JSON map).
pub const STATE_SHELL_VARS: &str = "shell_vars";
/// `session_state` key holding the shell's exported variable names.
pub const STATE_SHELL_EXPORTED: &str = "shell_exported";
/// `session_state` key holding the JSON of the main agent's TODO list.
pub const STATE_TODOS: &str = "todos";

/// Wire-format mirror of [`ToolCall`] for persistence. [`ToolCall`]'s own
/// `Serialize` impl produces the API request shape (`{"function": {...}}`),
/// which is not what we want on disk.
#[derive(Debug, Serialize, Deserialize)]
struct ToolCallRow {
    id: String,
    name: String,
    arguments: String,
}

/// Serialize pending tool calls as JSON for the `session_state` table.
pub fn tool_calls_to_json(calls: &[ToolCall]) -> String {
    let rows: Vec<ToolCallRow> = calls
        .iter()
        .map(|c| ToolCallRow {
            id: c.id.clone(),
            name: c.name.clone(),
            arguments: c.arguments.clone(),
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// Parse tool calls persisted with [`tool_calls_to_json`]; invalid input
/// yields an empty list.
pub fn tool_calls_from_json(json: &str) -> Vec<ToolCall> {
    serde_json::from_str::<Vec<ToolCallRow>>(json)
        .unwrap_or_default()
        .into_iter()
        .map(|r| ToolCall {
            id: r.id,
            name: r.name,
            arguments: r.arguments,
        })
        .collect()
}

/// Summary row shown by `/resume` lists and the resume picker.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: i64,
    pub name: String,
    pub updated_at: i64,
    pub model_id: String,
}

/// Persisted per-session metadata.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// The catus conversation ID (sent as the provider session header);
    /// restored on resume so the provider keeps seeing one stable session.
    pub session_id: String,
    pub model_id: String,
    pub request_count: u64,
    pub usage: Usage,
    pub active_skills: Vec<String>,
    pub tool_rounds: usize,
}

/// Persisted state of one subagent.
#[derive(Debug, Clone)]
pub struct SubagentRecord {
    pub id: String,
    pub name: String,
    pub task: String,
    /// [`crate::subagent::SubagentState::as_str`] value.
    pub state: String,
    /// [`crate::subagent::SubagentContextMode`] value ("fork" or "create").
    pub mode: String,
    /// Tool call id of the parent's `task` dispatch, if any. Restored so a
    /// resumed subagent can still inject its result into the parent turn.
    pub parent_call_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// A subagent record together with its conversation messages.
#[derive(Debug, Clone)]
pub struct SubagentSnapshot {
    pub record: SubagentRecord,
    pub messages: Vec<Message>,
}

/// Everything needed to resume one session.
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub subagents: Vec<SubagentSnapshot>,
    pub state: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_json_roundtrip() {
        let calls = vec![ToolCall {
            id: "call-1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls"}"#.to_string(),
        }];
        let json = tool_calls_to_json(&calls);
        assert_eq!(tool_calls_from_json(&json), calls);
        assert_eq!(tool_calls_from_json("not json"), Vec::new());
    }
}

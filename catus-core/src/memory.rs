//! Agent Memory subsystem.
//!
//! The memory store is an mdbook project (`book.toml` + `src/SUMMARY.md` +
//! topical chapter files) maintained by the `memory`-role subagent. When the
//! subsystem is enabled, catus dispatches the memory subagent twice around
//! each user turn:
//!
//! - **Recall** (before the main LLM request): the subagent decides whether
//!   the request is simple enough to skip memory; otherwise it searches the
//!   store and returns the relevant memory, which is injected into the main
//!   conversation as a system message.
//! - **Write** (after the turn completes): the subagent summarizes key facts
//!   of the turn, decides whether any are worth keeping, and updates the
//!   mdbook store.
//!
//! Both passes run through the regular subagent runtime (`completeTask`
//! protocol) in `create` mode, so the memory agent has its own context,
//! toolbox, and shell permissions. `mdbook build` is optional: when the
//! `mdbook` binary is unavailable the store is maintained as plain Markdown.
//!
//! The task prompts built here carry only the control-chain contract: pass
//! type, store and workspace paths, the user request / turn transcript, and
//! the exact `completeTask` JSON schema. All judgment rules (when recall is
//! worthwhile, what is worth keeping) and the store layout (mdbook scaffold,
//! `src/global/` vs. `src/workspaces/<slug>/` chapters) live in the memory
//! agent's definition (`agents/memory.md`).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::agents::{AgentRegistry, AgentRole};
use crate::config::AppConfig;
use crate::message::{Message, Role};

/// Upper bound for the memory injected into the main conversation.
const RECALL_MAX_CHARS: usize = 8000;
/// Upper bound for the per-turn transcript sent to the write pass.
const TRANSCRIPT_MAX_CHARS: usize = 24000;
/// Upper bound for one tool result line in the transcript.
const TRANSCRIPT_TOOL_CHARS: usize = 500;

/// Runtime state of the Agent Memory subsystem.
pub struct MemoryState {
    /// Whether memory can run at all: enabled in config and a `memory`-role
    /// agent definition exists.
    pub available: bool,
    /// Session-level toggle (`/memory on|off`); persisted with the session.
    pub session_enabled: bool,
    /// Root directory of the mdbook memory store.
    pub memory_dir: PathBuf,
    /// Subagent currently running a recall pass, if any.
    pub pending_recall: Option<String>,
    /// Subagent currently running a summarize/write pass, if any.
    pub pending_write: Option<String>,
    /// Startup warnings (e.g. enabled in config but no memory agent found).
    pub warnings: Vec<String>,
}

impl MemoryState {
    /// Initialize the memory subsystem from the config and agent registry.
    /// The store directory comes from the runtime `AppDirs` (`config.dirs`);
    /// production points it at the fixed XDG location.
    pub fn init(config: &AppConfig, registry: &AgentRegistry) -> Self {
        let mut state = Self {
            available: false,
            session_enabled: true,
            memory_dir: config.dirs.memory.clone(),
            pending_recall: None,
            pending_write: None,
            warnings: Vec::new(),
        };
        if !config.agent.memory.enabled {
            return state;
        }
        match registry.get_by_role(AgentRole::Memory) {
            Some(agent) => {
                state.available = true;
                log::info!(
                    "agent memory enabled; store at {}, agent '{}'",
                    state.memory_dir.display(),
                    agent.name
                );
            }
            None => {
                state.warnings.push(
                    "[agent.memory] enabled=true but no agent definition with `role: memory` \
                     was found; the Agent Memory subsystem stays disabled"
                        .to_string(),
                );
                log::warn!("{}", state.warnings[0]);
            }
        }
        state
    }

    /// Whether memory passes should actually run right now.
    pub fn enabled(&self) -> bool {
        self.available && self.session_enabled
    }
}

/// Build the task prompt for the pre-turn recall pass.
///
/// Carries only the control-chain contract; the judgment rules live in the
/// memory agent's definition (`agents/memory.md`).
pub fn recall_task(user_prompt: &str, memory_dir: &Path, workspace: &Path) -> String {
    format!(
        "Memory recall pass.\n\n\
         Memory store directory: {}\n\
         Current workspace: {}\n\n\
         The main agent is about to handle this user request:\n\
         <user_request>\n{}\n</user_request>\n\n\
         Follow your memory-management instructions to decide whether \
         recalling long-term memory would help; if so, search the store \
         (the current workspace's chapters first, then the global chapters) \
         and return the relevant memory.\n\n\
         Call completeTask with exactly one of:\n\
         - {{\"recall\": false}}\n\
         - {{\"recall\": true, \"memory\": \"<the relevant memory>\"}}\n\
         The `memory` value must be at most {} characters.",
        memory_dir.display(),
        workspace.display(),
        user_prompt,
        RECALL_MAX_CHARS
    )
}

/// Build the task prompt for the post-turn summarize/write pass.
///
/// Same split as [`recall_task`]: only the contract, no judgment rules.
pub fn summarize_task(transcript: &str, memory_dir: &Path, workspace: &Path) -> String {
    format!(
        "Memory write pass.\n\n\
         Memory store directory: {}\n\
         Current workspace: {}\n\n\
         Below is the transcript of the conversation turn that just \
         finished:\n<turn_transcript>\n{}\n</turn_transcript>\n\n\
         Follow your memory-management instructions to decide whether any \
         fact is worth keeping long-term; if so, update the store \
         accordingly (workspace-specific facts under the current workspace's \
         chapters, cross-workspace facts in the global chapters).\n\n\
         Call completeTask with exactly one of:\n\
         - {{\"written\": false}}\n\
         - {{\"written\": true, \"summary\": \"<one line describing what was recorded>\"}}",
        memory_dir.display(),
        workspace.display(),
        transcript
    )
}

/// Parsed result of a recall pass.
#[derive(Debug, Clone, Deserialize)]
pub struct RecallResult {
    #[serde(default)]
    pub recall: bool,
    #[serde(default)]
    pub memory: String,
}

/// Parsed result of a write pass.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteResult {
    #[serde(default)]
    pub written: bool,
    #[serde(default)]
    pub summary: String,
}

/// Parse a recall pass's `completeTask` result. Returns `None` when the
/// payload is not a recognizable JSON object; a `recall: false` (or missing)
/// flag yields a result with `recall == false`.
pub fn parse_recall_result(result: &str) -> Option<RecallResult> {
    let json = extract_json_object(result)?;
    serde_json::from_str(&json).ok()
}

/// Parse a write pass's `completeTask` result. Same tolerance as
/// [`parse_recall_result`].
pub fn parse_write_result(result: &str) -> Option<WriteResult> {
    let json = extract_json_object(result)?;
    serde_json::from_str(&json).ok()
}

/// Extract the first JSON object-looking span from a model reply: from the
/// first `{` to the last `}`. Models occasionally wrap their JSON in prose or
/// code fences, so the extraction is deliberately loose.
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    Some(text[start..=end].to_string())
}

/// Trim a recalled memory to the injection limit.
pub fn truncate_recall(memory: &str) -> String {
    truncate(memory, RECALL_MAX_CHARS)
}

/// Build a compact transcript of the current user turn: from the last user
/// message (exclusive of system/event chatter) to the end of the conversation.
///
/// Returns an empty string when there is no user message to summarize.
pub fn format_transcript(messages: &[Message]) -> String {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(messages.len());
    let mut out = String::new();
    for message in &messages[start..] {
        match message.role {
            Role::User => {
                push_line(&mut out, &format!("user: {}", message.content));
            }
            Role::Assistant => {
                if !message.content.trim().is_empty() {
                    push_line(&mut out, &format!("assistant: {}", message.content));
                }
                for call in &message.tool_calls {
                    push_line(
                        &mut out,
                        &format!(
                            "assistant tool call: {} {}",
                            call.name,
                            truncate(call.arguments.trim(), TRANSCRIPT_TOOL_CHARS)
                        ),
                    );
                }
            }
            Role::Tool => {
                push_line(
                    &mut out,
                    &format!(
                        "tool result: {}",
                        truncate(message.content.trim(), TRANSCRIPT_TOOL_CHARS)
                    ),
                );
            }
            // System prompt, injected skills/recall messages, and UI event
            // lines carry no turn facts.
            Role::System | Role::Event => {}
        }
        if out.len() >= TRANSCRIPT_MAX_CHARS {
            break;
        }
    }
    truncate(&out, TRANSCRIPT_MAX_CHARS)
}

fn push_line(out: &mut String, line: &str) {
    if out.len() >= TRANSCRIPT_MAX_CHARS {
        return;
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{}…", cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryConfig;

    fn config_with_memory(enabled: bool) -> AppConfig {
        let mut config = AppConfig::default();
        config.agent.memory = MemoryConfig {
            enabled,
            auto_recall: true,
            auto_write: true,
        };
        config
    }

    #[test]
    fn memory_state_disabled_by_default() {
        let registry = AgentRegistry::new();
        let state = MemoryState::init(&AppConfig::default(), &registry);
        assert!(!state.available);
        assert!(!state.enabled());
        assert!(state.warnings.is_empty());
    }

    #[test]
    fn memory_state_requires_memory_role_agent() {
        let config = config_with_memory(true);
        let registry = AgentRegistry::new();
        let state = MemoryState::init(&config, &registry);
        assert!(!state.available);
        assert!(!state.enabled());
        assert_eq!(state.warnings.len(), 1);
        assert!(state.warnings[0].contains("role: memory"));
    }

    #[test]
    fn recall_task_carries_contract_paths_and_prompt() {
        let task = recall_task(
            "fix the build",
            std::path::Path::new("/tmp/m"),
            std::path::Path::new("/tmp/ws"),
        );
        assert!(task.contains("fix the build"));
        assert!(task.contains("/tmp/m"));
        assert!(task.contains("/tmp/ws"));
        assert!(task.contains("\"recall\": false"));
        assert!(task.contains("\"recall\": true"));
    }

    #[test]
    fn summarize_task_carries_contract_paths_and_transcript() {
        let task = summarize_task(
            "user: hello",
            std::path::Path::new("/tmp/m"),
            std::path::Path::new("/tmp/ws"),
        );
        assert!(task.contains("user: hello"));
        assert!(task.contains("/tmp/m"));
        assert!(task.contains("/tmp/ws"));
        assert!(task.contains("\"written\": false"));
        assert!(task.contains("\"written\": true"));
    }

    #[test]
    fn parse_recall_result_variants() {
        let parsed = parse_recall_result(r#"{"recall": true, "memory": "likes tabs"}"#).unwrap();
        assert!(parsed.recall);
        assert_eq!(parsed.memory, "likes tabs");

        let parsed = parse_recall_result(r#"{"recall": false}"#).unwrap();
        assert!(!parsed.recall);

        // Tolerant of prose wrappers and code fences.
        let parsed = parse_recall_result(
            "Here you go:\n```json\n{\"recall\": true, \"memory\": \"m\"}\n```",
        )
        .unwrap();
        assert!(parsed.recall);
        assert_eq!(parsed.memory, "m");

        // Missing fields default to no-recall instead of failing.
        let parsed = parse_recall_result("{}").unwrap();
        assert!(!parsed.recall);

        assert!(parse_recall_result("no json at all").is_none());
        assert!(parse_recall_result("[1, 2]").is_none());
    }

    #[test]
    fn parse_write_result_variants() {
        let parsed = parse_write_result(r#"{"written": true, "summary": "added prefs"}"#).unwrap();
        assert!(parsed.written);
        assert_eq!(parsed.summary, "added prefs");

        let parsed = parse_write_result(r#"{"written": false}"#).unwrap();
        assert!(!parsed.written);

        assert!(parse_write_result("garbage").is_none());
    }

    #[test]
    fn format_transcript_from_last_user_message() {
        let messages = vec![
            Message::user("earlier turn"),
            Message::assistant("earlier answer"),
            Message::system("skill instructions"),
            Message::event("noise"),
            Message::user("current question"),
            Message::system("recalled memory"),
            Message::assistant("final answer"),
        ];
        let transcript = format_transcript(&messages);
        assert!(transcript.contains("current question"));
        assert!(transcript.contains("final answer"));
        assert!(!transcript.contains("earlier turn"));
        assert!(!transcript.contains("recalled memory"));
        assert!(!transcript.contains("noise"));
    }

    #[test]
    fn format_transcript_includes_tool_calls_and_results() {
        let mut assistant = Message::assistant("working");
        assistant.tool_calls.push(crate::tool::ToolCall {
            id: "call-1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"cargo test"}"#.to_string(),
        });
        let messages = vec![
            Message::user("run the tests"),
            assistant,
            Message::tool("status=0\nstdout=```\nok\n```", "call-1"),
        ];
        let transcript = format_transcript(&messages);
        assert!(transcript.contains("run the tests"));
        assert!(transcript.contains("assistant tool call: shell"));
        assert!(transcript.contains("cargo test"));
        assert!(transcript.contains("tool result:"));
    }

    #[test]
    fn format_transcript_empty_without_user_message() {
        let messages = vec![Message::assistant("only an answer")];
        assert_eq!(format_transcript(&messages), "");
    }

    #[test]
    fn transcript_is_truncated() {
        let long = "x".repeat(TRANSCRIPT_MAX_CHARS + 5000);
        let messages = vec![Message::user(long)];
        let transcript = format_transcript(&messages);
        assert!(transcript.chars().count() <= TRANSCRIPT_MAX_CHARS + 1);
        assert!(transcript.ends_with('…'));
    }
}

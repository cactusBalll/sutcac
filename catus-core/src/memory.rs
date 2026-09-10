//! Agent Memory subsystem.
//!
//! The memory store is an mdbook project (`book.toml` + `src/SUMMARY.md` +
//! topical chapter files) maintained by the `memory`-role subagent. When the
//! subsystem is enabled, catus dispatches the memory subagent twice:
//!
//! - **Recall** (once, before the main LLM request of the session's first
//!   user turn): the subagent decides whether the request is simple enough
//!   to skip memory; otherwise it searches the store and returns the
//!   relevant memory, which is injected into the main conversation as a
//!   system message. Later turns of the same session reuse that memory.
//! - **Write** (after a turn completes): the subagent summarizes key facts
//!   of the latest user turn, decides whether any are worth keeping, and
//!   updates the mdbook store. The pass runs in `fork` mode — it reuses the
//!   main agent's context prefix instead of receiving a transcript — and
//!   write requests that arrive while another pass is still running are
//!   queued, not skipped.
//!
//! Recall runs through the regular subagent runtime (`completeTask`
//! protocol) in `create` mode, so the memory agent has its own context,
//! toolbox, and shell permissions. `mdbook build` is optional: when the
//! `mdbook` binary is unavailable the store is maintained as plain Markdown.
//!
//! The task prompts built here carry only the control-chain contract: pass
//! type, store and workspace paths, and the exact `completeTask` JSON
//! schema. All judgment rules (when recall is worthwhile, what is worth
//! keeping) and the store layout (mdbook scaffold, `src/global/` vs.
//! `src/workspaces/<slug>/` chapters) live in the memory agent's definition
//! (`agents/memory.md`); the write prompt embeds that definition body
//! because `fork` mode does not inject it as a system message.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::agents::{AgentRegistry, AgentRole};
use crate::config::AppConfig;

/// Upper bound for the memory injected into the main conversation.
const RECALL_MAX_CHARS: usize = 8000;

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
    /// A write pass was requested while another memory pass was still
    /// running; it is dispatched when that pass finishes instead of being
    /// skipped.
    pub write_queued: bool,
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
            write_queued: false,
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
/// The pass runs in `fork` mode, so the full main-agent conversation prefix
/// (including the latest user turn) is already part of the subagent
/// context. The prompt carries the control contract plus the memory agent's
/// own definition body: `fork` mode does not inject the agent definition as
/// a system message, so the judgment rules must travel with the task.
pub fn summarize_task(agent_instructions: &str, memory_dir: &Path, workspace: &Path) -> String {
    format!(
        "Memory write pass.\n\n\
         You were forked from the main agent: the conversation above is the \
         full session context. Summarize the latest user turn (from the last \
         user message onwards).\n\n\
         Memory store directory: {}\n\
         Current workspace: {}\n\n\
         Your memory-management instructions are reproduced below. They \
         cover both the recall and the write pass; this is the write pass, \
         so only the store layout rules and the write flow apply:\n\
         <memory_instructions>\n{}\n</memory_instructions>\n\n\
         Call completeTask with exactly one of:\n\
         - {{\"written\": false}}\n\
         - {{\"written\": true, \"summary\": \"<one line describing what was recorded>\"}}",
        memory_dir.display(),
        workspace.display(),
        agent_instructions
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
    fn summarize_task_carries_contract_paths_and_instructions() {
        let task = summarize_task(
            "write rules here",
            std::path::Path::new("/tmp/m"),
            std::path::Path::new("/tmp/ws"),
        );
        assert!(task.contains("write rules here"));
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
    fn truncate_recall_caps_length() {
        let long = "x".repeat(RECALL_MAX_CHARS + 1000);
        let trimmed = truncate_recall(&long);
        assert!(trimmed.chars().count() <= RECALL_MAX_CHARS + 1);
        assert!(trimmed.ends_with('…'));
        assert_eq!(truncate_recall("short"), "short");
    }
}

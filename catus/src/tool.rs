//! Tool infrastructure for the catus Agent.
//!
//! This module defines the OpenAI-compatible function-calling protocol
//! ([`ToolCall`], [`ToolResult`], [`ToolDefinition`]) and the extensible
//! [`Tool`] interface with its [`Toolbox`] registry. It is deliberately free
//! of any concrete tool logic; implementations live in sibling modules:
//!
//! - `shell` — the built-in `shell` tool ([`ShellTool`]);
//! - `skill` — the built-in `use_skill` tool ([`SkillTool`]);
//! - `ask_user` — the built-in interactive `ask_user` tool ([`AskUserTool`]);
//! - `crate::mcp` — tools converted from MCP servers (`McpTool`).
//!
//! The application (`app.rs`) acts as the composition root: it registers the
//! built-in tools at startup and MCP tools as servers connect.

mod ask_user;
mod shell;
mod skill;

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use sutcac_sh::exec::ShellState;

use crate::message::Message;
use crate::skills::SkillRegistry;

pub use ask_user::{
    Answer, AskAnswer, AskOption, AskQuestion, AskUserTool, InteractionRequest, collect_answer,
};
pub use shell::ShellTool;
pub use skill::SkillTool;

/// Mutable host state made available to tools while they execute.
///
/// Constructed by the application (`app.rs`) for every dispatched call.
pub struct ToolContext<'a> {
    pub shell_state: &'a mut ShellState,
    pub skill_registry: &'a mut SkillRegistry,
    pub active_skills: &'a mut Vec<String>,
    pub messages: &'a mut Vec<Message>,
}

/// A tool the Agent can invoke, identified by its advertised name.
///
/// The `execute` method returns a boxed future so the trait stays dyn-safe
/// without pulling in an `async-trait` dependency.
pub trait Tool: Send + Sync {
    /// The tool name as advertised to (and called by) the LLM.
    fn name(&self) -> &str;
    /// The OpenAI-compatible definition advertised to the LLM.
    fn definition(&self) -> ToolDefinition;
    /// A short human-readable description of one call, used for the status
    /// bar and history echo. Defaults to the tool name.
    fn describe_call(&self, call: &ToolCall) -> String {
        call.name.clone()
    }
    /// Execute the call against the host state.
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}

/// Registry of all tools available to the Agent.
///
/// Starts out empty; the application registers the built-in tools at startup
/// and MCP tools as servers connect. Lookups go through [`Toolbox::get`] so
/// dispatch is driven entirely by the advertised tool name.
#[derive(Default)]
pub struct Toolbox {
    tools: Vec<Box<dyn Tool>>,
}

impl Toolbox {
    /// Register a tool. Re-registering an existing name replaces it.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.retain(|t| t.name() != tool.name());
        self.tools.push(tool);
    }

    /// Definitions of all registered tools, advertised to the LLM.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// Find a tool by its advertised name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True if no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// A parsed tool invocation returned by the model.
///
/// This is a pure protocol type: it carries no per-tool semantics. Argument
/// interpretation belongs to the tool implementation (e.g. the shell tool's
/// argument parsing in `shell.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl serde::Serialize for ToolCall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ToolCall", 3)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("type", "function")?;
        #[derive(serde::Serialize)]
        struct Function<'a> {
            name: &'a str,
            arguments: &'a str,
        }
        state.serialize_field(
            "function",
            &Function {
                name: &self.name,
                arguments: &self.arguments,
            },
        )?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ToolCall {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Helper {
            id: String,
            #[serde(rename = "type")]
            tool_type: String,
            function: FunctionHelper,
        }
        #[derive(serde::Deserialize)]
        struct FunctionHelper {
            name: String,
            arguments: String,
        }
        let helper = Helper::deserialize(deserializer)?;
        if helper.tool_type != "function" {
            return Err(serde::de::Error::custom(format!(
                "unsupported tool type: {}",
                helper.tool_type
            )));
        }
        Ok(ToolCall {
            id: helper.id,
            name: helper.function.name,
            arguments: helper.function.arguments,
        })
    }
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call: ToolCall,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Set when the tool needs input from the user before its result is
    /// final. The application pauses the turn, collects the answers through
    /// an interactive overlay, and only then sends the completed result to
    /// the LLM.
    pub interaction: Option<InteractionRequest>,
}

impl ToolResult {
    /// Format the result as a message to be sent back to the LLM.
    pub fn to_message(&self) -> String {
        format!(
            "status={}\nstdout=```\n{}\n```\nstderr=```\n{}\n```",
            self.status, self.stdout, self.stderr
        )
    }
}

/// OpenAI-compatible request tool description.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_toolbox() -> Toolbox {
        let mut toolbox = Toolbox::default();
        toolbox.register(Box::new(ShellTool));
        toolbox.register(Box::new(SkillTool));
        toolbox
    }

    #[test]
    fn tool_call_roundtrips_through_json() {
        let call = ToolCall {
            id: "call_abc".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls -la"}"#.to_string(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, call);
    }

    #[test]
    fn tool_call_rejects_non_function_type() {
        let json =
            r#"{"id":"call_abc","type":"custom","function":{"name":"shell","arguments":"{}"}}"#;
        assert!(serde_json::from_str::<ToolCall>(json).is_err());
    }

    #[test]
    fn toolbox_get_and_definitions() {
        let toolbox = test_toolbox();
        assert!(toolbox.get("shell").is_some());
        assert!(toolbox.get("use_skill").is_some());
        assert!(toolbox.get("nope").is_none());
        let names: Vec<String> = toolbox
            .definitions()
            .into_iter()
            .map(|d| d.function.name)
            .collect();
        assert_eq!(names, vec!["shell", "use_skill"]);
    }

    #[test]
    fn toolbox_register_replaces_same_name() {
        let mut toolbox = test_toolbox();
        assert_eq!(toolbox.len(), 2);
        toolbox.register(Box::new(ShellTool));
        assert_eq!(toolbox.len(), 2);
    }

    #[test]
    fn toolbox_starts_empty() {
        let toolbox = Toolbox::default();
        assert!(toolbox.is_empty());
        assert!(toolbox.definitions().is_empty());
    }
}

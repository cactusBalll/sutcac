//! The built-in `task` tool: dispatch a task to a subagent.
//!
//! Only the main agent is advertised this tool. The subagent runs
/// asynchronously; the result is injected into the parent conversation when
/// the subagent calls `completeTask`.
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};
use crate::subagent::SubagentContextMode;

/// The built-in `task` tool.
pub struct TaskTool;

const TASK_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "task",
    "description": "Delegate a task to a specialized subagent. The subagent runs asynchronously and its result will be injected into the conversation once it calls completeTask.",
    "parameters": {
      "type": "object",
      "properties": {
        "agent": {
          "type": "string",
          "description": "Name of the subagent to dispatch to."
        },
        "task": {
          "type": "string",
          "description": "Description of the task the subagent should perform."
        },
        "context": {
          "type": "string",
          "enum": ["fork", "create"],
          "description": "fork: inherit parent context; create: start with independent context."
        }
      },
      "required": ["agent", "task"]
    }
  }
}"#;

impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(TASK_TOOL_SCHEMA).expect("task tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<TaskArguments>(&call.arguments)
            .map(|args| format!("task: {} -> {}", args.agent, truncate(&args.task, 60)))
            .unwrap_or_else(|_| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            if !ctx.is_main_agent {
                return ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: "catus: subagents cannot spawn further subagents".to_string(),
                    interaction: None,
                };
            }

            let args: TaskArguments = match serde_json::from_str(&call.arguments) {
                Ok(a) => a,
                Err(_) => {
                    return ToolResult {
                        call: call.clone(),
                        status: 2,
                        stdout: String::new(),
                        stderr: format!(
                            "catus: arguments must be a JSON object {{\"agent\": \"...\", \"task\": \"...\", \"context\": \"fork|create\"}}; got: {}",
                            call.arguments
                        ),
                        interaction: None,
                    };
                }
            };

            let mode = args
                .context
                .as_deref()
                .unwrap_or("create")
                .parse::<SubagentContextMode>()
                .map_err(|e| e.to_string());

            let mode = match mode {
                Ok(m) => m,
                Err(e) => {
                    return ToolResult {
                        call: call.clone(),
                        status: 2,
                        stdout: String::new(),
                        stderr: e,
                        interaction: None,
                    };
                }
            };

            let agent_registry = match ctx.agent_registry {
                Some(ref mut r) => r,
                None => {
                    return ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: "catus: agent registry unavailable".to_string(),
                        interaction: None,
                    };
                }
            };
            let subagents = match ctx.subagents {
                Some(ref mut s) => s,
                None => {
                    return ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: "catus: subagent manager unavailable".to_string(),
                        interaction: None,
                    };
                }
            };

            let definition = match agent_registry.get(&args.agent) {
                Some(d) => d.clone(),
                None => {
                    return ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: format!("catus: subagent '{}' not found", args.agent),
                        interaction: None,
                    };
                }
            };

            let id = subagents.spawn(
                &definition,
                args.task.clone(),
                mode,
                ctx.messages.clone(),
                ctx.shell_state.clone(),
                ctx.active_skills.clone(),
                ctx.skill_registry.clone(),
                Some(call.id.clone()),
            );

            ToolResult {
                call: call.clone(),
                status: 0,
                stdout: format!(
                    "subagent {} started with id {}; result will be injected when completeTask is called",
                    args.agent, id
                ),
                stderr: String::new(),
                interaction: None,
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct TaskArguments {
    agent: String,
    task: String,
    context: Option<String>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

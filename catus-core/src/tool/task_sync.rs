//! The built-in `taskSync` tool: dispatch a task to a subagent and block until
//! it returns.
//!
//! Only the main agent is advertised this tool. The subagent runs in the
//! background, but the tool future waits for `SubagentEvent::Completed` before
//! returning, effectively suspending the parent agent's turn.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::timeout;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};
use crate::subagent::SubagentContextMode;

/// The built-in `taskSync` tool.
pub struct TaskSyncTool;

const TASK_SYNC_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "taskSync",
    "description": "Delegate a task to a specialized subagent and wait for the result. The parent agent's turn is suspended until the subagent calls completeTask.",
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

const SYNC_TIMEOUT: Duration = Duration::from_secs(600);

impl Tool for TaskSyncTool {
    fn name(&self) -> &str {
        "taskSync"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(TASK_SYNC_TOOL_SCHEMA).expect("taskSync tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<TaskSyncArguments>(&call.arguments)
            .map(|args| format!("taskSync: {} -> {}", args.agent, truncate(&args.task, 60)))
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

            let args: TaskSyncArguments = match serde_json::from_str(&call.arguments) {
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

            let (_id, mut rx) = subagents.spawn_sync(
                &definition,
                args.task.clone(),
                mode,
                ctx.messages.clone(),
                ctx.shell_state.clone(),
                ctx.active_skills.clone(),
                ctx.skill_registry.clone(),
                Some(call.id.clone()),
            );

            // Wait for the subagent to complete. The application event loop
            // continues to process UI events, but the parent tool round is
            // suspended until this future resolves.
            let result = wait_for_completion(&mut rx, SYNC_TIMEOUT).await;

            match result {
                Ok(output) => ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout: output,
                    stderr: String::new(),
                    interaction: None,
                },
                Err(e) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("catus: taskSync failed: {}", e),
                    interaction: None,
                },
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct TaskSyncArguments {
    agent: String,
    task: String,
    context: Option<String>,
}

async fn wait_for_completion(
    rx: &mut tokio::sync::oneshot::Receiver<Result<String, String>>,
    duration: Duration,
) -> Result<String, String> {
    match timeout(duration, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("subagent result channel cancelled".to_string()),
        Err(_) => Err("timed out waiting for subagent".to_string()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

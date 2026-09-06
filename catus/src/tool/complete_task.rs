//! The built-in `completeTask` tool used by subagents to return results.
//!
//! This tool is only advertised to subagents, not to the main agent. The
//! subagent runtime intercepts the call and emits a `SubagentEvent::Completed`
//! event; the tool itself returns a local acknowledgement so the turn can end.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// The built-in `completeTask` tool.
pub struct CompleteTaskTool;

const COMPLETE_TASK_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "completeTask",
    "description": "Signal that the subagent task is complete and return the final result to the parent agent. Use this once you have finished the assigned task.",
    "parameters": {
      "type": "object",
      "properties": {
        "result": {
          "type": "string",
          "description": "The final result of the subagent task."
        }
      },
      "required": ["result"]
    }
  }
}"#;

impl Tool for CompleteTaskTool {
    fn name(&self) -> &str {
        "completeTask"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(COMPLETE_TASK_SCHEMA).expect("completeTask tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<CompleteTaskArguments>(&call.arguments)
            .map(|args| format!("completeTask: {}", truncate(&args.result, 60)))
            .unwrap_or_else(|_| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let args: CompleteTaskArguments = match serde_json::from_str(&call.arguments) {
                Ok(a) => a,
                Err(_) => {
                    return ToolResult {
                        call: call.clone(),
                        status: 2,
                        stdout: String::new(),
                        stderr: format!(
                            "catus: arguments must be a JSON object {{\"result\": \"...\"}}; got: {}",
                            call.arguments
                        ),
                        interaction: None,
                    };
                }
            };
            ToolResult {
                call: call.clone(),
                status: 0,
                stdout: format!(
                    "task complete; result length {} characters",
                    args.result.len()
                ),
                stderr: String::new(),
                interaction: None,
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct CompleteTaskArguments {
    result: String,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

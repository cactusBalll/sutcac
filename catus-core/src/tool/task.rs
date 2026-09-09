//! The built-in `task` tool: dispatch a task to a subagent, and manage
//! dispatched subagents.
//!
//! Only the main agent is advertised this tool. Subagents run asynchronously;
//! when one completes, the parent receives a short completion notice and must
//! fetch the result itself with `action: "result"`. `action: "list"` reports
//! the states of all subagents.
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
    "description": "Delegate a task to a specialized subagent, or manage dispatched subagents. Subagents run asynchronously: when one finishes, a completion notice arrives and the result must be fetched with action=\"result\" (it is NOT injected automatically).",
    "parameters": {
      "type": "object",
      "properties": {
        "action": {
          "type": "string",
          "enum": ["dispatch", "list", "result"],
          "description": "dispatch (default): start a subagent (requires agent and task); list: show all subagents and their states; result: fetch a finished subagent's output (requires id)."
        },
        "agent": {
          "type": "string",
          "description": "Name of the subagent to dispatch to (action=dispatch)."
        },
        "task": {
          "type": "string",
          "description": "Description of the task the subagent should perform (action=dispatch)."
        },
        "context": {
          "type": "string",
          "enum": ["fork", "create"],
          "description": "fork: inherit parent context; create: start with independent context (action=dispatch)."
        },
        "id": {
          "type": "string",
          "description": "Subagent id whose result to fetch (action=result)."
        }
      }
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
            .map(|args| match args.action.as_deref() {
                Some("list") => "task: list subagents".to_string(),
                Some("result") => format!(
                    "task: fetch result of {}",
                    args.id.as_deref().unwrap_or("?")
                ),
                _ => format!(
                    "task: {} -> {}",
                    args.agent.as_deref().unwrap_or("?"),
                    truncate(args.task.as_deref().unwrap_or(""), 60)
                ),
            })
            .unwrap_or_else(|_| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            if !ctx.is_main_agent {
                return err(
                    call,
                    1,
                    "catus: subagents cannot spawn further subagents".to_string(),
                );
            }

            let args: TaskArguments = match serde_json::from_str(&call.arguments) {
                Ok(a) => a,
                Err(_) => {
                    return err(
                        call,
                        2,
                        format!(
                            "catus: arguments must be a JSON object {{\"action\": \"dispatch|list|result\", \"agent\": \"...\", \"task\": \"...\", \"id\": \"...\", \"context\": \"fork|create\"}}; got: {}",
                            call.arguments
                        ),
                    );
                }
            };

            match args.action.as_deref().unwrap_or("dispatch") {
                "dispatch" => dispatch(&call, &args, ctx),
                "list" => list(&call, ctx),
                "result" => fetch_result(&call, &args, ctx),
                other => err(
                    &call,
                    2,
                    format!(
                        "catus: unknown action '{}'; expected 'dispatch', 'list' or 'result'",
                        other
                    ),
                ),
            }
        })
    }
}

/// Start a subagent asynchronously.
fn dispatch(call: &ToolCall, args: &TaskArguments, ctx: &mut ToolContext<'_>) -> ToolResult {
    let agent = match args.agent.as_deref() {
        Some(a) => a,
        None => {
            return err(
                call,
                2,
                "catus: action=dispatch requires \"agent\"".to_string(),
            );
        }
    };
    let task = match args.task.as_deref() {
        Some(t) => t,
        None => {
            return err(
                call,
                2,
                "catus: action=dispatch requires \"task\"".to_string(),
            );
        }
    };
    let mode = match args
        .context
        .as_deref()
        .unwrap_or("create")
        .parse::<SubagentContextMode>()
    {
        Ok(m) => m,
        Err(e) => return err(call, 2, e),
    };

    let agent_registry = match ctx.agent_registry {
        Some(ref mut r) => r,
        None => {
            return err(call, 1, "catus: agent registry unavailable".to_string());
        }
    };
    let subagents = match ctx.subagents {
        Some(ref mut s) => s,
        None => {
            return err(call, 1, "catus: subagent manager unavailable".to_string());
        }
    };

    let definition = match agent_registry.get(agent) {
        Some(d) => d.clone(),
        None => {
            return err(call, 1, format!("catus: subagent '{}' not found", agent));
        }
    };

    let id = subagents.spawn(
        &definition,
        task.to_string(),
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
            "subagent {} started with id {}; it runs asynchronously and its result is NOT injected automatically. When it completes you receive a notice; then call this tool with {{\"action\": \"result\", \"id\": \"{}\"}} to fetch the result, or {{\"action\": \"list\"}} to check subagent states",
            agent, id, id
        ),
        stderr: String::new(),
        interaction: None,
    }
}

/// List all subagents with their states (result contents are not included).
fn list(call: &ToolCall, ctx: &mut ToolContext<'_>) -> ToolResult {
    let subagents = match ctx.subagents {
        Some(ref mut s) => s,
        None => {
            return err(call, 1, "catus: subagent manager unavailable".to_string());
        }
    };

    let subs = subagents.list();
    let stdout = if subs.is_empty() {
        "no subagents".to_string()
    } else {
        subs.iter()
            .map(|s| {
                let mut line = format!(
                    "{} [{}] {} - {}",
                    s.id,
                    s.state.as_str(),
                    s.name,
                    truncate(&s.task, 60)
                );
                if s.result.is_some() {
                    line.push_str(&format!(
                        " (result available; fetch with {{\"action\": \"result\", \"id\": \"{}\"}})",
                        s.id
                    ));
                } else if s.error.is_some() {
                    line.push_str(&format!(
                        " (failed; fetch details with {{\"action\": \"result\", \"id\": \"{}\"}})",
                        s.id
                    ));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    ToolResult {
        call: call.clone(),
        status: 0,
        stdout,
        stderr: String::new(),
        interaction: None,
    }
}

/// Fetch a finished subagent's result.
fn fetch_result(call: &ToolCall, args: &TaskArguments, ctx: &mut ToolContext<'_>) -> ToolResult {
    let id = match args.id.as_deref() {
        Some(i) => i,
        None => {
            return err(call, 2, "catus: action=result requires \"id\"".to_string());
        }
    };

    let subagents = match ctx.subagents {
        Some(ref mut s) => s,
        None => {
            return err(call, 1, "catus: subagent manager unavailable".to_string());
        }
    };

    match subagents.get(id) {
        None => err(
            call,
            1,
            format!(
                "catus: subagent '{}' not found; use {{\"action\": \"list\"}} to see known ids",
                id
            ),
        ),
        Some(s) => match (&s.result, &s.error) {
            (Some(result), _) => ToolResult {
                call: call.clone(),
                status: 0,
                stdout: result.clone(),
                stderr: String::new(),
                interaction: None,
            },
            (None, Some(error)) => err(
                call,
                1,
                format!("catus: subagent '{}' failed: {}", id, error),
            ),
            (None, None) => ToolResult {
                call: call.clone(),
                status: 0,
                stdout: format!(
                    "subagent '{}' has not finished yet (state: {}); call action=result again later",
                    id,
                    s.state.as_str()
                ),
                stderr: String::new(),
                interaction: None,
            },
        },
    }
}

fn err(call: &ToolCall, status: i32, stderr: String) -> ToolResult {
    ToolResult {
        call: call.clone(),
        status,
        stdout: String::new(),
        stderr,
        interaction: None,
    }
}

#[derive(Debug, Deserialize)]
struct TaskArguments {
    action: Option<String>,
    agent: Option<String>,
    task: Option<String>,
    context: Option<String>,
    id: Option<String>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::config::{AppConfig, TierModels};
    use crate::llm::Model;
    use crate::message::Message;
    use crate::subagent::{SubagentManager, SubagentState};
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;

    fn test_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-task".to_string(),
            name: "task".to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn test_registry(dir: &std::path::Path) -> AgentRegistry {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("coder.md"),
            "---\nname: coder\ndescription: test agent\n---\nDo things.\n",
        )
        .unwrap();
        AgentRegistry::discover(&[dir.to_path_buf()]).unwrap()
    }

    fn manager() -> SubagentManager {
        SubagentManager::new(
            Toolbox::default(),
            TierModels::default(),
            unreachable_model(),
            AppConfig::default(),
            None,
        )
    }

    /// A model whose provider endpoint refuses connections immediately, so a
    /// spawned runner fails fast without touching the network.
    fn unreachable_model() -> Model {
        Model {
            id: "test-model".to_string(),
            name: String::new(),
            context_window: 0,
            provider: crate::llm::Provider {
                name: "test".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: String::new(),
                session_header: None,
            },
        }
    }

    fn leak_registry(registry: AgentRegistry) -> &'static mut AgentRegistry {
        Box::leak(Box::new(registry))
    }

    fn test_context<'a>(
        shell_state: &'a mut ShellState,
        registry: &'a mut AgentRegistry,
        messages: &'a mut Vec<Message>,
        subagents: &'a mut SubagentManager,
    ) -> ToolContext<'a> {
        ToolContext {
            shell_state,
            skill_registry: Box::leak(Box::new(crate::skills::SkillRegistry::new())),
            active_skills: Box::leak(Box::new(Vec::new())),
            todos: Box::leak(Box::new(crate::tool::TodoList::new())),
            messages,
            toolbox: Box::leak(Box::new(Toolbox::default())),
            agent_registry: Some(registry),
            subagents: Some(subagents),
            current_agent: None,
            is_main_agent: true,
        }
    }

    #[tokio::test]
    async fn dispatch_starts_subagent_and_reports_fetch_instructions() {
        let dir = std::env::temp_dir().join(format!("catus_task_dispatch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );

        let tool = TaskTool;
        let result = tool
            .execute(
                &test_call(r#"{"agent": "coder", "task": "do things"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("started with id subagent-0-coder"));
        assert!(result.stdout.contains("\"action\": \"result\""));
        assert!(result.stdout.contains("NOT injected automatically"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_reports_states_without_result_contents() {
        let dir = std::env::temp_dir().join(format!("catus_task_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        // Seed managed subagents directly: one completed with a result, one
        // still streaming, one failed.
        let definition = registry.get("coder").unwrap().clone();
        let id1 = subagents.spawn(
            &definition,
            "done task".to_string(),
            SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            crate::skills::SkillRegistry::new(),
            None,
        );
        subagents.get_mut(&id1).unwrap().result = Some("secret result body".to_string());
        subagents.get_mut(&id1).unwrap().state = SubagentState::Completed;

        let id2 = subagents.spawn(
            &definition,
            "long running task".to_string(),
            SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            crate::skills::SkillRegistry::new(),
            None,
        );
        subagents.get_mut(&id2).unwrap().state = SubagentState::RunningTool;

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );

        let tool = TaskTool;
        let result = tool
            .execute(&test_call(r#"{"action": "list"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains(&format!("{} [completed]", id1)));
        assert!(result.stdout.contains("result available"));
        assert!(result.stdout.contains(&format!("{} [running tool]", id2)));
        // The completed result body must not leak into the listing.
        assert!(!result.stdout.contains("secret result body"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn result_returns_completed_result_and_reports_running() {
        let dir = std::env::temp_dir().join(format!("catus_task_result_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        let definition = registry.get("coder").unwrap().clone();
        let id = subagents.spawn(
            &definition,
            "do things".to_string(),
            SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            crate::skills::SkillRegistry::new(),
            None,
        );
        subagents.get_mut(&id).unwrap().result = Some("the final answer".to_string());
        subagents.get_mut(&id).unwrap().state = SubagentState::Completed;

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );

        let tool = TaskTool;
        let fetched = tool
            .execute(
                &test_call(&format!(r#"{{"action": "result", "id": "{}"}}"#, id)),
                &mut ctx,
            )
            .await;
        assert_eq!(fetched.status, 0);
        assert_eq!(fetched.stdout, "the final answer");

        // Still-running subagent: a soft "not finished yet" reply.
        let running = tool
            .execute(
                &test_call(r#"{"action": "result", "id": "subagent-99-coder"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(running.status, 1);
        assert!(running.stderr.contains("not found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn result_returns_error_details_for_failed_subagent() {
        let dir = std::env::temp_dir().join(format!("catus_task_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        let definition = registry.get("coder").unwrap().clone();
        let id = subagents.spawn(
            &definition,
            "do things".to_string(),
            SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            crate::skills::SkillRegistry::new(),
            None,
        );
        subagents.get_mut(&id).unwrap().state = SubagentState::Error;
        subagents.get_mut(&id).unwrap().error = Some("llm unavailable".to_string());

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );

        let tool = TaskTool;
        let fetched = tool
            .execute(
                &test_call(&format!(r#"{{"action": "result", "id": "{}"}}"#, id)),
                &mut ctx,
            )
            .await;
        assert_eq!(fetched.status, 1);
        assert!(fetched.stderr.contains("llm unavailable"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_unknown_action_and_missing_arguments() {
        let dir = std::env::temp_dir().join(format!("catus_task_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );

        let tool = TaskTool;
        let unknown = tool
            .execute(&test_call(r#"{"action": "nope"}"#), &mut ctx)
            .await;
        assert_eq!(unknown.status, 2);
        assert!(unknown.stderr.contains("unknown action"));

        let no_agent = tool
            .execute(&test_call(r#"{"action": "dispatch"}"#), &mut ctx)
            .await;
        assert_eq!(no_agent.status, 2);
        assert!(no_agent.stderr.contains("requires \"agent\""));

        let no_task = tool
            .execute(
                &test_call(r#"{"action": "dispatch", "agent": "coder"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(no_task.status, 2);
        assert!(no_task.stderr.contains("requires \"task\""));

        let no_id = tool
            .execute(&test_call(r#"{"action": "result"}"#), &mut ctx)
            .await;
        assert_eq!(no_id.status, 2);
        assert!(no_id.stderr.contains("requires \"id\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn subagents_cannot_spawn_or_query() {
        let dir = std::env::temp_dir().join(format!("catus_task_sub_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut subagents = manager();

        let mut shell = ShellState::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(
            &mut shell,
            leak_registry(registry),
            &mut messages,
            &mut subagents,
        );
        ctx.is_main_agent = false;

        let tool = TaskTool;
        for arguments in [
            r#"{"agent": "coder", "task": "t"}"#,
            r#"{"action": "list"}"#,
            r#"{"action": "result", "id": "subagent-0-coder"}"#,
        ] {
            let result = tool.execute(&test_call(arguments), &mut ctx).await;
            assert_eq!(result.status, 1);
            assert!(result.stderr.contains("cannot spawn further subagents"));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

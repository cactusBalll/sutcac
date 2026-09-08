//! The built-in `todo` tool: a session-scoped TODO list for the main agent.
//!
//! The list lives on the application ([`TodoList`] in [`ToolContext`]) —
//! it is not filesystem-backed and does not survive the process. Only the
//! main agent is advertised this tool; subagents never receive it (see
//! `build_subagent_toolbox` in `subagent.rs`), and execution additionally
//! rejects non-main-agent calls as a defensive check.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// One entry of the session TODO list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub text: String,
    pub done: bool,
}

/// Session-scoped TODO list owned by the application.
///
/// The list is persisted with the session snapshot (key `todos` in
/// `session_state`) so it survives an exit and `/resume`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoList {
    items: Vec<TodoItem>,
    next_id: u64,
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append items, returning their assigned ids.
    pub fn add(&mut self, texts: Vec<String>) -> Vec<u64> {
        let mut ids = Vec::with_capacity(texts.len());
        for text in texts {
            self.next_id += 1;
            let id = self.next_id;
            self.items.push(TodoItem {
                id,
                text,
                done: false,
            });
            ids.push(id);
        }
        ids
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Mark the item with `id` done. Returns false when no such item exists.
    pub fn complete(&mut self, id: u64) -> bool {
        match self.items.iter_mut().find(|i| i.id == id) {
            Some(item) => {
                item.done = true;
                true
            }
            None => false,
        }
    }

    /// Render the list for the LLM. Completed items are marked `[x]`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            let mark = if item.done { "[x]" } else { "[ ]" };
            out.push_str(&format!("{} {}. {}\n", mark, item.id, item.text));
        }
        out
    }
}

/// The built-in `todo` tool.
pub struct TodoTool;

const TODO_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "todo",
    "description": "Manage the session TODO list. Actions: \"add\" appends the given items; \"list\" shows all items with their ids and completion state; \"complete\" marks the item with the given id as done.",
    "parameters": {
      "type": "object",
      "properties": {
        "action": {
          "type": "string",
          "enum": ["add", "list", "complete"],
          "description": "Operation to perform on the TODO list."
        },
        "items": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Item texts to append. Required for action \"add\"."
        },
        "id": {
          "type": "integer",
          "description": "Id of the item to mark done. Required for action \"complete\"."
        }
      },
      "required": ["action"]
    }
  }
}"#;

impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(TODO_TOOL_SCHEMA).expect("todo tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<TodoArguments>(&call.arguments)
            .map(|args| match args {
                TodoArguments {
                    action: TodoAction::Add,
                    items: Some(items),
                    ..
                } => format!("todo: add {} item(s)", items.len()),
                TodoArguments {
                    action: TodoAction::Complete,
                    id: Some(id),
                    ..
                } => format!("todo: complete #{}", id),
                TodoArguments { action, .. } => format!("todo: {}", action.as_str()),
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
                return ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: "catus: only the main agent can use the todo tool".to_string(),
                    interaction: None,
                };
            }

            let args: TodoArguments = match serde_json::from_str(&call.arguments) {
                Ok(a) => a,
                Err(_) => {
                    return ToolResult {
                        call: call.clone(),
                        status: 2,
                        stdout: String::new(),
                        stderr: format!(
                            "catus: arguments must be a JSON object {{\"action\": \"add|list|complete\", \"items\": [...], \"id\": N}}; got: {}",
                            call.arguments
                        ),
                        interaction: None,
                    };
                }
            };

            let result = match args.action {
                TodoAction::Add => {
                    let Some(items) = args.items.filter(|i| !i.is_empty()) else {
                        return ToolResult {
                            call: call.clone(),
                            status: 2,
                            stdout: String::new(),
                            stderr: "catus: action \"add\" requires a non-empty \"items\" array"
                                .to_string(),
                            interaction: None,
                        };
                    };
                    let ids = ctx.todos.add(items);
                    format!(
                        "added item(s) {}\n\n{}",
                        format_ids(&ids),
                        ctx.todos.render()
                    )
                }
                TodoAction::List => {
                    if ctx.todos.is_empty() {
                        "the TODO list is empty".to_string()
                    } else {
                        ctx.todos.render()
                    }
                }
                TodoAction::Complete => {
                    let Some(id) = args.id else {
                        return ToolResult {
                            call: call.clone(),
                            status: 2,
                            stdout: String::new(),
                            stderr: "catus: action \"complete\" requires an \"id\"".to_string(),
                            interaction: None,
                        };
                    };
                    if !ctx.todos.complete(id) {
                        return ToolResult {
                            call: call.clone(),
                            status: 1,
                            stdout: String::new(),
                            stderr: format!("catus: no TODO item with id {}", id),
                            interaction: None,
                        };
                    }
                    format!("marked item {} done\n\n{}", id, ctx.todos.render())
                }
            };

            ToolResult {
                call: call.clone(),
                status: 0,
                stdout: result,
                stderr: String::new(),
                interaction: None,
            }
        })
    }
}

/// Arguments of a `todo` tool call.
#[derive(Debug, Deserialize)]
struct TodoArguments {
    action: TodoAction,
    items: Option<Vec<String>>,
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TodoAction {
    Add,
    List,
    Complete,
}

impl TodoAction {
    fn as_str(&self) -> &'static str {
        match self {
            TodoAction::Add => "add",
            TodoAction::List => "list",
            TodoAction::Complete => "complete",
        }
    }
}

/// Format assigned ids as `#1, #2`.
fn format_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(|id| format!("#{}", id))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::skills::SkillRegistry;
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;

    fn todo_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_abc".to_string(),
            name: "todo".to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn test_context<'a>(
        todos: &'a mut TodoList,
        messages: &'a mut Vec<crate::message::Message>,
        is_main_agent: bool,
    ) -> ToolContext<'a> {
        let toolbox: &'a Toolbox = Box::leak(Box::new(Toolbox::default()));
        ToolContext {
            shell_state: Box::leak(Box::new(ShellState::new())),
            skill_registry: Box::leak(Box::new(SkillRegistry::new())),
            active_skills: Box::leak(Box::new(Vec::new())),
            messages,
            todos,
            toolbox,
            agent_registry: Box::leak(Box::new(AgentRegistry::new())).into(),
            subagents: None,
            current_agent: None,
            is_main_agent,
        }
    }

    #[tokio::test]
    async fn todo_add_list_and_complete() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool
            .execute(
                &todo_call(r#"{"action":"add","items":["write lexer","write parser"]}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("#1, #2"));
        assert!(result.stdout.contains("[ ] 1. write lexer"));

        let result = tool
            .execute(&todo_call(r#"{"action":"list"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("[ ] 1. write lexer"));
        assert!(result.stdout.contains("[ ] 2. write parser"));

        let result = tool
            .execute(&todo_call(r#"{"action":"complete","id":1}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("[x] 1. write lexer"));
        assert!(result.stdout.contains("[ ] 2. write parser"));

        let result = tool
            .execute(&todo_call(r#"{"action":"list"}"#), &mut ctx)
            .await;
        assert!(result.stdout.contains("[x] 1. write lexer"));
    }

    #[tokio::test]
    async fn todo_list_empty_state() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool
            .execute(&todo_call(r#"{"action":"list"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("empty"));
    }

    #[tokio::test]
    async fn todo_complete_unknown_id_fails() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool
            .execute(&todo_call(r#"{"action":"complete","id":99}"#), &mut ctx)
            .await;
        assert_ne!(result.status, 0);
        assert!(result.stderr.contains("no TODO item with id 99"));
    }

    #[tokio::test]
    async fn todo_rejects_missing_or_empty_items() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool
            .execute(&todo_call(r#"{"action":"add"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("items"));

        let result = tool
            .execute(&todo_call(r#"{"action":"add","items":[]}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
    }

    #[tokio::test]
    async fn todo_complete_requires_id() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool
            .execute(&todo_call(r#"{"action":"complete"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("id"));
    }

    #[tokio::test]
    async fn todo_rejects_malformed_arguments() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, true);
        let tool = TodoTool;

        let result = tool.execute(&todo_call("not json"), &mut ctx).await;
        assert_eq!(result.status, 2);

        let result = tool
            .execute(&todo_call(r#"{"action":"bogus"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
    }

    #[tokio::test]
    async fn todo_rejects_non_main_agent() {
        let mut todos = TodoList::new();
        let mut messages = Vec::new();
        let mut ctx = test_context(&mut todos, &mut messages, false);
        let tool = TodoTool;

        let result = tool
            .execute(&todo_call(r#"{"action":"add","items":["x"]}"#), &mut ctx)
            .await;
        assert_ne!(result.status, 0);
        assert!(result.stderr.contains("only the main agent"));
        assert!(todos.is_empty());
    }

    #[test]
    fn describe_call_summarizes_actions() {
        let tool = TodoTool;
        assert_eq!(
            tool.describe_call(&todo_call(r#"{"action":"add","items":["a","b","c"]}"#)),
            "todo: add 3 item(s)"
        );
        assert_eq!(
            tool.describe_call(&todo_call(r#"{"action":"list"}"#)),
            "todo: list"
        );
        assert_eq!(
            tool.describe_call(&todo_call(r#"{"action":"complete","id":2}"#)),
            "todo: complete #2"
        );
    }
}

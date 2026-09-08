//! The built-in `read` tool.
//!
//! [`ReadTool`] reads a file, optionally starting at a 1-based line offset
//! and limited to a number of lines. The read itself runs through the
//! embedded `sutcac-sh` `readfile` builtin (via [`execute_shell_command`]),
//! so it goes through the shell's permission checks and audit logging like
//! any other command. Output lines are prefixed with their 1-based line
//! number so the model can reference them for subsequent `edit` calls.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::shell::execute_shell_command;
use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// The built-in `read` tool.
pub struct ReadTool;

/// JSON Schema for the read tool.
const READ_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "read",
    "description": "Read a text file, optionally starting at a 1-based line offset and limited to a number of lines. Output lines are prefixed with their line number. Prefer this over cat/sed for inspecting files.",
    "parameters": {
      "type": "object",
      "properties": {
        "file_path": {
          "type": "string",
          "description": "Path of the file to read."
        },
        "offset": {
          "type": "integer",
          "description": "1-based line number to start reading from. Defaults to 1."
        },
        "limit": {
          "type": "integer",
          "description": "Maximum number of lines to read. Defaults to 2000."
        }
      },
      "required": ["file_path"]
    }
  }
}"#;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(READ_TOOL_SCHEMA).expect("read tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        parse_arguments(&call.arguments)
            .map(|args| {
                let mut desc = format!("read {}", args.file_path);
                if let Some(offset) = args.offset {
                    desc.push_str(&format!(" from line {offset}"));
                }
                if let Some(limit) = args.limit {
                    desc.push_str(&format!(" ({limit} lines)"));
                }
                desc
            })
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(args) = parse_arguments(&call.arguments) else {
                log::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a single JSON object {{\"file_path\": \"...\", \"offset\": 1, \"limit\": 2000}}; \"offset\" and \"limit\" are optional integers; got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };

            log::info!(
                "read tool: {} (offset={:?}, limit={:?})",
                args.file_path,
                args.offset,
                args.limit
            );

            let mut command = format!("readfile {}", shell_quote(&args.file_path));
            if let Some(offset) = args.offset {
                command.push_str(&format!(" {offset}"));
            }
            if let Some(limit) = args.limit {
                // LIMIT requires OFFSET; keep the pair consistent.
                if args.offset.is_none() {
                    command.push_str(" 1");
                }
                command.push_str(&format!(" {limit}"));
            }
            let output = execute_shell_command(&command, ctx.shell_state);

            log::info!(
                "read tool finished: status={} stdout_len={} stderr_len={}",
                output.status,
                output.stdout.len(),
                output.stderr.len()
            );
            ToolResult {
                call: call.clone(),
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                interaction: None,
            }
        })
    }
}

/// Arguments of a `read` tool call.
#[derive(Debug, Deserialize)]
struct ReadArguments {
    file_path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

/// Extract the read arguments from a tool call's `arguments` string.
///
/// Accepts an object with a string `file_path` and optional integer
/// `offset`/`limit`. Anything else yields `None` so the caller can report a
/// malformed tool call back to the model.
fn parse_arguments(arguments: &str) -> Option<ReadArguments> {
    serde_json::from_str::<ReadArguments>(arguments).ok()
}

/// Quote a string as a single shell word so it survives lexing unchanged.
///
/// Everything is wrapped in double quotes; `"`, `\` and `$` are backslash
/// escaped (the only characters the expander treats specially inside double
/// quotes). Newlines and single quotes pass through untouched.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;

    fn read_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_abc".to_string(),
            name: "read".to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn test_shell_state() -> ShellState {
        let mut state = ShellState::new();
        state.audit_logger = sutcac_sh::audit::AuditLogger::null();
        state
    }

    fn test_context<'a>(
        shell_state: &'a mut ShellState,
        skill_registry: &'a mut SkillRegistry,
        active_skills: &'a mut Vec<String>,
        messages: &'a mut Vec<crate::message::Message>,
    ) -> ToolContext<'a> {
        let toolbox: &'a Toolbox = Box::leak(Box::new(Toolbox::default()));
        ToolContext {
            shell_state,
            skill_registry,
            active_skills,
            todos: Box::leak(Box::new(crate::tool::TodoList::new())),
            messages,
            toolbox,
            agent_registry: None,
            subagents: None,
            current_agent: None,
            is_main_agent: true,
        }
    }

    fn temp_file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn read_tool_prints_numbered_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "line one\nline two\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let args = format!(r#"{{"file_path":{:?}}}"#, file);
        let result = tool.execute(&read_call(&args), &mut ctx).await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert!(
            result.stdout.contains("line one"),
            "stdout: {}",
            result.stdout
        );
        assert!(
            result.stdout.contains("     2\tline two"),
            "stdout: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn read_tool_honours_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "l1\nl2\nl3\nl4\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let args = format!(r#"{{"file_path":{:?},"offset":2,"limit":2}}"#, file);
        let result = tool.execute(&read_call(&args), &mut ctx).await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("l2\n"), "stdout: {}", result.stdout);
        assert!(result.stdout.contains("l3\n"), "stdout: {}", result.stdout);
        assert!(!result.stdout.contains("l1\n"), "stdout: {}", result.stdout);
        assert!(!result.stdout.contains("l4\n"), "stdout: {}", result.stdout);
    }

    #[tokio::test]
    async fn read_tool_limit_without_offset_starts_at_line_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "l1\nl2\nl3\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let args = format!(r#"{{"file_path":{:?},"limit":1}}"#, file);
        let result = tool.execute(&read_call(&args), &mut ctx).await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("l1\n"), "stdout: {}", result.stdout);
        assert!(!result.stdout.contains("l2\n"), "stdout: {}", result.stdout);
    }

    #[tokio::test]
    async fn read_tool_reports_missing_file() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let result = tool
            .execute(
                &read_call(r#"{"file_path":"/nonexistent/path/file.txt"}"#),
                &mut ctx,
            )
            .await;
        assert_ne!(result.status, 0);
        assert!(
            result.stderr.contains("No such file"),
            "stderr: {}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn read_tool_rejects_malformed_arguments() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let result = tool
            .execute(&read_call(r#"{"path":"a.txt"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("format error"));
    }

    #[tokio::test]
    async fn read_tool_rejects_non_integer_offset() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ReadTool;
        let result = tool
            .execute(
                &read_call(r#"{"file_path":"a.txt","offset":"x"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("format error"));
    }

    #[test]
    fn shell_quote_escapes_double_quote_specials() {
        assert_eq!(shell_quote("abc"), "\"abc\"");
        assert_eq!(shell_quote("it's"), "\"it's\"");
        assert_eq!(shell_quote(""), "\"\"");
        assert_eq!(shell_quote("x \"y\" \\ $z"), "\"x \\\"y\\\" \\\\ \\$z\"");
    }
}

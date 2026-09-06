//! The built-in `shell` tool.
//!
//! [`ShellTool`] runs a command through the embedded `sutcac-sh` interpreter
//! and returns its output. This module also hosts [`execute_shell_command`],
//! the helper that actually drives the shell execution and resets the shell
//! state after each call.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use sutcac_sh::exec::{CommandOutput, ShellState, execute_command};
use sutcac_sh::parser::Parser;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// The built-in `shell` tool.
pub struct ShellTool;

/// JSON Schema for the shell tool.
const SHELL_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "shell",
    "description": "Execute a shell command via the embedded sutcac-sh interpreter.",
    "parameters": {
      "type": "object",
      "properties": {
        "command": {
          "type": "string",
          "description": "The shell command to execute."
        }
      },
      "required": ["command"]
    }
  }
}"#;

impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(SHELL_TOOL_SCHEMA).expect("shell tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        parse_command(&call.arguments).unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(command) = parse_command(&call.arguments) else {
                // Malformed arguments: report back to the model and consume the
                // call instead of leaving it pending forever.
                log::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a single JSON object {{\"command\": \"<shell command>\"}} with exactly one string \"command\" field; got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };

            log::info!("running shell command: {}", command);
            let output = execute_shell_command(&command, ctx.shell_state);
            log::info!(
                "shell command finished: status={} stdout_len={} stderr_len={}",
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

/// Arguments of a `shell` tool call.
#[derive(Debug, Deserialize)]
struct ShellArguments {
    command: String,
}

/// Extract the shell command from a tool call's `arguments` string.
///
/// Only the canonical form is accepted: an object with a single string
/// `"command"` field. Anything else yields `None` so the caller can report
/// a malformed tool call back to the model.
fn parse_command(arguments: &str) -> Option<String> {
    serde_json::from_str::<ShellArguments>(arguments)
        .ok()
        .map(|args| args.command)
}

/// Execute a shell command against the provided `ShellState`.
///
/// After execution the shell context is reset to its pre-call state: the
/// working directory is restored and variable/function state is cleared so
/// that successive tool calls do not accumulate side effects.
pub fn execute_shell_command(cmd: &str, state: &mut ShellState) -> CommandOutput {
    let original_cwd = state.cwd.clone();
    let original_env_cwd = std::env::current_dir().unwrap_or_else(|_| original_cwd.clone());

    let output = match Parser::new(cmd) {
        Ok(mut parser) => match parser.parse() {
            Ok(cmds) => {
                let mut status = 0;
                let mut stdout = String::new();
                let mut stderr = String::new();
                for cmd in cmds {
                    let out = execute_command(&cmd, state);
                    status = out.status;
                    stdout.push_str(&out.stdout);
                    stderr.push_str(&out.stderr);
                }
                CommandOutput::with_output(status, stdout, stderr)
            }
            Err(e) => CommandOutput::with_output(
                2,
                String::new(),
                format!(
                    "parse error: {}. Hint: check shell syntax (quotes, parentheses, and reserved words).",
                    e
                ),
            ),
        },
        Err(e) => CommandOutput::with_output(
            2,
            String::new(),
            format!(
                "lexer error: {}. Hint: check for unclosed quotes or invalid characters.",
                e
            ),
        ),
    };

    // Restore the working directory so later tool calls start from the same
    // directory the Agent was launched in.
    if let Err(e) = std::env::set_current_dir(&original_env_cwd) {
        log::warn!("failed to restore working directory: {}", e);
    }
    state.cwd = original_cwd;

    // Clear mutable shell state so variables, functions and positional
    // parameters set by one tool call do not leak into the next.
    state.vars.clear();
    state.exported.clear();
    state.args.clear();
    state.funcs.clear();
    state.last_status = 0;
    state.last_bg_pid = None;

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use crate::tool::Toolbox;

    fn shell_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_abc".to_string(),
            name: "shell".to_string(),
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
            messages,
            toolbox,
            agent_registry: None,
            subagents: None,
            current_agent: None,
            is_main_agent: true,
        }
    }

    #[tokio::test]
    async fn shell_tool_executes_command() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ShellTool;
        let result = tool
            .execute(&shell_call(r#"{"command":"echo hi"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("hi"));
    }

    #[tokio::test]
    async fn shell_tool_rejects_malformed_arguments() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = ShellTool;
        let result = tool.execute(&shell_call(r#"{"cmd":"ls"}"#), &mut ctx).await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("format error"));
    }

    #[test]
    fn parse_command_accepts_canonical_form() {
        assert_eq!(
            parse_command(r#"{"command":"ls -la"}"#),
            Some("ls -la".to_string())
        );
    }

    #[test]
    fn parse_command_rejects_duplicate_keys() {
        assert_eq!(
            parse_command(
                r#"{"command":"cat Cargo.toml AGENTS.md","command":"find . -name '*.rs' | head -5"}"#
            ),
            None
        );
    }

    #[test]
    fn parse_command_rejects_non_string_command() {
        assert_eq!(parse_command(r#"{"command":["pwd","ls -la"]}"#), None);
    }

    #[test]
    fn parse_command_rejects_non_canonical_input() {
        // A blob with no string "command" field must not reach the shell.
        assert_eq!(parse_command(r#"{"cmd":"ls"}"#), None);
        // A bare quoted string is not the canonical form either.
        assert_eq!(parse_command(r#""ls -la""#), None);
    }

    #[test]
    fn command_substitution_executes_inner_command() {
        let mut state = test_shell_state();
        let output = execute_shell_command("echo $(echo hi)", &mut state);
        assert_eq!(output.status, 0);
        assert!(
            output.stdout.contains("hi"),
            "stdout should contain substituted output, got: {:?}",
            output.stdout
        );
    }
}

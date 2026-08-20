//! Tool execution for the catus Agent.
//!
//! Implements the OpenAI-compatible function-calling protocol for a single
//! tool: `shell`, which runs a command through the embedded `sutcac-sh`
//! interpreter.

use serde::{Deserialize, Serialize};
use sutcac_sh::exec::{CommandOutput, ShellState, execute_command};
use sutcac_sh::parser::Parser;

/// JSON Schema description for the shell tool.
pub const SHELL_TOOL_SCHEMA: &str = r#"{
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

/// A parsed tool invocation returned by the model.
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

impl ToolCall {
    /// Extract the shell command from the `command` argument.
    pub fn shell_command(&self) -> Option<String> {
        if self.name != "shell" {
            return None;
        }
        match serde_json::from_str::<ShellArguments>(&self.arguments) {
            Ok(args) => Some(args.command),
            Err(_) => {
                // Fallback: if the model produced a bare string, use it as-is.
                let trimmed = self.arguments.trim();
                if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
                    Some(trimmed[1..trimmed.len() - 1].to_string())
                } else {
                    Some(self.arguments.clone())
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShellArguments {
    command: String,
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call: ToolCall,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
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

/// Return the tool definitions advertised to the LLM.
pub fn shell_tool_definition() -> ToolDefinition {
    serde_json::from_str(SHELL_TOOL_SCHEMA).expect("shell tool schema is valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_tool_call() {
        let call = ToolCall {
            id: "call_abc".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls -la"}"#.to_string(),
        };
        assert_eq!(call.shell_command(), Some("ls -la".to_string()));
    }

    #[test]
    fn shell_tool_definition_valid() {
        let def = shell_tool_definition();
        assert_eq!(def.function.name, "shell");
    }

    #[test]
    fn command_substitution_executes_inner_command() {
        let mut state = ShellState::new();
        state.audit_logger = sutcac_sh::audit::AuditLogger::null();
        let output = execute_shell_command("echo $(echo hi)", &mut state);
        assert_eq!(output.status, 0);
        assert!(
            output.stdout.contains("hi"),
            "stdout should contain substituted output, got: {:?}",
            output.stdout
        );
    }
}

//! The built-in `use_skill` tool.
//!
//! [`SkillTool`] activates an Agent Skill by name: it loads the skill's
//! instructions and injects them into the conversation as a system message.
//! This replaces the old text-marker protocol (`use_skill:<name>`) with a
//! regular tool call.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};
use crate::message::Message;

/// The built-in `use_skill` tool.
pub struct SkillTool;

/// JSON Schema for the use_skill tool.
const SKILL_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "use_skill",
    "description": "Activate an Agent Skill by name, injecting its instructions into the conversation. Use when a task matches a skill's description.",
    "parameters": {
      "type": "object",
      "properties": {
        "name": {
          "type": "string",
          "description": "The name of the skill to activate."
        }
      },
      "required": ["name"]
    }
  }
}"#;

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(SKILL_TOOL_SCHEMA).expect("use_skill tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<SkillArguments>(&call.arguments)
            .map(|args| format!("use_skill: {}", args.name))
            .unwrap_or_else(|_| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            match activate_skill(ctx, &call.arguments) {
                Ok(stdout) => ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout,
                    stderr: String::new(),
                },
                Err(stderr) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("catus: {}", stderr),
                },
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct SkillArguments {
    name: String,
}

/// Shared skill-activation logic used by `SkillTool`.
///
/// Validates the arguments, loads the skill's instructions, records the skill
/// as active, and injects a system message carrying the instructions.
fn activate_skill(ctx: &mut ToolContext<'_>, arguments: &str) -> Result<String, String> {
    let args: SkillArguments = serde_json::from_str(arguments).map_err(|_| {
        format!(
            "arguments must be a JSON object with a string \"name\" field; got: {}",
            arguments
        )
    })?;
    let name = args.name.trim().to_string();
    if name.is_empty() {
        return Err("skill name must not be empty".to_string());
    }
    if ctx.active_skills.contains(&name) {
        return Ok(format!("skill '{}' is already active", name));
    }

    let skill = ctx
        .skill_registry
        .activate(&name)?
        .ok_or_else(|| format!("skill not found: {}", name))?;
    let instructions = skill.instructions().unwrap_or("").to_string();
    if instructions.trim().is_empty() {
        return Ok(format!("skill '{}' has no instructions", name));
    }
    ctx.active_skills.push(name.clone());
    ctx.messages.push(Message::system(format!(
        "Skill '{}' instructions:\n{}",
        name, instructions
    )));
    Ok(format!("activated skill '{}'", name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use sutcac_sh::exec::ShellState;

    fn skill_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_skill".to_string(),
            name: "use_skill".to_string(),
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
        messages: &'a mut Vec<Message>,
    ) -> ToolContext<'a> {
        ToolContext {
            shell_state,
            skill_registry,
            active_skills,
            messages,
        }
    }

    #[tokio::test]
    async fn skill_tool_activates_and_injects_instructions() {
        let dir = std::env::temp_dir().join(format!("catus_skill_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo.\n---\nDo the demo thing.",
        )
        .unwrap();

        let registry = SkillRegistry::discover(&[dir.clone()]).unwrap();
        let mut ctx_fields = (test_shell_state(), registry, Vec::new(), Vec::new());
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = SkillTool;
        let result = tool
            .execute(&skill_call(r#"{"name":"demo"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("activated skill 'demo'"));
        assert_eq!(ctx.active_skills.as_slice(), &["demo".to_string()]);
        let system = ctx
            .messages
            .iter()
            .find(|m| m.is_system() && m.content.contains("Skill 'demo' instructions"))
            .expect("system message with instructions");
        assert!(system.content.contains("Do the demo thing."));

        // Re-activation is idempotent.
        let again = tool
            .execute(&skill_call(r#"{"name":"demo"}"#), &mut ctx)
            .await;
        assert_eq!(again.status, 0);
        assert_eq!(
            ctx.messages
                .iter()
                .filter(|m| m.content.contains("Skill 'demo' instructions"))
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skill_tool_rejects_unknown_skill() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = SkillTool;
        let result = tool
            .execute(&skill_call(r#"{"name":"missing"}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 1);
        assert!(result.stderr.contains("skill not found"));
        assert!(ctx.messages.is_empty());
    }

    #[tokio::test]
    async fn skill_tool_rejects_malformed_arguments() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = SkillTool;
        let result = tool.execute(&skill_call(r#""demo""#), &mut ctx).await;
        assert_eq!(result.status, 1);
        assert!(result.stderr.contains("name"));
    }
}

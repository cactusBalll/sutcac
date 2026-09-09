//! The built-in `ask_permission` tool.
//!
//! [`AskPermissionTool`] lets the model ask the user to grant permission
//! tags (e.g. `network`) to the shell so currently denied operations can
//! proceed. Like `ask_user`, the tool never blocks: it validates the tags
//! and returns a [`ToolResult`] carrying an [`InteractionRequest`]. The
//! application (`app.rs`) opens the ask overlay; when the user chooses an
//! answer, the tags are granted on the shell's [`PermissionPolicy`] for the
//! rest of the session (or denied), and the result is sent back to the model.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::ask_user::{AskOption, AskQuestion, InteractionRequest};
use super::{Tool, ToolCall, ToolDefinition, ToolResult};

/// Overlay option: grant the tags for the rest of the session.
pub const GRANT_SESSION: &str = "Allow for this session";
/// Overlay option: refuse the grant.
pub const GRANT_DENY: &str = "Deny";

/// The built-in `ask_permission` tool.
pub struct AskPermissionTool;

/// Arguments of an `ask_permission` tool call. `tags` accepts either a
/// single string or an array of strings; the legacy `tag` field is still
/// accepted.
#[derive(Debug, Deserialize)]
struct AskPermissionArguments {
    #[serde(default)]
    tag: Option<StringOrList>,
    #[serde(default)]
    tags: Option<StringOrList>,
    /// Optional explanation shown to the user.
    #[serde(default)]
    reason: Option<String>,
}

/// A JSON value that is either a string or an array of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn into_tags(self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s],
            StringOrList::Many(v) => v,
        }
    }
}

/// JSON Schema for the ask_permission tool.
const ASK_PERMISSION_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "ask_permission",
    "description": "Ask the user to grant extra permission tags to the shell so currently denied operations can run. Use this only after a command was rejected with `permission denied`. You may request several tags at once. The user chooses to allow them for the whole session or to deny them; the result message reports the decision. Do not use this to ask questions (use ask_user instead).",
    "parameters": {
      "type": "object",
      "properties": {
        "tags": {
          "anyOf": [
            { "type": "string", "description": "Single tag or comma-separated tag list." },
            {
              "type": "array",
              "items": { "type": "string" },
              "description": "List of tags to request.",
              "minItems": 1
            }
          ],
          "description": "Permission tag(s) to request, e.g. \"network\", \"read\", \"write\", or a custom tag from the shell policy (case-insensitive). Use the tag names as they appear in the permission-denied error message."
        },
        "reason": {
          "type": "string",
          "description": "Optional short explanation of why the permissions are needed, shown to the user."
        }
      },
      "required": ["tags"]
    }
  }
}"#;

/// Extract and normalize the requested tags from an `ask_permission` call's
/// `arguments` string. Accepts `tags` as a string (possibly comma-separated)
/// or an array of strings, plus the legacy single `tag` field. `read`/`write`
/// map to `READ`/`WRITE`; other tags are uppercased, mirroring the shell's
/// tag normalization. Returns `None` when the arguments are malformed or no
/// non-empty tag is present.
pub fn parse_ask_permission_tags(arguments: &str) -> Option<Vec<String>> {
    let args: AskPermissionArguments = serde_json::from_str(arguments).ok()?;
    let mut raw = Vec::new();
    if let Some(tags) = args.tags {
        raw.extend(tags.into_tags());
    }
    if let Some(tag) = args.tag {
        raw.extend(tag.into_tags());
    }
    let mut tags = Vec::new();
    for entry in raw {
        for tag in entry.split(',') {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            let normalized = match tag.to_ascii_lowercase().as_str() {
                "read" => "READ".to_string(),
                "write" => "WRITE".to_string(),
                _ => tag.to_ascii_uppercase(),
            };
            if !tags.contains(&normalized) {
                tags.push(normalized);
            }
        }
    }
    if tags.is_empty() { None } else { Some(tags) }
}

impl Tool for AskPermissionTool {
    fn name(&self) -> &str {
        "ask_permission"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(ASK_PERMISSION_TOOL_SCHEMA)
            .expect("ask_permission tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        parse_ask_permission_tags(&call.arguments)
            .map(|tags| format!("ask_permission: {}", tags.join(", ")))
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut super::ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(tags) = parse_ask_permission_tags(&call.arguments) else {
                log::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a JSON object {{\"tags\": [...] , \"reason\": \"...\"}} with at least one non-empty tag; got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };
            let tag_list = tags.join(", ");

            // Nothing to ask for when the policy already allows everything.
            if matches!(
                ctx.shell_state.permissions.mode,
                sutcac_sh::permissions::PermissionMode::AllowAll
            ) {
                return ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout: format!(
                        "no permission needed: the shell policy already allows everything (tags {} were not granted because it is unnecessary)",
                        tag_list
                    ),
                    stderr: String::new(),
                    interaction: None,
                };
            }

            let args: AskPermissionArguments =
                serde_json::from_str(&call.arguments).expect("arguments parsed successfully above");
            let prompt = match args.reason.as_deref().map(str::trim) {
                Some(reason) if !reason.is_empty() => format!(
                    "Grant shell permission(s) \"{}\"?\n\nReason: {}",
                    tag_list, reason
                ),
                _ => format!("Grant shell permission(s) \"{}\"?", tag_list),
            };
            let question = AskQuestion {
                prompt,
                title: "Permission".to_string(),
                options: vec![
                    AskOption {
                        label: GRANT_SESSION.to_string(),
                        description: Some("The tags stay granted until catus exits.".to_string()),
                    },
                    AskOption {
                        label: GRANT_DENY.to_string(),
                        description: None,
                    },
                ],
                multi_select: false,
            };

            // The turn pauses here: the application opens the ask overlay and
            // completes this call once the user decides.
            ToolResult {
                call: call.clone(),
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
                interaction: Some(InteractionRequest {
                    questions: vec![question],
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use crate::tool::ToolContext;
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;
    use sutcac_sh::permissions::{PermissionPolicy, PermissionSet};

    fn ask_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_ask_perm".to_string(),
            name: "ask_permission".to_string(),
            arguments: arguments.to_string(),
        }
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

    #[tokio::test]
    async fn ask_permission_tool_requests_interaction() {
        let mut ctx_fields = (
            ShellState::with_policy_and_logger(
                PermissionPolicy::parse("deny:network").unwrap(),
                sutcac_sh::audit::AuditLogger::null(),
            ),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        let result = tool
            .execute(
                &ask_call(r#"{"tags":["network"],"reason":"curl the API"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 0);
        let request = result.interaction.expect("interaction request");
        assert_eq!(request.questions.len(), 1);
        let question = &request.questions[0];
        assert!(question.prompt.contains("NETWORK"));
        assert!(question.prompt.contains("curl the API"));
        assert!(!question.multi_select);
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].label, GRANT_SESSION);
        assert_eq!(question.options[1].label, GRANT_DENY);
    }

    #[tokio::test]
    async fn ask_permission_tool_requests_multiple_tags_at_once() {
        let mut ctx_fields = (
            ShellState::with_policy_and_logger(
                PermissionPolicy::parse("deny:network").unwrap(),
                sutcac_sh::audit::AuditLogger::null(),
            ),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        let result = tool
            .execute(&ask_call(r#"{"tags":["network","write"]}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        let request = result.interaction.expect("interaction request");
        let prompt = &request.questions[0].prompt;
        assert!(prompt.contains("NETWORK"));
        assert!(prompt.contains("WRITE"));
    }

    #[tokio::test]
    async fn ask_permission_tool_short_circuits_allow_all() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        let result = tool
            .execute(&ask_call(r#"{"tags":["write"]}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.interaction.is_none());
        assert!(result.stdout.contains("no permission needed"));
    }

    #[tokio::test]
    async fn ask_permission_tool_rejects_missing_tag() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        for arguments in [
            "{}",
            r#"{"tags":"  "}"#,
            r#"{"tags":[]}"#,
            r#"{"tags":["", "  "]}"#,
            "not json",
        ] {
            let result = tool.execute(&ask_call(arguments), &mut ctx).await;
            assert_eq!(result.status, 2, "arguments: {}", arguments);
            assert!(
                result.stderr.contains("format error"),
                "arguments: {}\nstderr: {}",
                arguments,
                result.stderr
            );
            assert!(result.interaction.is_none());
        }
    }

    #[test]
    fn parse_tags_normalizes_and_deduplicates() {
        assert_eq!(
            parse_ask_permission_tags(r#"{"tags":"network"}"#),
            Some(vec!["NETWORK".to_string()])
        );
        assert_eq!(
            parse_ask_permission_tags(r#"{"tags":[" Read ", "write"]}"#),
            Some(vec!["READ".to_string(), "WRITE".to_string()])
        );
        // Comma-separated string form.
        assert_eq!(
            parse_ask_permission_tags(r#"{"tags":"network, my_tag"}"#),
            Some(vec!["NETWORK".to_string(), "MY_TAG".to_string()])
        );
        // Legacy single `tag` field still accepted.
        assert_eq!(
            parse_ask_permission_tags(r#"{"tag":"network"}"#),
            Some(vec!["NETWORK".to_string()])
        );
        // Duplicates collapse.
        assert_eq!(
            parse_ask_permission_tags(r#"{"tags":["a","a","A"]}"#),
            Some(vec!["A".to_string()])
        );
        assert_eq!(parse_ask_permission_tags("{}"), None);
    }

    #[test]
    fn definition_parses_and_advertises_ask_permission() {
        let tool = AskPermissionTool;
        let definition = tool.definition();
        assert_eq!(tool.name(), "ask_permission");
        assert_eq!(definition.function.name, "ask_permission");
        assert_eq!(definition.tool_type, "function");
        assert!(definition.function.description.contains("permission"));
    }

    #[test]
    fn describe_call_lists_the_tags() {
        let tool = AskPermissionTool;
        let call = ask_call(r#"{"tags":["network","write"]}"#);
        assert_eq!(tool.describe_call(&call), "ask_permission: NETWORK, WRITE");
    }

    #[test]
    fn grant_tag_changes_policy_check() {
        use sutcac_sh::permissions::Permission;
        let mut network = PermissionSet::empty();
        network.insert(Permission::Custom("NETWORK".to_string()));
        let mut policy = PermissionPolicy::parse("deny:network").unwrap();
        assert!(policy.check(&network).is_err());
        policy.grant_tag("network");
        assert!(policy.check(&network).is_ok());
    }
}

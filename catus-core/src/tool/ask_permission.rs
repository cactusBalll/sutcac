//! The built-in `ask_permission` tool.
//!
//! [`AskPermissionTool`] lets the model ask the user to grant permission
//! tags (e.g. `network`) or directory read/write access to the shell so
//! currently denied operations can proceed. Like `ask_user`, the tool never
//! blocks: it validates the request and returns a [`ToolResult`] carrying an
//! [`InteractionRequest`]. The application (`app.rs`) opens the ask overlay;
//! when the user chooses an answer, the tags and paths are granted on the
//! shell's [`PermissionPolicy`] for the rest of the session (or denied), and
//! the result is sent back to the model.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::Deserialize;

use super::ask_user::{AskOption, AskQuestion, InteractionRequest};
use super::{Tool, ToolCall, ToolDefinition, ToolResult};

/// Overlay option: grant the tags/paths for the rest of the session.
pub const GRANT_SESSION: &str = "Allow for this session";
/// Overlay option: refuse the grant.
pub const GRANT_DENY: &str = "Deny";

/// The built-in `ask_permission` tool.
pub struct AskPermissionTool;

/// Arguments of an `ask_permission` tool call. `tags`, `read_paths`, and
/// `write_paths` each accept either a single string or an array of strings;
/// the legacy `tag` field is still accepted. At least one of the three must
/// be present.
#[derive(Debug, Deserialize)]
struct AskPermissionArguments {
    #[serde(default)]
    tag: Option<StringOrList>,
    #[serde(default)]
    tags: Option<StringOrList>,
    #[serde(default)]
    read_paths: Option<StringOrList>,
    #[serde(default)]
    write_paths: Option<StringOrList>,
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

/// A validated `ask_permission` request: permission tags and directory
/// read/write access to grant for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskPermissionRequest {
    pub tags: Vec<String>,
    /// Directories to grant read access for (absolute paths).
    pub read_paths: Vec<PathBuf>,
    /// Directories to grant read-write access for (absolute paths).
    pub write_paths: Vec<PathBuf>,
}

impl AskPermissionRequest {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.read_paths.is_empty() && self.write_paths.is_empty()
    }

    /// One-line summary used in the overlay prompt and tool call previews.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.tags.is_empty() {
            parts.push(format!("tags {}", self.tags.join(", ")));
        }
        if !self.read_paths.is_empty() {
            parts.push(format!(
                "read {}",
                self.read_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.write_paths.is_empty() {
            parts.push(format!(
                "write {}",
                self.write_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.join("; ")
    }
}

/// JSON Schema for the ask_permission tool.
const ASK_PERMISSION_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "ask_permission",
    "description": "Ask the user to grant extra permissions to the shell so currently denied operations can run: permission tags (e.g. \"network\") and/or directory read/write access when a path was rejected as outside the allowed directories. Use this only after a command was rejected with `permission denied`. You may request several items at once. The user chooses to allow them for the whole session or to deny them; the result message reports the decision. Do not use this to ask questions (use ask_user instead).",
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
        "read_paths": {
          "anyOf": [
            { "type": "string", "description": "Single directory path or comma-separated list." },
            {
              "type": "array",
              "items": { "type": "string" },
              "description": "List of directory paths.",
              "minItems": 1
            }
          ],
          "description": "Director(y/ies) to grant READ access to (relative paths are resolved against the current working directory). Request this when a command was denied because a path is outside the allowed directories."
        },
        "write_paths": {
          "anyOf": [
            { "type": "string", "description": "Single directory path or comma-separated list." },
            {
              "type": "array",
              "items": { "type": "string" },
              "description": "List of directory paths.",
              "minItems": 1
            }
          ],
          "description": "Director(y/ies) to grant READ+WRITE access to (relative paths are resolved against the current working directory). Request this when a command was denied because it writes outside the allowed directories."
        },
        "reason": {
          "type": "string",
          "description": "Optional short explanation of why the permissions are needed, shown to the user."
        }
      },
      "anyOf": [
        { "required": ["tags"] },
        { "required": ["read_paths"] },
        { "required": ["write_paths"] }
      ]
    }
  }
}"#;

/// Split a comma-separated string-or-list into trimmed, non-empty entries.
fn split_entries(list: StringOrList) -> Vec<String> {
    let mut entries = Vec::new();
    for raw in list.into_tags() {
        for entry in raw.split(',') {
            let entry = entry.trim();
            if !entry.is_empty() {
                entries.push(entry.to_string());
            }
        }
    }
    entries
}

/// Extract and normalize the requested tags and paths from an
/// `ask_permission` call's `arguments` string. Tags accept `tags` as a
/// string (possibly comma-separated) or an array of strings, plus the legacy
/// single `tag` field; `read`/`write` map to `READ`/`WRITE`, other tags are
/// uppercased. Paths are resolved against `cwd` and returned as absolute
/// paths. Returns `None` when the arguments are malformed or nothing is
/// requested.
pub fn parse_ask_permission_request(
    arguments: &str,
    cwd: &std::path::Path,
) -> Option<AskPermissionRequest> {
    let args: AskPermissionArguments = serde_json::from_str(arguments).ok()?;
    let mut request = AskPermissionRequest {
        tags: Vec::new(),
        read_paths: Vec::new(),
        write_paths: Vec::new(),
    };
    if let Some(tags) = args.tags {
        request.tags.extend(split_entries(tags));
    }
    if let Some(tag) = args.tag {
        request.tags.extend(split_entries(tag));
    }
    let mut tags = Vec::new();
    for tag in request.tags {
        let normalized = match tag.to_ascii_lowercase().as_str() {
            "read" => "READ".to_string(),
            "write" => "WRITE".to_string(),
            _ => tag.to_ascii_uppercase(),
        };
        if !tags.contains(&normalized) {
            tags.push(normalized);
        }
    }
    request.tags = tags;
    for entry in args.read_paths.map(split_entries).unwrap_or_default() {
        let path = resolve_request_path(&entry, cwd);
        if !request.read_paths.contains(&path) {
            request.read_paths.push(path);
        }
    }
    for entry in args.write_paths.map(split_entries).unwrap_or_default() {
        let path = resolve_request_path(&entry, cwd);
        if !request.write_paths.contains(&path) {
            request.write_paths.push(path);
        }
    }
    if request.is_empty() {
        None
    } else {
        Some(request)
    }
}

fn resolve_request_path(entry: &str, cwd: &std::path::Path) -> PathBuf {
    let path = std::path::Path::new(entry);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize_lexical(&abs)
}

/// Remove `.` and `..` components lexically (the paths may not exist yet).
fn normalize_lexical(path: &std::path::Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Legacy helper: extract and normalize only the requested tags. Returns
/// `None` when the arguments are malformed or no non-empty tag is present.
pub fn parse_ask_permission_tags(arguments: &str) -> Option<Vec<String>> {
    parse_ask_permission_request(arguments, std::path::Path::new("/")).map(|r| r.tags)
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
        parse_ask_permission_request(&call.arguments, std::path::Path::new("/"))
            .map(|request| format!("ask_permission: {}", request.summary()))
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut super::ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let cwd = ctx.shell_state.cwd.clone();
            let Some(request) = parse_ask_permission_request(&call.arguments, &cwd) else {
                log::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a JSON object {{\"tags\": [...], \"read_paths\": [...], \"write_paths\": [...], \"reason\": \"...\"}} with at least one requested tag or path; got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };
            let summary = request.summary();

            // Tag requests are pointless when the policy already allows
            // everything, but path grants are still meaningful (path checks
            // are independent of the mode).
            if request.read_paths.is_empty()
                && request.write_paths.is_empty()
                && matches!(
                    ctx.shell_state.permissions.mode,
                    sutcac_sh::permissions::PermissionMode::AllowAll
                )
            {
                return ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout: format!(
                        "no permission needed: the shell policy already allows everything (tags {} were not granted because it is unnecessary)",
                        request.tags.join(", ")
                    ),
                    stderr: String::new(),
                    interaction: None,
                };
            }

            let args: AskPermissionArguments =
                serde_json::from_str(&call.arguments).expect("arguments parsed successfully above");
            let prompt = match args.reason.as_deref().map(str::trim) {
                Some(reason) if !reason.is_empty() => format!(
                    "Grant shell permission(s)?\n\nRequest: {}\nReason: {}",
                    summary, reason
                ),
                _ => format!("Grant shell permission(s)?\n\nRequest: {}", summary),
            };
            let question = AskQuestion {
                prompt,
                title: "Permission".to_string(),
                options: vec![
                    AskOption {
                        label: GRANT_SESSION.to_string(),
                        description: Some(
                            "The tags and paths stay granted until catus exits.".to_string(),
                        ),
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
    fn describe_call_lists_the_request() {
        let tool = AskPermissionTool;
        let call = ask_call(r#"{"tags":["network","write"]}"#);
        assert_eq!(
            tool.describe_call(&call),
            "ask_permission: tags NETWORK, WRITE"
        );
        let call = ask_call(r#"{"write_paths":["/tmp/data"]}"#);
        assert_eq!(tool.describe_call(&call), "ask_permission: write /tmp/data");
    }

    #[test]
    fn parse_request_paths_resolve_against_cwd() {
        let cwd = std::path::Path::new("/home/user/ws");
        let request = parse_ask_permission_request(
            r#"{"read_paths":"/tmp/data, /var/log","write_paths":["/opt/app"]}"#,
            cwd,
        )
        .unwrap();
        assert!(request.tags.is_empty());
        assert_eq!(
            request.read_paths,
            vec![
                std::path::PathBuf::from("/tmp/data"),
                std::path::PathBuf::from("/var/log")
            ]
        );
        assert_eq!(
            request.write_paths,
            vec![std::path::PathBuf::from("/opt/app")]
        );

        // Relative paths resolve against the cwd; duplicates collapse.
        let request =
            parse_ask_permission_request(r#"{"read_paths":["../shared", "shared"]}"#, cwd).unwrap();
        assert_eq!(
            request.read_paths,
            vec![
                std::path::PathBuf::from("/home/user/shared"),
                std::path::PathBuf::from("/home/user/ws/shared")
            ]
        );

        // Tags and paths can be combined.
        let request =
            parse_ask_permission_request(r#"{"tags":"network","write_paths":["/tmp"]}"#, cwd)
                .unwrap();
        assert_eq!(request.tags, vec!["NETWORK".to_string()]);
        assert_eq!(request.write_paths, vec![std::path::PathBuf::from("/tmp")]);

        assert_eq!(parse_ask_permission_request("{}", cwd), None);
    }

    #[tokio::test]
    async fn ask_permission_tool_requests_path_access() {
        let mut ctx_fields = (
            ShellState::with_policy_and_logger(
                PermissionPolicy::allow_all().with_base_dir(std::path::Path::new("/ws")),
                sutcac_sh::audit::AuditLogger::null(),
            ),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        state.cwd = std::path::PathBuf::from("/ws");
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        let result = tool
            .execute(
                &ask_call(r#"{"write_paths":["/tmp/build"],"reason":"build output"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 0);
        let request = result.interaction.expect("interaction request");
        let prompt = &request.questions[0].prompt;
        assert!(prompt.contains("/tmp/build"));
        assert!(prompt.contains("build output"));
    }

    #[tokio::test]
    async fn ask_permission_tool_asks_for_paths_even_under_allow_all() {
        let mut ctx_fields = (
            ShellState::with_policy_and_logger(
                PermissionPolicy::allow_all().with_base_dir(std::path::Path::new("/ws")),
                sutcac_sh::audit::AuditLogger::null(),
            ),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskPermissionTool;
        // Path checks are independent of the mode, so a path request must
        // still reach the user even though tags would be redundant.
        let result = tool
            .execute(&ask_call(r#"{"read_paths":["/etc"]}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 0);
        assert!(result.interaction.is_some());
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

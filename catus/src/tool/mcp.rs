//! The per-server MCP gateway tool.
//!
//! [`McpServerTool`] collapses every tool advertised by one MCP server into a
//! single catus [`Tool`] named `mcp_{server}` so the request tool definitions
//! stay small regardless of how many MCP tools exist. The LLM enumerates the
//! server's tools with the `list` action (name + one-line description), fetch
//! one tool's JSON input schema with `help`, and executes it with `invoke`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{FunctionDefinition, Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};
use crate::mcp::{CachedTool, McpManager};

/// The per-server MCP gateway tool exposed to the LLM.
pub struct McpServerTool {
    manager: Arc<McpManager>,
    server: String,
    tool_name: String,
}

impl McpServerTool {
    /// Build the gateway tool for one connected server.
    pub fn new(manager: Arc<McpManager>, server: &str) -> Self {
        Self {
            manager,
            server: server.to_string(),
            tool_name: gateway_tool_name(server),
        }
    }

    /// Dispatch one gateway call by its `action` field.
    ///
    /// On success returns `(stdout, stderr)`; stderr carries MCP results that
    /// report diagnostic content.
    async fn run(&self, call: &ToolCall) -> Result<(String, String), String> {
        let args: GatewayArguments = serde_json::from_str(&call.arguments).map_err(|_| {
            format!(
                "arguments must be a JSON object with a string \"action\" field \
                 (\"list\", \"help\", or \"invoke\"); got: {}",
                call.arguments
            )
        })?;

        match args.action.as_str() {
            "list" => {
                let catalog = self.catalog()?;
                Ok((format_catalog(&self.server, catalog), String::new()))
            }
            "help" => {
                let tool = args.require_tool()?;
                let catalog = self.catalog()?;
                let cached = catalog.iter().find(|t| t.name == tool).ok_or_else(|| {
                    format!("mcp server '{}' has no tool '{}'", self.server, tool)
                })?;
                let schema = serde_json::to_string_pretty(&cached.input_schema)
                    .unwrap_or_else(|_| "{}".to_string());
                Ok((
                    format!(
                        "tool '{}': {}\n\n{}",
                        cached.name, cached.description, schema
                    ),
                    String::new(),
                ))
            }
            "invoke" => {
                let tool = args.require_tool()?.to_string();
                let arguments = match args.arguments {
                    None | Some(Value::Null) => serde_json::Map::new(),
                    Some(Value::Object(map)) => map,
                    Some(other) => {
                        return Err(format!(
                            "\"arguments\" must be a JSON object; got: {}",
                            other
                        ));
                    }
                };
                match self.manager.call_tool(&self.server, &tool, arguments).await {
                    Ok(result) => {
                        log::info!(
                            "mcp tool finished: {}:{} status={} stdout_len={} stderr_len={}",
                            self.server,
                            tool,
                            result.status,
                            result.stdout.len(),
                            result.stderr.len()
                        );
                        Ok((result.stdout, result.stderr))
                    }
                    Err(e) => {
                        log::warn!("mcp tool failed: {}:{} error={}", self.server, tool, e);
                        Err(format!("mcp tool failed: {}", e))
                    }
                }
            }
            other => Err(format!(
                "unknown action \"{}\"; expected \"list\", \"help\", or \"invoke\"",
                other
            )),
        }
    }

    /// The server's cached tool catalog.
    fn catalog(&self) -> Result<&[CachedTool], String> {
        self.manager
            .tool_catalog(&self.server)
            .ok_or_else(|| format!("unknown mcp server: {}", self.server))
    }
}

/// Sanitize a server name and prefix it into a valid gateway tool name.
///
/// Function names must match `[a-zA-Z0-9_-]+`; every other character in the
/// server name is replaced with `_`.
pub fn gateway_tool_name(server: &str) -> String {
    let sanitized: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("mcp_{}", sanitized)
}

/// Arguments accepted by the gateway tool.
#[derive(Debug, Deserialize)]
struct GatewayArguments {
    action: String,
    tool: Option<String>,
    arguments: Option<Value>,
}

impl GatewayArguments {
    /// Require the `tool` field, returning an error message otherwise.
    fn require_tool(&self) -> Result<&str, String> {
        self.tool
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                format!(
                    "the \"tool\" field is required for action \"{}\"",
                    self.action
                )
            })
    }
}

/// Render the cached catalog as a compact `name: description` listing.
fn format_catalog(server: &str, catalog: &[CachedTool]) -> String {
    if catalog.is_empty() {
        return format!("no tools available on mcp server '{}'", server);
    }
    let mut out = String::new();
    for tool in catalog {
        out.push_str(&format!(
            "- {}: {}\n",
            tool.name,
            tool.first_description_line()
        ));
    }
    out.push_str("\nUse action \"help\" with a tool name to get its JSON schema.");
    out
}

impl Tool for McpServerTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: self.tool_name.clone(),
                description: format!(
                    "Tools from the MCP server '{}'. Actions: \"list\" enumerates the \
                     available tools (name + one-line description); \"help\" returns the \
                     JSON input schema of one tool; \"invoke\" calls it with arguments. \
                     Start with \"list\".",
                    self.server
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "help", "invoke"],
                            "description": "list: enumerate tools; help: show one tool's schema; invoke: call a tool."
                        },
                        "tool": {
                            "type": "string",
                            "description": "The MCP tool name. Required for \"help\" and \"invoke\"."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "Arguments object forwarded to the MCP tool. Required for \"invoke\"."
                        }
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<GatewayArguments>(&call.arguments)
            .ok()
            .map(|args| match args.tool.as_deref() {
                Some(tool) if !tool.trim().is_empty() => {
                    format!("{}: {} {}", self.tool_name, args.action, tool.trim())
                }
                _ => format!("{}: {}", self.tool_name, args.action),
            })
            .unwrap_or_else(|| self.tool_name.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            match self.run(call).await {
                Ok((stdout, stderr)) => ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout,
                    stderr,
                    interaction: None,
                },
                Err(stderr) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("catus: {}", stderr),
                    interaction: None,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use serde_json::json;

    fn cached(name: &str, description: &str) -> CachedTool {
        CachedTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn gateway_tool_name_sanitizes_server_names() {
        assert_eq!(gateway_tool_name("calc"), "mcp_calc");
        assert_eq!(gateway_tool_name("my server.v2"), "mcp_my_server_v2");
        assert_eq!(gateway_tool_name("文件"), "mcp___");
    }

    #[test]
    fn format_catalog_lists_names_and_descriptions() {
        let catalog = vec![
            cached("sum", "Add two numbers.\nDetails."),
            cached("div", "Divide."),
        ];
        let out = format_catalog("calc", &catalog);
        assert!(out.contains("- sum: Add two numbers."));
        assert!(out.contains("- div: Divide."));
        assert!(!out.contains("Details."));
        assert!(out.contains("\"help\""));
    }

    #[test]
    fn format_catalog_empty() {
        assert_eq!(
            format_catalog("calc", &[]),
            "no tools available on mcp server 'calc'"
        );
    }

    fn test_manager() -> Arc<McpManager> {
        let catalogs = HashMap::from_iter([(
            "calc".to_string(),
            vec![cached("sum", "Add two numbers.\nDetails.")],
        )]);
        Arc::new(McpManager::with_catalogs(catalogs))
    }

    #[test]
    fn definition_advertises_gateway_actions() {
        let tool = McpServerTool::new(test_manager(), "calc");
        assert_eq!(tool.name(), "mcp_calc");
        let def = tool.definition();
        assert_eq!(def.function.name, "mcp_calc");
        assert!(def.function.description.contains("calc"));
        assert_eq!(
            def.function.parameters.get("required"),
            Some(&json!(["action"]))
        );
        assert_eq!(
            def.function.parameters["properties"]["action"]["enum"],
            json!(["list", "help", "invoke"])
        );
    }

    #[test]
    fn describe_call_compact_format() {
        let tool = McpServerTool::new(test_manager(), "calc");
        let call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"invoke","tool":"sum"}"#.to_string(),
        };
        assert_eq!(tool.describe_call(&call), "mcp_calc: invoke sum");

        let bare = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"list"}"#.to_string(),
        };
        assert_eq!(tool.describe_call(&bare), "mcp_calc: list");

        let malformed = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: "not json".to_string(),
        };
        assert_eq!(tool.describe_call(&malformed), "mcp_calc");
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let tool = McpServerTool::new(test_manager(), "calc");
        let call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"teleport"}"#.to_string(),
        };
        let mut ctx = test_context();
        let result = tool.execute(&call, &mut ctx).await;
        assert_eq!(result.status, 1);
        assert!(result.stderr.contains("unknown action \"teleport\""));
    }

    #[tokio::test]
    async fn malformed_arguments_are_rejected() {
        let tool = McpServerTool::new(test_manager(), "calc");
        let call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: "\"list\"".to_string(),
        };
        let mut ctx = test_context();
        let result = tool.execute(&call, &mut ctx).await;
        assert_eq!(result.status, 1);
        assert!(result.stderr.contains("action"));
    }

    #[tokio::test]
    async fn list_and_help_serve_cached_catalog() {
        let tool = McpServerTool::new(test_manager(), "calc");
        let mut ctx = test_context();

        let list_call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"list"}"#.to_string(),
        };
        let result = tool.execute(&list_call, &mut ctx).await;
        assert_eq!(result.status, 0);
        assert!(
            result.stdout.contains("- sum: Add two numbers."),
            "{}",
            result.stdout
        );
        assert!(!result.stdout.contains("Details."), "{}", result.stdout);

        let help_call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"help","tool":"sum"}"#.to_string(),
        };
        let result = tool.execute(&help_call, &mut ctx).await;
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains("tool 'sum'"), "{}", result.stdout);
        assert!(result.stdout.contains("\"type\""), "{}", result.stdout);

        // help for an unknown tool is rejected.
        let help_missing = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"help","tool":"nope"}"#.to_string(),
        };
        let result = tool.execute(&help_missing, &mut ctx).await;
        assert_eq!(result.status, 1);
        assert!(
            result.stderr.contains("no tool 'nope'"),
            "{}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn invoke_requires_object_arguments() {
        let tool = McpServerTool::new(test_manager(), "calc");
        let mut ctx = test_context();

        let call = ToolCall {
            id: "c".to_string(),
            name: "mcp_calc".to_string(),
            arguments: r#"{"action":"invoke","tool":"sum","arguments":"bad"}"#.to_string(),
        };
        let result = tool.execute(&call, &mut ctx).await;
        assert_eq!(result.status, 1);
        assert!(result.stderr.contains("arguments"), "{}", result.stderr);
    }

    fn test_context() -> ToolContext<'static> {
        ToolContext {
            shell_state: Box::leak(Box::new({
                let mut state = sutcac_sh::exec::ShellState::new();
                state.audit_logger = sutcac_sh::audit::AuditLogger::null();
                state
            })),
            skill_registry: Box::leak(Box::new(crate::skills::SkillRegistry::new())),
            active_skills: Box::leak(Box::new(Vec::new())),
            todos: Box::leak(Box::new(crate::tool::TodoList::new())),
            messages: Box::leak(Box::new(Vec::new())),
            toolbox: Box::leak(Box::new(super::super::Toolbox::default())),
            agent_registry: None,
            subagents: None,
            current_agent: None,
            is_main_agent: true,
        }
    }
}

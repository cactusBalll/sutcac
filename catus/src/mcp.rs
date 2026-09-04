//! MCP client support for catus.
//!
//! Connects to configured MCP servers via the `rmcp` SDK — either local stdio
//! child processes or remote Streamable HTTP endpoints — discovers their tools,
//! and forwards tool calls from the LLM to the correct server. Each discovered
//! tool is wrapped in an [`McpTool`] and registered in the application's
//! `Toolbox` alongside the built-in tools.

use std::collections::HashMap;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock, Tool as RmcpTool},
    service::{RoleClient, RunningService},
    transport::{TokioChildProcess, streamable_http_client::StreamableHttpClientTransportConfig},
};
use serde_json::Value;
use tokio::process::Command;

use crate::config::{McpServerConfig, McpTransport};
use crate::tool::{FunctionDefinition, Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// Separator used to prefix MCP tool names with their server name.
const SERVER_PREFIX_SEP: &str = "__";

/// Errors that can occur while using MCP servers.
#[derive(Debug)]
pub enum McpError {
    /// Failed to spawn or initialize the MCP server connection.
    Connect(String),
    /// An MCP protocol or transport error occurred.
    Rmcp(rmcp::service::ServiceError),
    /// The server prefix of the tool name does not match any connected server.
    UnknownServer(String),
    /// The tool name is not in the expected `{server}__{tool}` format.
    InvalidToolName(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Connect(msg) => write!(f, "mcp connect error: {}", msg),
            McpError::Rmcp(e) => write!(f, "mcp error: {}", e),
            McpError::UnknownServer(name) => write!(f, "unknown mcp server: {}", name),
            McpError::InvalidToolName(name) => {
                write!(f, "invalid mcp tool name (expected server__tool): {}", name)
            }
        }
    }
}

impl std::error::Error for McpError {}

impl From<rmcp::service::ServiceError> for McpError {
    fn from(e: rmcp::service::ServiceError) -> Self {
        McpError::Rmcp(e)
    }
}

impl From<std::io::Error> for McpError {
    fn from(e: std::io::Error) -> Self {
        McpError::Connect(e.to_string())
    }
}

impl From<rmcp::service::ClientInitializeError> for McpError {
    fn from(e: rmcp::service::ClientInitializeError) -> Self {
        McpError::Connect(e.to_string())
    }
}

/// A connection to a single MCP server.
pub struct McpClient {
    peer: RunningService<RoleClient, ()>,
}

impl McpClient {
    /// Connect to a configured MCP server using its configured transport.
    async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        let peer = match config.transport {
            McpTransport::Stdio => {
                if config.command.is_empty() {
                    return Err(McpError::Connect(format!(
                        "mcp server '{}' has no command",
                        config.name
                    )));
                }
                let mut command = Command::new(&config.command);
                command.args(&config.args);
                for (key, value) in &config.env {
                    command.env(key, value);
                }

                let transport = TokioChildProcess::new(command)?;
                ().serve(transport).await?
            }
            McpTransport::StreamableHttp => {
                let url = config.url.as_deref().ok_or_else(|| {
                    McpError::Connect(format!(
                        "mcp server '{}' has no url for streamable-http transport",
                        config.name
                    ))
                })?;
                let transport_config = StreamableHttpClientTransportConfig::with_uri(url)
                    .custom_headers(build_headers(&config.headers)?);
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::from_config(transport_config);
                ().serve(transport).await?
            }
        };
        Ok(Self { peer })
    }

    /// Return all tools advertised by this server.
    async fn list_tools(&self) -> Result<Vec<RmcpTool>, McpError> {
        Ok(self.peer.list_all_tools().await?)
    }

    /// Call a tool on this server.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Map<String, Value>,
    ) -> Result<ToolResult, McpError> {
        let params = CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments);
        let result = self.peer.call_tool(params).await?;
        Ok(convert_call_tool_result(result))
    }
}

/// Manager for all configured MCP server connections.
pub struct McpManager {
    clients: HashMap<String, McpClient>,
}

impl McpManager {
    /// Connect to every configured MCP server.
    ///
    /// Returns the manager and a list of per-server connection warnings. A
    /// single failing server does not prevent the others from being used.
    pub async fn connect(servers: &[McpServerConfig]) -> (Self, Vec<String>) {
        let mut clients = HashMap::new();
        let mut warnings = Vec::new();

        for config in servers {
            match McpClient::connect(config).await {
                Ok(client) => {
                    log::info!("connected to mcp server '{}'", config.name);
                    clients.insert(config.name.clone(), client);
                }
                Err(e) => {
                    log::warn!("failed to connect to mcp server '{}': {}", config.name, e);
                    warnings.push(format!("{}: {}", config.name, e));
                }
            }
        }

        (Self { clients }, warnings)
    }

    /// Return true if no MCP servers are connected.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Number of connected MCP servers.
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Names of all connected MCP servers.
    pub fn server_names(&self) -> Vec<&str> {
        self.clients.keys().map(|s| s.as_str()).collect()
    }

    /// Collect tool definitions from all connected servers, prefixing each tool
    /// name with its server name.
    pub async fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        for (server_name, client) in &self.clients {
            match client.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        definitions.push(tool_to_definition(server_name, tool));
                    }
                }
                Err(e) => {
                    log::warn!(
                        "failed to list tools from mcp server '{}': {}",
                        server_name,
                        e
                    );
                }
            }
        }
        definitions
    }

    /// Call an MCP tool. `prefixed_name` must be `{server}__{tool}`.
    pub async fn call_tool(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        let (server_name, tool_name) = split_prefixed_name(&call.name)?;
        let client = self
            .clients
            .get(server_name)
            .ok_or_else(|| McpError::UnknownServer(server_name.to_string()))?;

        let arguments = parse_arguments(&call.arguments)?;
        let result = client.call_tool(tool_name, arguments).await?;
        Ok(ToolResult {
            call: call.clone(),
            ..result
        })
    }
}

/// A single MCP tool converted into a catus [`Tool`].
///
/// Holds the shared manager and the tool's advertised definition; execution
/// forwards the call to the owning server through the manager.
pub struct McpTool {
    manager: Arc<McpManager>,
    definition: ToolDefinition,
}

impl McpTool {
    fn new(manager: Arc<McpManager>, definition: ToolDefinition) -> Self {
        Self {
            manager,
            definition,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.definition.function.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut ToolContext<'_>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            match self.manager.call_tool(call).await {
                Ok(result) => {
                    log::info!(
                        "mcp tool finished: {} status={} stdout_len={} stderr_len={}",
                        call.name,
                        result.status,
                        result.stdout.len(),
                        result.stderr.len()
                    );
                    result
                }
                Err(e) => {
                    log::warn!("mcp tool failed: {} error={}", call.name, e);
                    ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: format!("catus: mcp tool failed: {}", e),
                        interaction: None,
                    }
                }
            }
        })
    }
}

/// Wrap every tool advertised by the connected servers as an [`McpTool`].
///
/// Servers that fail to list their tools are logged and skipped.
pub async fn mcp_tools(manager: Arc<McpManager>) -> Vec<McpTool> {
    let mut tools = Vec::new();
    for definition in manager.all_tool_definitions().await {
        tools.push(McpTool::new(manager.clone(), definition));
    }
    tools
}

/// Convert configured header strings into the `http` crate types expected by
/// the Streamable HTTP transport config.
fn build_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| McpError::Connect(format!("invalid header name {:?}: {}", name, e)))?;
            let value = HeaderValue::from_str(value).map_err(|e| {
                McpError::Connect(format!("invalid header value for {:?}: {}", name, e))
            })?;
            Ok((name, value))
        })
        .collect()
}

/// Parse the JSON arguments from a tool call. Empty or non-object input is
/// treated as an empty object.
fn parse_arguments(arguments: &str) -> Result<serde_json::Map<String, Value>, McpError> {
    if arguments.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let value: Value = serde_json::from_str(arguments)
        .map_err(|e| McpError::Connect(format!("invalid tool arguments: {}", e)))?;
    match value {
        Value::Object(obj) => Ok(obj),
        _ => Ok(serde_json::Map::new()),
    }
}

/// Split a prefixed MCP tool name into `(server_name, tool_name)`.
fn split_prefixed_name(prefixed_name: &str) -> Result<(&str, &str), McpError> {
    let (server, tool) = prefixed_name
        .split_once(SERVER_PREFIX_SEP)
        .ok_or_else(|| McpError::InvalidToolName(prefixed_name.to_string()))?;
    if server.is_empty() || tool.is_empty() {
        return Err(McpError::InvalidToolName(prefixed_name.to_string()));
    }
    Ok((server, tool))
}

/// Convert an rmcp `Tool` into the OpenAI-compatible `ToolDefinition`, prefixing
/// the tool name with the server name.
fn tool_to_definition(server_name: &str, tool: RmcpTool) -> ToolDefinition {
    let name = format!("{}{}{}", server_name, SERVER_PREFIX_SEP, tool.name);
    let description = tool
        .description
        .as_deref()
        .unwrap_or("MCP tool")
        .to_string();
    let parameters = Value::Object(tool.input_schema.as_ref().clone());

    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name,
            description,
            parameters,
        },
    }
}

/// Convert an rmcp `CallToolResult` into the `ToolResult` format used by catus.
fn convert_call_tool_result(result: rmcp::model::CallToolResult) -> ToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let status = if is_error { 1 } else { 0 };

    let mut stdout = String::new();
    let mut stderr = String::new();
    for block in result.content {
        match block {
            ContentBlock::Text(t) => {
                append_line(&mut stdout, &t.text);
            }
            ContentBlock::Resource(r) => {
                append_line(&mut stdout, &r.get_text());
            }
            other => {
                append_line(
                    &mut stderr,
                    &format!("unsupported content block: {:?}", other),
                );
            }
        }
    }

    ToolResult {
        call: ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        },
        status,
        stdout,
        stderr,
        interaction: None,
    }
}

fn append_line(buf: &mut String, line: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_valid_prefixed_name() {
        assert_eq!(
            split_prefixed_name("filesystem__read_file").unwrap(),
            ("filesystem", "read_file")
        );
    }

    #[test]
    fn split_rejects_missing_prefix() {
        assert!(matches!(
            split_prefixed_name("read_file").unwrap_err(),
            McpError::InvalidToolName(_)
        ));
    }

    #[test]
    fn split_rejects_empty_parts() {
        assert!(matches!(
            split_prefixed_name("__read_file").unwrap_err(),
            McpError::InvalidToolName(_)
        ));
        assert!(matches!(
            split_prefixed_name("filesystem__").unwrap_err(),
            McpError::InvalidToolName(_)
        ));
    }

    #[test]
    fn build_headers_converts_entries() {
        let headers = HashMap::from_iter([
            ("Authorization".to_string(), "Bearer sk-test".to_string()),
            ("X-Custom".to_string(), "value".to_string()),
        ]);
        let built = build_headers(&headers).unwrap();
        assert_eq!(built.len(), 2);
        assert_eq!(
            built.get(&HeaderName::from_static("authorization")),
            Some(&HeaderValue::from_static("Bearer sk-test"))
        );
    }

    #[test]
    fn build_headers_rejects_invalid_name() {
        let headers = HashMap::from_iter([("bad name".to_string(), "v".to_string())]);
        assert!(matches!(
            build_headers(&headers).unwrap_err(),
            McpError::Connect(_)
        ));
    }

    #[test]
    fn build_headers_rejects_invalid_value() {
        let headers = HashMap::from_iter([("x-ok".to_string(), "bad\nvalue".to_string())]);
        assert!(matches!(
            build_headers(&headers).unwrap_err(),
            McpError::Connect(_)
        ));
    }

    #[test]
    fn parse_arguments_accepts_object() {
        let args = r#"{"path": "/tmp"}"#;
        let parsed = parse_arguments(args).unwrap();
        assert_eq!(parsed.get("path"), Some(&json!("/tmp")));
    }

    #[test]
    fn parse_arguments_treats_empty_as_empty_object() {
        let parsed = parse_arguments("").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn tool_definition_gets_prefixed_name() {
        let tool = RmcpTool::new(
            "read_file",
            "Read a file",
            serde_json::Map::from_iter([("type".to_string(), json!("object"))]),
        );
        let def = tool_to_definition("fs", tool);
        assert_eq!(def.function.name, "fs__read_file");
        assert_eq!(def.function.description, "Read a file");
        assert_eq!(def.tool_type, "function");
    }

    #[test]
    fn convert_text_result() {
        let result = rmcp::model::CallToolResult::success(vec![ContentBlock::text("hello")]);
        let converted = convert_call_tool_result(result);
        assert_eq!(converted.status, 0);
        assert_eq!(converted.stdout, "hello");
        assert!(converted.stderr.is_empty());
    }

    #[test]
    fn convert_error_result() {
        let result =
            rmcp::model::CallToolResult::error(vec![ContentBlock::text("something went wrong")]);
        let converted = convert_call_tool_result(result);
        assert_eq!(converted.status, 1);
        assert_eq!(converted.stdout, "something went wrong");
    }
}

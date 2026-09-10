//! MCP client support for catus.
//!
//! Connects to configured MCP servers via the `rmcp` SDK — either local stdio
//! child processes or remote Streamable HTTP endpoints — and caches each
//! server's tool catalog at connect time. Tool calls from the LLM are routed
//! through the per-server gateway tool ([`crate::tool::McpServerTool`]), which
//! uses [`McpManager`] to forward invocations to the right server. Caching the
//! catalog keeps `list`/`help`/`/mcp list` free of server round-trips and lets
//! a single gateway tool per server stand in for every advertised MCP tool.

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
use crate::tool::{ToolCall, ToolResult};

/// Errors that can occur while using MCP servers.
#[derive(Debug)]
pub enum McpError {
    /// Failed to spawn or initialize the MCP server connection.
    Connect(String),
    /// An MCP protocol or transport error occurred.
    Rmcp(rmcp::service::ServiceError),
    /// The server name does not match any connected server.
    UnknownServer(String),
    /// The tool name is not advertised by the given server.
    UnknownTool { server: String, tool: String },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Connect(msg) => write!(f, "mcp connect error: {}", msg),
            McpError::Rmcp(e) => write!(f, "mcp error: {}", e),
            McpError::UnknownServer(name) => write!(f, "unknown mcp server: {}", name),
            McpError::UnknownTool { server, tool } => {
                write!(f, "mcp server '{}' has no tool '{}'", server, tool)
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

/// A lightweight snapshot of one tool advertised by an MCP server.
///
/// The full JSON input schema is kept so the gateway tool can serve it on
/// demand (`help`) without contacting the server again.
#[derive(Debug, Clone)]
pub struct CachedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl CachedTool {
    /// Convert an rmcp `Tool` advertisement into a cached snapshot.
    fn from_rmcp(tool: RmcpTool) -> Self {
        Self {
            name: tool.name.to_string(),
            description: tool
                .description
                .map(|d| d.to_string())
                .unwrap_or_else(|| "MCP tool".to_string()),
            input_schema: Value::Object(tool.input_schema.as_ref().clone()),
        }
    }

    /// First line of the description, for compact listing.
    pub fn first_description_line(&self) -> &str {
        self.description.lines().next().unwrap_or("")
    }
}

/// Session-scoped set of temporarily disabled MCP server names, shared
/// between `App` and every registered gateway tool.
///
/// Toggling a server mutates this set instead of adding/removing gateway
/// tools, so the advertised tool list — and with it the request prefix the
/// provider caches — stays byte-stable. A disabled gateway tool stays
/// advertised but rejects every action until re-enabled.
#[derive(Clone, Default)]
pub struct McpDisabledServers(Arc<std::sync::Mutex<Vec<String>>>);

impl McpDisabledServers {
    pub fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.lock().unwrap().iter().any(|n| n == name)
    }

    pub fn set(&self, name: &str, disabled: bool) {
        let mut guard = self.0.lock().unwrap();
        if disabled {
            if !guard.iter().any(|n| n == name) {
                guard.push(name.to_string());
            }
        } else {
            guard.retain(|n| n != name);
        }
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn replace(&self, names: Vec<String>) {
        *self.0.lock().unwrap() = names;
    }

    /// A copy of the disabled names (for persistence and snapshots).
    pub fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
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
///
/// Each server's tool catalog is fetched once at connect time and cached in
/// [`McpManager::catalogs`]; only `invoke` traffic goes to the server.
///
/// The connection maps sit behind mutexes so servers can be connected on
/// demand (the web UI's connect action) while the manager lives behind an
/// `Arc` shared with the gateway tools.
pub struct McpManager {
    clients: std::sync::Mutex<HashMap<String, Arc<McpClient>>>,
    catalogs: std::sync::Mutex<HashMap<String, Vec<CachedTool>>>,
}

impl McpManager {
    /// An empty manager with no connections (used before the first connect).
    pub fn empty() -> Self {
        Self {
            clients: std::sync::Mutex::new(HashMap::new()),
            catalogs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Connect to every configured MCP server and cache their tool catalogs.
    ///
    /// Returns the manager and a list of per-server connection warnings. A
    /// single failing server does not prevent the others from being used.
    /// A server that connects but fails to list its tools is kept with an
    /// empty catalog and produces a warning.
    pub async fn connect(servers: &[McpServerConfig]) -> (Self, Vec<String>) {
        let mut clients: HashMap<String, Arc<McpClient>> = HashMap::new();
        let mut catalogs = HashMap::new();
        let mut warnings = Vec::new();

        for config in servers {
            match McpClient::connect(config).await {
                Ok(client) => {
                    log::info!("connected to mcp server '{}'", config.name);
                    let catalog = match client.list_tools().await {
                        Ok(tools) => {
                            log::info!(
                                "mcp server '{}' advertises {} tool(s)",
                                config.name,
                                tools.len()
                            );
                            tools.into_iter().map(CachedTool::from_rmcp).collect()
                        }
                        Err(e) => {
                            log::warn!(
                                "failed to list tools from mcp server '{}': {}",
                                config.name,
                                e
                            );
                            warnings.push(format!("{}: {}", config.name, e));
                            Vec::new()
                        }
                    };
                    catalogs.insert(config.name.clone(), catalog);
                    clients.insert(config.name.clone(), Arc::new(client));
                }
                Err(e) => {
                    log::warn!("failed to connect to mcp server '{}': {}", config.name, e);
                    warnings.push(format!("{}: {}", config.name, e));
                }
            }
        }

        (
            Self {
                clients: std::sync::Mutex::new(clients),
                catalogs: std::sync::Mutex::new(catalogs),
            },
            warnings,
        )
    }

    /// Connect a single server on demand and cache its tool catalog.
    ///
    /// Used by the web UI's connect action for servers that failed at
    /// startup or were configured after boot. An already-connected server is
    /// reconnected only by explicit request; callers should check
    /// [`McpManager::is_connected`] first.
    pub async fn connect_one(&self, config: &McpServerConfig) -> Result<Vec<CachedTool>, McpError> {
        let client = McpClient::connect(config).await?;
        let catalog = client.list_tools().await?;
        let cached: Vec<CachedTool> = catalog.into_iter().map(CachedTool::from_rmcp).collect();
        log::info!(
            "connected to mcp server '{}' ({} tool(s))",
            config.name,
            cached.len()
        );
        self.catalogs
            .lock()
            .unwrap()
            .insert(config.name.clone(), cached.clone());
        self.clients
            .lock()
            .unwrap()
            .insert(config.name.clone(), Arc::new(client));
        Ok(cached)
    }

    /// Whether one server currently has a live connection.
    pub fn is_connected(&self, server: &str) -> bool {
        self.clients.lock().unwrap().contains_key(server)
    }

    /// Drop one server's connection and cached tool catalog. Returns whether
    /// a connection was removed. Used before a reconnect and when a server
    /// leaves the effective configuration.
    pub fn disconnect(&self, server: &str) -> bool {
        let mut clients = self.clients.lock().unwrap();
        let had = clients.remove(server).is_some();
        drop(clients);
        self.catalogs.lock().unwrap().remove(server);
        had
    }

    /// Return true if no MCP servers are connected.
    pub fn is_empty(&self) -> bool {
        self.clients.lock().unwrap().is_empty()
    }

    /// Number of connected MCP servers.
    pub fn len(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Names of all connected MCP servers.
    pub fn server_names(&self) -> Vec<String> {
        // Sorted so the gateway-tool registration order (part of the
        // advertised tool list) is deterministic across toolbox rebuilds.
        let mut names: Vec<String> = self.clients.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    /// The cached tool catalog of one server, cloned out from under the lock.
    pub fn tool_catalog(&self, server: &str) -> Option<Vec<CachedTool>> {
        self.catalogs.lock().unwrap().get(server).cloned()
    }

    /// Invoke one tool on a server. `server` must be a connected server name
    /// and `tool_name` must be advertised by it (checked against the cached
    /// catalog); `arguments` is forwarded verbatim to the server.
    pub async fn call_tool(
        &self,
        server: &str,
        tool_name: &str,
        arguments: serde_json::Map<String, Value>,
    ) -> Result<ToolResult, McpError> {
        if !self.is_connected(server) {
            return Err(McpError::UnknownServer(server.to_string()));
        }
        let known = self
            .catalogs
            .lock()
            .unwrap()
            .get(server)
            .map(|c| c.iter().any(|t| t.name == tool_name))
            .unwrap_or(false);
        if !known {
            return Err(McpError::UnknownTool {
                server: server.to_string(),
                tool: tool_name.to_string(),
            });
        }

        let client = self.clients.lock().unwrap().get(server).cloned();
        let Some(client) = client else {
            return Err(McpError::UnknownServer(server.to_string()));
        };
        client.call_tool(tool_name, arguments).await
    }
}

#[cfg(test)]
impl McpManager {
    /// Build a manager holding only cached catalogs (no live connections).
    ///
    /// Enough for `list`/`help` tests; `invoke` reports an unknown server.
    pub(crate) fn with_catalogs(catalogs: HashMap<String, Vec<CachedTool>>) -> Self {
        Self {
            clients: std::sync::Mutex::new(HashMap::new()),
            catalogs: std::sync::Mutex::new(catalogs),
        }
    }
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

    fn cached(name: &str, description: &str) -> CachedTool {
        CachedTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn cached_tool_first_description_line() {
        let tool = cached("sum", "Add two numbers.\nMore details.");
        assert_eq!(tool.first_description_line(), "Add two numbers.");
        let empty = cached("x", "");
        assert_eq!(empty.first_description_line(), "");
    }

    #[test]
    fn cached_tool_from_rmcp_defaults_description() {
        let tool = CachedTool::from_rmcp(RmcpTool::new_with_raw(
            "read_file",
            None,
            serde_json::Map::from_iter([("type".to_string(), json!("object"))]),
        ));
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description, "MCP tool");
        assert_eq!(tool.input_schema, json!({"type": "object"}));
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

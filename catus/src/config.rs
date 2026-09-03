//! Configuration for the catus Agent tool.
//!
//! All settings live in a single TOML file, searched in this order:
//!
//! 1. `./.sutcac/config.toml` (current working directory)
//! 2. `$XDG_CONFIG_HOME/catus/config.toml`
//! 3. `~/.config/catus/config.toml`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use log::LevelFilter;
use serde::{Deserialize, Serialize};
use sutcac_sh::config::ShellConfig;

/// Full application configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub api: ApiConfig,
    pub agent: AgentConfig,
    pub shell: Option<ShellConfig>,
    /// Optional MCP client configuration. When present, catus connects to the
    /// configured MCP servers and exposes their tools to the LLM alongside the
    /// built-in `shell` tool.
    pub mcp: Option<McpConfig>,
}

/// MCP client configuration.
#[derive(Debug, Deserialize, Clone, Serialize, Default)]
#[serde(default)]
pub struct McpConfig {
    /// MCP servers to connect to at startup.
    pub servers: Vec<McpServerConfig>,
}

/// Configuration for a single MCP server connection.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Human-readable name for this server. Used to prefix tool names, e.g.
    /// `{name}__read_file`.
    pub name: String,
    /// How to reach this server. Defaults to spawning a local stdio child.
    pub transport: McpTransport,
    /// Executable to spawn. Resolved via PATH if not an absolute path. Only
    /// used for `transport = "stdio"`; may be omitted for remote servers.
    pub command: String,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// Optional environment variables added to the spawned process.
    pub env: HashMap<String, String>,
    /// Server endpoint URL. Required for `transport = "streamable-http"`.
    pub url: Option<String>,
    /// Custom HTTP headers sent with every request, e.g.
    /// `Authorization = "Bearer sk-..."`. Only used for streamable-http.
    pub headers: HashMap<String, String>,
}

/// Transport used to reach an MCP server.
#[derive(Debug, Deserialize, Clone, Copy, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    /// Spawn a local child process and talk stdio (default).
    #[default]
    Stdio,
    /// Connect to a remote Streamable HTTP endpoint (`url`).
    StreamableHttp,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: McpTransport::default(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        }
    }
}

/// OpenAI-compatible API configuration.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default)]
pub struct ApiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Agent behavior configuration.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_tool_rounds: usize,
    /// Optional directory that holds JSON conversation-history files. When set,
    /// catus can load a previous conversation with the `/resume` command and
    /// saves the current session to a timestamped JSON file on exit.
    pub history_path: Option<PathBuf>,
    /// Optional log file path. When omitted, defaults to `.sutcac/catus.log`.
    pub log_path: Option<PathBuf>,
    /// Optional log level: trace, debug, info, warn, error. Defaults to info.
    pub log_level: String,
    /// Optional additional directories to scan for Agent Skills.
    pub skill_paths: Option<Vec<PathBuf>>,
    /// Whether to include discovered skills in the system prompt automatically.
    pub auto_include_skills: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            agent: AgentConfig::default(),
            shell: None,
            mcp: None,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: concat!(
                "You are a helpful assistant running inside a simplified Bash shell. ",
                "You can execute shell commands using the provided `shell` function. ",
                "Call it with a JSON object like {\"command\": \"the command\"}. ",
                "The user will see the command output and you can continue. ",
                "When you have enough information, answer the user directly without tools."
            )
            .to_string(),
            max_tool_rounds: 30,
            history_path: None,
            log_path: None,
            log_level: "info".to_string(),
            skill_paths: None,
            auto_include_skills: true,
        }
    }
}

impl AppConfig {
    /// Load the configuration from the first available config file.
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::find_config_file().ok_or(
            "no config file found; create .sutcac/config.toml or ~/.config/catus/config.toml",
        )?;
        let contents = std::fs::read_to_string(&path)?;
        let cfg: AppConfig = toml::from_str(&contents)?;
        Ok(cfg)
    }

    /// Return the effective log file path. Uses `agent.log_path` if set,
    /// otherwise defaults to `.sutcac/catus.log` in the current directory.
    pub fn effective_log_path(&self) -> PathBuf {
        self.agent
            .log_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(".sutcac/catus.log"))
    }

    /// Return the effective log level. Parses `agent.log_level`; unrecognized
    /// values fall back to `info`.
    pub fn effective_log_level(&self) -> LevelFilter {
        match self.agent.log_level.to_lowercase().as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        }
    }

    /// Save the configuration to the given TOML file path.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Return the path of the config file that would be used.
    pub fn find_config_file() -> Option<PathBuf> {
        let candidates = [Self::workspace_config(), Self::xdg_config()];
        for path in &candidates {
            if path.exists() {
                return Some(path.clone());
            }
        }
        None
    }

    fn workspace_config() -> PathBuf {
        let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        path.push(".sutcac");
        path.push("config.toml");
        path
    }

    fn xdg_config() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| {
            let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            home.push(".config");
            home
        });
        let mut path = base;
        path.push("catus");
        path.push("config.toml");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_example_config() {
        let input = r#"
[api]
base_url = "https://api.example.com/v1"
api_key = "sk-test"
model = "gpt-4o"

[agent]
max_tool_rounds = 5

[shell]
perm_mode = "deny:write"
audit_format = "json"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        assert_eq!(cfg.api.base_url, "https://api.example.com/v1");
        assert_eq!(cfg.api.api_key, "sk-test");
        assert_eq!(cfg.agent.max_tool_rounds, 5);
        assert!(cfg.agent.history_path.is_none());
        let shell = cfg.shell.unwrap();
        assert_eq!(shell.perm_mode.as_deref(), Some("deny:write"));
    }

    #[test]
    fn parse_mcp_servers_config() {
        let input = r#"
[api]
base_url = "https://api.example.com/v1"
api_key = "sk-test"
model = "gpt-4o"

[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.env]
HOME = "/home/user"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let mcp = cfg.mcp.expect("mcp config should be present");
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(mcp.servers[0].name, "filesystem");
        assert_eq!(mcp.servers[0].transport, McpTransport::Stdio);
        assert_eq!(mcp.servers[0].command, "npx");
        assert_eq!(
            mcp.servers[0].args,
            vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert_eq!(
            mcp.servers[0].env.get("HOME"),
            Some(&"/home/user".to_string())
        );
        assert!(mcp.servers[0].url.is_none());
    }

    #[test]
    fn parse_remote_streamable_http_server_config() {
        let input = r#"
[api]
base_url = "https://api.example.com/v1"
api_key = "sk-test"
model = "gpt-4o"

[[mcp.servers]]
name = "remote-calc"
transport = "streamable-http"
url = "https://mcp.example.com/calc"

[mcp.servers.headers]
Authorization = "Bearer sk-remote"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let mcp = cfg.mcp.expect("mcp config should be present");
        assert_eq!(mcp.servers.len(), 1);
        let server = &mcp.servers[0];
        assert_eq!(server.name, "remote-calc");
        assert_eq!(server.transport, McpTransport::StreamableHttp);
        assert_eq!(server.url.as_deref(), Some("https://mcp.example.com/calc"));
        assert!(server.command.is_empty());
        assert_eq!(
            server.headers.get("Authorization"),
            Some(&"Bearer sk-remote".to_string())
        );
    }
}

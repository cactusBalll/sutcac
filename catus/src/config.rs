//! Configuration for the catus Agent tool.
//!
//! All settings live in a single TOML file, searched in this order:
//!
//! 1. `./.sutcac/config.toml` (current working directory)
//! 2. `$XDG_CONFIG_HOME/catus/config.toml`
//! 3. `~/.config/catus/config.toml`

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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            agent: AgentConfig::default(),
            shell: None,
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
}

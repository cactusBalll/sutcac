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

use crate::llm::{Model, Provider};

/// Full application configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    /// API vendor endpoints (`[[providers]]`).
    pub providers: Vec<Provider>,
    /// Configured models (`[[models]]`); each entry references a provider by
    /// name. See [`AppConfig::resolve_models`].
    pub models: Vec<ModelEntry>,
    pub agent: AgentConfig,
    pub shell: Option<ShellConfig>,
    /// Optional MCP client configuration. When present, catus connects to the
    /// configured MCP servers and exposes their tools to the LLM alongside the
    /// built-in `shell` tool.
    pub mcp: Option<McpConfig>,
}

/// Configuration form of a model: like [`Model`], but `provider` names a
/// configured provider instead of embedding it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelEntry {
    /// Model id sent in the API request's `model` field.
    pub id: String,
    /// Name shown in the UI. Falls back to `id` when empty.
    pub name: String,
    /// Context window in tokens; 0 means unspecified. Accepts a plain integer
    /// or a human-readable string like `"512k"` / `"1M"` — see
    /// [`parse_context_window`].
    #[serde(default, deserialize_with = "deserialize_context_window")]
    pub context_window: usize,
    /// Name of the `[[providers]]` entry this model talks through.
    pub provider: String,
}

impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            context_window: 0,
            provider: String::new(),
        }
    }
}

/// Untagged form accepted for `context_window`: a plain integer or a string
/// with a size suffix.
#[derive(Deserialize)]
#[serde(untagged)]
enum ContextWindowInput {
    Number(usize),
    Text(String),
}

fn deserialize_context_window<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match ContextWindowInput::deserialize(deserializer)? {
        ContextWindowInput::Number(n) => Ok(n),
        ContextWindowInput::Text(s) => parse_context_window(&s).map_err(serde::de::Error::custom),
    }
}

/// Parse a context-window token count written as a plain integer (`131072`)
/// or a human-readable string (`"128k"`, `"1M"`, `"0.5m"`, `"1mb"`).
///
/// Suffixes are case-insensitive and 1024-based: `k` = 1024, `m` = 1024²,
/// `g` = 1024³; an optional trailing `b` is ignored. Fractional values are
/// truncated towards zero.
pub fn parse_context_window(value: &str) -> Result<usize, String> {
    let s = value.trim();
    if let Ok(n) = s.parse::<usize>() {
        return Ok(n);
    }
    let invalid = || {
        format!(
            "invalid context window '{value}'; expected an integer or a value like '128k', '1M'"
        )
    };
    let s = s.strip_suffix(['b', 'B']).unwrap_or(s);
    let (num_part, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1024u64),
        Some('m' | 'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => return Err(invalid()),
    };
    let num: f64 = num_part.trim().parse().map_err(|_| invalid())?;
    if num < 0.0 {
        return Err(invalid());
    }
    let total = num * mult as f64;
    if total >= usize::MAX as f64 {
        return Err(format!("context window '{value}' is out of range"));
    }
    Ok(total as usize)
}

/// Capability tier an agent definition can request via its `model`
/// frontmatter field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// High-capability model. Must be configured (`[agent.models]`).
    Performance,
    /// Cost-effective model. Optional; falls back to `performance`.
    Efficient,
}

impl ModelTier {
    /// Parse a tier label. Only `performance` and `efficient` are accepted.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "performance" => Ok(Self::Performance),
            "efficient" => Ok(Self::Efficient),
            other => Err(format!(
                "unknown model tier '{}'; expected 'performance' or 'efficient'",
                other
            )),
        }
    }

    /// The tier label as written in configuration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Efficient => "efficient",
        }
    }
}

/// Tier-to-model mapping configured under `[agent.models]`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct TierModelConfig {
    /// Model id (or display name) used for the `performance` tier. Required.
    pub performance: String,
    /// Model id (or display name) used for the `efficient` tier. Optional;
    /// falls back to `performance` when unset.
    pub efficient: Option<String>,
}

/// Resolved tier mapping: the concrete [`Model`] behind each tier. The
/// `efficient` model always falls back to `performance` when unconfigured.
#[derive(Debug, Clone)]
pub struct TierModels {
    pub performance: Model,
    pub efficient: Model,
}

impl Default for TierModels {
    fn default() -> Self {
        Self {
            performance: Model::default(),
            efficient: Model::default(),
        }
    }
}

impl TierModels {
    /// Return the model for the given tier.
    pub fn get(&self, tier: ModelTier) -> &Model {
        match tier {
            ModelTier::Performance => &self.performance,
            ModelTier::Efficient => &self.efficient,
        }
    }
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

/// Agent Memory configuration (`[agent.memory]`).
///
/// The memory store is an mdbook project: `book.toml` plus a `src/`
/// directory holding `SUMMARY.md` and topical chapter files. When enabled,
/// catus dispatches the `memory`-role subagent before each turn (recall) and
/// after each turn (summarize/write). Building with the `mdbook` binary is
/// optional: when it is unavailable the store is maintained as plain
/// Markdown.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Master switch. Disabled by default so existing configurations are
    /// unaffected.
    pub enabled: bool,
    /// Root directory of the mdbook memory store.
    pub path: PathBuf,
    /// Whether to dispatch a recall pass before each user turn.
    pub auto_recall: bool,
    /// Whether to dispatch a summarize/write pass after each turn completes.
    pub auto_write: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: PathBuf::from(".sutcac/memory"),
            auto_recall: true,
            auto_write: true,
        }
    }
}

/// Agent behavior configuration.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_tool_rounds: usize,
    /// Optional directory holding the SQLite session-history database
    /// (`sessions.db`). When set, catus persists each conversation
    /// incrementally and can resume it later with the `/resume` command.
    pub history_path: Option<PathBuf>,
    /// Optional log file path. When omitted, defaults to `.sutcac/catus.log`.
    pub log_path: Option<PathBuf>,
    /// Optional log level: trace, debug, info, warn, error. Defaults to info.
    pub log_level: String,
    /// Optional additional directories to scan for Agent Skills.
    pub skill_paths: Option<Vec<PathBuf>>,
    /// Whether to include discovered skills in the system prompt automatically.
    pub auto_include_skills: bool,
    /// Optional additional directories to scan for Agent definitions.
    pub agent_paths: Option<Vec<PathBuf>>,
    /// Tier-to-model mapping. `performance` must reference a configured
    /// `[[models]]` entry; `efficient` is optional and falls back to
    /// `performance`.
    pub models: TierModelConfig,
    /// Agent Memory subsystem settings (`[agent.memory]`).
    pub memory: MemoryConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            models: Vec::new(),
            agent: AgentConfig::default(),
            shell: None,
            mcp: None,
        }
    }
}

impl AppConfig {
    /// Resolve the configured models into [`Model`]s with their providers
    /// attached. Fails when a model references an unknown provider, or when
    /// no providers/models are configured at all.
    pub fn resolve_models(&self) -> Result<Vec<Model>, String> {
        if self.providers.is_empty() {
            return Err("no [[providers]] configured".to_string());
        }
        if self.models.is_empty() {
            return Err("no [[models]] configured".to_string());
        }
        self.models
            .iter()
            .map(|entry| {
                let provider = self
                    .providers
                    .iter()
                    .find(|p| p.name == entry.provider)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "model '{}' references unknown provider '{}'",
                            entry.id, entry.provider
                        )
                    })?;
                Ok(Model {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    context_window: entry.context_window,
                    provider,
                })
            })
            .collect()
    }

    /// Find a configured model entry by id or (non-empty) display name.
    fn find_model_entry(&self, reference: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.id == reference || (!m.name.trim().is_empty() && m.name == reference))
    }

    /// Validate the `[agent.models]` tier mapping without resolving providers.
    ///
    /// `performance` is required and must reference a configured `[[models]]`
    /// entry; `efficient` is optional but, when set, must also reference one.
    pub fn validate_tier_models(&self) -> Result<(), String> {
        let reference = self.agent.models.performance.trim();
        if reference.is_empty() {
            return Err(
                "[agent.models].performance is required; set it to a [[models]] id or name"
                    .to_string(),
            );
        }
        if self.find_model_entry(reference).is_none() {
            return Err(format!(
                "[agent.models].performance '{}' does not match any configured [[models]] id or name",
                reference
            ));
        }
        if let Some(efficient) = self
            .agent
            .models
            .efficient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if self.find_model_entry(efficient).is_none() {
                return Err(format!(
                    "[agent.models].efficient '{}' does not match any configured [[models]] id or name",
                    efficient
                ));
            }
        }
        Ok(())
    }

    /// Resolve the `[agent.models]` tier mapping into concrete [`Model`]s with
    /// their providers attached. An unset `efficient` tier falls back to the
    /// `performance` model.
    pub fn resolve_tier_models(&self, models: &[Model]) -> Result<TierModels, String> {
        let resolve = |reference: &str| -> Result<Model, String> {
            models
                .iter()
                .find(|m| {
                    m.id == reference
                        || (!m.display_name().trim().is_empty() && m.display_name() == reference)
                })
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "tier reference '{}' does not match any configured [[models]] id or name",
                        reference
                    )
                })
        };

        let performance_ref = self.agent.models.performance.trim();
        if performance_ref.is_empty() {
            return Err(
                "[agent.models].performance is required; set it to a [[models]] id or name"
                    .to_string(),
            );
        }
        let performance = resolve(performance_ref)?;
        let efficient = match self
            .agent
            .models
            .efficient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(efficient_ref) => resolve(efficient_ref)?,
            None => performance.clone(),
        };
        Ok(TierModels {
            performance,
            efficient,
        })
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_rounds: 30,
            history_path: None,
            log_path: None,
            log_level: "info".to_string(),
            skill_paths: None,
            auto_include_skills: true,
            agent_paths: None,
            models: TierModelConfig::default(),
            memory: MemoryConfig::default(),
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
[[providers]]
name = "openai"
base_url = "https://api.example.com/v1"
api_key = "sk-test"

[[models]]
id = "gpt-4o"
name = "GPT-4o"
provider = "openai"

[agent]
max_tool_rounds = 5

[shell]
perm_mode = "deny:write"
audit_format = "json"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "openai");
        assert_eq!(cfg.providers[0].api_key, "sk-test");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].id, "gpt-4o");
        assert_eq!(cfg.agent.max_tool_rounds, 5);
        assert!(cfg.agent.history_path.is_none());
        let shell = cfg.shell.unwrap();
        assert_eq!(shell.perm_mode.as_deref(), Some("deny:write"));
    }

    #[test]
    fn resolve_models_attaches_providers() {
        let input = r#"
[[providers]]
name = "opencode"
base_url = "https://opencode.example.com/v1"
api_key = "sk-oc"
session_header = "x-opencode-session"

[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "kimi-k2"
name = "Kimi K2"
context_window = 131072
provider = "opencode"

[[models]]
id = "gpt-4o-mini"
provider = "openai"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let models = cfg.resolve_models().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].display_name(), "Kimi K2");
        assert_eq!(models[0].context_window, 131072);
        assert_eq!(models[0].provider.name, "opencode");
        assert_eq!(
            models[0].provider.session_header.as_deref(),
            Some("x-opencode-session")
        );
        // Empty display name falls back to the id.
        assert_eq!(models[1].display_name(), "gpt-4o-mini");
        assert!(!models[1].provider.sends_session_id());
    }

    #[test]
    fn context_window_accepts_human_readable_sizes() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "m1"
context_window = "512k"
provider = "openai"

[[models]]
id = "m2"
context_window = "1M"
provider = "openai"

[[models]]
id = "m3"
context_window = "0.5m"
provider = "openai"

[[models]]
id = "m4"
context_window = 131072
provider = "openai"

[[models]]
id = "m5"
provider = "openai"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let windows: Vec<usize> = cfg.models.iter().map(|m| m.context_window).collect();
        assert_eq!(
            windows,
            vec![512 * 1024, 1024 * 1024, 512 * 1024, 131072, 0]
        );
    }

    #[test]
    fn parse_context_window_sizes() {
        assert_eq!(parse_context_window("131072").unwrap(), 131072);
        assert_eq!(parse_context_window("128k").unwrap(), 131072);
        assert_eq!(parse_context_window("128K").unwrap(), 131072);
        assert_eq!(parse_context_window("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_context_window("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_context_window(" 1m ").unwrap(), 1024 * 1024);
        assert_eq!(parse_context_window("0.5m").unwrap(), 512 * 1024);
        assert_eq!(parse_context_window("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_context_window("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_context_window("0").unwrap(), 0);
        assert!(parse_context_window("521x").is_err());
        assert!(parse_context_window("").is_err());
        assert!(parse_context_window("m").is_err());
        assert!(parse_context_window("-1k").is_err());
    }

    #[test]
    fn resolve_models_rejects_unknown_provider() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "gpt-4o"
provider = "nosuch"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let err = cfg.resolve_models().unwrap_err();
        assert!(err.contains("unknown provider"), "unexpected: {}", err);
    }

    #[test]
    fn resolve_models_requires_configuration() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.resolve_models().unwrap_err().contains("providers"));
    }

    #[test]
    fn parse_tier_model_config() {
        let input = r#"
[agent.models]
performance = "kimi-k2"
efficient = "gpt-4o-mini"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        assert_eq!(cfg.agent.models.performance, "kimi-k2");
        assert_eq!(cfg.agent.models.efficient.as_deref(), Some("gpt-4o-mini"));

        // The whole section is optional at parse time; validation catches the
        // missing `performance` reference.
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.agent.models.performance.is_empty());
        assert!(cfg.validate_tier_models().is_err());
    }

    #[test]
    fn resolve_tier_models_falls_back_to_performance() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "kimi-k2"
name = "Kimi K2"
provider = "openai"

[[models]]
id = "gpt-4o-mini"
provider = "openai"

[agent.models]
performance = "Kimi K2"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        cfg.validate_tier_models().unwrap();
        let models = cfg.resolve_models().unwrap();
        let tiers = cfg.resolve_tier_models(&models).unwrap();
        assert_eq!(tiers.performance.id, "kimi-k2");
        // No `efficient` configured: fall back to the performance model.
        assert_eq!(tiers.efficient.id, "kimi-k2");
    }

    #[test]
    fn resolve_tier_models_resolves_both_tiers() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "kimi-k2"
provider = "openai"

[[models]]
id = "gpt-4o-mini"
provider = "openai"

[agent.models]
performance = "kimi-k2"
efficient = "gpt-4o-mini"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let models = cfg.resolve_models().unwrap();
        let tiers = cfg.resolve_tier_models(&models).unwrap();
        assert_eq!(tiers.performance.id, "kimi-k2");
        assert_eq!(tiers.efficient.id, "gpt-4o-mini");
        assert_eq!(
            tiers.get(ModelTier::Efficient).display_name(),
            "gpt-4o-mini"
        );
    }

    #[test]
    fn tier_models_validation_rejects_missing_or_unknown_performance() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-oa"

[[models]]
id = "kimi-k2"
provider = "openai"
"#;
        // No [agent.models] at all.
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let err = cfg.validate_tier_models().unwrap_err();
        assert!(err.contains("performance"), "unexpected: {}", err);

        // Unknown performance reference.
        let input = format!("{}\n[agent.models]\nperformance = \"nosuch\"\n", input);
        let cfg: AppConfig = toml::from_str(&input).unwrap();
        let err = cfg.validate_tier_models().unwrap_err();
        assert!(err.contains("nosuch"), "unexpected: {}", err);
    }

    #[test]
    fn model_tier_parse_is_strict() {
        assert_eq!(
            ModelTier::parse("performance").unwrap(),
            ModelTier::Performance
        );
        assert_eq!(
            ModelTier::parse(" Efficient ").unwrap(),
            ModelTier::Efficient
        );
        assert!(ModelTier::parse("性能").is_err());
    }

    #[test]
    fn parse_memory_config() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.example.com/v1"
api_key = "sk-test"

[agent.memory]
enabled = true
path = "custom/memory-book"
auto_recall = false
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let memory = &cfg.agent.memory;
        assert!(memory.enabled);
        assert_eq!(memory.path, PathBuf::from("custom/memory-book"));
        assert!(!memory.auto_recall);
        // Unset sub-switch falls back to its default.
        assert!(memory.auto_write);

        // The whole section is optional; defaults keep memory disabled.
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(!cfg.agent.memory.enabled);
        assert_eq!(cfg.agent.memory.path, PathBuf::from(".sutcac/memory"));
        assert!(cfg.agent.memory.auto_recall);
        assert!(cfg.agent.memory.auto_write);
    }

    #[test]
    fn parse_mcp_servers_config() {
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.example.com/v1"
api_key = "sk-test"

[[models]]
id = "gpt-4o"
provider = "openai"

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
[[providers]]
name = "openai"
base_url = "https://api.example.com/v1"
api_key = "sk-test"

[[models]]
id = "gpt-4o"
provider = "openai"

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

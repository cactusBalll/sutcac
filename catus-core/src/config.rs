//! Configuration for the catus Agent tool.
//!
//! All settings live in TOML files under the XDG base directory
//! (`$XDG_CONFIG_HOME/catus/`, falling back to `~/.config/catus/`):
//!
//! 1. On startup the XDG directory is created and initialized from the
//!    embedded resources (`config.toml`, agent definitions, skills) when it
//!    does not exist yet.
//! 2. The XDG `config.toml` is loaded as the base configuration.
//! 3. `./.sutcac/config.toml` (the workspace configuration) is loaded on top
//!    when present: scalar and table fields set there override the XDG base;
//!    `[[providers]]` (by `name`), `[[models]]` (by `id`) and `[[mcp.servers]]`
//!    (by `name`) are merged entry by entry.
//!
//! Logs, the audit trail, the session history database and the Agent Memory
//! store live at fixed locations under the XDG directory and are not
//! configurable; see [`xdg_log_path`], [`xdg_history_dir`] and
//! [`xdg_memory_dir`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// Optional RAG configuration (`[rag]` table): the local hybrid index
    /// exposed through tools and optional per-turn injection. When absent or
    /// `enabled = false`, catus-rag stays dormant.
    pub rag: Option<catus_rag::RagConfig>,
    /// Runtime storage directories. Not configurable via TOML; production
    /// binaries set them from the XDG base directory via
    /// [`AppDirs::from_xdg`], tests keep the disabled default so persistence
    /// stays hermetic.
    #[serde(skip)]
    pub dirs: AppDirs,
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
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
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

/// Runtime storage directories for the Agent (history database and memory
/// store). Not part of the TOML configuration: production wires them to the
/// XDG base directory, tests use the disabled default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// Directory of the SQLite session-history database; `None` disables
    /// session persistence.
    pub history: Option<PathBuf>,
    /// Root directory of the Agent Memory mdbook store.
    pub memory: PathBuf,
}

impl Default for AppDirs {
    fn default() -> Self {
        Self {
            history: None,
            memory: PathBuf::from(".sutcac/memory"),
        }
    }
}

impl AppDirs {
    /// Production directories under the XDG base directory
    /// ([`xdg_history_dir`] and [`xdg_memory_dir`]).
    pub fn from_xdg() -> Self {
        Self {
            history: Some(xdg_history_dir()),
            memory: xdg_memory_dir(),
        }
    }
}

/// Base XDG directory for catus: `$XDG_CONFIG_HOME/catus`, falling back to
/// `~/.config/catus` (and `./` when even the home directory is unknown).
pub fn xdg_catus_dir() -> PathBuf {
    let mut base = dirs::config_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(".config"))
            .unwrap_or_else(|| PathBuf::from("."))
    });
    base.push("catus");
    base
}

/// The XDG configuration file (`<xdg>/catus/config.toml`).
pub fn xdg_config_path() -> PathBuf {
    xdg_catus_dir().join("config.toml")
}

/// The workspace configuration file (`./.sutcac/config.toml`), which overrides
/// the XDG base configuration when present.
pub fn workspace_config_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".sutcac")
        .join("config.toml")
}

/// A config file scope: the per-workspace overlay or the global XDG base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigScope {
    /// `./.sutcac/config.toml` (overrides the global config).
    Workspace,
    /// `<xdg>/catus/config.toml` (shared by all workspaces).
    Global,
}

/// The config file backing one [`ConfigScope`].
pub fn scope_config_path(scope: ConfigScope) -> PathBuf {
    match scope {
        ConfigScope::Workspace => workspace_config_path(),
        ConfigScope::Global => xdg_config_path(),
    }
}

/// Per-scope config state for editor frontends: the file path, whether it
/// exists yet, the values of the editable keys as written in that file (an
/// empty string means the key is not set in that scope), and the
/// `[[models]]` / `[[mcp.servers]]` entries as written in that file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigScopeSnapshot {
    pub scope: ConfigScope,
    pub path: String,
    pub exists: bool,
    pub fields: Vec<(String, String)>,
    /// `[[models]]` entries defined in this scope's file (not the merged
    /// effective list; frontends merge the scopes themselves).
    pub models: Vec<ModelEntry>,
    /// `[[mcp.servers]]` entries defined in this scope's file (not the
    /// merged effective list; frontends merge the scopes themselves).
    pub mcp_servers: Vec<McpServerConfig>,
}

/// The value type of one editable config field (drives editor widgets and
/// TOML value conversion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFieldKind {
    /// Integer scalar.
    Int,
    /// `true`/`false` scalar.
    Bool,
    /// Free-form string.
    String,
    /// Comma-separated string list.
    List,
}

/// Metadata about one editable config field: dotted key, value kind, and a
/// short description for editor UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFieldSpec {
    pub key: &'static str,
    pub kind: ConfigFieldKind,
    pub description: &'static str,
}

/// Parse a config file into a `toml::Value`; a missing file yields an empty
/// table so scope-aware edits can start from scratch.
pub fn read_config_toml(path: &Path) -> Result<toml::Value, Box<dyn std::error::Error>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map_err(|e| format!("invalid TOML in {}: {}", path.display(), e).into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(Default::default()))
        }
        Err(e) => Err(format!("cannot read {}: {}", path.display(), e).into()),
    }
}

/// Load the merged effective configuration as raw TOML: the XDG base with
/// the workspace overlay merged on top (same rules as [`AppConfig::load`]).
pub fn merged_config_toml() -> Result<toml::Value, Box<dyn std::error::Error>> {
    let mut merged = read_config_toml(&xdg_config_path())?;
    let ws_path = workspace_config_path();
    if ws_path.exists() {
        let ws = read_config_toml(&ws_path)?;
        merge_toml_values(&mut merged, &ws);
    }
    Ok(merged)
}

/// Follow a dotted key (`agent.max_tool_rounds`) through a TOML value.
pub fn value_get_dotted<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = value;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

/// Render one TOML scalar for display: strings without quotes, arrays as
/// comma-separated elements, everything else via its TOML representation.
pub fn toml_display_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Array(items) => items
            .iter()
            .map(toml_display_value)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Set a dotted key on a `toml_edit` document, creating intermediate tables,
/// while preserving all untouched content (comments, formatting, unknown
/// keys).
pub fn doc_set_dotted(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    item: toml_edit::Item,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parts: Vec<&str> = key.split('.').collect();
    let last = parts.pop().ok_or_else(|| "empty key".to_string())?;
    let mut table = doc.as_table_mut();
    for part in parts {
        let entry = table
            .entry(part)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| format!("key '{}' is not a table", part))?;
    }
    table.insert(last, item);
    Ok(())
}

/// Remove a dotted key from a `toml_edit` document. Returns whether the key
/// was present.
pub fn doc_remove_dotted(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut parts: Vec<&str> = key.split('.').collect();
    let last = parts.pop().ok_or_else(|| "empty key".to_string())?;
    let mut table = doc.as_table_mut();
    for part in parts {
        table = table
            .get_mut(part)
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| format!("key '{}' not found", key))?;
    }
    Ok(table.remove(last).is_some())
}

/// Extract the `[[models]]` entries from a parsed config TOML; `None` when
/// the key is absent. Errors when `models` is present but not a list of
/// well-formed model tables.
pub fn models_from_toml(value: &toml::Value) -> Result<Option<Vec<ModelEntry>>, String> {
    let Some(raw) = value.get("models") else {
        return Ok(None);
    };
    let models = raw
        .clone()
        .try_into()
        .map_err(|e| format!("invalid [[models]] entries: {}", e))?;
    Ok(Some(models))
}

/// Insert or update one `[[models]]` entry in a `toml_edit` document,
/// matching existing entries by `id` and appending a new table otherwise,
/// while preserving all untouched content (comments, formatting, unknown
/// keys). An empty `name` and a zero `context_window` are not written.
pub fn doc_upsert_model(
    doc: &mut toml_edit::DocumentMut,
    entry: &ModelEntry,
) -> Result<(), Box<dyn std::error::Error>> {
    let item = doc
        .as_table_mut()
        .entry("models")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ));
    let array = item
        .as_array_of_tables_mut()
        .ok_or_else(|| "'models' in the config file is not an array of tables".to_string())?;
    let slot = array
        .iter_mut()
        .find(|t| t.get("id").and_then(|i| i.as_str()) == Some(entry.id.as_str()));
    let table = match slot {
        Some(table) => table,
        None => {
            array.push(toml_edit::Table::new());
            array
                .iter_mut()
                .last()
                .expect("pushed table must be present")
        }
    };
    table.insert("id", toml_edit::value(entry.id.clone()));
    if entry.name.trim().is_empty() {
        table.remove("name");
    } else {
        table.insert("name", toml_edit::value(entry.name.clone()));
    }
    if entry.context_window == 0 {
        table.remove("context_window");
    } else {
        let window = i64::try_from(entry.context_window)
            .map_err(|_| "context window value is out of range".to_string())?;
        table.insert("context_window", toml_edit::value(window));
    }
    table.insert("provider", toml_edit::value(entry.provider.clone()));
    Ok(())
}

/// Remove one `[[models]]` entry (matched by `id`) from a `toml_edit`
/// document. Returns whether an entry was removed; the `models` key itself
/// is dropped when the array becomes empty.
pub fn doc_remove_model(
    doc: &mut toml_edit::DocumentMut,
    id: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(item) = doc.as_table_mut().get_mut("models") else {
        return Ok(false);
    };
    let array = item
        .as_array_of_tables_mut()
        .ok_or_else(|| "'models' in the config file is not an array of tables".to_string())?;
    let Some(index) = array
        .iter()
        .position(|t| t.get("id").and_then(|i| i.as_str()) == Some(id))
    else {
        return Ok(false);
    };
    array.remove(index);
    if array.is_empty() {
        doc.as_table_mut().remove("models");
    }
    Ok(true)
}

/// Parse the `[[mcp.servers]]` entries out of a raw config value; a missing
/// `mcp.servers` key yields `None`.
pub fn mcp_servers_from_toml(value: &toml::Value) -> Result<Option<Vec<McpServerConfig>>, String> {
    let Some(raw) = value.get("mcp").and_then(|m| m.get("servers")) else {
        return Ok(None);
    };
    let servers = raw
        .clone()
        .try_into()
        .map_err(|e| format!("invalid [[mcp.servers]] entries: {}", e))?;
    Ok(Some(servers))
}

/// Write one `McpServerConfig` table into a `toml_edit` document (`env` and
/// `headers` become inline tables; fields that do not apply to the entry's
/// transport are removed). Existing entries are matched by `name`.
pub fn doc_upsert_mcp_server(
    doc: &mut toml_edit::DocumentMut,
    entry: &McpServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mcp_item = doc
        .as_table_mut()
        .entry("mcp")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let mcp = mcp_item
        .as_table_mut()
        .ok_or_else(|| "'mcp' in the config file is not a table".to_string())?;
    let item = mcp
        .entry("servers")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ));
    let array = item
        .as_array_of_tables_mut()
        .ok_or_else(|| "'mcp.servers' in the config file is not an array of tables".to_string())?;
    let slot = array
        .iter_mut()
        .find(|t| t.get("name").and_then(|i| i.as_str()) == Some(entry.name.as_str()));
    let table = match slot {
        Some(table) => table,
        None => {
            array.push(toml_edit::Table::new());
            array
                .iter_mut()
                .last()
                .expect("pushed table must be present")
        }
    };
    table.insert("name", toml_edit::value(entry.name.clone()));
    let transport = match entry.transport {
        McpTransport::Stdio => "stdio",
        McpTransport::StreamableHttp => "streamable-http",
    };
    table.insert("transport", toml_edit::value(transport));
    match entry.transport {
        McpTransport::Stdio => {
            table.insert("command", toml_edit::value(entry.command.clone()));
            if entry.args.is_empty() {
                table.remove("args");
            } else {
                table.insert(
                    "args",
                    toml_edit::value(toml_edit::Array::from_iter(entry.args.iter().cloned())),
                );
            }
            if entry.env.is_empty() {
                table.remove("env");
            } else {
                table.insert("env", string_map_item(&entry.env));
            }
            table.remove("url");
            table.remove("headers");
        }
        McpTransport::StreamableHttp => {
            table.insert(
                "url",
                toml_edit::value(entry.url.clone().unwrap_or_default()),
            );
            if entry.headers.is_empty() {
                table.remove("headers");
            } else {
                table.insert("headers", string_map_item(&entry.headers));
            }
            table.remove("command");
            table.remove("args");
            table.remove("env");
        }
    }
    Ok(())
}

/// Render a `HashMap<String, String>` as a `toml_edit` inline table.
fn string_map_item(map: &std::collections::HashMap<String, String>) -> toml_edit::Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    for (key, value) in map {
        table.insert(key, toml_edit::value(value.clone()));
    }
    toml_edit::Item::Value(toml_edit::Value::InlineTable(table.into_inline_table()))
}

/// Remove one `[[mcp.servers]]` entry (matched by `name`) from a
/// `toml_edit` document. Returns whether an entry was removed; the
/// `servers` key (and an empty `mcp` table) is dropped when it becomes
/// empty.
pub fn doc_remove_mcp_server(
    doc: &mut toml_edit::DocumentMut,
    name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(item) = doc
        .as_table_mut()
        .get_mut("mcp")
        .and_then(|m| m.as_table_mut())
        .and_then(|m| m.get_mut("servers"))
    else {
        return Ok(false);
    };
    let array = item
        .as_array_of_tables_mut()
        .ok_or_else(|| "'mcp.servers' in the config file is not an array of tables".to_string())?;
    let Some(index) = array
        .iter()
        .position(|t| t.get("name").and_then(|i| i.as_str()) == Some(name))
    else {
        return Ok(false);
    };
    array.remove(index);
    if array.is_empty() {
        if let Some(mcp) = doc
            .as_table_mut()
            .get_mut("mcp")
            .and_then(|m| m.as_table_mut())
        {
            mcp.remove("servers");
            if mcp.is_empty() {
                doc.as_table_mut().remove("mcp");
            }
        }
    }
    Ok(true)
}

/// The application log file (`<xdg>/catus/catus.log`). Not configurable.
pub fn xdg_log_path() -> PathBuf {
    xdg_catus_dir().join("catus.log")
}

/// The shell audit log (`<xdg>/catus/audit.log`). Not configurable.
pub fn xdg_audit_log_path() -> PathBuf {
    xdg_catus_dir().join("audit.log")
}

/// Directory of the SQLite session history
/// (`<xdg>/catus/history/sessions.db`). Not configurable; sessions are
/// distinguished by the working directory they were created in.
pub fn xdg_history_dir() -> PathBuf {
    xdg_catus_dir().join("history")
}

/// Root of the Agent Memory mdbook store (`<xdg>/catus/memory`). Not
/// configurable; one global store is shared by all working directories.
pub fn xdg_memory_dir() -> PathBuf {
    xdg_catus_dir().join("memory")
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
    /// Whether to dispatch a recall pass before each user turn.
    pub auto_recall: bool,
    /// Whether to dispatch a summarize/write pass after each turn completes.
    pub auto_write: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
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
    /// Log level: trace, debug, info, warn, error. Defaults to info. The log
    /// file itself lives at [`xdg_log_path`] and is not configurable.
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
    /// Agent Memory subsystem settings (`[agent.memory]`). The store root is
    /// fixed at [`xdg_memory_dir`].
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
            rag: None,
            dirs: AppDirs::default(),
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
        let resolve = |tier: &str, reference: &str| -> Result<Model, String> {
            models
                .iter()
                .find(|m| {
                    m.id == reference
                        || (!m.display_name().trim().is_empty() && m.display_name() == reference)
                })
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "[agent.models].{} '{}' does not match any configured [[models]] id or name",
                        tier, reference
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
        let performance = resolve("performance", performance_ref)?;
        let efficient = match self
            .agent
            .models
            .efficient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(efficient_ref) => resolve("efficient", efficient_ref)?,
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
    /// Load the effective configuration.
    ///
    /// When the XDG directory does not exist yet it is created and
    /// initialized from the embedded resources. The XDG `config.toml` is the
    /// base; a workspace `./.sutcac/config.toml` is merged on top with
    /// workspace values taking priority (see [`merge_toml_values`]).
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = xdg_catus_dir();
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            if let Err(e) = crate::resources::install_xdg_config() {
                tracing::warn!("failed to initialize XDG config directory: {}", e);
            }
        }

        let xdg_path = xdg_config_path();
        let xdg_contents = std::fs::read_to_string(&xdg_path)
            .map_err(|e| format!("cannot read {}: {}", xdg_path.display(), e))?;
        let mut merged: toml::Value = toml::from_str(&xdg_contents)
            .map_err(|e| format!("invalid TOML in {}: {}", xdg_path.display(), e))?;

        let ws_path = workspace_config_path();
        if ws_path.exists() {
            let ws_contents = std::fs::read_to_string(&ws_path)
                .map_err(|e| format!("cannot read {}: {}", ws_path.display(), e))?;
            let ws: toml::Value = toml::from_str(&ws_contents)
                .map_err(|e| format!("invalid TOML in {}: {}", ws_path.display(), e))?;
            merge_toml_values(&mut merged, &ws);
        }

        let cfg: AppConfig = merged
            .try_into()
            .map_err(|e| format!("invalid configuration: {}", e))?;
        Ok(cfg)
    }

    /// Return the effective log file path: the fixed XDG location
    /// (`<xdg>/catus/catus.log`).
    pub fn effective_log_path(&self) -> PathBuf {
        xdg_log_path()
    }

    /// Return the effective log level. Parses `agent.log_level`; unrecognized
    /// values fall back to `info`.
    pub fn effective_log_level(&self) -> tracing::Level {
        match self.agent.log_level.to_lowercase().as_str() {
            "trace" => tracing::Level::TRACE,
            "debug" => tracing::Level::DEBUG,
            "info" => tracing::Level::INFO,
            "warn" => tracing::Level::WARN,
            "error" => tracing::Level::ERROR,
            _ => tracing::Level::INFO,
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

    /// Return the path configuration changes should be saved to: the
    /// workspace config when it exists, otherwise the XDG config.
    pub fn config_save_path() -> Option<PathBuf> {
        let ws = workspace_config_path();
        if ws.exists() {
            return Some(ws);
        }
        let xdg = xdg_config_path();
        xdg.exists().then_some(xdg)
    }
}

/// Recursively merge `over` into `base` (workspace over XDG).
///
/// Tables are merged key by key; any other conflicting value in `over`
/// replaces the base value. `[[providers]]`, `[[models]]` and `[[mcp.servers]]`
/// arrays are merged by key (`name`/`id`): entries present on both sides are
/// merged field-wise, entries only in `over` are appended.
pub fn merge_toml_values(base: &mut toml::Value, over: &toml::Value) {
    let (Some(base_table), Some(over_table)) = (base.as_table_mut(), over.as_table()) else {
        *base = over.clone();
        return;
    };
    for (key, over_value) in over_table {
        let merge_arrays_by_key = matches!(key.as_str(), "providers" | "models" | "servers")
            && base_table.get(key).is_some_and(toml::Value::is_array)
            && over_value.is_array();
        if merge_arrays_by_key {
            let key_field = if key == "models" { "id" } else { "name" };
            merge_array_by_key(base_table.get_mut(key).unwrap(), over_value, key_field);
        } else if base_table.get(key).is_some_and(toml::Value::is_table) && over_value.is_table() {
            merge_toml_values(base_table.get_mut(key).unwrap(), over_value);
        } else {
            base_table.insert(key.clone(), over_value.clone());
        }
    }
}

/// Merge arrays of tables by key: `over` entries are merged field-wise into
/// matching base entries or appended. Models are matched by `id`, providers
/// and MCP servers by `name`.
fn merge_array_by_key(base: &mut toml::Value, over: &toml::Value, key_field: &str) {
    let Some(over_items) = over.as_array() else {
        *base = over.clone();
        return;
    };
    if !base.is_array() {
        *base = over.clone();
        return;
    }
    let entry_key = |item: &toml::Value| item.as_table().and_then(|t| t.get(key_field)).cloned();
    let base_items = base.as_array_mut().unwrap();
    for item in over_items {
        match entry_key(item) {
            Some(key) => {
                let slot = base_items
                    .iter_mut()
                    .find(|b| entry_key(b).as_ref() == Some(&key));
                match slot {
                    Some(slot) => merge_toml_values(slot, item),
                    None => base_items.push(item.clone()),
                }
            }
            None => base_items.push(item.clone()),
        }
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
        let shell = cfg.shell.unwrap();
        assert_eq!(shell.perm_mode.as_deref(), Some("deny:write"));
    }

    #[test]
    fn parse_config_ignores_removed_path_keys() {
        // Keys that are no longer configurable must be silently ignored so
        // old configuration files keep loading.
        let input = r#"
[[providers]]
name = "openai"
base_url = "https://api.example.com/v1"
api_key = "sk-test"

[[models]]
id = "gpt-4o"
provider = "openai"

[agent]
history_path = ".sutcac/history"
log_path = ".sutcac/catus.log"

[agent.memory]
enabled = true
path = ".sutcac/memory"

[shell]
audit_log = ".sutcac/audit.log"
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        assert!(cfg.agent.memory.enabled);
        assert_eq!(cfg.effective_log_path(), xdg_log_path());
    }

    #[test]
    fn merge_workspace_overrides_xdg_scalars() {
        let mut base: toml::Value =
            toml::from_str("[agent]\nmax_tool_rounds = 30\n[shell]\nperm_mode = \"allow_all\"\n")
                .unwrap();
        let over: toml::Value =
            toml::from_str("[agent]\nmax_tool_rounds = 5\n[shell]\nperm_mode = \"deny:write\"\n")
                .unwrap();
        merge_toml_values(&mut base, &over);
        let cfg: AppConfig = base.try_into().unwrap();
        assert_eq!(cfg.agent.max_tool_rounds, 5);
        assert_eq!(cfg.shell.unwrap().perm_mode.as_deref(), Some("deny:write"));
    }

    #[test]
    fn merge_keeps_xdg_fields_not_set_in_workspace() {
        let mut base: toml::Value = toml::from_str(
            "[agent]\nmax_tool_rounds = 30\nlog_level = \"debug\"\n[shell]\nperm_mode = \"deny:write\"\naudit_format = \"json\"\n",
        )
        .unwrap();
        let over: toml::Value =
            toml::from_str("[agent]\nmax_tool_rounds = 5\n[shell]\nperm_mode = \"allow_all\"\n")
                .unwrap();
        merge_toml_values(&mut base, &over);
        let cfg: AppConfig = base.try_into().unwrap();
        assert_eq!(cfg.agent.max_tool_rounds, 5);
        // log_level and audit_format come from the XDG base.
        assert_eq!(cfg.agent.log_level, "debug");
        let shell = cfg.shell.unwrap();
        assert_eq!(shell.perm_mode.as_deref(), Some("allow_all"));
        assert_eq!(shell.audit_format.as_deref(), Some("json"));
    }

    #[test]
    fn merge_providers_and_models_by_key() {
        let mut base: toml::Value = toml::from_str(
            r#"
[[providers]]
name = "openai"
base_url = "https://xdg.example.com/v1"
api_key = "sk-xdg"

[[providers]]
name = "other"
base_url = "https://other.example.com/v1"
api_key = "sk-other"

[[models]]
id = "m1"
name = "Model One"
context_window = 4096
provider = "openai"
"#,
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            r#"
[[providers]]
name = "openai"
api_key = "sk-workspace"

[[providers]]
name = "extra"
base_url = "https://extra.example.com/v1"
api_key = "sk-extra"

[[models]]
id = "m1"
context_window = 8192

[[models]]
id = "m2"
provider = "openai"
"#,
        )
        .unwrap();
        merge_toml_values(&mut base, &over);
        let cfg: AppConfig = base.try_into().unwrap();

        // Same-name provider: workspace fields override, XDG fields survive.
        let openai = cfg.providers.iter().find(|p| p.name == "openai").unwrap();
        assert_eq!(openai.api_key, "sk-workspace");
        assert_eq!(openai.base_url, "https://xdg.example.com/v1");
        // Unrelated XDG provider survives; workspace-only provider appended.
        assert_eq!(cfg.providers.len(), 3);
        assert!(cfg.providers.iter().any(|p| p.name == "other"));
        assert!(cfg.providers.iter().any(|p| p.name == "extra"));

        // Same-id model: field-wise merge; XDG-only model survives.
        assert_eq!(cfg.models.len(), 2);
        let m1 = cfg.models.iter().find(|m| m.id == "m1").unwrap();
        assert_eq!(m1.context_window, 8192);
        assert_eq!(m1.name, "Model One");
        assert_eq!(m1.provider, "openai");
        assert!(cfg.models.iter().any(|m| m.id == "m2"));
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
auto_recall = false
"#;
        let cfg: AppConfig = toml::from_str(input).unwrap();
        let memory = &cfg.agent.memory;
        assert!(memory.enabled);
        assert!(!memory.auto_recall);
        // Unset sub-switch falls back to its default.
        assert!(memory.auto_write);

        // The whole section is optional; defaults keep memory disabled.
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(!cfg.agent.memory.enabled);
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

    #[test]
    fn models_from_toml_round_trips_entries() {
        let value: toml::Value = toml::from_str(
            r#"
[[models]]
id = "m1"
name = "Model One"
context_window = "128k"
provider = "openai"

[[models]]
id = "m2"
provider = "openai"
"#,
        )
        .unwrap();
        let models = models_from_toml(&value).unwrap().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_window, 128 * 1024);
        assert_eq!(models[1].context_window, 0);

        let empty: toml::Value = toml::from_str("[shell]\nperm_mode = \"allow_all\"\n").unwrap();
        assert!(models_from_toml(&empty).unwrap().is_none());

        let bad: toml::Value = toml::from_str("models = 3\n").unwrap();
        assert!(models_from_toml(&bad).is_err());
    }

    #[test]
    fn doc_upsert_model_appends_updates_and_preserves_content() {
        let mut doc: toml_edit::DocumentMut = r#"
# api vendor
[[providers]]
name = "openai"

[[models]]
id = "m1"
name = "Model One"
context_window = 4096
provider = "openai"
"#
        .parse()
        .unwrap();

        // New entry appended.
        doc_upsert_model(
            &mut doc,
            &ModelEntry {
                id: "m2".to_string(),
                name: String::new(),
                context_window: 0,
                provider: "openai".to_string(),
            },
        )
        .unwrap();
        // Existing entry updated.
        doc_upsert_model(
            &mut doc,
            &ModelEntry {
                id: "m1".to_string(),
                name: String::new(),
                context_window: 8192,
                provider: "other".to_string(),
            },
        )
        .unwrap();

        let saved = doc.to_string();
        let models = models_from_toml(&toml::from_str::<toml::Value>(&saved).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(models.len(), 2);
        let m1 = models.iter().find(|m| m.id == "m1").unwrap();
        assert_eq!(m1.context_window, 8192);
        assert_eq!(m1.provider, "other");
        assert!(m1.name.is_empty());
        let m2 = models.iter().find(|m| m.id == "m2").unwrap();
        assert_eq!(m2.provider, "openai");

        // Untouched content survives.
        let saved = doc.to_string();
        assert!(saved.contains("# api vendor"));
        assert!(saved.contains("name = \"openai\""));
    }

    #[test]
    fn doc_remove_model_drops_entry_and_empty_array() {
        let mut doc: toml_edit::DocumentMut = "[[models]]\nid = \"m1\"\nprovider = \"p\"\n\n[[models]]\nid = \"m2\"\nprovider = \"p\"\n"
            .parse()
            .unwrap();
        assert!(doc_remove_model(&mut doc, "m1").unwrap());
        assert!(!doc_remove_model(&mut doc, "m1").unwrap());
        assert!(doc_remove_model(&mut doc, "m2").unwrap());
        // The emptied array is dropped from the document.
        assert!(doc.as_table().get("models").is_none());
        assert!(doc_remove_model(&mut doc, "nosuch").unwrap() == false);
    }

    #[test]
    fn doc_upsert_mcp_server_appends_updates_and_preserves_content() {
        let mut doc: toml_edit::DocumentMut = r#"
# calculators
[mcp]

[[mcp.servers]]
name = "calc"
command = "calc-server"
args = ["--stdio"]

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "https://mcp.example.com/mcp"

[mcp.servers.headers]
Authorization = "Bearer old"
"#
        .parse()
        .unwrap();

        // New stdio entry appended.
        doc_upsert_mcp_server(
            &mut doc,
            &McpServerConfig {
                name: "calc2".to_string(),
                transport: McpTransport::Stdio,
                command: "calc2-server".to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                url: None,
                headers: std::collections::HashMap::new(),
            },
        )
        .unwrap();
        // Existing http entry updated (transport switch drops stale fields).
        doc_upsert_mcp_server(
            &mut doc,
            &McpServerConfig {
                name: "remote".to_string(),
                transport: McpTransport::StreamableHttp,
                command: String::new(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                url: Some("http://127.0.0.1:8000/mcp".to_string()),
                headers: [("X-Session".to_string(), "abc".to_string())]
                    .into_iter()
                    .collect(),
            },
        )
        .unwrap();

        let saved = doc.to_string();
        let servers = mcp_servers_from_toml(&toml::from_str::<toml::Value>(&saved).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(servers.len(), 3);
        let remote = servers.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport, McpTransport::StreamableHttp);
        assert_eq!(remote.url.as_deref(), Some("http://127.0.0.1:8000/mcp"));
        assert!(remote.command.is_empty());
        assert_eq!(remote.headers.get("X-Session"), Some(&"abc".to_string()));
        assert!(!remote.headers.contains_key("Authorization"));
        let calc2 = servers.iter().find(|s| s.name == "calc2").unwrap();
        assert_eq!(calc2.transport, McpTransport::Stdio);
        assert!(calc2.command == "calc2-server");

        // Untouched content survives.
        assert!(saved.contains("# calculators"));
        assert!(saved.contains("name = \"calc\""));
    }

    #[test]
    fn doc_upsert_mcp_server_creates_nested_table_in_fresh_document() {
        let mut doc: toml_edit::DocumentMut =
            "[shell]\nperm_mode = \"allow_all\"\n".parse().unwrap();
        doc_upsert_mcp_server(
            &mut doc,
            &McpServerConfig {
                name: "calc".to_string(),
                transport: McpTransport::Stdio,
                command: "mcp-calc-server".to_string(),
                args: vec!["--http".to_string()],
                env: std::collections::HashMap::new(),
                url: None,
                headers: std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let servers =
            mcp_servers_from_toml(&toml::from_str::<toml::Value>(&doc.to_string()).unwrap())
                .unwrap()
                .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].args, vec!["--http"]);
        // Untouched content survives.
        assert!(doc.to_string().contains("perm_mode"));
    }

    #[test]
    fn doc_remove_mcp_server_drops_entry_and_empty_tables() {
        let mut doc: toml_edit::DocumentMut = r#"
[[mcp.servers]]
name = "calc"
command = "calc-server"

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "https://mcp.example.com/mcp"
"#
        .parse()
        .unwrap();
        assert!(doc_remove_mcp_server(&mut doc, "calc").unwrap());
        assert!(!doc_remove_mcp_server(&mut doc, "calc").unwrap());
        assert!(doc_remove_mcp_server(&mut doc, "remote").unwrap());
        // The emptied array and the bare `mcp` table are dropped.
        assert!(doc.as_table().get("mcp").is_none());
        assert!(!doc_remove_mcp_server(&mut doc, "nosuch").unwrap());
    }
}

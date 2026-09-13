//! Frontend-independent application state: the Agent session, its turn state
//! machine, and the event stream consumed by frontends.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::ShellState;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::agents::{AgentDefinition, AgentRegistry, AgentRole};
use crate::config::{AppConfig, TierModels};
use crate::llm::{LlmClient, LlmError, Model, StreamEvent, Usage};
use crate::mcp::McpManager;
use crate::message::{Message, Role};
use crate::skills::SkillRegistry;
use crate::subagent::SubagentManager;
use crate::tool::McpServerTool;
use crate::tool::{
    AskAnswer, AskPermissionTool, AskQuestion, AskUserTool, EditTool, GRANT_SESSION, ReadTool,
    ShellTool, SkillTool, TaskSyncTool, TaskTool, TodoList, TodoTool, Tool, ToolCall, ToolContext,
    ToolResult, Toolbox, parse_ask_permission_request,
};

pub mod command;
pub mod commands;

pub use command::{CommandError, CommandOutcome, CommandRegistry, SlashCommand, UiRequest};

/// An event emitted by the runtime for the active frontend.
///
/// Frontends consume these to update their own view state; the session state
/// itself is always readable through `App`. The stream variants carry the
/// raw deltas so streaming frontends (a TUI auto-scroll, a Web SSE bridge)
/// can react incrementally.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// An assistant text chunk was appended to the conversation.
    StreamText(String),
    /// An assistant reasoning chunk was appended to the conversation.
    StreamReasoning(String),
    /// The model emitted a tool call (stored, not yet executed).
    ToolCallAdded(ToolCall),
    /// Token usage was reported for a completed LLM request.
    UsageUpdated(Usage),
    /// The conversation content changed; view state such as auto-scroll may
    /// want to follow.
    MessagesChanged,
    /// A tool is waiting for the user to answer questions. The turn stays
    /// paused until the frontend answers via `complete_interaction` /
    /// `cancel_interaction`.
    InteractionRequested(Vec<AskQuestion>),
    /// A subagent reported an event.
    SubagentEvent(crate::subagent::SubagentEvent),
    /// The current turn finished (or failed); no further stream activity is
    /// expected until the next user message. Background work such as the
    /// memory write pass may still be running.
    TurnComplete,
}

impl serde::Serialize for RuntimeEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            RuntimeEvent::StreamText(text) => {
                map.serialize_entry("type", "stream_text")?;
                map.serialize_entry("text", text)?;
            }
            RuntimeEvent::StreamReasoning(text) => {
                map.serialize_entry("type", "stream_reasoning")?;
                map.serialize_entry("text", text)?;
            }
            RuntimeEvent::ToolCallAdded(call) => {
                map.serialize_entry("type", "tool_call_added")?;
                map.serialize_entry("call", call)?;
            }
            RuntimeEvent::UsageUpdated(usage) => {
                map.serialize_entry("type", "usage_updated")?;
                map.serialize_entry("usage", usage)?;
            }
            RuntimeEvent::MessagesChanged => {
                map.serialize_entry("type", "messages_changed")?;
            }
            RuntimeEvent::InteractionRequested(questions) => {
                map.serialize_entry("type", "interaction_requested")?;
                map.serialize_entry("questions", questions)?;
            }
            RuntimeEvent::SubagentEvent(event) => {
                map.serialize_entry("type", "subagent_event")?;
                map.serialize_entry("event", event)?;
            }
            RuntimeEvent::TurnComplete => {
                map.serialize_entry("type", "turn_complete")?;
            }
        }
        map.end()
    }
}

/// Current high-level state of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    Idle,
    Streaming,
    RunningTool,
    Error,
}

/// Outcome of submitting one input line to the app.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputLineOutcome {
    /// The input was a slash command and has been handled.
    Handled(CommandOutcome),
    /// The input was submitted as a user message; the caller should resume
    /// the LLM stream (the runtime gates it on pending memory passes).
    Submitted,
    /// The input was empty; nothing happened.
    Empty,
}

/// What the turn state machine wants after an LLM stream completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    /// Tool results are ready; a follow-up LLM request is needed.
    ContinueStream,
    /// The turn is paused waiting for user interaction.
    PausedInteraction,
    /// The turn has finished (or failed).
    Complete,
}

/// Summary of one subagent, as exposed to remote frontends.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentSummary {
    pub id: String,
    pub name: String,
    pub task: String,
    pub state: crate::subagent::SubagentState,
    pub mode: crate::subagent::SubagentContextMode,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// One configured model in the snapshot. The provider itself is skipped by
/// serde (it carries the api key), so only its name is exposed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelSnapshot {
    pub id: String,
    pub name: String,
    pub context_window: usize,
    pub provider_name: String,
}

impl From<&Model> for ModelSnapshot {
    fn from(model: &Model) -> Self {
        ModelSnapshot {
            id: model.id.clone(),
            name: model.name.clone(),
            context_window: model.context_window,
            provider_name: model.provider.name.clone(),
        }
    }
}

/// Serializable snapshot of the session state for remote frontends.
///
/// A frontend that cannot read `App` fields directly (e.g. a Tauri webview)
/// pulls this once at startup and after every `MessagesChanged` it missed,
/// instead of keeping a full parallel mirror of the core state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppSnapshot {
    pub status: AppStatus,
    pub status_message: String,
    pub session_id: String,
    pub session_cwd: String,
    pub current_model: Model,
    pub models: Vec<ModelSnapshot>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub request_count: usize,
    pub active_skills: Vec<String>,
    pub todos: TodoList,
    pub subagents: Vec<SubagentSummary>,
    pub should_quit: bool,
    pub memory_available: bool,
    pub memory_session_enabled: bool,
    /// Discovered skill catalog with per-session disabled flags (the web
    /// skill manager page).
    pub skill_catalog: Vec<SkillCatalogEntry>,
    /// Configured MCP servers with connection and enable state (the web MCP
    /// manager page).
    pub mcp_servers: Vec<McpServerInfo>,
    /// Discovered agent definitions with role and disable state (the web
    /// agent manager page).
    pub agent_catalog: Vec<AgentCatalogEntry>,
    /// Editable config fields as (key, current_value) pairs (the config page).
    pub config_fields: Vec<(String, String)>,
    /// Metadata for the editable config fields (kind + description, drives
    /// the web config editor's widgets).
    pub config_field_specs: Vec<crate::config::ConfigFieldSpec>,
    /// Per-scope config files with their editable field values (the web
    /// config editor; workspace values override global ones).
    pub config_scopes: Vec<crate::config::ConfigScopeSnapshot>,
}

/// One skill in the snapshot's skill catalog.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
    /// Session-scoped disable flag: disabled skills are hidden from the LLM
    /// (prompt catalog + `use_skill`); manual activation still works.
    pub disabled: bool,
}

/// One configured MCP server in the snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerInfo {
    pub name: String,
    /// Session-scoped enable flag; disabled servers keep their connection
    /// but their gateway tool is hidden from the LLM.
    pub enabled: bool,
    /// Whether the manager currently holds a live connection.
    pub connected: bool,
    /// Tool names from the cached catalog.
    pub tools: Vec<String>,
}

/// One agent definition in the snapshot's agent catalog.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentCatalogEntry {
    pub name: String,
    pub description: String,
    /// `main`/`memory` for special-role agents; `None` for plain subagents.
    pub role: Option<String>,
    /// Session-scoped dispatch disable flag.
    pub disabled: bool,
    /// Whether the file can be edited (special-role agents are preview-only).
    pub editable: bool,
}

/// Full detail of one agent definition for the editor page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentDetail {
    pub name: String,
    pub description: String,
    pub role: Option<String>,
    pub disabled: bool,
    /// Special-role agents are preview-only.
    pub editable: bool,
    pub source_path: String,
    /// Raw `.md` file content (frontmatter + body).
    pub content: String,
}

/// Full raw contents of one skill's `SKILL.md` for the preview page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillPreview {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Mutable application state shared between the TUI and async workers.
pub struct App {
    pub config: AppConfig,
    pub client: LlmClient,
    /// All models resolved from the config (providers attached), in config
    /// order. The list shown by `/model`.
    pub models: Vec<Model>,
    /// The model currently in use; drives the API request's `model` field.
    pub current_model: Model,
    /// One stable ID per conversation, sent to providers that request a
    /// session header.
    pub session_id: String,
    pub shell_state: ShellState,
    pub messages: Vec<Message>,
    pub status: AppStatus,
    pub status_message: String,
    pub max_tool_rounds: usize,
    pending_tool_calls: Vec<ToolCall>,
    tool_rounds_this_turn: usize,
    /// Monotonic turn counter, incremented at every user message submission.
    /// Logging-only correlation field so flat file log lines from the LLM
    /// stream task, tools, and subagents can be grouped per user turn.
    turn_seq: u64,
    /// A tool call that is paused waiting for the user to answer questions
    /// in the ask overlay. The turn resumes via `complete_interaction` or
    /// `cancel_interaction` once the overlay closes.
    pending_interaction: Option<(ToolCall, Vec<AskQuestion>)>,
    /// SQLite-backed session history (XDG location, per-`config.dirs`; `None`
    /// disables persistence, e.g. in tests).
    pub history_store: Option<crate::history::SessionStore>,
    /// Database row id of the session currently being continued, if any.
    pub current_session_id: Option<i64>,
    /// Name for the not-yet-persisted current session (timestamp-based).
    session_name: String,
    /// Working directory recorded with the session history rows; sessions are
    /// listed per workspace.
    pub session_cwd: String,
    /// Cumulative token usage across all completed LLM requests.
    pub usage: Usage,
    /// Number of completed LLM requests in this session.
    pub request_count: usize,
    /// Set to true by `/exit` to request a clean shutdown.
    pub should_quit: bool,
    /// Instant after which `status_message` should be auto-cleared.
    pub status_message_clear_at: Option<Instant>,
    /// Path of the config file currently in use, if one was found.
    pub config_path: Option<std::path::PathBuf>,
    /// Discovered Agent definitions registry.
    pub agent_registry: AgentRegistry,
    /// The main agent definition driving this conversation.
    pub main_agent: AgentDefinition,
    /// Whether `main_agent` was loaded from a file (true) or synthesized from
    /// the config (false). The real binary requires a `main.md` file.
    pub main_agent_from_file: bool,
    /// Running subagents and their state.
    pub subagents: SubagentManager,
    /// Discovered Agent Skills registry.
    pub skill_registry: SkillRegistry,
    /// Names of skills currently active in the conversation.
    pub active_skills: Vec<String>,
    /// Session TODO list, only used by the main agent's `todo` tool.
    pub todos: TodoList,
    /// Connected MCP servers, if any.
    pub mcp_manager: Option<Arc<McpManager>>,
    /// MCP servers temporarily disabled for this session. Shared with the
    /// gateway tools: disabled servers keep their connection and advertised
    /// tool (request prefix stays stable); the tool just rejects actions.
    pub disabled_mcp: crate::mcp::McpDisabledServers,
    /// Agent Memory subsystem state (recall/write passes and toggles).
    pub memory: crate::memory::MemoryState,
    /// All tools available to the LLM: built-in plus MCP-converted.
    pub toolbox: Toolbox,
    /// Events queued for the frontend, drained via [`App::take_event`].
    pending_events: VecDeque<RuntimeEvent>,
    /// Sender half of the LLM stream event channel; every spawned stream
    /// task shares it.
    pub(crate) stream_tx: mpsc::Sender<StreamEvent>,
    /// Receiver half of the LLM stream event channel; the runtime merges it
    /// into its event loop.
    pub(crate) stream_rx: mpsc::Receiver<StreamEvent>,
    /// Sender half of the stream-completion channel.
    pub(crate) done_tx: mpsc::Sender<Result<(), LlmError>>,
    /// Receiver half of the stream-completion channel.
    pub(crate) done_rx: mpsc::Receiver<Result<(), LlmError>>,
}

/// How long transient status-bar messages remain visible before clearing.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Config fields editable through `/config set` and the web config editor.
/// The kind drives value conversion and editor widgets; the runtime effect
/// of each field is documented in [`App::apply_config_field_effect`].
pub const CONFIG_FIELD_SPECS: &[crate::config::ConfigFieldSpec] = &[
    crate::config::ConfigFieldSpec {
        key: "agent.max_tool_rounds",
        kind: crate::config::ConfigFieldKind::Int,
        description: "max tool-call rounds per turn",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.log_level",
        kind: crate::config::ConfigFieldKind::String,
        description: "trace | debug | info | warn | error (applies on next start)",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.models.performance",
        kind: crate::config::ConfigFieldKind::String,
        description: "performance-tier model: a [[models]] id or name",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.models.efficient",
        kind: crate::config::ConfigFieldKind::String,
        description: "efficient-tier model (empty = falls back to performance)",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.auto_include_skills",
        kind: crate::config::ConfigFieldKind::Bool,
        description: "append the skill catalog to the system prompt",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.memory.enabled",
        kind: crate::config::ConfigFieldKind::Bool,
        description: "master memory switch (needs a role: memory agent)",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.memory.auto_recall",
        kind: crate::config::ConfigFieldKind::Bool,
        description: "dispatch a recall pass before each user turn",
    },
    crate::config::ConfigFieldSpec {
        key: "agent.memory.auto_write",
        kind: crate::config::ConfigFieldKind::Bool,
        description: "dispatch a summarize pass after each turn",
    },
    crate::config::ConfigFieldSpec {
        key: "shell.perm_mode",
        kind: crate::config::ConfigFieldKind::String,
        description: "allow_all | allow:<tags> | deny:<tags> | allow1:<cmds> | deny1:<cmds>",
    },
    crate::config::ConfigFieldSpec {
        key: "shell.read_paths",
        kind: crate::config::ConfigFieldKind::List,
        description: "additional readable directories (comma-separated)",
    },
    crate::config::ConfigFieldSpec {
        key: "shell.write_paths",
        kind: crate::config::ConfigFieldKind::List,
        description: "additional writable directories (comma-separated)",
    },
];

/// Parse a `true`/`false` (case-insensitive) config value.
fn parse_bool_value(key: &str, value: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match value.trim().to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("'{}' expects true or false, got '{}'", key, value).into()),
    }
}

/// Parse a comma-separated string list; empty segments are dropped.
fn parse_string_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Convert a user-supplied value into the TOML item stored for `key`,
/// rejecting unknown keys and unparseable values.
fn config_field_toml_item(
    key: &str,
    value: &str,
) -> Result<toml_edit::Item, Box<dyn std::error::Error>> {
    let spec = CONFIG_FIELD_SPECS
        .iter()
        .find(|spec| spec.key == key)
        .ok_or_else(|| format!("unknown config field: {}", key))?;
    match spec.kind {
        crate::config::ConfigFieldKind::Int => {
            let rounds: i64 = value
                .trim()
                .parse()
                .map_err(|_| format!("'{}' expects a number, got '{}'", key, value))?;
            Ok(toml_edit::value(rounds))
        }
        crate::config::ConfigFieldKind::Bool => Ok(toml_edit::value(parse_bool_value(key, value)?)),
        crate::config::ConfigFieldKind::String => Ok(toml_edit::value(value.to_string())),
        crate::config::ConfigFieldKind::List => {
            let items = toml_edit::Array::from_iter(parse_string_list(value));
            Ok(toml_edit::value(items))
        }
    }
}

/// Read one editable field's value out of a config struct (also used for
/// the built-in defaults).
fn config_field_value_of(config: &crate::config::AppConfig, key: &str) -> Option<String> {
    match key {
        "agent.max_tool_rounds" => Some(config.agent.max_tool_rounds.to_string()),
        "agent.log_level" => Some(config.agent.log_level.clone()),
        "agent.models.performance" => Some(config.agent.models.performance.clone()),
        "agent.models.efficient" => Some(config.agent.models.efficient.clone().unwrap_or_default()),
        "agent.auto_include_skills" => Some(config.agent.auto_include_skills.to_string()),
        "agent.memory.enabled" => Some(config.agent.memory.enabled.to_string()),
        "agent.memory.auto_recall" => Some(config.agent.memory.auto_recall.to_string()),
        "agent.memory.auto_write" => Some(config.agent.memory.auto_write.to_string()),
        "shell.perm_mode" => config
            .shell
            .as_ref()
            .map(|shell| shell.perm_mode.clone().unwrap_or_default()),
        "shell.read_paths" => config
            .shell
            .as_ref()
            .map(|shell| shell.read_paths.clone().unwrap_or_default().join(", ")),
        "shell.write_paths" => config
            .shell
            .as_ref()
            .map(|shell| shell.write_paths.clone().unwrap_or_default().join(", ")),
        _ => None,
    }
}

/// Whether the workspace config file sets `key` (it then overrides the
/// global config regardless of what the global file says).
fn workspace_overrides_key(key: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let path = crate::config::workspace_config_path();
    if !path.exists() {
        return Ok(false);
    }
    let value = crate::config::read_config_toml(&path)?;
    Ok(crate::config::value_get_dotted(&value, key).is_some())
}

/// The merged effective TOML for a scope-aware edit: global-base +
/// workspace-over (the same rule as config loading), with the edited
/// scope's file standing in for its side of the merge. `edited` is the
/// document as it will be written to the scope's file.
fn merged_scope_toml(
    scope: crate::config::ConfigScope,
    edited: &str,
) -> Result<toml::Value, Box<dyn std::error::Error>> {
    use crate::config::{merge_toml_values, read_config_toml};
    let edited_value: toml::Value =
        toml::from_str(edited).map_err(|e| format!("invalid TOML produced by the edit: {}", e))?;
    match scope {
        crate::config::ConfigScope::Workspace => {
            // The global file is the base; the edited workspace overlays it.
            let mut merged = read_config_toml(&crate::config::xdg_config_path())?;
            merge_toml_values(&mut merged, &edited_value);
            Ok(merged)
        }
        crate::config::ConfigScope::Global => {
            // The edited global file is the base; the workspace overlays it.
            let mut merged = edited_value;
            let ws = read_config_toml(&crate::config::workspace_config_path())?;
            merge_toml_values(&mut merged, &ws);
            Ok(merged)
        }
    }
}

/// Built-in default for an editable config key (used when neither scope
/// sets it).
fn default_config_field(key: &str) -> Option<String> {
    config_field_value_of(&crate::config::AppConfig::default(), key)
}

/// Generate the per-conversation session ID: stable for the lifetime of one
/// process (one conversation in the TUI).
///
/// The `ses_` prefix matches the session-id shape platform gateways expect
/// (e.g. the opencode zen `x-opencode-session`), so session-scoped routing
/// and prompt-cache keys recognize the value.
fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // A stack address mixes in ASLR entropy so two ids generated within the
    // same millisecond still differ.
    let marker = 0u8;
    let ptr = &marker as *const u8 as usize;
    format!(
        "ses_{:016x}{:08x}",
        nanos,
        std::process::id() as u64 ^ (ptr as u64).rotate_left(17)
    )
}

/// Generate the default history session name: timestamp-based, matching the
/// previous `history_<timestamp>` naming.
fn new_session_name() -> String {
    format!(
        "history_{}",
        chrono::Local::now().format("%Y-%m-%dT%H_%M_%S")
    )
}

/// Canonical working directory string recorded with history sessions.
pub fn session_cwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    cwd.canonicalize()
        .unwrap_or(cwd)
        .to_string_lossy()
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let session_id = new_session_id();
        let models = match config.resolve_models() {
            Ok(models) => models,
            Err(e) => {
                // Tests and minimal setups run without a configured model; real
                // runs are rejected earlier by config validation in main.
                tracing::warn!("no usable model configuration ({}); using placeholder", e);
                vec![Model::default()]
            }
        };
        let current_model = models.first().cloned().unwrap_or_default();
        let tier_models = config.resolve_tier_models(&models).unwrap_or_else(|e| {
            // Tests and minimal setups run without a tier configuration; real
            // runs are rejected earlier by config validation in main.
            tracing::warn!(
                "invalid tier model configuration ({}); tiers fall back to the current model",
                e
            );
            TierModels {
                performance: current_model.clone(),
                efficient: current_model.clone(),
            }
        });

        let (agent_registry, main_agent, main_agent_from_file) = Self::load_main_agent(&config);

        let shell_perm = main_agent
            .permission
            .clone()
            .or_else(|| config.shell.as_ref().and_then(|s| s.perm_mode.clone()));
        let shell_config = {
            let mut shell = config.shell.clone().unwrap_or_default();
            shell.perm_mode = shell_perm;
            shell
        };
        let (permissions, audit_logger) = (
            shell_config.permission_policy(),
            shell_config.audit_logger(),
        );

        let mut shell_state = ShellState::with_policy_and_logger(permissions, audit_logger);
        shell_state.args = Vec::new();
        let session_cwd = session_cwd();
        // Path-permission base: the session workspace is always readable and
        // writable; everything else is denied unless explicitly allowed
        // (config path lists, session grants via ask_permission).
        shell_state.permissions.base_dir = std::path::Path::new(&session_cwd).canonicalize().ok();

        let mut search_paths = SkillRegistry::default_paths();
        if let Some(extra) = &config.agent.skill_paths {
            search_paths.extend(extra.iter().cloned());
        }
        let skill_registry = SkillRegistry::discover(&search_paths).unwrap_or_else(|e| {
            tracing::warn!("failed to discover skills: {}", e);
            SkillRegistry::new()
        });

        let max_tool_rounds = config.agent.max_tool_rounds;

        // Composition root: register the built-in tools; MCP tools are added
        // in `connect_mcp` as servers come online.
        let mut toolbox = Toolbox::default();
        toolbox.register(std::sync::Arc::new(ShellTool));
        toolbox.register(std::sync::Arc::new(EditTool));
        toolbox.register(std::sync::Arc::new(ReadTool));
        toolbox.register(std::sync::Arc::new(SkillTool));
        toolbox.register(std::sync::Arc::new(AskUserTool));
        toolbox.register(std::sync::Arc::new(AskPermissionTool));
        // Task dispatch tools are only available to the main agent.
        toolbox.register(std::sync::Arc::new(TaskTool));
        toolbox.register(std::sync::Arc::new(TaskSyncTool));
        // The TODO list is a main-agent-only session tool.
        toolbox.register(std::sync::Arc::new(TodoTool));

        // Apply the main agent's allowed-tools filter if it specifies any.
        let toolbox = if main_agent.allowed_tools.is_empty() || main_agent.inherits_tools() {
            toolbox
        } else {
            toolbox.filter(&main_agent.explicit_tools())
        };

        let subagents = SubagentManager::new(
            toolbox.clone(),
            tier_models.clone(),
            current_model.clone(),
            config.clone(),
            None,
        );

        let system_prompt = Self::build_system_prompt(&main_agent, &config, &skill_registry);

        let memory = crate::memory::MemoryState::init(&config, &agent_registry);

        let (history_store, session_name) = match &config.dirs.history {
            Some(dir) => match crate::history::SessionStore::open(dir) {
                Ok(store) => (Some(store), new_session_name()),
                Err(e) => {
                    tracing::warn!("failed to open history database: {}", e);
                    (None, String::new())
                }
            },
            None => (None, String::new()),
        };

        let (stream_tx, stream_rx) = mpsc::channel(128);
        let (done_tx, done_rx) = mpsc::channel(1);

        Self {
            config,
            client: LlmClient::new(
                current_model.provider.clone(),
                current_model.clone(),
                &session_id,
            ),
            models,
            current_model,
            session_id,
            shell_state,
            messages: vec![Message::system(system_prompt)],
            agent_registry,
            main_agent,
            main_agent_from_file,
            subagents,
            skill_registry,
            active_skills: Vec::new(),
            todos: TodoList::new(),
            mcp_manager: None,
            disabled_mcp: crate::mcp::McpDisabledServers::new(),
            memory,
            toolbox,
            status: AppStatus::Idle,
            status_message: String::new(),
            max_tool_rounds,
            pending_tool_calls: Vec::new(),
            tool_rounds_this_turn: 0,
            turn_seq: 0,
            pending_interaction: None,
            history_store,
            current_session_id: None,
            session_name,
            session_cwd,
            usage: Usage::default(),
            request_count: 0,
            should_quit: false,
            status_message_clear_at: None,
            config_path: AppConfig::config_save_path(),
            pending_events: VecDeque::new(),
            stream_tx,
            stream_rx,
            done_tx,
            done_rx,
        }
    }

    /// Queue an event for the frontend.
    pub(crate) fn queue_event(&mut self, event: RuntimeEvent) {
        self.pending_events.push_back(event);
    }

    /// Take the next event queued for the frontend, if any.
    pub fn take_event(&mut self) -> Option<RuntimeEvent> {
        self.pending_events.pop_front()
    }

    /// Discover agent definitions and select the main agent.
    ///
    /// If `main.md` is found in the agent search paths, it is used as the main
    /// agent. Otherwise a synthetic placeholder main agent with an empty body
    /// is returned so the app struct stays constructible (tests, minimal
    /// setups); the real binary enforces the presence of `main.md` in
    /// `main.rs` and exits when it is missing.
    fn load_main_agent(config: &AppConfig) -> (AgentRegistry, AgentDefinition, bool) {
        let mut search_paths = AgentRegistry::default_paths();
        if let Some(extra) = &config.agent.agent_paths {
            search_paths.extend(extra.iter().cloned());
        }
        let registry = AgentRegistry::discover(&search_paths).unwrap_or_else(|e| {
            tracing::warn!("failed to discover agents: {}", e);
            AgentRegistry::new()
        });

        let main = registry.get("main").cloned();
        if let Some(main) = main {
            return (registry, main, true);
        }

        tracing::warn!("main.md agent definition not found; using empty placeholder");
        let placeholder = AgentDefinition {
            name: "main".to_string(),
            description: "Default main agent".to_string(),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: config.shell.as_ref().and_then(|s| s.perm_mode.clone()),
            skills: Vec::new(),
            role: Some(AgentRole::Main),
            body: String::new(),
            source_path: std::path::PathBuf::new(),
        };
        (registry, placeholder, false)
    }

    /// Build the initial system prompt from the main agent body, followed by
    /// the skill catalog when `auto_include_skills` is set. Tools are not
    /// listed here — they are advertised to the model through the API's
    /// native `tools` field.
    pub fn build_system_prompt(
        main_agent: &AgentDefinition,
        config: &AppConfig,
        registry: &SkillRegistry,
    ) -> String {
        let mut prompt = main_agent.body.clone();

        // The catalog always lists every discovered skill, including
        // session-disabled ones: the prompt must stay byte-stable across
        // disable/enable toggles so the provider's prefix cache survives.
        // Disabled skills are rejected by the `use_skill` tool instead.
        if config.agent.auto_include_skills && !registry.is_empty() {
            prompt.push_str("\nThe following Agent Skills are available. ");
            prompt.push_str("When a task matches a skill's description, activate it ");
            prompt.push_str("by calling the `use_skill` tool with the skill name, ");
            prompt.push_str("then follow the skill's instructions.\n\n");
            for (name, description) in registry.names_and_descriptions() {
                prompt.push_str(&format!("- {}: {}\n", name, description));
            }
        }
        prompt
    }

    /// Connect to configured MCP servers.
    ///
    /// Failures are logged and returned as warnings; the manager stays usable
    /// for any servers that did connect.
    pub async fn connect_mcp(&mut self) -> Vec<String> {
        let Some(mcp_config) = self.config.mcp.as_ref() else {
            return Vec::new();
        };
        if mcp_config.servers.is_empty() {
            return Vec::new();
        }

        let (manager, warnings) = McpManager::connect(&mcp_config.servers).await;
        if manager.is_empty() {
            tracing::warn!("no mcp servers connected");
            self.mcp_manager = None;
        } else {
            tracing::info!("{} mcp server(s) connected", manager.len());
            self.mcp_manager = Some(Arc::new(manager));
        }
        self.rebuild_toolbox();
        warnings
    }

    /// Rebuild the full toolbox: built-in tools, one MCP gateway tool per
    /// connected server (disabled servers keep their tool — it just rejects
    /// actions at execute time, so the advertised list stays stable), then
    /// the main agent's allowed-tools filter. The subagent manager's parent
    /// snapshot is kept in sync.
    fn rebuild_toolbox(&mut self) {
        let mut toolbox = Toolbox::default();
        toolbox.register(std::sync::Arc::new(ShellTool));
        toolbox.register(std::sync::Arc::new(EditTool));
        toolbox.register(std::sync::Arc::new(ReadTool));
        toolbox.register(std::sync::Arc::new(SkillTool));
        toolbox.register(std::sync::Arc::new(AskUserTool));
        toolbox.register(std::sync::Arc::new(AskPermissionTool));
        // Task dispatch tools are only available to the main agent.
        toolbox.register(std::sync::Arc::new(TaskTool));
        toolbox.register(std::sync::Arc::new(TaskSyncTool));
        // The TODO list is a main-agent-only session tool.
        toolbox.register(std::sync::Arc::new(TodoTool));

        if let Some(manager) = &self.mcp_manager {
            for server in manager.server_names() {
                let tool = McpServerTool::new(manager.clone(), &server, self.disabled_mcp.clone());
                tracing::info!(
                    "registered mcp gateway tool '{}' for server '{}'",
                    tool.name(),
                    server
                );
                toolbox.register(Arc::new(tool));
            }
        }

        let toolbox =
            if self.main_agent.allowed_tools.is_empty() || self.main_agent.inherits_tools() {
                toolbox
            } else {
                toolbox.filter(&self.main_agent.explicit_tools())
            };
        self.toolbox = toolbox;
        self.subagents
            .update_parent_toolbox(self.toolbox.clone(), self.mcp_manager.clone());
    }

    /// Append `text` as a user message.
    ///
    /// When the Agent Memory subsystem is enabled with `auto_recall`, the
    /// session's first user message also dispatches the memory subagent's
    /// recall pass; the caller must wait for it to finish (via
    /// `awaiting_memory_recall`) before starting the main LLM stream. Later
    /// turns of the same session reuse the memory injected at session start
    /// and start streaming immediately.
    pub fn submit_user_message(&mut self, text: String) {
        let session_start = !self.messages.iter().any(|m| m.role == Role::User);
        self.turn_seq += 1;
        self.messages.push(Message::user(text.clone()));
        self.tool_rounds_this_turn = 0;
        tracing::info!(turn = self.turn_seq, "user message submitted");
        self.queue_event(RuntimeEvent::MessagesChanged);
        if session_start {
            self.maybe_dispatch_memory_recall(&text);
        }
        self.persist_session();
    }

    /// Handle one submitted input line: slash commands take precedence;
    /// otherwise the line is submitted as a user message and the caller
    /// should resume the LLM stream.
    ///
    /// The caller owns the input line state: it decides whether to clear the
    /// line and whether to record the input in its history (use
    /// `CommandOutcome::record_history` for commands).
    pub async fn handle_input_line(&mut self, input: &str) -> InputLineOutcome {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return InputLineOutcome::Empty;
        }

        if trimmed.starts_with('/') {
            let outcome = self.handle_command(trimmed).await;
            InputLineOutcome::Handled(outcome)
        } else {
            self.submit_user_message(trimmed.to_string());
            InputLineOutcome::Submitted
        }
    }

    /// Prepare a fresh assistant message for streaming.
    pub fn start_assistant_message(&mut self) {
        self.messages.push(Message::assistant(String::new()));
        self.pending_tool_calls.clear();
        self.status = AppStatus::Streaming;
        self.status_message = "Streaming...".to_string();
    }

    /// Append a text chunk to the most recent assistant message.
    pub fn append_stream_text(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == Role::Assistant {
                last.content.push_str(text);
            }
        }
    }

    /// Append a reasoning chunk to the most recent assistant message.
    pub fn append_stream_reasoning(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == Role::Assistant {
                last.reasoning_content.push_str(text);
            }
        }
    }

    /// Store a tool call received from the streaming parser.
    pub fn add_tool_call(&mut self, call: ToolCall) {
        tracing::info!("pending tool call added: {} -> {}", call.id, call.name);
        self.pending_tool_calls.push(call.clone());
        if let Some(last) = self.messages.last_mut() {
            if last.role == Role::Assistant {
                last.had_tool_calls = true;
                last.tool_calls.push(call);
            }
        }
    }

    /// Mark streaming as finished and switch back to idle.
    ///
    /// If the assistant produced neither text nor tool calls, the empty
    /// placeholder message is removed so it cannot break future API requests.
    pub fn finish_stream(&mut self) {
        self.status = AppStatus::Idle;
        self.status_message.clear();
        self.queue_event(RuntimeEvent::MessagesChanged);

        if let Some(last) = self.messages.last() {
            tracing::info!(
                "assistant finished: content_len={} had_tool_calls={} pending_tool_calls={}",
                last.content.len(),
                last.had_tool_calls,
                self.pending_tool_calls.len()
            );
        }

        // Skill activation is driven by `use_skill` tool calls handled in
        // `run_pending_tool`, not by text markers.

        if self.has_empty_assistant_placeholder() {
            tracing::warn!("assistant response was empty; dropping placeholder message");
            self.messages.pop();
            self.messages.push(Message::event(
                "Assistant returned an empty response".to_string(),
            ));
            self.queue_event(RuntimeEvent::MessagesChanged);
        }
    }

    /// Return true if the most recent message is an empty assistant placeholder
    /// (no content, no reasoning content, and no tool calls).
    pub fn has_empty_assistant_placeholder(&self) -> bool {
        self.messages
            .last()
            .map(|last| {
                last.role == Role::Assistant
                    && last.content.is_empty()
                    && last.reasoning_content.is_empty()
                    && !last.had_tool_calls
            })
            .unwrap_or(false)
    }

    /// Accumulate token usage reported for one completed LLM request.
    pub fn record_usage(&mut self, usage: &Usage) {
        tracing::info!(
            "usage recorded: prompt={} completion={} total={} cached={}",
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            usage.cached_tokens
        );
        self.request_count += 1;
        self.usage.prompt_tokens += usage.prompt_tokens;
        self.usage.completion_tokens += usage.completion_tokens;
        self.usage.total_tokens = self
            .usage
            .total_tokens
            .max(self.usage.prompt_tokens + self.usage.completion_tokens);
        self.usage.cached_tokens += usage.cached_tokens;
        self.persist_session();
    }

    /// Return a human-readable list of discovered skill names.
    pub fn skill_names_list(&self) -> String {
        let names: Vec<&str> = self
            .skill_registry
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        if names.is_empty() {
            "no skills discovered".to_string()
        } else {
            format!("available skills: {}", names.join(", "))
        }
    }

    /// Return a human-readable list of discovered agents.
    pub fn agent_names_list(&self) -> String {
        let items = self.agent_registry.names_and_descriptions();
        if items.is_empty() {
            "no agents discovered".to_string()
        } else {
            items
                .into_iter()
                .map(|(name, desc)| format!("- {}: {}", name, desc))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// Spawn a subagent manually from the TUI.
    pub fn spawn_subagent(
        &mut self,
        name: &str,
        task: &str,
        mode: crate::subagent::SubagentContextMode,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let definition = self
            .agent_registry
            .get(name)
            .ok_or_else(|| format!("agent not found: {}", name))?
            .clone();
        if self.agent_registry.is_disabled(name) {
            return Err(format!(
                "agent '{}' is disabled for this session; it cannot be dispatched",
                name
            )
            .into());
        }
        let id = self.subagents.spawn(
            &definition,
            task.to_string(),
            mode,
            self.messages.clone(),
            self.shell_state.clone(),
            self.active_skills.clone(),
            self.skill_registry.clone(),
            None,
        );
        Ok(format!(
            "started subagent {} ({}): {}",
            definition.name, id, task
        ))
    }

    /// Validate a subagent id for the frontend's monitor page.
    ///
    /// The watch view itself is frontend-owned; the core only checks that the
    /// subagent exists.
    pub fn watch_subagent(&mut self, id: &str) -> Result<String, Box<dyn std::error::Error>> {
        if self.subagents.get(id).is_none() {
            return Err(format!("subagent not found: {}", id).into());
        }
        Ok(format!("watching subagent {}", id))
    }

    /// Remove a subagent from the status list.
    pub fn close_subagent(&mut self, id: &str) -> Result<String, Box<dyn std::error::Error>> {
        if self.subagents.remove(id) {
            Ok(format!("closed subagent {}", id))
        } else {
            Err(format!("subagent not found: {}", id).into())
        }
    }

    /// Dispatch the memory subagent's recall pass for the session's first
    /// user message.
    ///
    /// No-op unless memory is enabled with `auto_recall`, no memory pass is
    /// already running, and the message is the first user message of the
    /// session (later turns reuse the memory injected at session start).
    /// The main stream is withheld until the pass completes (see
    /// [`App::awaiting_memory_recall`]).
    pub fn maybe_dispatch_memory_recall(&mut self, user_prompt: &str) {
        if !self.memory.enabled() || !self.config.agent.memory.auto_recall {
            return;
        }
        if self.memory.pending_recall.is_some() || self.memory.pending_write.is_some() {
            return;
        }
        if self
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count()
            > 1
        {
            return;
        }
        let Some(definition) = self.agent_registry.get_by_role(AgentRole::Memory).cloned() else {
            return;
        };
        let task =
            crate::memory::recall_task(user_prompt, &self.memory.memory_dir, &self.shell_state.cwd);
        let id = self.spawn_memory_subagent(
            &definition,
            task,
            crate::subagent::SubagentContextMode::Create,
        );
        self.memory.pending_recall = Some(id);
        self.status = AppStatus::RunningTool;
        self.status_message = "Recalling memory...".to_string();
    }

    /// Dispatch the memory subagent's summarize/write pass for the turn that
    /// just completed.
    ///
    /// The pass runs in `fork` mode: it reuses the main agent's context
    /// prefix (full conversation, including the latest user turn) instead of
    /// receiving a transcript. When a memory pass is already running, the
    /// request is queued and dispatched when that pass finishes instead of
    /// being skipped.
    ///
    /// Runs in the background: the finished turn is already visible to the
    /// user, and the pass reports through a subagent event when done.
    pub fn maybe_dispatch_memory_write(&mut self) {
        if !self.memory.enabled() || !self.config.agent.memory.auto_write {
            return;
        }
        if self.memory.pending_recall.is_some() || self.memory.pending_write.is_some() {
            // A pass is still running: queue this write instead of skipping.
            self.memory.write_queued = true;
            return;
        }
        let Some(definition) = self.agent_registry.get_by_role(AgentRole::Memory).cloned() else {
            return;
        };
        // Without any user message there is no turn to summarize.
        if !self.messages.iter().any(|m| m.role == Role::User) {
            return;
        }
        let task = crate::memory::summarize_task(
            &definition.body,
            &self.memory.memory_dir,
            &self.shell_state.cwd,
        );
        let id = self.spawn_memory_subagent(
            &definition,
            task,
            crate::subagent::SubagentContextMode::Fork,
        );
        tracing::info!("memory write pass dispatched as {}", id);
        self.memory.pending_write = Some(id);
    }

    /// Dispatch a queued write pass once the running one has finished.
    fn dispatch_queued_memory_write(&mut self) {
        if self.memory.write_queued && self.memory.pending_write.is_none() {
            self.memory.write_queued = false;
            self.maybe_dispatch_memory_write();
        }
    }

    fn spawn_memory_subagent(
        &mut self,
        definition: &AgentDefinition,
        task: String,
        mode: crate::subagent::SubagentContextMode,
    ) -> crate::subagent::SubagentId {
        self.subagents.spawn(
            definition,
            task,
            mode,
            self.messages.clone(),
            self.shell_state.clone(),
            self.active_skills.clone(),
            self.skill_registry.clone(),
            None,
        )
    }

    /// Whether a user-submitted message is still waiting for its memory
    /// recall pass to finish (the main stream must not start yet).
    pub fn awaiting_memory_recall(&self) -> bool {
        self.memory.pending_recall.is_some()
    }

    /// Drive the subagent event loop until both pending memory passes have
    /// finished (including any write pass queued behind them). Used by
    /// headless (`--test`) mode where nothing else drains
    /// `subagents.event_rx`.
    pub async fn await_memory_passes(&mut self) {
        while self.memory.pending_recall.is_some()
            || self.memory.pending_write.is_some()
            || self.memory.write_queued
        {
            match self.subagents.event_rx.recv().await {
                Some(event) => {
                    self.handle_subagent_event(event);
                }
                None => {
                    self.memory.pending_recall = None;
                    self.memory.pending_write = None;
                    self.memory.write_queued = false;
                    break;
                }
            }
        }
    }

    /// Human-readable summary of the memory subsystem, shown by `/memory`.
    pub fn memory_status_list(&self) -> String {
        let mut lines = vec![format!(
            "memory store: {}",
            self.memory.memory_dir.display()
        )];
        lines.push(format!(
            "state: {}",
            if self.memory.available {
                if self.memory.session_enabled {
                    "enabled"
                } else {
                    "disabled for this session (/memory on)"
                }
            } else {
                "unavailable (enable [agent.memory] and provide a role: memory agent)"
            }
        ));
        lines.push(format!(
            "auto_recall: {}, auto_write: {}",
            self.config.agent.memory.auto_recall, self.config.agent.memory.auto_write
        ));
        if self.memory.pending_recall.is_some() {
            lines.push("recall pass: running".to_string());
        }
        if self.memory.pending_write.is_some() {
            lines.push("write pass: running".to_string());
        } else if self.memory.write_queued {
            lines.push("write pass: queued".to_string());
        }
        lines.join("\n")
    }

    /// Toggle the session-level memory switch (`/memory on|off`).
    pub fn set_memory_enabled(&mut self, enabled: bool) -> String {
        if !self.memory.available {
            return "memory is unavailable; enable [agent.memory] and provide a role: memory agent"
                .to_string();
        }
        self.memory.session_enabled = enabled;
        self.persist_session();
        if enabled {
            "memory enabled for this session".to_string()
        } else {
            "memory disabled for this session".to_string()
        }
    }

    /// Build a serializable snapshot of the session state for remote
    /// frontends.
    pub fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            status: self.status,
            status_message: self.status_message.clone(),
            session_id: self.session_id.clone(),
            session_cwd: self.session_cwd.clone(),
            current_model: self.current_model.clone(),
            models: self.models.iter().map(ModelSnapshot::from).collect(),
            messages: self.messages.clone(),
            usage: self.usage,
            request_count: self.request_count,
            active_skills: self.active_skills.clone(),
            todos: self.todos.clone(),
            subagents: self
                .subagents
                .list()
                .iter()
                .map(|s| SubagentSummary {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    task: s.task.clone(),
                    state: s.state,
                    mode: s.mode,
                    result: s.result.clone(),
                    error: s.error.clone(),
                })
                .collect(),
            should_quit: self.should_quit,
            memory_available: self.memory.available,
            memory_session_enabled: self.memory.session_enabled,
            skill_catalog: self.skill_catalog(),
            mcp_servers: self.mcp_catalog(),
            agent_catalog: self.agent_catalog(),
            config_fields: self.config_fields(),
            config_field_specs: self.config_field_specs(),
            config_scopes: self.config_scopes(),
        }
    }

    /// Return a human-readable status list of running subagents.
    pub fn subagent_status_list(&self) -> String {
        let subs = self.subagents.list();
        if subs.is_empty() {
            return "no active subagents".to_string();
        }
        subs.iter()
            .map(|s| {
                let task = truncate(&s.task, 40);
                format!("- {} [{}] {} ({})", s.id, s.state.as_str(), s.name, task)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Return a human-readable list of configured MCP servers and their tools.
    ///
    /// Reads the catalogs cached at connect time; no server round-trips.
    pub fn mcp_server_list(&self) -> String {
        let configured: Vec<&str> = self
            .config
            .mcp
            .as_ref()
            .map(|m| m.servers.iter().map(|s| s.name.as_str()).collect())
            .unwrap_or_default();
        if configured.is_empty() {
            return "no mcp servers configured".to_string();
        }

        let mut lines = vec![format!("configured mcp servers: {}", configured.join(", "))];
        if let Some(manager) = &self.mcp_manager {
            let mut total = 0;
            for server in manager.server_names() {
                let catalog = manager.tool_catalog(&server).unwrap_or_default();
                lines.push(format!("server '{}':", server));
                if catalog.is_empty() {
                    lines.push("  (no tools)".to_string());
                } else {
                    for tool in &catalog {
                        lines.push(format!(
                            "- {}: {}",
                            tool.name,
                            tool.first_description_line()
                        ));
                        total += 1;
                    }
                }
            }
            if total == 0 {
                lines.push("no mcp tools available".to_string());
            }
        } else {
            lines.push("mcp manager not connected".to_string());
        }
        lines.join("\n")
    }

    /// Return a concise MCP connection status message.
    pub fn mcp_status_message(&self) -> String {
        let configured = self
            .config
            .mcp
            .as_ref()
            .map(|m| m.servers.len())
            .unwrap_or(0);
        let connected = self.mcp_manager.as_ref().map(|m| m.len()).unwrap_or(0);
        format!("mcp: {}/{} servers connected", connected, configured)
    }

    /// Activate a skill by name and inject its instructions into the conversation.
    pub fn activate_skill(&mut self, name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let skill = self
            .skill_registry
            .activate(name)?
            .ok_or_else(|| format!("skill not found: {}", name))?;
        let instructions = skill.instructions().unwrap_or("").to_string();
        if instructions.trim().is_empty() {
            return Ok(format!("skill '{}' has no instructions", name));
        }
        if !self.active_skills.contains(&name.to_string()) {
            self.active_skills.push(name.to_string());
        }
        self.messages.push(Message::system(format!(
            "Skill '{}' instructions:\n{}",
            name, instructions
        )));
        self.persist_session();
        Ok(format!("activated skill '{}'", name))
    }

    /// Enable or disable a skill for the LLM (session-scoped).
    ///
    /// Disabled skills are rejected by the `use_skill` tool. The system
    /// prompt is deliberately left untouched — it must stay byte-stable
    /// across toggles so the provider's prompt prefix cache survives; the
    /// catalog keeps listing disabled skills, and a disabled `use_skill`
    /// call returns an error the model can read.
    ///
    /// Manual activation (`/skill use`) and already-active skills are
    /// unaffected.
    pub fn set_skill_disabled(
        &mut self,
        name: &str,
        disabled: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        if self.skill_registry.get(name).is_none() {
            return Err(format!("skill not found: {}", name).into());
        }
        self.skill_registry.set_disabled(name, disabled);
        self.persist_session();
        Ok(if disabled {
            format!(
                "skill '{}' disabled: the use_skill tool rejects it; the system \
                 prompt is unchanged (manual /skill use still works)",
                name
            )
        } else {
            format!("skill '{}' enabled", name)
        })
    }

    /// The discovered skill catalog with session disable flags.
    pub fn skill_catalog(&self) -> Vec<SkillCatalogEntry> {
        self.skill_registry
            .iter()
            .map(|s| SkillCatalogEntry {
                name: s.name.clone(),
                description: s.description.clone(),
                disabled: self.skill_registry.is_disabled(&s.name),
            })
            .collect()
    }

    /// Full raw contents of a skill's `SKILL.md` for the preview page.
    pub fn skill_preview(&self, name: &str) -> Result<SkillPreview, Box<dyn std::error::Error>> {
        let skill = self
            .skill_registry
            .get(name)
            .ok_or_else(|| format!("skill not found: {}", name))?;
        let path = skill.root.join("SKILL.md");
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        Ok(SkillPreview {
            name: skill.name.clone(),
            description: skill.description.clone(),
            content,
        })
    }

    /// The configured MCP servers with connection and enable state.
    pub fn mcp_catalog(&self) -> Vec<McpServerInfo> {
        let Some(mcp) = self.config.mcp.as_ref() else {
            return Vec::new();
        };
        mcp.servers
            .iter()
            .map(|s| {
                let connected = self
                    .mcp_manager
                    .as_ref()
                    .map(|m| m.is_connected(&s.name))
                    .unwrap_or(false);
                let tools = self
                    .mcp_manager
                    .as_ref()
                    .and_then(|m| m.tool_catalog(&s.name))
                    .map(|catalog| catalog.iter().map(|t| t.name.clone()).collect())
                    .unwrap_or_default();
                McpServerInfo {
                    name: s.name.clone(),
                    enabled: !self.disabled_mcp.contains(&s.name),
                    connected,
                    tools,
                }
            })
            .collect()
    }

    /// Enable or disable an MCP server for the LLM (session-scoped).
    ///
    /// Disabling only flips a shared flag the gateway tools check at execute
    /// time: the connection (if any) stays open and the advertised tool list
    /// is untouched, so the request prefix the provider caches stays stable.
    pub fn set_mcp_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let configured = self
            .config
            .mcp
            .as_ref()
            .map(|m| m.servers.iter().any(|s| s.name == name))
            .unwrap_or(false);
        if !configured {
            return Err(format!("mcp server not configured: {}", name).into());
        }
        self.disabled_mcp.set(name, !enabled);
        self.persist_session();
        Ok(if enabled {
            format!("mcp server '{}' enabled", name)
        } else {
            format!(
                "mcp server '{}' disabled: its gateway tool rejects actions; \
                 the advertised tool list is unchanged",
                name
            )
        })
    }

    /// Connect to one configured MCP server on demand (the web UI's connect
    /// action). An already-connected server is disconnected first, so this
    /// doubles as a reconnect for dead connections (e.g. an exited stdio
    /// child that still sits in the connection map); the gateway tool is
    /// registered after a successful connect.
    pub async fn connect_mcp_server(
        &mut self,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let config = self
            .config
            .mcp
            .as_ref()
            .and_then(|m| m.servers.iter().find(|s| s.name == name))
            .cloned()
            .ok_or_else(|| format!("mcp server not configured: {}", name))?;
        if self.disabled_mcp.contains(name) {
            return Err(format!("mcp server '{}' is disabled; enable it first", name).into());
        }
        let mut verb = "connected";
        if let Some(manager) = &self.mcp_manager {
            if manager.is_connected(name) {
                verb = "reconnected";
                manager.disconnect(name);
            }
        }
        let manager = self
            .mcp_manager
            .get_or_insert_with(|| Arc::new(McpManager::empty()));
        let catalog = manager
            .connect_one(&config)
            .await
            .map_err(|e| format!("failed to connect mcp server '{}': {}", name, e))?;
        let count = catalog.len();
        // Providers render `tools` ahead of the messages, so a changed tool
        // set invalidates their prompt-prefix cache for this conversation.
        let tools_before: Vec<String> = self
            .toolbox
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        self.rebuild_toolbox();
        let tools_after: Vec<String> = self
            .toolbox
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        self.persist_session();
        let cache_note = if tools_before == tools_after {
            String::new()
        } else {
            "; the advertised tool set changed, so the provider's prompt-prefix \
             cache resets from the next request"
                .to_string()
        };
        Ok(format!(
            "{} mcp server '{}' ({} tool(s)){}",
            verb, name, count, cache_note
        ))
    }

    /// Check one prospective `[[mcp.servers]]` entry is usable: the name is
    /// set, and the entry carries the fields its transport requires.
    fn validate_mcp_server(
        server: &crate::config::McpServerConfig,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::config::McpTransport;

        if server.name.trim().is_empty() {
            return Err("mcp server name must not be empty".into());
        }
        match server.transport {
            McpTransport::Stdio => {
                if server.command.trim().is_empty() {
                    return Err(format!(
                        "mcp server '{}' needs a command for the stdio transport",
                        server.name
                    )
                    .into());
                }
            }
            McpTransport::StreamableHttp => {
                let url = server.url.as_deref().unwrap_or("").trim();
                if url.is_empty() {
                    return Err(format!(
                        "mcp server '{}' needs a url for the streamable-http transport",
                        server.name
                    )
                    .into());
                }
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(format!(
                        "mcp server '{}' url must start with http:// or https://",
                        server.name
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Insert or update one `[[mcp.servers]]` entry in one scope
    /// (`workspace` or `global`), writing the entry into that scope's TOML
    /// file while preserving all untouched content (comments, formatting,
    /// unknown keys). The name is the entry key: renaming means removing the
    /// entry and adding it again.
    ///
    /// On success the runtime adopts the merged effective server list; a
    /// changed entry's live connection is dropped so the next connect uses
    /// the new configuration.
    pub fn upsert_mcp_server_in(
        &mut self,
        scope: crate::config::ConfigScope,
        server: crate::config::McpServerConfig,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{doc_upsert_mcp_server, mcp_servers_from_toml};

        Self::validate_mcp_server(&server)?;
        let old = self
            .config
            .mcp
            .as_ref()
            .and_then(|m| m.servers.iter().find(|s| s.name == server.name))
            .cloned();

        let path = crate::config::scope_config_path(scope);
        let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or_default();
        // Whether the entry was already defined in this scope's file.
        let existed = {
            let before: toml::Value = toml::from_str(&doc.to_string())
                .map_err(|e| format!("invalid TOML in {}: {}", path.display(), e))?;
            mcp_servers_from_toml(&before)?
                .unwrap_or_default()
                .iter()
                .any(|s| s.name == server.name)
        };
        doc_upsert_mcp_server(&mut doc, &server)?;

        // Validate against the merged configuration that results from this
        // edit: always global-base + workspace-over, with the edited scope's
        // file (as it will be written) standing in for its side of the merge.
        let merged = merged_scope_toml(scope, &doc.to_string())?;
        let servers = mcp_servers_from_toml(&merged)?.unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, doc.to_string())?;

        // Adopt the effective list; a stale connection for a changed entry
        // is dropped so the next connect uses the new configuration.
        let changed = old.as_ref() != Some(&server);
        self.config.mcp = Some(crate::config::McpConfig { servers });
        if changed {
            if let Some(manager) = &self.mcp_manager {
                manager.disconnect(&server.name);
            }
        }
        self.rebuild_toolbox();
        self.persist_session();
        let verb = if existed { "updated" } else { "added" };
        Ok(format!(
            "{} mcp server '{}' in {}",
            verb,
            server.name,
            path.display()
        ))
    }

    /// Remove one `[[mcp.servers]]` entry (matched by `name`) from one
    /// scope. The runtime adopts the remaining effective list; a connection
    /// to a server that is no longer effective is dropped.
    pub fn remove_mcp_server_in(
        &mut self,
        scope: crate::config::ConfigScope,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{ConfigScope, doc_remove_mcp_server, mcp_servers_from_toml};

        let name = name.trim();
        if name.is_empty() {
            return Err("mcp server name must not be empty".into());
        }
        let path = crate::config::scope_config_path(scope);
        if !path.exists() {
            return Err(format!("no config file at {}", path.display()).into());
        }
        let text = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| format!("invalid TOML in {}: {}", path.display(), e))?;
        if !doc_remove_mcp_server(&mut doc, name)? {
            return Err(
                format!("mcp server '{}' is not defined in {}", name, path.display()).into(),
            );
        }

        // Validate the merged configuration that results from this removal.
        let merged = merged_scope_toml(scope, &doc.to_string())?;
        let servers = mcp_servers_from_toml(&merged)?.unwrap_or_default();

        std::fs::write(&path, doc.to_string())?;

        self.config.mcp = Some(crate::config::McpConfig { servers });
        let still_effective = self
            .config
            .mcp
            .as_ref()
            .is_some_and(|m| m.servers.iter().any(|s| s.name == name));
        if !still_effective {
            if let Some(manager) = &self.mcp_manager {
                manager.disconnect(name);
            }
        }
        self.rebuild_toolbox();
        self.persist_session();

        let mut note = String::new();
        if still_effective && scope == ConfigScope::Global {
            note =
                "; note: the workspace config still defines this server, so it remains effective"
                    .to_string();
        }
        Ok(format!(
            "removed mcp server '{}' from {}{}",
            name,
            path.display(),
            note
        ))
    }

    /// Enable or disable an agent for dispatch (session-scoped).
    ///
    /// Special-role agents (`main`, `memory`) cannot be disabled: the main
    /// agent drives the conversation and the memory subsystem depends on its
    /// agent. Restoring saved subagents ignores the flag, so `/resume` keeps
    /// working.
    pub fn set_agent_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let agent = self
            .agent_registry
            .get(name)
            .ok_or_else(|| format!("agent not found: {}", name))?;
        if agent.role.is_some() {
            return Err(
                format!("agent '{}' has a special role and cannot be disabled", name).into(),
            );
        }
        self.agent_registry.set_disabled(name, !enabled);
        self.persist_session();
        Ok(if enabled {
            format!("agent '{}' enabled", name)
        } else {
            format!(
                "agent '{}' disabled: task/taskSync dispatch and /agent use reject it",
                name
            )
        })
    }

    /// The discovered agent catalog with role and disable state.
    pub fn agent_catalog(&self) -> Vec<AgentCatalogEntry> {
        self.agent_registry
            .iter()
            .map(|a| AgentCatalogEntry {
                name: a.name.clone(),
                description: a.description.clone(),
                role: a.role.map(|r| r.as_str().to_string()),
                disabled: self.agent_registry.is_disabled(&a.name),
                editable: a.role.is_none(),
            })
            .collect()
    }

    /// Detail of one agent definition for the editor page: the raw `.md`
    /// file content plus role/disable state. Special-role agents return the
    /// content too (preview-only; saving is refused by `save_agent`).
    pub fn agent_detail(&self, name: &str) -> Result<AgentDetail, Box<dyn std::error::Error>> {
        let agent = self
            .agent_registry
            .get(name)
            .ok_or_else(|| format!("agent not found: {}", name))?;
        let content = if agent.source_path.as_os_str().is_empty() {
            String::new()
        } else {
            std::fs::read_to_string(&agent.source_path)
                .map_err(|e| format!("failed to read {}: {}", agent.source_path.display(), e))?
        };
        Ok(AgentDetail {
            name: agent.name.clone(),
            description: agent.description.clone(),
            role: agent.role.map(|r| r.as_str().to_string()),
            disabled: self.agent_registry.is_disabled(name),
            editable: agent.role.is_none(),
            source_path: agent.source_path.display().to_string(),
            content,
        })
    }

    /// Validate agent `.md` content against a candidate file name without
    /// touching any real definition file (a temp file is used and removed).
    fn validate_agent_content(name: &str, content: &str) -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!(
            "catus-agent-validate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create validation dir: {}", e))?;
        let path = dir.join(format!("{}.md", name));
        let result = (|| -> Result<(), String> {
            std::fs::write(&path, content)
                .map_err(|e| format!("failed to write validation file: {}", e))?;
            let definition = AgentDefinition::load(&path)
                .map_err(|e| format!("invalid agent definition: {}", e))?;
            if definition.name != name {
                return Err(format!(
                    "frontmatter name '{}' does not match the agent name '{}'",
                    definition.name, name
                ));
            }
            if definition.role.is_some() {
                return Err(
                    "new/edited agents must be plain subagents (no `role` field)".to_string(),
                );
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        result
    }

    /// Re-discover the agent registry, keeping the session's disabled set
    /// for agents that still exist.
    fn reload_agent_registry(&mut self) -> Result<(), String> {
        let mut search_paths = AgentRegistry::default_paths();
        if let Some(extra) = &self.config.agent.agent_paths {
            search_paths.extend(extra.iter().cloned());
        }
        let registry = AgentRegistry::discover(&search_paths)?;
        let disabled: Vec<String> = self
            .agent_registry
            .disabled_names()
            .to_vec()
            .into_iter()
            .filter(|n| registry.get(n).is_some())
            .collect();
        self.agent_registry = registry;
        self.agent_registry.set_disabled_names(disabled);
        Ok(())
    }

    /// Save an edited agent definition back to its source file. Special-role
    /// agents (`main`, `memory`) are read-only. The content is validated
    /// before the file is written; the registry is re-discovered afterwards.
    pub fn save_agent(
        &mut self,
        name: &str,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let agent = self
            .agent_registry
            .get(name)
            .ok_or_else(|| format!("agent not found: {}", name))?;
        if agent.role.is_some() {
            return Err(format!(
                "agent '{}' has a special role and is read-only (preview only)",
                name
            )
            .into());
        }
        let path = agent.source_path.clone();
        Self::validate_agent_content(name, content)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        self.reload_agent_registry()
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        Ok(format!("saved agent '{}' to {}", name, path.display()))
    }

    /// Create a new agent definition in the workspace `.sutcac/agents/`
    /// directory and re-discover the registry.
    pub fn create_agent(
        &mut self,
        name: &str,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        crate::frontmatter::validate_name(name)
            .map_err(|e| format!("invalid agent name '{}': {}", name, e))?;
        if self.agent_registry.get(name).is_some() {
            return Err(format!("agent '{}' already exists", name).into());
        }
        let mut dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        dir.push(".sutcac");
        dir.push("agents");
        let path = dir.join(format!("{}.md", name));
        Self::validate_agent_content(name, content)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, content)?;
        self.reload_agent_registry()
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        Ok(format!("created agent '{}' at {}", name, path.display()))
    }

    /// Return the editable config fields as (key, current_value) pairs.
    pub fn config_fields(&self) -> Vec<(String, String)> {
        CONFIG_FIELD_SPECS
            .iter()
            .map(|spec| {
                (
                    spec.key.to_string(),
                    config_field_value_of(&self.config, spec.key).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Metadata about the editable config fields (drives editor widgets).
    pub fn config_field_specs(&self) -> Vec<crate::config::ConfigFieldSpec> {
        CONFIG_FIELD_SPECS.to_vec()
    }

    /// Rebuild the LLM client from the current model and session ID.
    fn rebuild_client(&mut self) {
        self.client = LlmClient::new(
            self.current_model.provider.clone(),
            self.current_model.clone(),
            &self.session_id,
        );
    }

    /// Switch to a configured model, matched by id or display name. The new
    /// client takes effect with the next LLM request.
    pub fn set_model(&mut self, name_or_id: &str) -> Result<String, Box<dyn std::error::Error>> {
        let wanted = name_or_id.trim();
        let model = self
            .models
            .iter()
            .find(|m| m.id == wanted || m.display_name() == wanted)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "model not found: {} (available: {})",
                    wanted,
                    self.model_names_list()
                )
            })?;
        let msg = format!(
            "switched to model {} ({})",
            model.display_name(),
            model.provider.name
        );
        self.current_model = model;
        self.rebuild_client();
        // Tier-less subagent definitions resolve against the parent's current
        // model, so keep the subagent manager's snapshot in sync.
        self.subagents
            .update_parent_model(self.current_model.clone());
        self.persist_session();
        Ok(msg)
    }

    /// Human-readable list of configured models, marking the current one.
    /// Each entry shows the model id (the value accepted by `/model`), with
    /// the display name alongside when it differs.
    pub fn model_names_list(&self) -> String {
        if self.models.is_empty() {
            return "no models configured".to_string();
        }
        self.models
            .iter()
            .map(|m| {
                let name = m.display_name();
                let label = if name == m.id {
                    m.id.clone()
                } else {
                    format!("{} ({})", name, m.id)
                };
                if m.id == self.current_model.id {
                    format!("{} * (current)", label)
                } else {
                    label
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Set the `[agent.models]` tier mapping (`performance` or `efficient`)
    /// to a configured model (matched by id or display name) and save the
    /// change: to the workspace config when it exists, otherwise to the XDG
    /// config. The reference is validated before the file is written; on
    /// success the tier resolution is refreshed for future subagents.
    pub fn set_tier_model(
        &mut self,
        tier: crate::config::ModelTier,
        reference: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(format!("usage: /model set-{} <model-id-or-name>", tier.as_str()).into());
        }
        let key = format!("agent.models.{}", tier.as_str());
        let scope = if crate::config::workspace_config_path().exists() {
            crate::config::ConfigScope::Workspace
        } else {
            crate::config::ConfigScope::Global
        };
        self.set_config_field_in(scope, &key, reference)?;
        // Report the resolved display name when the reference matches one.
        let label = self
            .models
            .iter()
            .find(|m| {
                m.id == reference
                    || (!m.display_name().trim().is_empty() && m.display_name() == reference)
            })
            .map(|m| m.display_name().to_string())
            .unwrap_or_else(|| reference.to_string());
        Ok(format!("{} model set to {}", tier.as_str(), label))
    }

    /// Update a single config field by key, save the config file, and refresh
    /// any runtime component that depends on the changed value.
    pub fn set_config_field(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let path = self
            .config_path
            .clone()
            .ok_or("no config file found; cannot save changes")?;

        self.apply_config_field_effect(key, value)?;

        self.config.save(&path)?;
        Ok(format!("saved {} to {}", key, path.display()))
    }

    /// Runtime update for one config field: mutates `self.config` and any
    /// runtime component that depends on the changed value. Errors on
    /// unknown keys or values that fail to parse.
    ///
    /// Effects by field:
    /// - `agent.max_tool_rounds` — applied to the live turn loop.
    /// - `agent.log_level` — config only; picked up on the next start.
    /// - `agent.models.performance` / `agent.models.efficient` — config +
    ///   tier resolution; an empty `efficient` unsets it (falls back to
    ///   `performance`), an empty `performance` is rejected. The reference
    ///   must match a configured `[[models]]` id or name.
    /// - `agent.auto_include_skills` — rebuilds the system prompt in place.
    /// - `agent.memory.enabled` — config; also syncs the session toggle so
    ///   the change is visible immediately when memory is available.
    /// - `agent.memory.auto_recall` / `auto_write` — read live per dispatch.
    /// - `shell.*` — rebuilds the shell permission policy.
    fn apply_config_field_effect(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match key {
            "agent.max_tool_rounds" => {
                self.config.agent.max_tool_rounds = value.parse()?;
                self.max_tool_rounds = self.config.agent.max_tool_rounds;
            }
            "agent.log_level" => self.config.agent.log_level = value.to_string(),
            "agent.models.performance" | "agent.models.efficient" => {
                let reference = value.trim().to_string();
                // Validate on a copy first so a rejected value (unknown
                // model reference, empty performance tier) leaves both the
                // config and the runtime state untouched.
                let mut config = self.config.clone();
                if key == "agent.models.performance" {
                    if reference.is_empty() {
                        return Err(
                            "[agent.models].performance is required; set it to a [[models]] \
                             id or name"
                                .into(),
                        );
                    }
                    config.agent.models.performance = reference;
                } else {
                    // An empty efficient tier unsets it (falls back to
                    // performance).
                    config.agent.models.efficient = (!reference.is_empty()).then_some(reference);
                }
                let tiers = config
                    .resolve_tier_models(&self.models)
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                self.config = config;
                // Subagents resolve their tier through the parent's tier
                // mapping, so keep the manager's snapshot in sync.
                self.subagents.update_parent_tier_models(tiers);
                self.persist_session();
            }
            "agent.auto_include_skills" => {
                self.config.agent.auto_include_skills = parse_bool_value(key, value)?;
                let prompt =
                    Self::build_system_prompt(&self.main_agent, &self.config, &self.skill_registry);
                if let Some(first) = self.messages.first_mut() {
                    if first.is_system() {
                        // The system prompt sits at the head of every
                        // request; rewriting it invalidates the provider's
                        // prefix cache for the rest of the conversation.
                        let changed = first.content != prompt;
                        *first = Message::system(prompt);
                        if changed {
                            self.add_event_message(
                                "system prompt changed: the provider's prompt-prefix cache \
                                 is invalidated for the rest of this conversation"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            "agent.memory.enabled" => {
                self.config.agent.memory.enabled = parse_bool_value(key, value)?;
                if self.memory.available {
                    self.memory.session_enabled = self.config.agent.memory.enabled;
                }
            }
            "agent.memory.auto_recall" => {
                self.config.agent.memory.auto_recall = parse_bool_value(key, value)?;
            }
            "agent.memory.auto_write" => {
                self.config.agent.memory.auto_write = parse_bool_value(key, value)?;
            }
            "shell.perm_mode" => {
                let shell = self.config.shell.get_or_insert_with(ShellConfig::default);
                shell.perm_mode = Some(value.to_string());
                self.rebuild_shell_policy();
            }
            "shell.read_paths" | "shell.write_paths" => {
                let paths = Some(parse_string_list(value));
                let shell = self.config.shell.get_or_insert_with(ShellConfig::default);
                if key == "shell.read_paths" {
                    shell.read_paths = paths;
                } else {
                    shell.write_paths = paths;
                }
                self.rebuild_shell_policy();
            }
            _ => return Err(format!("unknown config field: {}", key).into()),
        }
        Ok(())
    }

    /// Set a config field in one scope (`workspace` or `global`), writing the
    /// value into that scope's TOML file while preserving all untouched
    /// content (comments, formatting, unknown keys).
    ///
    /// The runtime only reflects the new value when the written scope wins:
    /// the workspace file overrides the global file key by key, so writing a
    /// key to the global config that the workspace also sets leaves the
    /// effective value untouched (the reply message says so).
    pub fn set_config_field_in(
        &mut self,
        scope: crate::config::ConfigScope,
        key: &str,
        value: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{ConfigScope, doc_set_dotted};

        let item = config_field_toml_item(key, value)?;
        let path = crate::config::scope_config_path(scope);

        let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or_default();
        doc_set_dotted(&mut doc, key, item)?;

        // Apply the runtime effect before persisting: a value that fails
        // validation (e.g. a tier reference no configured model matches)
        // then leaves both the file and the runtime state untouched.
        let overridden = matches!(scope, ConfigScope::Global) && workspace_overrides_key(key)?;
        if !overridden {
            self.apply_config_field_effect(key, value)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, doc.to_string())?;

        if overridden {
            Ok(format!(
                "saved {} to {}; note: the workspace config overrides this key, so the effective value is unchanged",
                key,
                path.display()
            ))
        } else if scope == ConfigScope::Workspace {
            Ok(format!(
                "saved {} to {} (workspace override)",
                key,
                path.display()
            ))
        } else {
            Ok(format!("saved {} to {}", key, path.display()))
        }
    }

    /// Remove a config field from one scope. The runtime then reflects the
    /// remaining effective value: the other scope's value when set, otherwise
    /// the built-in default.
    pub fn remove_config_field_in(
        &mut self,
        scope: crate::config::ConfigScope,
        key: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{doc_remove_dotted, toml_display_value, value_get_dotted};

        let path = crate::config::scope_config_path(scope);
        if !path.exists() {
            return Err(format!("no config file at {}", path.display()).into());
        }
        let text = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| format!("invalid TOML in {}: {}", path.display(), e))?;
        let removed = doc_remove_dotted(&mut doc, key)?;
        if !removed {
            return Err(format!("{} is not set in {}", key, path.display()).into());
        }

        // The effective value after the removal: global-base + workspace-over
        // (the same rule as config loading) with the edited scope's file (the
        // key gone) standing in for its side of the merge.
        let merged = merged_scope_toml(scope, &doc.to_string())?;
        let effective = value_get_dotted(&merged, key)
            .map(toml_display_value)
            .or_else(|| default_config_field(key))
            .ok_or_else(|| format!("unknown config field: {}", key))?;

        // Apply the runtime effect before persisting: an effective value
        // that fails validation (e.g. no performance tier left) then leaves
        // the file untouched and the key still set.
        self.apply_config_field_effect(key, &effective)?;
        std::fs::write(&path, doc.to_string())?;
        Ok(format!(
            "removed {} from {}; effective value: {}",
            key,
            path.display(),
            effective
        ))
    }

    /// Insert or update one `[[models]]` entry in one scope (`workspace` or
    /// `global`), writing the entry into that scope's TOML file while
    /// preserving all untouched content (comments, formatting, unknown keys).
    ///
    /// The edit is validated against the merged effective configuration that
    /// results from it: the id/provider must be set, the provider must exist,
    /// and the `[agent.models]` tier references must still resolve. On
    /// success the runtime re-resolves its model list and keeps the current
    /// model in sync (falling back to the performance-tier model when it was
    /// replaced).
    pub fn upsert_model_in(
        &mut self,
        scope: crate::config::ConfigScope,
        entry: crate::config::ModelEntry,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{doc_upsert_model, models_from_toml};

        if entry.id.trim().is_empty() {
            return Err("model id must not be empty".into());
        }
        if entry.provider.trim().is_empty() {
            return Err(format!("model '{}' must reference a provider", entry.id).into());
        }

        let mut doc: toml_edit::DocumentMut =
            std::fs::read_to_string(crate::config::scope_config_path(scope))
                .ok()
                .and_then(|text| text.parse().ok())
                .unwrap_or_default();
        doc_upsert_model(&mut doc, &entry)?;

        // Validate against the merged configuration that results from this
        // edit: always global-base + workspace-over, with the edited scope's
        // file (as it will be written) standing in for its side of the merge.
        let merged = merged_scope_toml(scope, &doc.to_string())?;
        let models = models_from_toml(&merged)?.unwrap_or_default();
        Self::validate_model_entries(&self.config, &models)?;
        let existed = models.iter().any(|m| m.id == entry.id);
        let path = crate::config::scope_config_path(scope);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, doc.to_string())?;

        self.refresh_models(models)?;
        let verb = if existed { "updated" } else { "added" };
        Ok(format!(
            "{} model '{}' in {}",
            verb,
            entry.id,
            path.display()
        ))
    }

    /// Remove one `[[models]]` entry (matched by `id`) from one scope. The
    /// remaining effective configuration must still satisfy
    /// [`Self::validate_model_entries`], so removing a model referenced by
    /// `[agent.models]` is rejected. On success the runtime re-resolves its
    /// model list.
    pub fn remove_model_in(
        &mut self,
        scope: crate::config::ConfigScope,
        id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use crate::config::{ConfigScope, doc_remove_model, models_from_toml, read_config_toml};

        let id = id.trim();
        if id.is_empty() {
            return Err("model id must not be empty".into());
        }
        let path = crate::config::scope_config_path(scope);
        if !path.exists() {
            return Err(format!("no config file at {}", path.display()).into());
        }
        let text = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| format!("invalid TOML in {}: {}", path.display(), e))?;
        if !doc_remove_model(&mut doc, id)? {
            return Err(format!("model '{}' is not defined in {}", id, path.display()).into());
        }

        // Validate the merged configuration that results from this removal.
        let merged = merged_scope_toml(scope, &doc.to_string())?;
        let models = models_from_toml(&merged)?.unwrap_or_default();
        Self::validate_model_entries(&self.config, &models)?;

        std::fs::write(&path, doc.to_string())?;

        self.refresh_models(models)?;
        // The id may still be effective through the other scope.
        let mut note = String::new();
        if scope == ConfigScope::Global {
            let ws = read_config_toml(&crate::config::workspace_config_path())?;
            if let Some(models) = models_from_toml(&ws)? {
                if models.iter().any(|m| m.id == id) {
                    note = "; note: the workspace config still defines this id, so it remains effective"
                        .to_string();
                }
            }
        }
        Ok(format!(
            "removed model '{}' from {}{}",
            id,
            path.display(),
            note
        ))
    }

    /// Check that a prospective effective `[[models]]` list is usable: every
    /// entry needs an id and an existing provider, and the `[agent.models]`
    /// tier references (merged config) must still match a configured model.
    fn validate_model_entries(
        config: &AppConfig,
        models: &[crate::config::ModelEntry],
    ) -> Result<(), Box<dyn std::error::Error>> {
        for model in models {
            if model.id.trim().is_empty() {
                return Err("a [[models]] entry is missing its id".into());
            }
            if config.providers.iter().all(|p| p.name != model.provider) {
                return Err(format!(
                    "model '{}' references unknown provider '{}'",
                    model.id, model.provider
                )
                .into());
            }
        }
        let matches = |reference: &str| {
            models.iter().any(|m| {
                m.id == reference || (!m.name.trim().is_empty() && m.name.trim() == reference)
            })
        };
        let performance = config.agent.models.performance.trim();
        if performance.is_empty() || !matches(performance) {
            return Err(format!(
                "[agent.models].performance '{}' does not match any configured [[models]] id or name",
                performance
            )
            .into());
        }
        if let Some(efficient) = config
            .agent
            .models
            .efficient
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !matches(efficient) {
                return Err(format!(
                    "[agent.models].efficient '{}' does not match any configured [[models]] id or name",
                    efficient
                )
                .into());
            }
        }
        Ok(())
    }

    /// Adopt a new effective `[[models]]` list into the runtime: re-resolve
    /// the model list, keep the current model in sync (or fall back to the
    /// performance-tier model when it disappeared), and persist the session.
    fn refresh_models(
        &mut self,
        models: Vec<crate::config::ModelEntry>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.config.models = models;
        self.models = self
            .config
            .resolve_models()
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        if let Some(model) = self.models.iter().find(|m| m.id == self.current_model.id) {
            self.current_model = model.clone();
        } else {
            let tiers = self
                .config
                .resolve_tier_models(&self.models)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            self.current_model = tiers.performance;
        }
        self.rebuild_client();
        // Tier-less subagent definitions resolve against the parent's current
        // model, so keep the subagent manager's snapshot in sync.
        self.subagents
            .update_parent_model(self.current_model.clone());
        self.persist_session();
        Ok(())
    }

    /// Per-scope config state for editor frontends: both config files with
    /// the values of the editable keys and the `[[models]]` entries as
    /// written in each of them.
    pub fn config_scopes(&self) -> Vec<crate::config::ConfigScopeSnapshot> {
        use crate::config::{
            ConfigScope, mcp_servers_from_toml, models_from_toml, toml_display_value,
            value_get_dotted,
        };

        [ConfigScope::Workspace, ConfigScope::Global]
            .into_iter()
            .map(|scope| {
                let path = crate::config::scope_config_path(scope);
                let exists = path.exists();
                let parsed = if exists {
                    crate::config::read_config_toml(&path).ok()
                } else {
                    None
                };
                let fields = CONFIG_FIELD_SPECS
                    .iter()
                    .map(|spec| {
                        let value = parsed
                            .as_ref()
                            .and_then(|v| value_get_dotted(v, spec.key))
                            .map(toml_display_value)
                            .unwrap_or_default();
                        (spec.key.to_string(), value)
                    })
                    .collect();
                let models = parsed
                    .as_ref()
                    .and_then(|v| models_from_toml(v).ok())
                    .flatten()
                    .unwrap_or_default();
                let mcp_servers = parsed
                    .as_ref()
                    .and_then(|v| mcp_servers_from_toml(v).ok())
                    .flatten()
                    .unwrap_or_default();
                crate::config::ConfigScopeSnapshot {
                    scope,
                    path: path.display().to_string(),
                    exists,
                    fields,
                    models,
                    mcp_servers,
                }
            })
            .collect()
    }

    /// Set a status-bar message with an optional auto-clear timeout.
    pub fn set_status_message(&mut self, msg: impl Into<String>, timeout: Option<Duration>) {
        self.status_message = msg.into();
        self.status_message_clear_at = timeout.map(|d| Instant::now() + d);
    }

    /// Set a transient status-bar message that clears after
    /// `STATUS_MESSAGE_TIMEOUT`.
    pub fn set_transient_message(&mut self, msg: impl Into<String>) {
        self.set_status_message(msg, Some(STATUS_MESSAGE_TIMEOUT));
    }

    /// Clear `status_message` if its auto-clear time has passed, also dropping
    /// an Error status back to Idle.
    pub fn maybe_clear_status_message(&mut self) {
        if let Some(clear_at) = self.status_message_clear_at {
            if Instant::now() >= clear_at {
                self.status_message.clear();
                self.status_message_clear_at = None;
                if self.status == AppStatus::Error {
                    self.status = AppStatus::Idle;
                }
            }
        }
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = AppStatus::Error;
        self.set_transient_message(msg);
    }

    /// Add a display-only event message to the history (errors, notices) and
    /// log it at the provided level.
    pub fn add_event_message(&mut self, msg: impl Into<String>) {
        let text = msg.into();
        tracing::warn!("{}", text);
        self.messages.push(Message::event(text));
        self.queue_event(RuntimeEvent::MessagesChanged);
    }

    pub fn clear_error(&mut self) {
        if self.status == AppStatus::Error {
            self.status = AppStatus::Idle;
            self.status_message.clear();
        }
    }

    /// Return the number of pending tool calls received from the current
    /// assistant response.
    pub fn pending_tool_calls_count(&self) -> usize {
        self.pending_tool_calls.len()
    }

    /// Rebuild the main agent's shell permission policy and audit logger from
    /// the configured `[shell]` section, preserving the path-permission base
    /// directory (the session workspace).
    fn rebuild_shell_policy(&mut self) {
        let (mut permissions, audit_logger) = self
            .config
            .shell
            .clone()
            .map(|s| (s.permission_policy(), s.audit_logger()))
            .unwrap_or_else(|| {
                let default = ShellConfig::default();
                (default.permission_policy(), default.audit_logger())
            });
        permissions.base_dir = self.path_base_dir();
        self.shell_state.set_permission_policy(permissions);
        self.shell_state.set_audit_logger(audit_logger);
    }

    /// The directory that is always readable and writable for the main agent:
    /// the canonicalized session workspace.
    fn path_base_dir(&self) -> Option<std::path::PathBuf> {
        std::path::Path::new(&self.session_cwd).canonicalize().ok()
    }

    /// Human-readable summary of the main agent's current shell permission
    /// policy: mode, interactively granted session tags, and path
    /// restrictions.
    pub fn permission_summary(&self) -> String {
        let policy = &self.shell_state.permissions;
        let mode = match &policy.mode {
            sutcac_sh::permissions::PermissionMode::AllowAll => "allow_all".to_string(),
            sutcac_sh::permissions::PermissionMode::Deny(deny) => format!("deny:{}", deny),
            sutcac_sh::permissions::PermissionMode::AllowOnly(allow) => format!("allow:{}", allow),
            sutcac_sh::permissions::PermissionMode::Restrict { allow, deny } => {
                format!("allow:{} deny:{}", allow, deny)
            }
        };
        let mut lines = vec![
            format!("mode: {}", mode),
            format!(
                "session grants: {}",
                if policy.session_grants.is_empty() {
                    "none".to_string()
                } else {
                    policy.session_grants.to_string()
                }
            ),
            format!(
                "path restrictions: {}",
                match policy.base_dir.as_deref() {
                    Some(base) => format!(
                        "base {}, read: {} dir(s), write: {} dir(s)",
                        base.display(),
                        policy.read_paths.len(),
                        policy.write_paths.len()
                    ),
                    None if policy.read_paths.is_empty() && policy.write_paths.is_empty() => {
                        "none".to_string()
                    }
                    None => format!(
                        "read: {} dir(s), write: {} dir(s)",
                        policy.read_paths.len(),
                        policy.write_paths.len()
                    ),
                }
            ),
        ];
        if !policy.command_permissions.is_empty() {
            lines.push(format!(
                "per-command tags: {} command(s)",
                policy.command_permissions.len()
            ));
        }
        lines.join("\n")
    }

    /// Grant permission tags to the main agent's shell for the rest of the
    /// session. `tags` is a comma-separated list (case-insensitive).
    pub fn grant_session_permissions(&mut self, tags: &str) {
        self.shell_state.permissions.grant_tag(tags);
        tracing::info!("session permissions granted via /permission: {}", tags);
    }

    /// Revoke previously granted session permission tags from the main
    /// agent's shell policy. `tags` is a comma-separated list.
    pub fn revoke_session_permissions(&mut self, tags: &str) {
        self.shell_state.permissions.revoke_grants(tags);
        tracing::info!("session permissions revoked via /permission: {}", tags);
    }

    /// Reset the main agent's shell permission policy to the configured
    /// default (drops all session grants).
    pub fn reset_session_permissions(&mut self) {
        self.rebuild_shell_policy();
        tracing::info!("session permissions reset via /permission");
    }

    /// Switch the main agent's shell permission policy to allow-all for the
    /// rest of the session. Path restrictions are kept.
    pub fn set_session_permissions_allow_all(&mut self) {
        self.shell_state.permissions.mode = sutcac_sh::permissions::PermissionMode::AllowAll;
        self.shell_state.permissions.session_grants =
            sutcac_sh::permissions::PermissionSet::empty();
        tracing::info!("session permissions switched to allow_all via /auto");
    }

    /// Return true if the per-turn tool round limit has been reached.
    pub fn is_tool_round_limit_reached(&self) -> bool {
        self.tool_rounds_this_turn >= self.max_tool_rounds
    }

    /// Return the configured maximum number of tool rounds per user turn.
    pub fn max_tool_rounds(&self) -> usize {
        self.max_tool_rounds
    }

    /// Drop any pending tool calls without executing them.
    pub fn clear_pending_tool_calls(&mut self) {
        self.pending_tool_calls.clear();
    }

    /// Return true if there is at least one pending tool call and the round
    /// limit has not been reached.
    pub fn has_pending_tool_call(&self) -> bool {
        !self.pending_tool_calls.is_empty() && !self.is_tool_round_limit_reached()
    }

    /// Execute the first pending tool call and append the result as a tool
    /// message. Returns the formatted tool result message.
    ///
    /// Dispatch is driven entirely by the `Toolbox`: the advertised tool name
    /// selects the implementation (built-in or MCP-converted).
    pub async fn run_pending_tool(&mut self) -> Option<String> {
        let call = self.pending_tool_calls.first()?.clone();

        let result = match self.toolbox.get(&call.name) {
            Some(tool) => {
                self.status = AppStatus::RunningTool;
                let description = tool.describe_call(&call);
                self.status_message = format!("Running: {}", description);
                tracing::info!(turn = self.turn_seq, tool = %call.name, "running tool: {description}");
                self.messages
                    .push(Message::event(format!("tool: {}", description)));

                let mut ctx = ToolContext {
                    shell_state: &mut self.shell_state,
                    skill_registry: &mut self.skill_registry,
                    active_skills: &mut self.active_skills,
                    todos: &mut self.todos,
                    messages: &mut self.messages,
                    toolbox: &self.toolbox,
                    agent_registry: Some(&mut self.agent_registry),
                    subagents: Some(&mut self.subagents),
                    current_agent: Some(&self.main_agent),
                    is_main_agent: true,
                };
                tool.execute(&call, &mut ctx).await
            }
            None => {
                tracing::warn!(turn = self.turn_seq, tool = %call.name, "unknown tool call");
                ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("catus: unknown tool '{}'", call.name),
                    interaction: None,
                }
            }
        };

        self.queue_event(RuntimeEvent::MessagesChanged);

        if let Some(request) = result.interaction {
            // The tool is asking the user questions: pause the turn and let
            // the frontend collect the answers; the turn resumes via
            // `complete_interaction` once they arrive. No result message is
            // pushed yet.
            tracing::info!(
                turn = self.turn_seq,
                tool = %call.name,
                "tool is waiting for user input"
            );
            self.pending_interaction = Some((call, request.questions.clone()));
            self.queue_event(RuntimeEvent::InteractionRequested(request.questions));
            return None;
        }

        let message = result.to_message();
        self.messages
            .push(Message::tool(message.clone(), call.id.clone()));
        self.pending_tool_calls.remove(0);
        self.tool_rounds_this_turn += 1;

        self.status = AppStatus::Idle;
        self.status_message.clear();
        self.queue_event(RuntimeEvent::MessagesChanged);
        self.persist_session();

        Some(message)
    }

    /// True while a tool call is paused waiting for the ask overlay.
    pub fn has_pending_interaction(&self) -> bool {
        self.pending_interaction.is_some()
    }

    /// Complete the paused tool call with the user's answers, append the tool
    /// result message, and resume the LLM turn. Returns false when there is
    /// no paused interaction (nothing was done).
    pub fn complete_interaction(&mut self, answers: Vec<AskAnswer>) -> bool {
        // An `ask_permission` call applies the user's decision to the shell
        // permission policy instead of returning raw answer JSON.
        if self
            .pending_interaction
            .as_ref()
            .is_some_and(|(call, _)| call.name == "ask_permission")
        {
            let (call, _) = self.pending_interaction.as_ref().unwrap();
            let request = parse_ask_permission_request(&call.arguments, &self.shell_state.cwd);
            let choice = answers.first().map(|a| match &a.answer {
                crate::tool::Answer::One(label) => label.as_str(),
                crate::tool::Answer::Many(labels) => {
                    labels.first().map(String::as_str).unwrap_or_default()
                }
            });
            let (status, stdout) = match (request, choice) {
                (Some(request), Some(GRANT_SESSION)) => {
                    for tag in &request.tags {
                        self.shell_state.permissions.grant_tag(tag);
                    }
                    if !request.read_paths.is_empty() || !request.write_paths.is_empty() {
                        self.shell_state.permissions.grant_paths(
                            &request.read_paths,
                            &request.write_paths,
                            &self.shell_state.cwd,
                        );
                    }
                    tracing::info!("session permissions granted: {}", request.summary());
                    (
                        0,
                        format!("granted for this session: {}", request.summary()),
                    )
                }
                (request, _) => (
                    1,
                    format!(
                        "denied: the user did not grant {}",
                        request
                            .map(|r| r.summary())
                            .unwrap_or_else(|| "(unknown request)".to_string())
                    ),
                ),
            };
            return self.finish_interaction(|call| ToolResult {
                call,
                status,
                stdout,
                stderr: String::new(),
                interaction: None,
            });
        }
        let stdout = serde_json::to_string(&answers).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize ask_user answers: {}", e);
            "[]".to_string()
        });
        self.finish_interaction(|call| ToolResult {
            call,
            status: 0,
            stdout,
            stderr: String::new(),
            interaction: None,
        })
    }

    /// Cancel the paused tool call (the user dismissed the ask overlay) and
    /// resume the LLM turn with an error result so the model can recover.
    pub fn cancel_interaction(&mut self) -> bool {
        self.finish_interaction(|call| ToolResult {
            call,
            status: 1,
            stdout: String::new(),
            stderr: "catus: user cancelled the question".to_string(),
            interaction: None,
        })
    }

    /// Shared tail of `complete_interaction` / `cancel_interaction`: append
    /// the result message, drop the paused call, and count it as a tool round.
    fn finish_interaction(&mut self, make_result: impl FnOnce(ToolCall) -> ToolResult) -> bool {
        let Some((call, _questions)) = self.pending_interaction.take() else {
            return false;
        };
        let result = make_result(call);
        let message = result.to_message();
        self.messages
            .push(Message::tool(message, result.call.id.clone()));
        self.pending_tool_calls.remove(0);
        self.tool_rounds_this_turn += 1;

        self.status = AppStatus::Idle;
        self.status_message.clear();
        self.queue_event(RuntimeEvent::MessagesChanged);
        self.persist_session();
        true
    }

    /// List available session names from the history database, newest first.
    pub fn list_session_names(&self) -> Vec<String> {
        let Some(store) = self.history_store.as_ref() else {
            return Vec::new();
        };
        match store.list_sessions(&self.session_cwd) {
            Ok(sessions) => sessions.into_iter().map(|s| s.name).collect(),
            Err(e) => {
                tracing::warn!("failed to list sessions: {}", e);
                Vec::new()
            }
        }
    }

    /// The newest `limit` sessions of the current workspace, for the web
    /// sidebar's history menu.
    pub fn list_recent_sessions(&self, limit: usize) -> Vec<crate::history::SessionSummary> {
        self.history_store
            .as_ref()
            .map(|store| {
                store
                    .list_sessions(&self.session_cwd)
                    .unwrap_or_default()
                    .into_iter()
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every session of every workspace, grouped client-side by `cwd`.
    pub fn list_all_sessions(&self) -> Vec<crate::history::SessionSummary> {
        self.history_store
            .as_ref()
            .map(|store| store.list_sessions_all().unwrap_or_default())
            .unwrap_or_default()
    }

    /// First user prompt of the current session, whitespace-collapsed and
    /// truncated; persisted with the session row so lists can hint at the
    /// conversation content.
    pub fn session_summary(&self) -> String {
        let raw = self
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate(&collapsed, 200)
    }

    /// Return a human-readable list of available history names for the status
    /// bar.
    pub fn history_names_list(&self) -> String {
        let names = self.list_session_names();
        if names.is_empty() {
            "no saved histories".to_string()
        } else {
            format!("available: {}", names.join(", "))
        }
    }

    /// Build the key/value state persisted alongside the conversation.
    fn persist_state(&self) -> std::collections::HashMap<String, String> {
        use crate::history::{
            STATE_DISABLED_AGENTS, STATE_DISABLED_MCP, STATE_DISABLED_SKILLS, STATE_MEMORY_ENABLED,
            STATE_PENDING_INTERACTION, STATE_PENDING_TOOL_CALLS, STATE_SHELL_CWD,
            STATE_SHELL_EXPORTED, STATE_SHELL_VARS, STATE_TODOS,
        };
        let mut state = std::collections::HashMap::new();
        state.insert(
            STATE_PENDING_TOOL_CALLS.to_string(),
            crate::history::tool_calls_to_json(&self.pending_tool_calls),
        );
        if let Some((call, questions)) = &self.pending_interaction {
            if let Ok(json) = serde_json::to_string(&(call, questions)) {
                state.insert(STATE_PENDING_INTERACTION.to_string(), json);
            }
        }
        state.insert(
            STATE_SHELL_CWD.to_string(),
            self.shell_state.cwd.to_string_lossy().to_string(),
        );
        if let Ok(json) = serde_json::to_string(&self.shell_state.vars) {
            state.insert(STATE_SHELL_VARS.to_string(), json);
        }
        if let Ok(json) = serde_json::to_string(&self.shell_state.exported) {
            state.insert(STATE_SHELL_EXPORTED.to_string(), json);
        }
        if let Ok(json) = serde_json::to_string(&self.todos) {
            state.insert(STATE_TODOS.to_string(), json);
        }
        state.insert(
            STATE_MEMORY_ENABLED.to_string(),
            self.memory.session_enabled.to_string(),
        );
        if let Ok(json) = serde_json::to_string(self.skill_registry.disabled_names()) {
            state.insert(STATE_DISABLED_SKILLS.to_string(), json);
        }
        if let Ok(json) = serde_json::to_string(&self.disabled_mcp.snapshot()) {
            state.insert(STATE_DISABLED_MCP.to_string(), json);
        }
        if let Ok(json) = serde_json::to_string(self.agent_registry.disabled_names()) {
            state.insert(STATE_DISABLED_AGENTS.to_string(), json);
        }
        state
    }

    /// Persist a full snapshot of the current session: main messages,
    /// subagent records and messages, metadata, and pending state.
    ///
    /// No-op when history is disabled. The session row is created lazily on
    /// the first call. Called incrementally — after every LLM turn, tool
    /// result, usage report, and subagent event — so exiting (even while a
    /// subagent is running) always leaves a complete record on disk.
    pub fn persist_session(&mut self) {
        if self.history_store.is_none() {
            return;
        }
        if self.current_session_id.is_none() {
            let store = self.history_store.as_ref().unwrap();
            let base = self.session_name.clone();
            let mut name = base.clone();
            let mut created = None;
            for attempt in 1..100u32 {
                match store.create_session(
                    &name,
                    &self.session_id,
                    &self.current_model.id,
                    &self.session_cwd,
                    &self.session_summary(),
                ) {
                    Ok(id) => {
                        created = Some(id);
                        break;
                    }
                    Err(_) => name = format!("{}-{}", base, attempt + 1),
                }
            }
            match created {
                Some(id) => {
                    self.current_session_id = Some(id);
                    self.session_name = name;
                }
                None => {
                    tracing::warn!("failed to create history session row; skipping persist");
                    return;
                }
            }
        }
        let session = self.current_session_id.unwrap();
        let meta = crate::history::SessionMeta {
            id: session,
            name: self.session_name.clone(),
            created_at: 0,
            updated_at: 0,
            session_id: self.session_id.clone(),
            model_id: self.current_model.id.clone(),
            request_count: self.request_count as u64,
            usage: self.usage,
            active_skills: self.active_skills.clone(),
            tool_rounds: self.tool_rounds_this_turn,
            summary: Some(self.session_summary()),
        };
        let subagents = self.subagents.snapshot();
        let state = self.persist_state();
        let store = self.history_store.as_ref().unwrap();
        if let Err(e) = store.save_meta(&meta) {
            tracing::warn!("failed to save session metadata: {}", e);
        }
        if let Err(e) =
            store.replace_messages(session, crate::history::MAIN_AGENT_ID, &self.messages)
        {
            tracing::warn!("failed to save conversation messages: {}", e);
        }
        if let Err(e) = store.replace_subagents(session, &subagents) {
            tracing::warn!("failed to save subagent state: {}", e);
        }
        if let Err(e) = store.replace_state(session, &state) {
            tracing::warn!("failed to save session state: {}", e);
        }
    }

    /// Process a subagent event: update managed state, inject completion
    /// notices into the parent conversation (without the result itself; the
    /// model fetches it via the `task` tool's `result` action), and resume
    /// the parent turn when an asynchronous subagent finishes.
    ///
    /// Returns `true` when the parent agent should start a new LLM stream to
    /// react to an asynchronous `task` result.
    pub fn handle_subagent_event(&mut self, event: crate::subagent::SubagentEvent) -> bool {
        use crate::subagent::SubagentEvent;
        let mut should_resume = false;
        match &event {
            SubagentEvent::Started { id } => {
                tracing::info!("subagent {} started", id);
                self.add_event_message(format!("subagent {} started", id));
            }
            SubagentEvent::StateChanged { id, state } => {
                tracing::info!("subagent {} state changed to {:?}", id, state);
            }
            SubagentEvent::Message { id, message } => {
                tracing::debug!("subagent {} message: {:?}", id, message.role);
            }
            SubagentEvent::Completed { id, result } => {
                tracing::info!("subagent {} completed", id);
                self.add_event_message(format!(
                    "subagent {} completed ({} chars)",
                    id,
                    result.len()
                ));
                // Memory passes are dispatched internally (no parent call id)
                // and resume the turn differently from `task` results.
                if self.memory.pending_recall.as_deref() == Some(id.as_str()) {
                    self.memory.pending_recall = None;
                    match crate::memory::parse_recall_result(result) {
                        Some(parsed) if parsed.recall => {
                            let memory = crate::memory::truncate_recall(&parsed.memory);
                            if !memory.trim().is_empty() {
                                self.messages.push(Message::system(format!(
                                    "Recalled memory from the Agent Memory store:\n{}",
                                    memory.trim()
                                )));
                            }
                        }
                        Some(_) => tracing::info!("memory recall pass returned recall=false"),
                        None => tracing::warn!("memory recall pass returned an unparseable result"),
                    }
                    // The user's turn was withheld for the recall pass; start
                    // the main stream with whatever was injected.
                    should_resume = true;
                } else if self.memory.pending_write.as_deref() == Some(id.as_str()) {
                    self.memory.pending_write = None;
                    match crate::memory::parse_write_result(result) {
                        Some(parsed) if parsed.written => {
                            tracing::info!("memory write pass recorded: {}", parsed.summary);
                            self.add_event_message(format!("memory updated: {}", parsed.summary));
                        }
                        Some(_) => tracing::info!("memory write pass recorded nothing"),
                        None => tracing::warn!("memory write pass returned an unparseable result"),
                    }
                } else if let Some(sub) = self.subagents.get(id) {
                    // Notify the parent that the subagent finished, without
                    // injecting the full result: the model must fetch it via
                    // the `task` tool's `result` action. `taskSync` dispatches
                    // deliver the result through the tool return instead, so
                    // they get no notice.
                    if sub.parent_call_id.is_some() && !self.subagents.is_sync(id) {
                        let call_id = sub.parent_call_id.clone().unwrap();
                        self.messages.push(Message::tool(
                            format!(
                                "status=0\nstdout=```\nsubagent {} ({}) completed; call the task tool with {{\"action\": \"result\", \"id\": \"{}\"}} to retrieve its output.\n```\nstderr=```\n\n```",
                                id, sub.name, id
                            ),
                            call_id,
                        ));
                        should_resume = self.status == AppStatus::Idle;
                    }
                }
                // A write requested while this pass was running is next.
                self.dispatch_queued_memory_write();
            }
            SubagentEvent::Error { id, error } => {
                tracing::error!("subagent {} error: {}", id, error);
                self.add_event_message(format!("subagent {} error: {}", id, error));
                // A failed memory pass degrades to "no memory" instead of
                // blocking the user's turn.
                if self.memory.pending_recall.as_deref() == Some(id.as_str()) {
                    self.memory.pending_recall = None;
                    tracing::warn!("memory recall pass failed; continuing without memory");
                    should_resume = true;
                } else if self.memory.pending_write.as_deref() == Some(id.as_str()) {
                    self.memory.pending_write = None;
                    tracing::warn!("memory write pass failed: {}", error);
                } else if let Some(sub) = self.subagents.get(id) {
                    if sub.parent_call_id.is_some() && !self.subagents.is_sync(id) {
                        let call_id = sub.parent_call_id.clone().unwrap();
                        self.messages.push(Message::tool(
                            format!(
                                "status=0\nstdout=```\nsubagent {} ({}) failed; call the task tool with {{\"action\": \"result\", \"id\": \"{}\"}} to retrieve the error details.\n```\nstderr=```\n\n```",
                                id, sub.name, id
                            ),
                            call_id,
                        ));
                        should_resume = self.status == AppStatus::Idle;
                    }
                }
                self.dispatch_queued_memory_write();
            }
        }
        self.subagents.handle_event(&event);
        self.persist_session();
        should_resume
    }

    /// Resume a saved session. With no `name`, returns the list of available
    /// histories. With a name, loads the full snapshot from the history
    /// database: messages, token usage, model, provider session id, active
    /// skills, pending tool calls / interactions, shell state, and subagents
    /// (still-running ones restart their turn loop from the saved messages).
    pub async fn resume_history(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.history_names_list());
        };
        let Some(store) = self.history_store.as_ref() else {
            return Err("session history is unavailable".into());
        };
        let id = store
            .find_session(name, &self.session_cwd)?
            .ok_or_else(|| format!("history not found: {}", name))?;
        let snapshot = store
            .load_session(id)?
            .ok_or_else(|| format!("history not found: {}", name))?;

        // Session-scoped disable sets are restored before the system prompt
        // rebuild so the skill catalog reflects them.
        if let Some(names) = snapshot
            .state
            .get(crate::history::STATE_DISABLED_SKILLS)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.skill_registry.set_disabled_names(names);
        }
        if let Some(names) = snapshot
            .state
            .get(crate::history::STATE_DISABLED_MCP)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.disabled_mcp.replace(names);
        }
        if let Some(names) = snapshot
            .state
            .get(crate::history::STATE_DISABLED_AGENTS)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.agent_registry.set_disabled_names(names);
        }

        // Reset to the configured system prompt, then load the saved messages.
        let system_prompt =
            Self::build_system_prompt(&self.main_agent, &self.config, &self.skill_registry);
        self.messages = vec![Message::system(system_prompt)];
        let mut loaded = snapshot.messages;

        // Drop any leading system messages so the configured system prompt
        // remains authoritative, and drop empty assistant placeholders left
        // by interrupted streams.
        while let Some(first) = loaded.first() {
            if first.is_system() {
                loaded.remove(0);
            } else {
                break;
            }
        }
        loaded
            .retain(|m| !(m.role == Role::Assistant && m.content.is_empty() && !m.had_tool_calls));
        self.messages.extend(loaded);

        // Session metadata.
        self.usage = snapshot.meta.usage;
        self.request_count = snapshot.meta.request_count as usize;
        self.active_skills = snapshot.meta.active_skills.clone();
        self.tool_rounds_this_turn = snapshot.meta.tool_rounds;

        // Model and provider session id: keep one stable session across the
        // process restart so providers with a session header stay connected.
        if !snapshot.meta.session_id.is_empty() {
            self.session_id = snapshot.meta.session_id.clone();
        }
        if let Some(model) = self.models.iter().find(|m| m.id == snapshot.meta.model_id) {
            self.current_model = model.clone();
        } else if !snapshot.meta.model_id.is_empty() {
            tracing::warn!(
                "saved model '{}' is not configured; keeping the current model",
                snapshot.meta.model_id
            );
        }
        self.client = LlmClient::new(
            self.current_model.provider.clone(),
            self.current_model.clone(),
            &self.session_id,
        );
        // Restored subagents resolve their tier-less model against this
        // snapshot, so it must match the restored session model.
        self.subagents
            .update_parent_model(self.current_model.clone());

        // Pending tool calls: re-run them so the assistant's tool_calls keep
        // matching tool results in the conversation. task/taskSync dispatches
        // are not re-run — their subagents are restored separately below.
        self.pending_tool_calls = snapshot
            .state
            .get(crate::history::STATE_PENDING_TOOL_CALLS)
            .map(|j| crate::history::tool_calls_from_json(j))
            .unwrap_or_default();
        let restored_calls = std::mem::take(&mut self.pending_tool_calls);
        for call in restored_calls {
            if call.name == "task" || call.name == "taskSync" {
                self.messages.push(Message::tool(
                    "status=1\nstdout=```\n\n```\nstderr=```\ncatus: task dispatch was \
                     interrupted by an exit; the subagent has been restored separately\n```",
                    call.id,
                ));
                self.tool_rounds_this_turn += 1;
            } else {
                self.pending_tool_calls.push(call);
                self.run_pending_tool().await;
            }
        }

        // A paused ask_user interaction is re-raised as an event; the turn
        // continues once the user answers.
        self.pending_interaction = snapshot
            .state
            .get(crate::history::STATE_PENDING_INTERACTION)
            .and_then(|j| serde_json::from_str(j).ok());
        if let Some((_, questions)) = &self.pending_interaction {
            self.queue_event(RuntimeEvent::InteractionRequested(questions.clone()));
        }

        // Shell working directory and variables.
        if let Some(cwd) = snapshot.state.get(crate::history::STATE_SHELL_CWD) {
            let path = std::path::PathBuf::from(cwd);
            if path.is_dir() {
                self.shell_state.cwd = path;
            }
        }
        if let Some(vars) = snapshot
            .state
            .get(crate::history::STATE_SHELL_VARS)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.shell_state.vars = vars;
        }
        if let Some(exported) = snapshot
            .state
            .get(crate::history::STATE_SHELL_EXPORTED)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.shell_state.exported = exported;
        }

        // Main agent's TODO list.
        if let Some(todos) = snapshot
            .state
            .get(crate::history::STATE_TODOS)
            .and_then(|j| serde_json::from_str(j).ok())
        {
            self.todos = todos;
        }

        // Session-level memory toggle. Interrupted memory passes are not
        // restarted; they degrade to "no memory" for the resumed turn.
        if let Some(enabled) = snapshot
            .state
            .get(crate::history::STATE_MEMORY_ENABLED)
            .and_then(|s| s.parse::<bool>().ok())
        {
            self.memory.session_enabled = enabled;
        }
        self.memory.pending_recall = None;
        self.memory.pending_write = None;
        self.memory.write_queued = false;

        // Subagents: terminal records restore as-is; still-running ones
        // restart their turn loop from the saved messages.
        let parent_shell = self.shell_state.clone();
        let parent_skills = self.active_skills.clone();
        let parent_registry = self.skill_registry.clone();
        let mut restore_errors = Vec::new();
        for sub in &snapshot.subagents {
            if let Err(e) = self.subagents.restore(
                sub,
                &self.agent_registry,
                &parent_shell,
                &parent_skills,
                &parent_registry,
            ) {
                restore_errors.push(e);
            }
        }
        for error in restore_errors {
            self.add_event_message(error);
        }

        self.current_session_id = Some(id);
        // Keep the loaded session's name so subsequent persists update the
        // same row instead of renaming it.
        self.session_name = snapshot.meta.name.clone();
        self.queue_event(RuntimeEvent::MessagesChanged);
        Ok("session resumed".to_string())
    }

    /// Persist the current session and start a fresh conversation context
    /// (`/new`).
    ///
    /// With `path`, the process fully switches workspace first: config,
    /// agent definitions, skills, MCP servers, and the session cwd are
    /// reloaded from the target directory (mirroring a `-w` restart) while
    /// the process stays alive. Without `path`, only the conversation state
    /// resets; registries are re-discovered so on-disk changes are picked up.
    ///
    /// Refuses to run while anything is in flight (stream, pending tool
    /// calls or interaction, memory pass, running subagents).
    pub async fn start_new_session(
        &mut self,
        path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        if self.status == AppStatus::Streaming || self.status == AppStatus::RunningTool {
            return Err(
                "cannot start a new session while a turn is in progress; wait for it to finish"
                    .into(),
            );
        }
        if self.pending_interaction.is_some() {
            return Err("cannot start a new session while a question is pending".into());
        }
        if !self.pending_tool_calls.is_empty() {
            return Err("cannot start a new session while tool calls are pending".into());
        }
        if self.memory.pending_recall.is_some()
            || self.memory.pending_write.is_some()
            || self.memory.write_queued
        {
            return Err("cannot start a new session while a memory pass is running".into());
        }
        if self.subagents.running_count() > 0 {
            return Err(
                "cannot start a new session while subagents are running; close them with /agent close"
                    .into(),
            );
        }

        // Finalize the old session under the old workspace before anything
        // changes.
        self.persist_session();

        let mut switched_workspace = false;
        if let Some(path) = path.map(str::trim).filter(|s| !s.is_empty()) {
            let resolved = std::path::Path::new(path)
                .canonicalize()
                .map_err(|e| format!("cannot use workspace '{}': {}", path, e))?;
            if !resolved.is_dir() {
                return Err(
                    format!("workspace '{}' is not a directory", resolved.display()).into(),
                );
            }
            // Pre-validate that the target workspace provides a main agent so
            // a failed switch leaves the process untouched. The candidate
            // workspace path replaces the cwd-derived default search path.
            let mut search = vec![resolved.join(".sutcac").join("agents")];
            search.extend(AgentRegistry::default_paths().into_iter().skip(1));
            let registry = AgentRegistry::discover(&search)
                .map_err(|e| format!("agent discovery failed in {}: {}", resolved.display(), e))?;
            if registry.get("main").is_none() {
                return Err(format!(
                    "workspace '{}' has no .sutcac/agents/main.md; create it before switching",
                    resolved.display()
                )
                .into());
            }

            std::env::set_current_dir(&resolved)
                .map_err(|e| format!("cannot enter workspace '{}': {}", resolved.display(), e))?;
            let mut config = AppConfig::load()
                .map_err(|e| format!("failed to load config in new workspace: {}", e))?;
            // Storage directories (history, memory) stay at the fixed XDG
            // locations; only workspace-resolved settings are reloaded.
            config.dirs = self.config.dirs.clone();
            self.config = config;
            self.config_path = AppConfig::config_save_path();

            let models = match self.config.resolve_models() {
                Ok(models) => models,
                Err(e) => {
                    tracing::warn!(
                        "no usable model configuration in new workspace ({}); using placeholder",
                        e
                    );
                    vec![Model::default()]
                }
            };
            self.models = models;
            // Keep the current model when the new workspace still configures
            // it; otherwise fall back to the first configured model.
            self.current_model = self
                .models
                .iter()
                .find(|m| m.id == self.current_model.id)
                .cloned()
                .or_else(|| self.models.first().cloned())
                .unwrap_or_default();

            let (agent_registry, main_agent, main_agent_from_file) =
                Self::load_main_agent(&self.config);
            self.agent_registry = agent_registry;
            self.main_agent = main_agent;
            self.main_agent_from_file = main_agent_from_file;
            self.rebuild_client();
            switched_workspace = true;
        }

        // Fresh session identity: the next persist creates a new history row.
        self.session_id = new_session_id();
        self.session_name = new_session_name();
        self.current_session_id = None;
        self.session_cwd = session_cwd();
        self.shell_state.cwd = std::path::PathBuf::from(&self.session_cwd);

        // Re-discover workspace skills; session disable flags reset.
        let mut search_paths = SkillRegistry::default_paths();
        if let Some(extra) = &self.config.agent.skill_paths {
            search_paths.extend(extra.iter().cloned());
        }
        self.skill_registry = SkillRegistry::discover(&search_paths).unwrap_or_else(|e| {
            tracing::warn!("failed to discover skills: {}", e);
            SkillRegistry::new()
        });
        self.agent_registry.set_disabled_names(Vec::new());
        self.disabled_mcp.clear();

        // Reset turn and session state.
        self.usage = Usage::default();
        self.request_count = 0;
        self.tool_rounds_this_turn = 0;
        self.pending_tool_calls = Vec::new();
        self.pending_interaction = None;
        self.active_skills = Vec::new();
        self.todos = TodoList::new();

        // Rebuild the shell policy from the config + main agent permission,
        // which also drops session permission grants and re-bases path
        // permissions on the (possibly new) workspace.
        let shell_perm = self
            .main_agent
            .permission
            .clone()
            .or_else(|| self.config.shell.as_ref().and_then(|s| s.perm_mode.clone()));
        let mut shell_config = self.config.shell.clone().unwrap_or_default();
        shell_config.perm_mode = shell_perm;
        let (permissions, audit_logger) = (
            shell_config.permission_policy(),
            shell_config.audit_logger(),
        );
        self.shell_state.set_permission_policy(permissions);
        self.shell_state.set_audit_logger(audit_logger);
        self.shell_state.permissions.base_dir = self.path_base_dir();

        // Fresh subagent manager with the reset parent snapshot.
        let tier_models = self
            .config
            .resolve_tier_models(&self.models)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "invalid tier model configuration ({}); tiers fall back to the current model",
                    e
                );
                TierModels {
                    performance: self.current_model.clone(),
                    efficient: self.current_model.clone(),
                }
            });
        self.subagents = SubagentManager::new(
            self.toolbox.clone(),
            tier_models,
            self.current_model.clone(),
            self.config.clone(),
            None,
        );

        // Memory subsystem: the session toggle returns to the config default.
        self.memory = crate::memory::MemoryState::init(&self.config, &self.agent_registry);

        // New conversation with a system prompt honoring the fresh registry.
        let system_prompt =
            Self::build_system_prompt(&self.main_agent, &self.config, &self.skill_registry);
        self.messages = vec![Message::system(system_prompt)];

        if switched_workspace {
            self.mcp_manager = None;
            self.connect_mcp().await;
        } else {
            self.rebuild_toolbox();
        }

        self.queue_event(RuntimeEvent::MessagesChanged);
        Ok(format!(
            "started a new session (workspace: {})",
            self.session_cwd
        ))
    }

    /// Handle a slash command. Returns the outcome describing how the input
    /// was treated; presentation intents are carried in
    /// `CommandOutcome::ui`.
    pub async fn handle_command(&mut self, input: &str) -> CommandOutcome {
        commands::handle_command(self, input).await
    }

    /// Process a streaming event from the LLM worker.
    pub fn handle_stream_event(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Text(text) => self.append_stream_text(&text),
            StreamEvent::Reasoning(text) => self.append_stream_reasoning(&text),
            StreamEvent::ToolCall(call) => self.add_tool_call(call),
            StreamEvent::Usage(usage) => self.record_usage(&usage),
        }
    }

    /// Start an async LLM stream.
    ///
    /// Events flow through the runtime's internal stream channel; the stream
    /// task signals completion through the internal done channel.
    pub async fn start_llm_stream(&mut self) {
        // Snapshot the conversation *before* adding the assistant placeholder so
        // the API request never contains an empty assistant message.
        let messages = self.messages.clone();
        let tools = self.toolbox.definitions();
        self.start_assistant_message();
        let client = self.client.clone();
        let event_tx = self.stream_tx.clone();
        let done_tx = self.done_tx.clone();
        let turn = self.turn_seq;

        // The stream task outlives this call; carry the span into it so the
        // LLM request, stream parsing, and usage events stay nested under the
        // same trace as the rest of the turn.
        let span = tracing::info_span!(
            "llm_stream_task",
            turn,
            model = %client.model_id(),
        );
        tokio::spawn(
            async move {
                let result = client.stream_chat(&messages, &tools, event_tx).await;
                let _ = done_tx.send(result).await;
            }
            .instrument(span),
        );
    }

    /// Handle the completion of an LLM stream.
    ///
    /// Runs any pending tool calls and returns the phase the runtime should
    /// drive next: a follow-up request, a pause for user interaction, or the
    /// end of the turn.
    pub async fn handle_llm_done(&mut self, result: Result<(), LlmError>) -> TurnPhase {
        match result {
            Ok(()) => {
                self.finish_stream();
                if self.has_pending_tool_call() {
                    tracing::info!(
                        turn = self.turn_seq,
                        count = self.pending_tool_calls_count(),
                        "pending tool call(s); running tool"
                    );
                    self.run_pending_tool().await;
                    if self.pending_interaction.is_some() {
                        // The tool turned into an interactive question; the
                        // frontend collects the answer and the turn resumes
                        // once it is submitted.
                        return TurnPhase::PausedInteraction;
                    }
                    self.persist_session();
                    TurnPhase::ContinueStream
                } else if self.pending_tool_calls_count() > 0 {
                    let count = self.pending_tool_calls_count();
                    if self.is_tool_round_limit_reached() {
                        let msg = format!(
                            "Reached max tool rounds ({}) for this turn; {} pending tool call(s) ignored.",
                            self.max_tool_rounds(),
                            count
                        );
                        tracing::warn!("{}", msg);
                        self.add_event_message(msg);
                    }
                    self.clear_pending_tool_calls();
                    self.persist_session();
                    TurnPhase::Complete
                } else {
                    tracing::info!(turn = self.turn_seq, "no pending tool call; turn complete");
                    // Turn finished: hand the turn over to the memory
                    // subagent's summarize/write pass (forks the main
                    // context, runs in background; queued when busy).
                    self.maybe_dispatch_memory_write();
                    self.persist_session();
                    TurnPhase::Complete
                }
            }
            Err(e) => {
                // Remove the empty assistant placeholder so a failed request does
                // not leave an invalid assistant message in the conversation.
                if self.has_empty_assistant_placeholder() {
                    self.messages.pop();
                }
                tracing::error!(turn = self.turn_seq, error = %e, "llm stream error");
                self.add_event_message(format!("LLM request failed: {}", e));
                self.set_error("LLM request failed".to_string());
                self.persist_session();
                TurnPhase::Complete
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn test_config_with_history_dir(dir: &std::path::Path) -> AppConfig {
        AppConfig {
            providers: vec![crate::llm::Provider {
                name: "test".to_string(),
                base_url: "https://example.com".to_string(),
                api_key: "test".to_string(),
                session_header: None,
            }],
            models: vec![crate::config::ModelEntry {
                id: "test-model".to_string(),
                name: "Test Model".to_string(),
                context_window: 4096,
                provider: "test".to_string(),
            }],
            agent: crate::config::AgentConfig {
                max_tool_rounds: 5,
                log_level: "info".to_string(),
                skill_paths: None,
                auto_include_skills: false,
                agent_paths: None,
                models: crate::config::TierModelConfig {
                    performance: "test-model".to_string(),
                    efficient: None,
                },
                memory: crate::config::MemoryConfig {
                    enabled: false,
                    auto_recall: true,
                    auto_write: true,
                },
            },
            shell: None,
            mcp: None,
            dirs: crate::config::AppDirs {
                history: Some(dir.to_path_buf()),
                memory: dir.join("memory-book"),
            },
        }
    }

    #[test]
    fn system_prompt_contains_agent_body_and_skills() {
        let dir = std::env::temp_dir().join(format!("catus_sysprompt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo-skill")).unwrap();
        std::fs::write(
            dir.join("demo-skill/SKILL.md"),
            "---\nname: demo-skill\ndescription: A demo skill.\n---\nDo demo things.\n",
        )
        .unwrap();
        let registry = SkillRegistry::discover(&[dir.clone()]).unwrap();

        let mut config = test_config_with_history_dir(&dir);
        config.agent.auto_include_skills = true;
        let main_agent = AgentDefinition {
            name: "main".to_string(),
            description: "Test".to_string(),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: None,
            skills: Vec::new(),
            role: Some(AgentRole::Main),
            body: "test prompt".to_string(),
            source_path: std::path::PathBuf::new(),
        };
        let prompt = App::build_system_prompt(&main_agent, &config, &registry);

        // Main agent body comes first, then the skill catalog; no tool section.
        let base_end = prompt
            .find("The following Agent Skills are available")
            .unwrap();
        assert!(prompt[..base_end].contains("test prompt"));
        assert!(!prompt.contains("tools are available"));
        assert!(prompt[base_end..].contains("- demo-skill: A demo skill."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_demo_skill(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("skills");
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nDo demo things.",
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn session_summary_and_disabled_toggles_persist_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = write_demo_skill(dir.path());

        let mut app = App::new(test_config_with_history_dir(dir.path()));
        app.skill_registry = SkillRegistry::discover(&[skills_dir.clone()]).unwrap();
        app.config.agent.auto_include_skills = true;
        let prompt = App::build_system_prompt(&app.main_agent, &app.config, &app.skill_registry);
        app.messages[0] = Message::system(prompt);

        // The first user prompt becomes the session summary.
        app.submit_user_message("summarize this unique prompt 42".to_string());
        assert!(
            app.session_summary()
                .contains("summarize this unique prompt 42")
        );

        // The disable flags persist as session state.
        assert!(app.handle_command("/skill disable demo").await.handled);
        assert!(app.skill_registry.is_disabled("demo"));
        // /new starts a fresh context and clears the flags.
        app.handle_command("/new").await;
        assert!(!app.skill_registry.is_disabled("demo"));
        app.skill_registry.set_disabled("demo", true);
        app.persist_session();

        let name = app.list_session_names().into_iter().next().unwrap();

        // A fresh app resumes the session with the flags intact.
        let mut app2 = App::new(test_config_with_history_dir(dir.path()));
        app2.skill_registry = SkillRegistry::discover(&[skills_dir.clone()]).unwrap();
        app2.config.agent.auto_include_skills = true;
        app2.resume_history(Some(&name)).await.unwrap();
        assert!(app2.skill_registry.is_disabled("demo"));
        // The prompt keeps listing the disabled skill (prefix stability).
        assert!(app2.messages[0].content.contains("- demo:"));

        // The persisted summary is available through the store listing.
        let summaries = app2
            .history_store
            .as_ref()
            .unwrap()
            .list_sessions_all()
            .unwrap();
        assert!(
            summaries
                .iter()
                .any(|s| s.summary.contains("summarize this unique prompt 42"))
        );
    }

    #[tokio::test]
    async fn start_new_session_guards_in_flight_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(test_config_with_history_dir(dir.path()));
        app.status = AppStatus::RunningTool;
        assert!(app.start_new_session(None).await.is_err());

        app.status = AppStatus::Idle;
        app.pending_tool_calls.push(ToolCall {
            id: "call-1".to_string(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
        });
        assert!(app.start_new_session(None).await.is_err());

        app.pending_tool_calls.clear();
        let msg = app.start_new_session(None).await.unwrap();
        assert!(msg.contains("started a new session"));
        assert_eq!(app.messages.len(), 1);
        assert!(app.current_session_id.is_none());
    }

    #[test]
    fn agent_catalog_reports_roles_and_editability() {
        let dir = std::env::temp_dir().join(format!("catus_agent_catalog_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.md"),
            "---\nname: main\ndescription: main\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            dir.join("memory.md"),
            "---\nname: memory\ndescription: memory\nrole: memory\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            dir.join("coder.md"),
            "---\nname: coder\ndescription: coder\n---\nBody",
        )
        .unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.agent_registry = AgentRegistry::discover(&[dir.clone()]).unwrap();

        let catalog = app.agent_catalog();
        let by_name = |name: &str| catalog.iter().find(|a| a.name == name).unwrap().clone();
        assert!(by_name("main").role.as_deref() == Some("main"));
        assert!(!by_name("main").editable);
        assert!(!by_name("memory").editable);
        assert!(by_name("coder").role.is_none());
        assert!(by_name("coder").editable);

        // Disabling shows up in the catalog; special roles are refused.
        app.set_agent_enabled("coder", false).unwrap();
        assert!(
            app.agent_catalog()
                .iter()
                .find(|a| a.name == "coder")
                .unwrap()
                .disabled
        );
        assert!(app.set_agent_enabled("memory", false).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_skills_when_empty_or_disabled() {
        let dir = std::env::temp_dir().join(format!("catus_sysprompt_min_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = test_config_with_history_dir(&dir);
        let main_agent = AgentDefinition {
            name: "main".to_string(),
            description: "Test".to_string(),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: None,
            skills: Vec::new(),
            role: Some(AgentRole::Main),
            body: "test prompt".to_string(),
            source_path: std::path::PathBuf::new(),
        };
        let prompt = App::build_system_prompt(&main_agent, &config, &SkillRegistry::new());
        assert_eq!(prompt, "test prompt");

        let registry = SkillRegistry::discover(&[dir.clone()]).unwrap();
        let prompt = App::build_system_prompt(&main_agent, &config, &registry);
        assert!(!prompt.contains("Agent Skills"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Seed a session row with messages directly in the store under test.
    /// The row records the test process's working directory so the `App`
    /// under test lists it.
    fn seed_session(
        dir: &std::path::Path,
        name: &str,
        messages: &[Message],
    ) -> crate::history::SessionStore {
        let store = crate::history::SessionStore::open(dir).unwrap();
        let id = store
            .create_session(name, "catus-seed", "test-model", &session_cwd(), "")
            .unwrap();
        store
            .replace_messages(id, crate::history::MAIN_AGENT_ID, messages)
            .unwrap();
        store
    }

    #[tokio::test]
    async fn resume_lists_available_histories() {
        let dir = std::env::temp_dir().join(format!("catus_resume_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        seed_session(&dir, "alpha", &[Message::user("hello")]);

        let app = App::new(test_config_with_history_dir(&dir));
        let list = app.history_names_list();
        assert!(
            list.contains("alpha"),
            "list should contain alpha: {}",
            list
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_loads_history_and_sets_current_session() {
        let dir = std::env::temp_dir().join(format!("catus_resume_load_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = seed_session(
            &dir,
            "session",
            &[Message::user("previous"), Message::assistant("ok")],
        );
        let id = store
            .find_session("session", &session_cwd())
            .unwrap()
            .unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.current_session_id.is_none());

        app.resume_history(Some("session")).await.unwrap();
        assert_eq!(app.current_session_id, Some(id));
        assert!(
            app.messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "previous")
        );
        assert!(
            app.messages
                .iter()
                .any(|m| m.role == Role::Assistant && m.content == "ok")
        );
        assert_eq!(app.messages.first().unwrap().role, Role::System);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persist_updates_loaded_session() {
        let dir = std::env::temp_dir().join(format!("catus_resume_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = seed_session(&dir, "existing", &[Message::user("old")]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.resume_history(Some("existing")).await.unwrap();
        app.messages.push(Message::user("new"));
        app.persist_session();

        let id = store
            .find_session("existing", &session_cwd())
            .unwrap()
            .unwrap();
        let snap = store.load_session(id).unwrap().unwrap();
        assert!(snap.messages.iter().any(|m| m.content == "new"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn todo_list_survives_resume() {
        let dir = std::env::temp_dir().join(format!("catus_resume_todo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Drive the todo tool through the normal dispatch path so persistence
        // happens exactly as it does in production.
        let mut app = App::new(test_config_with_history_dir(&dir));
        app.pending_tool_calls.push(ToolCall {
            id: "call-todo-1".to_string(),
            name: "todo".to_string(),
            arguments: r#"{"action":"add","items":["write lexer","write parser"]}"#.to_string(),
        });
        app.run_pending_tool().await;
        app.pending_tool_calls.push(ToolCall {
            id: "call-todo-2".to_string(),
            name: "todo".to_string(),
            arguments: r#"{"action":"complete","id":1}"#.to_string(),
        });
        app.run_pending_tool().await;
        assert!(app.todos.items()[0].done);
        assert_eq!(app.todos.items()[1].text, "write parser");
        let name = app.session_name.clone();

        // A fresh process resumes the session and finds the same list.
        let mut app2 = App::new(test_config_with_history_dir(&dir));
        app2.resume_history(Some(&name)).await.unwrap();
        assert_eq!(app2.todos, app.todos);
        assert!(app2.todos.items()[0].done);
        assert!(!app2.todos.items()[1].done);

        // The tool stays usable after the resume and keeps ids stable.
        app2.pending_tool_calls.push(ToolCall {
            id: "call-todo-3".to_string(),
            name: "todo".to_string(),
            arguments: r#"{"action":"add","items":["write exec"]}"#.to_string(),
        });
        app2.run_pending_tool().await;
        assert_eq!(app2.todos.items()[2].id, 3);
        assert!(app2.todos.items()[2].text == "write exec");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persist_creates_timestamped_session_row_for_new_session() {
        let dir = std::env::temp_dir().join(format!("catus_resume_new_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.current_session_id.is_none());
        app.persist_session();
        assert!(app.current_session_id.is_some());
        assert!(dir.join("sessions.db").exists());

        let store = crate::history::SessionStore::open(&dir).unwrap();
        let names = store.list_sessions(&session_cwd()).unwrap();
        assert_eq!(names.len(), 1);
        assert!(names[0].name.starts_with("history_"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_restores_metadata_and_model() {
        let dir = std::env::temp_dir().join(format!("catus_resume_meta_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::user("hello"));
        app.usage = crate::llm::Usage {
            prompt_tokens: 120,
            completion_tokens: 60,
            total_tokens: 180,
            cached_tokens: 30,
        };
        app.request_count = 4;
        app.active_skills.push("demo".to_string());
        app.tool_rounds_this_turn = 2;
        let saved_session_id = app.session_id.clone();
        app.persist_session();
        let name = {
            let store = app.history_store.as_ref().unwrap();
            store.list_sessions(&session_cwd()).unwrap()[0].name.clone()
        };

        // A fresh app resumes the persisted session.
        let mut app = App::new(test_config_with_history_dir(&dir));
        assert_ne!(app.session_id, saved_session_id);
        app.resume_history(Some(&name)).await.unwrap();

        assert_eq!(app.usage.prompt_tokens, 120);
        assert_eq!(app.usage.total_tokens, 180);
        assert_eq!(app.request_count, 4);
        assert_eq!(app.active_skills, vec!["demo".to_string()]);
        assert_eq!(app.tool_rounds_this_turn, 2);
        // The provider session id is restored for session-header continuity.
        assert_eq!(app.session_id, saved_session_id);
        assert_eq!(app.current_model.id, "test-model");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_empty_assistant_placeholder_detects_empty_assistant() {
        let dir = std::env::temp_dir().join(format!("catus_placeholder_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant(String::new()));
        assert!(app.has_empty_assistant_placeholder());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_stream_drops_empty_assistant_placeholder() {
        let dir = std::env::temp_dir().join(format!("catus_drop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::user("hello"));
        app.messages.push(Message::assistant(String::new()));
        app.finish_stream();

        assert!(
            !app.messages.iter().any(|m| m.role == Role::Assistant),
            "empty assistant placeholder should be removed"
        );
        assert!(app.messages.iter().any(|m| m.is_event()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_stream_keeps_assistant_with_content() {
        let dir = std::env::temp_dir().join(format!("catus_keep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant("hello".to_string()));
        app.finish_stream();

        assert_eq!(app.messages.len(), 2); // system + assistant
        assert_eq!(app.messages.last().unwrap().content, "hello");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_stream_keeps_assistant_with_tool_calls() {
        let dir = std::env::temp_dir().join(format!("catus_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"pwd"}"#.to_string(),
        });
        app.finish_stream();

        assert_eq!(app.messages.len(), 2); // system + assistant
        let last = app.messages.last().unwrap();
        assert!(last.is_assistant());
        assert!(last.had_tool_calls);
        assert_eq!(last.tool_calls.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_prefix_is_byte_stable_across_tool_rounds() {
        let dir = std::env::temp_dir().join(format!("catus_prefix_round_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages[0] = Message::system("test system prompt".to_string());

        app.submit_user_message("check the todo list".to_string());
        let round1 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round1.len(), 2); // system + user

        // Stream a tool call, then run it through the real dispatch path.
        app.start_assistant_message();
        app.append_stream_text("Let me check.");
        app.add_tool_call(ToolCall {
            id: "call_1".to_string(),
            name: "todo".to_string(),
            arguments: r#"{"action":"list"}"#.to_string(),
        });
        app.run_pending_tool().await;

        let round2 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round2.len(), 4); // + assistant(tool_calls) + tool result
        assert!(
            round2[..round1.len()] == round1[..],
            "the earlier request must be a byte-stable prefix of the later one"
        );

        // A follow-up assistant-only round keeps extending without rewrite.
        app.start_assistant_message();
        app.append_stream_text("done");
        let round3 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round3.len(), 5); // + final assistant reply
        assert!(
            round3[..round2.len()] == round2[..],
            "the tool-round request must be a byte-stable prefix of the final one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_prefix_is_byte_stable_across_interaction_pause() {
        let dir = std::env::temp_dir().join(format!("catus_prefix_ask_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages[0] = Message::system("test system prompt".to_string());
        app.submit_user_message("ask me something".to_string());
        let round1 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round1.len(), 2); // system + user

        // The assistant calls ask_user; the turn pauses for the overlay.
        app.start_assistant_message();
        app.add_tool_call(ToolCall {
            id: "call_ask".to_string(),
            name: "ask_user".to_string(),
            arguments: r#"{"questions":[{"prompt":"Pick","title":"Pick","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}"#.to_string(),
        });
        let round1 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round1.len(), 3); // system + user + assistant(tool_calls)

        app.run_pending_tool().await;
        assert!(app.has_pending_interaction());

        // The user answers; the tool result is appended and the turn resumes.
        assert!(app.complete_interaction(vec![crate::tool::AskAnswer {
            prompt: "Pick".to_string(),
            answer: crate::tool::Answer::One("a".to_string()),
        }]));
        let round2 = crate::llm::api_request_messages(&app.messages);
        assert_eq!(round2.len(), 4); // + tool result
        assert!(round2[..round1.len()] == round1[..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_session_id_uses_platform_session_format() {
        let a = new_session_id();
        let b = new_session_id();
        assert!(a.starts_with("ses_"), "{}", a);
        assert_ne!(a, b);
    }

    #[test]
    fn auto_include_skills_toggle_notes_prefix_invalidation() {
        use crate::skills::SkillRegistry;
        let dir = std::env::temp_dir().join(format!("catus_auto_skills_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nDo the demo thing.",
        )
        .unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.skill_registry = SkillRegistry::discover(&[dir.clone()]).unwrap();

        // Enabling appends the catalog to messages[0]: a real prefix change.
        app.apply_config_field_effect("agent.auto_include_skills", "true")
            .unwrap();
        assert!(app.messages[0].content.contains("- demo:"));
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("prompt-prefix cache")),
            "a changed system prompt must surface the cache-invalidation notice"
        );

        // Writing the same value again must not add another notice.
        app.apply_config_field_effect("agent.auto_include_skills", "true")
            .unwrap();
        let notices = app
            .messages
            .iter()
            .filter(|m| m.is_event() && m.content.contains("prompt-prefix cache"))
            .count();
        assert_eq!(notices, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_filters_empty_assistant_placeholders() {
        let dir = std::env::temp_dir().join(format!("catus_load_filter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        seed_session(
            &dir,
            "bad",
            &[
                Message::system("old".to_string()),
                Message::user("hello".to_string()),
                Message::assistant(String::new()),
                Message::event("LLM request failed".to_string()),
            ],
        );

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.resume_history(Some("bad")).await.unwrap();

        assert!(
            !app.messages.iter().any(|m| m.role == Role::Assistant),
            "empty assistant placeholder should be filtered from loaded history"
        );
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_user() && m.content == "hello")
        );
        assert!(app.messages.iter().any(|m| m.is_event()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_usage_accumulates_session_totals() {
        let dir = std::env::temp_dir().join(format!("catus_usage_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert_eq!(app.usage.prompt_tokens, 0);

        app.record_usage(&Usage {
            prompt_tokens: 19,
            completion_tokens: 13,
            total_tokens: 32,
            cached_tokens: 12,
        });
        app.record_usage(&Usage {
            prompt_tokens: 100,
            completion_tokens: 5,
            total_tokens: 105,
            cached_tokens: 0,
        });

        assert_eq!(app.request_count, 2);
        // Prompt tokens accumulate; context size is tracked via totals.
        assert_eq!(app.usage.prompt_tokens, 119);
        assert_eq!(app.usage.completion_tokens, 18);
        assert_eq!(app.usage.cached_tokens, 12);
        assert_eq!(
            app.usage.total_tokens,
            app.usage.prompt_tokens + app.usage.completion_tokens
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_command_opens_overlay_with_usage() {
        let dir = std::env::temp_dir().join(format!("catus_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.record_usage(&Usage {
            prompt_tokens: 19,
            completion_tokens: 13,
            total_tokens: 32,
            cached_tokens: 12,
        });
        let outcome = app.handle_command("/status").await;
        assert!(outcome.handled);
        assert_eq!(app.usage.prompt_tokens, 19);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_with_name_argument_loads_directly_without_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_resume_arg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_session(&dir, "foo", &[]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        let outcome = app.handle_command("/resume foo").await;
        assert!(outcome.handled);
        assert!(outcome.ui.is_none());
        assert_eq!(app.status_message, "session resumed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn config_command_requests_config_ui() {
        let dir = std::env::temp_dir().join(format!("catus_config_overlay_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        let outcome = app.handle_command("/config").await;
        assert!(outcome.handled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_fields_lists_editable_keys() {
        let dir = std::env::temp_dir().join(format!("catus_config_fields_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let app = App::new(test_config_with_history_dir(&dir));
        let fields = app.config_fields();
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"agent.max_tool_rounds"));
        assert!(keys.contains(&"agent.log_level"));
        assert!(keys.contains(&"agent.models.performance"));
        assert!(keys.contains(&"agent.models.efficient"));
        assert!(!keys.iter().any(|k| k.starts_with("api.")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_config_field_updates_value_and_saves() {
        let dir = std::env::temp_dir().join(format!("catus_config_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create a config file so there is a path to save to.
        let config_path = dir.join("config.toml");
        let initial = r#"
[[providers]]
name = "test"
base_url = "https://example.com/v1"
api_key = "test"

[[models]]
id = "test-model"
provider = "test"

[agent]
max_tool_rounds = 5
log_level = "info"
"#;
        std::fs::write(&config_path, initial).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.config_path = Some(config_path.clone());
        app.set_config_field("agent.max_tool_rounds", "12")
            .expect("set_config_field should succeed");
        assert_eq!(app.config.agent.max_tool_rounds, 12);
        assert_eq!(app.max_tool_rounds, 12);

        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("max_tool_rounds = 12"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_tier_model_updates_config_and_subagent_tiers() {
        let dir = std::env::temp_dir().join(format!("catus_tier_model_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _xdg = isolate_xdg_config(&dir);
        let _ws = isolate_workspace(&dir);

        // Global config with two models; no workspace config file yet, so
        // tier edits land in the global file.
        let global = r#"
[[providers]]
name = "test"
base_url = "https://example.com/v1"
api_key = "test"

[[models]]
id = "test-model"
provider = "test"

[[models]]
id = "fast-model"
provider = "test"

[agent.models]
performance = "test-model"
"#;
        std::fs::create_dir_all(crate::config::xdg_catus_dir()).unwrap();
        std::fs::write(crate::config::xdg_config_path(), global).unwrap();

        let mut config = test_config_with_history_dir(&dir);
        config.models.push(crate::config::ModelEntry {
            id: "fast-model".to_string(),
            name: String::new(),
            context_window: 2048,
            provider: "test".to_string(),
        });
        let mut app = App::new(config);
        assert_eq!(
            app.subagents.parent_tier_models_for_test().efficient.id,
            "test-model"
        );

        // Setting the efficient tier updates the config, the resolved tiers
        // for future subagents, and the global file.
        let msg = app
            .set_tier_model(
                crate::config::ModelTier::parse("efficient").unwrap(),
                "fast-model",
            )
            .unwrap();
        assert!(msg.contains("efficient"), "unexpected message: {}", msg);
        assert_eq!(
            app.config.agent.models.efficient.as_deref(),
            Some("fast-model")
        );
        assert_eq!(
            app.subagents.parent_tier_models_for_test().efficient.id,
            "fast-model"
        );
        let saved = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(saved.contains("efficient = \"fast-model\""));
        // Untouched content survives the edit.
        assert!(saved.contains("performance = \"test-model\""));

        // A reference that matches no model is rejected without touching
        // the file or the runtime state.
        let before = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(
            app.set_tier_model(
                crate::config::ModelTier::parse("efficient").unwrap(),
                "nosuch"
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(crate::config::xdg_config_path()).unwrap(),
            before
        );
        assert_eq!(
            app.subagents.parent_tier_models_for_test().efficient.id,
            "fast-model"
        );

        // Clearing the efficient tier falls back to the performance model.
        app.remove_config_field_in(crate::config::ConfigScope::Global, "agent.models.efficient")
            .unwrap();
        assert_eq!(app.config.agent.models.efficient, None);
        assert_eq!(
            app.subagents.parent_tier_models_for_test().efficient.id,
            "test-model"
        );

        // Removing the performance tier is rejected: no fallback remains,
        // and the file keeps the key.
        let before = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(
            app.remove_config_field_in(
                crate::config::ConfigScope::Global,
                "agent.models.performance",
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(crate::config::xdg_config_path()).unwrap(),
            before
        );

        // Once a workspace config file exists, tier edits go there.
        std::fs::create_dir_all(crate::config::workspace_config_path().parent().unwrap()).unwrap();
        std::fs::write(
            crate::config::workspace_config_path(),
            "[agent.models]\nperformance = \"test-model\"\n",
        )
        .unwrap();
        app.set_tier_model(
            crate::config::ModelTier::parse("efficient").unwrap(),
            "fast-model",
        )
        .unwrap();
        let saved = std::fs::read_to_string(crate::config::workspace_config_path()).unwrap();
        assert!(saved.contains("efficient = \"fast-model\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn model_command_set_tier_updates_config() {
        let dir = std::env::temp_dir().join(format!("catus_tier_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _xdg = isolate_xdg_config(&dir);
        let _ws = isolate_workspace(&dir);

        let global = r#"
[[providers]]
name = "test"
base_url = "https://example.com/v1"
api_key = "test"

[[models]]
id = "test-model"
provider = "test"

[agent.models]
performance = "test-model"
"#;
        std::fs::create_dir_all(crate::config::xdg_catus_dir()).unwrap();
        std::fs::write(crate::config::xdg_config_path(), global).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));

        let outcome = app.handle_command("/model set-efficient test-model").await;
        assert!(outcome.handled);
        assert!(
            app.status_message.contains("efficient model set to"),
            "unexpected status: {}",
            app.status_message
        );
        assert_eq!(
            app.config.agent.models.efficient.as_deref(),
            Some("test-model")
        );
        let saved = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(saved.contains("efficient = \"test-model\""));

        // A reference matching no configured model surfaces the validation
        // error without changing the effective tier.
        assert!(
            app.handle_command("/model set-efficient nosuch")
                .await
                .handled
        );
        assert!(
            app.status_message.contains("does not match any configured"),
            "unexpected status: {}",
            app.status_message
        );
        assert_eq!(
            app.config.agent.models.efficient.as_deref(),
            Some("test-model")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_remove_mcp_server_in_edits_scopes_and_runtime() {
        let dir = std::env::temp_dir().join(format!("catus_mcp_edit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _xdg = isolate_xdg_config(&dir);
        let _ws = isolate_workspace(&dir);

        let global = r#"
[[providers]]
name = "test"
base_url = "https://example.com/v1"
api_key = "test"

[[models]]
id = "test-model"
provider = "test"

[agent.models]
performance = "test-model"

[[mcp.servers]]
name = "calc"
command = "calc-server"
"#;
        std::fs::create_dir_all(crate::config::xdg_catus_dir()).unwrap();
        std::fs::write(crate::config::xdg_config_path(), global).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));

        // The scope snapshot lists the global mcp server entry.
        let scopes = app.config_scopes();
        assert_eq!(scopes[1].mcp_servers.len(), 1);
        assert_eq!(scopes[0].mcp_servers.len(), 0);

        // Validation failures leave the file untouched.
        let before = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        for server in [
            crate::config::McpServerConfig {
                name: "bad".to_string(),
                transport: crate::config::McpTransport::Stdio,
                command: String::new(),
                ..Default::default()
            },
            crate::config::McpServerConfig {
                name: "bad".to_string(),
                transport: crate::config::McpTransport::StreamableHttp,
                url: None,
                ..Default::default()
            },
            crate::config::McpServerConfig {
                name: "bad".to_string(),
                transport: crate::config::McpTransport::StreamableHttp,
                url: Some("ftp://example.com".to_string()),
                ..Default::default()
            },
        ] {
            assert!(
                app.upsert_mcp_server_in(crate::config::ConfigScope::Global, server)
                    .is_err()
            );
        }
        assert_eq!(
            std::fs::read_to_string(crate::config::xdg_config_path()).unwrap(),
            before
        );

        // Adding a remote server writes the entry and adopts the effective
        // list into the runtime.
        let msg = app
            .upsert_mcp_server_in(
                crate::config::ConfigScope::Global,
                crate::config::McpServerConfig {
                    name: "remote".to_string(),
                    transport: crate::config::McpTransport::StreamableHttp,
                    url: Some("https://mcp.example.com/mcp".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(msg.contains("added"), "unexpected message: {}", msg);
        let saved = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(saved.contains("name = \"remote\""));
        assert!(saved.contains("transport = \"streamable-http\""));
        let effective = app.config.mcp.as_ref().unwrap();
        assert!(effective.servers.iter().any(|s| s.name == "remote"));
        let catalog = app.mcp_catalog();
        assert!(catalog.iter().any(|s| s.name == "remote" && !s.connected));

        // Updating an existing entry keeps its slot and updates the runtime.
        let msg = app
            .upsert_mcp_server_in(
                crate::config::ConfigScope::Global,
                crate::config::McpServerConfig {
                    name: "calc".to_string(),
                    transport: crate::config::McpTransport::Stdio,
                    command: "calc-server-v2".to_string(),
                    args: vec!["--verbose".to_string()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(msg.contains("updated"), "unexpected message: {}", msg);
        let effective = &app.config.mcp.as_ref().unwrap().servers;
        let calc = effective.iter().find(|s| s.name == "calc").unwrap();
        assert_eq!(calc.command, "calc-server-v2");
        assert_eq!(calc.args, vec!["--verbose".to_string()]);

        // A workspace override wins over the global entry (matched by name).
        app.upsert_mcp_server_in(
            crate::config::ConfigScope::Workspace,
            crate::config::McpServerConfig {
                name: "calc".to_string(),
                transport: crate::config::McpTransport::Stdio,
                command: "workspace-calc".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let effective = &app.config.mcp.as_ref().unwrap().servers;
        assert_eq!(
            effective.iter().find(|s| s.name == "calc").unwrap().command,
            "workspace-calc"
        );
        let scopes = app.config_scopes();
        assert!(scopes[0].mcp_servers.iter().any(|s| s.name == "calc"));
        assert!(scopes[1].mcp_servers.iter().any(|s| s.name == "calc"));

        // Removing the global entry is noted: the workspace still defines
        // the server, so it remains effective.
        let msg = app
            .remove_mcp_server_in(crate::config::ConfigScope::Global, "calc")
            .unwrap();
        assert!(msg.contains("remains effective"), "unexpected: {}", msg);
        assert!(
            app.config
                .mcp
                .as_ref()
                .unwrap()
                .servers
                .iter()
                .any(|s| s.name == "calc")
        );

        // Bringing the global entry back loses against the workspace value.
        app.upsert_mcp_server_in(
            crate::config::ConfigScope::Global,
            crate::config::McpServerConfig {
                name: "calc".to_string(),
                transport: crate::config::McpTransport::Stdio,
                command: "calc-server-v2".to_string(),
                args: vec!["--verbose".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let effective = &app.config.mcp.as_ref().unwrap().servers;
        assert_eq!(
            effective.iter().find(|s| s.name == "calc").unwrap().command,
            "workspace-calc"
        );

        // Removing the workspace entry falls back to the global command.
        let msg = app
            .remove_mcp_server_in(crate::config::ConfigScope::Workspace, "calc")
            .unwrap();
        assert!(
            !msg.contains("remains effective"),
            "unexpected note: {}",
            msg
        );
        let effective = &app.config.mcp.as_ref().unwrap().servers;
        assert_eq!(
            effective.iter().find(|s| s.name == "calc").unwrap().command,
            "calc-server-v2"
        );

        // Removing a server not defined in the scope fails.
        assert!(
            app.remove_mcp_server_in(crate::config::ConfigScope::Workspace, "nosuch")
                .is_err()
        );

        // Removing the global entry now drops it from the effective list.
        app.remove_mcp_server_in(crate::config::ConfigScope::Global, "calc")
            .unwrap();
        let effective = &app.config.mcp.as_ref().unwrap().servers;
        assert!(!effective.iter().any(|s| s.name == "calc"));
        assert!(effective.iter().any(|s| s.name == "remote"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_model_switches_current_model_and_rebuilds_client() {
        let dir = std::env::temp_dir().join(format!("catus_set_model_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = test_config_with_history_dir(&dir);
        config.models.push(crate::config::ModelEntry {
            id: "other-model".to_string(),
            name: "Other".to_string(),
            context_window: 8192,
            provider: "test".to_string(),
        });
        let mut app = App::new(config);
        assert_eq!(app.current_model.id, "test-model");
        assert_eq!(
            app.subagents.parent_current_model_for_test().id,
            "test-model"
        );

        let msg = app.set_model("Other").unwrap();
        assert!(msg.contains("Other"));
        assert_eq!(app.current_model.id, "other-model");
        assert_eq!(app.client.session_id(), app.session_id);
        // Tier-less subagents resolve against the parent's current model, so
        // the manager's snapshot must follow the switch.
        assert_eq!(
            app.subagents.parent_current_model_for_test().id,
            "other-model"
        );

        assert!(app.set_model("nosuch").is_err());
        // Failed switches keep the previous model.
        assert_eq!(app.current_model.id, "other-model");
        assert_eq!(
            app.subagents.parent_current_model_for_test().id,
            "other-model"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_pending_tool_reports_malformed_arguments() {
        let dir = std::env::temp_dir().join(format!("catus_badargs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ToolCall {
            id: "call_bad".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"cmd":"ls"}"#.to_string(),
        });

        let result = app.run_pending_tool().await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("tool call format error"),
            "result should point out the format problem: {}",
            text
        );
        // The call is consumed and the error is fed back as a tool message.
        assert_eq!(app.pending_tool_calls_count(), 0);
        assert_eq!(
            app.messages.last().unwrap().tool_call_id,
            Some("call_bad".into())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mcp_status_command_shows_connection_counts() {
        let dir = std::env::temp_dir().join(format!("catus_mcp_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/mcp status").await.handled);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("mcp: 0/0 servers connected"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn help_command_shows_help_in_history() {
        let dir = std::env::temp_dir().join(format!("catus_help_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/help").await.handled);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("/help"))
        );
        assert!(app.status_message.contains("Help"));
        assert!(app.status_message_clear_at.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn help_command_with_argument_shows_command_help() {
        let dir = std::env::temp_dir().join(format!("catus_help_arg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/help mcp list").await.handled);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("/mcp list"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn help_command_reports_unknown_command() {
        let dir = std::env::temp_dir().join(format!("catus_help_unknown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/help nosuch").await.handled);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("unknown command: /nosuch"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_command_requests_quit() {
        let dir = std::env::temp_dir().join(format!("catus_exit_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/exit").await.handled);
        assert!(app.should_quit);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_status_message_auto_clears() {
        let dir = std::env::temp_dir().join(format!("catus_status_timeout_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.set_transient_message("test message");
        assert_eq!(app.status_message, "test message");
        assert!(app.status_message_clear_at.is_some());

        // Force the clear time into the past.
        app.status_message_clear_at = Some(Instant::now() - Duration::from_secs(1));
        app.maybe_clear_status_message();
        assert!(app.status_message.is_empty());
        assert!(app.status_message_clear_at.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_status_auto_clears_after_timeout() {
        let dir = std::env::temp_dir().join(format!("catus_error_timeout_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.set_error("something went wrong");
        assert_eq!(app.status, AppStatus::Error);

        app.status_message_clear_at = Some(Instant::now() - Duration::from_secs(1));
        app.maybe_clear_status_message();
        assert_eq!(app.status, AppStatus::Idle);
        assert!(app.status_message.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_pending_tool_dispatches_use_skill_through_toolbox() {
        let dir = std::env::temp_dir().join(format!("catus_app_skill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo.\n---\nDo the demo thing.",
        )
        .unwrap();

        let mut config = test_config_with_history_dir(&dir);
        config.agent.skill_paths = Some(vec![dir.clone()]);
        let mut app = App::new(config);
        assert!(app.toolbox.get("use_skill").is_some());

        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ToolCall {
            id: "call_1".to_string(),
            name: "use_skill".to_string(),
            arguments: r#"{"name":"demo"}"#.to_string(),
        });

        let result = app.run_pending_tool().await;
        assert!(
            result.unwrap().contains("activated skill 'demo'"),
            "tool result should confirm activation"
        );
        assert_eq!(app.active_skills, vec!["demo".to_string()]);
        assert!(
            app.messages
                .iter()
                .any(|m| m.content.contains("Skill 'demo' instructions")),
            "instructions should be injected as a system message"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_pending_tool_reports_unknown_tool() {
        let dir = std::env::temp_dir().join(format!("catus_app_unknown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ToolCall {
            id: "call_1".to_string(),
            name: "nonexistent".to_string(),
            arguments: "{}".to_string(),
        });

        let result = app.run_pending_tool().await;
        assert!(
            result.unwrap().contains("unknown tool 'nonexistent'"),
            "unknown tools should be reported back"
        );
        assert_eq!(app.pending_tool_calls_count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ask_tool_call() -> ToolCall {
        ToolCall {
            id: "call_ask".to_string(),
            name: "ask_user".to_string(),
            arguments: r#"{"questions":[{"prompt":"Pick one","title":"Pick","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}"#
                .to_string(),
        }
    }

    #[tokio::test]
    async fn ask_user_tool_pauses_turn_for_interaction() {
        let mut app = App::new(AppConfig::default());
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ask_tool_call());

        let result = app.run_pending_tool().await;
        assert!(
            result.is_none(),
            "no result message while waiting for the user"
        );
        assert!(app.has_pending_interaction());
        assert_eq!(app.pending_tool_calls_count(), 1, "call stays pending");
        assert!(
            {
                let mut requested = false;
                while let Some(event) = app.take_event() {
                    if matches!(event, RuntimeEvent::InteractionRequested(_)) {
                        requested = true;
                    }
                }
                requested
            },
            "the frontend must be told to collect the user's answers"
        );
        assert!(
            !app.messages.iter().any(|m| m.role == Role::Tool),
            "tool result must not be sent before the user answers"
        );
    }

    #[test]
    fn complete_interaction_appends_result_and_clears_pending() {
        let mut app = App::new(AppConfig::default());
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ask_tool_call());
        app.pending_interaction = Some((
            ask_tool_call(),
            vec![serde_json::from_str(
                r#"{"prompt":"Pick one","title":"Pick","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}"#,
            )
            .unwrap()],
        ));

        let answers = vec![AskAnswer {
            prompt: "Pick one".to_string(),
            answer: crate::tool::Answer::One("a".to_string()),
        }];
        assert!(app.complete_interaction(answers));
        assert!(!app.has_pending_interaction());
        assert_eq!(app.pending_tool_calls_count(), 0);
        let last = app.messages.last().expect("tool result message");
        assert_eq!(last.role, Role::Tool);
        assert!(last.content.contains(r#""answer":"a""#));
    }

    #[test]
    fn cancel_interaction_reports_cancellation() {
        let mut app = App::new(AppConfig::default());
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(ask_tool_call());
        app.pending_interaction = Some((ask_tool_call(), Vec::new()));

        assert!(app.cancel_interaction());
        assert!(!app.has_pending_interaction());
        let last = app.messages.last().expect("tool result message");
        assert!(last.content.contains("user cancelled"));
        assert!(app.messages.iter().filter(|m| m.role == Role::Tool).count() == 1);
    }

    #[test]
    fn ask_permission_interaction_grants_session_tags() {
        let mut app = App::new(AppConfig::default());
        app.shell_state.permissions =
            sutcac_sh::permissions::PermissionPolicy::parse("deny:network").unwrap();
        let call = ToolCall {
            id: "call_perm".to_string(),
            name: "ask_permission".to_string(),
            arguments: r#"{"tags":["network","write"],"reason":"curl"}"#.to_string(),
        };
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(call.clone());
        app.pending_interaction = Some((call, Vec::new()));

        let answers = vec![AskAnswer {
            prompt: "Grant shell permission(s)?\n\nRequest: tags NETWORK, WRITE".to_string(),
            answer: crate::tool::Answer::One(GRANT_SESSION.to_string()),
        }];
        assert!(app.complete_interaction(answers));
        assert!(!app.has_pending_interaction());
        let last = app.messages.last().expect("tool result message");
        assert!(
            last.content
                .contains("granted for this session: tags NETWORK, WRITE")
        );
        let mut network = sutcac_sh::permissions::PermissionSet::empty();
        network.insert(sutcac_sh::permissions::Permission::Custom(
            "NETWORK".to_string(),
        ));
        assert!(app.shell_state.permissions.check(&network).is_ok());
        assert!(
            app.shell_state
                .permissions
                .check(&sutcac_sh::permissions::PermissionSet::write())
                .is_ok()
        );
    }

    #[test]
    fn ask_permission_interaction_deny_reports_denial() {
        let mut app = App::new(AppConfig::default());
        app.shell_state.permissions =
            sutcac_sh::permissions::PermissionPolicy::parse("deny:network").unwrap();
        let call = ToolCall {
            id: "call_perm".to_string(),
            name: "ask_permission".to_string(),
            arguments: r#"{"tags":["network"]}"#.to_string(),
        };
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(call.clone());
        app.pending_interaction = Some((call, Vec::new()));

        let answers = vec![AskAnswer {
            prompt: "Grant shell permission(s) \"NETWORK\"?".to_string(),
            answer: crate::tool::Answer::One(crate::tool::GRANT_DENY.to_string()),
        }];
        assert!(app.complete_interaction(answers));
        let last = app.messages.last().expect("tool result message");
        assert!(last.content.contains("denied"));
        let mut network = sutcac_sh::permissions::PermissionSet::empty();
        network.insert(sutcac_sh::permissions::Permission::Custom(
            "NETWORK".to_string(),
        ));
        assert!(app.shell_state.permissions.check(&network).is_err());
    }

    #[test]
    fn completing_without_pending_interaction_is_a_noop() {
        let mut app = App::new(AppConfig::default());
        assert!(!app.complete_interaction(Vec::new()));
        assert!(!app.cancel_interaction());
        assert!(app.messages.iter().all(|m| m.role != Role::Tool));
    }

    #[test]
    fn ask_permission_interaction_grants_session_paths() {
        use sutcac_sh::permissions::{CommandPath, PathAccess};

        let mut app = App::new(AppConfig::default());
        let ws = std::env::temp_dir().join(format!("catus_perm_ws_{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("catus_perm_out_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        let _ = std::fs::create_dir_all(&outside);
        app.session_cwd = ws.display().to_string();
        app.shell_state.permissions =
            sutcac_sh::permissions::PermissionPolicy::allow_all().with_base_dir(&ws);
        app.shell_state.cwd = ws.clone();

        let call = ToolCall {
            id: "call_perm_paths".to_string(),
            name: "ask_permission".to_string(),
            arguments: format!(
                r#"{{"write_paths":["{}"],"reason":"build output"}}"#,
                outside.display()
            ),
        };
        app.messages.push(Message::assistant(String::new()));
        app.add_tool_call(call.clone());
        app.pending_interaction = Some((call, Vec::new()));

        let answers = vec![AskAnswer {
            prompt: "Grant shell permission(s)?".to_string(),
            answer: crate::tool::Answer::One(crate::tool::GRANT_SESSION.to_string()),
        }];
        assert!(app.complete_interaction(answers));
        let last = app.messages.last().expect("tool result message");
        assert!(last.content.contains("granted for this session"));
        assert!(last.content.contains("write"));

        // The granted directory is now writable; the workspace stays allowed
        // and unrelated directories remain denied.
        app.shell_state
            .permissions
            .check_paths(&[CommandPath::new(&outside, PathAccess::Write)], &ws)
            .unwrap();
        app.shell_state
            .permissions
            .check_paths(&[CommandPath::new(&ws, PathAccess::Write)], &ws)
            .unwrap();

        let other = std::env::temp_dir().join(format!("catus_perm_other_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&other);
        assert!(
            app.shell_state
                .permissions
                .check_paths(&[CommandPath::new(&other, PathAccess::Write)], &ws)
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// Build a config whose agent definitions live in `dir` (must contain
    /// `main.md` and, optionally, a `role: memory` agent) with memory enabled.
    fn memory_test_config(dir: &std::path::Path, auto_write: bool) -> AppConfig {
        let mut config = test_config_with_history_dir(dir);
        config.agent.agent_paths = Some(vec![dir.to_path_buf()]);
        config.agent.memory = crate::config::MemoryConfig {
            enabled: true,
            auto_recall: true,
            auto_write,
        };
        // Keep the spawned memory runner away from real hosts.
        config.providers[0].base_url = "http://127.0.0.1:9/v1".to_string();
        config
    }

    fn write_agent_files(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("main.md"),
            "---\nname: main\ndescription: Main agent.\n---\nMain body.",
        )
        .unwrap();
        std::fs::write(
            dir.join("memory.md"),
            "---\nname: memory\ndescription: Memory agent.\nrole: memory\nmodel: efficient\n\
             tools: [shell, read, edit]\n---\nMemory agent body.",
        )
        .unwrap();
    }

    /// Serialize XDG isolation and redirect `XDG_CONFIG_HOME` to a scratch
    /// directory for the guard's lifetime, so ambient agent/skill definitions
    /// under the real XDG directory cannot leak into agent discovery and
    /// collide with the test's own definitions.
    static XDG_ISOLATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct XdgIsolation {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for XdgIsolation {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
                None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
            }
        }
    }

    fn isolate_xdg_config(base: &std::path::Path) -> XdgIsolation {
        let lock = XDG_ISOLATION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        let xdg = base.join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        XdgIsolation {
            _lock: lock,
            previous,
        }
    }

    /// Serialize current-directory changes and point the workspace config
    /// path (`current_dir/.sutcac/config.toml`) into a scratch directory for
    /// the guard's lifetime.
    static WORKSPACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct WorkspaceGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: std::path::PathBuf,
    }

    impl Drop for WorkspaceGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    fn isolate_workspace(base: &std::path::Path) -> WorkspaceGuard {
        let lock = WORKSPACE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::env::set_current_dir(&work).unwrap();
        WorkspaceGuard {
            _lock: lock,
            previous,
        }
    }

    #[test]
    fn upsert_model_in_adds_updates_and_refreshes_runtime() {
        let dir = std::env::temp_dir().join(format!("catus_upsert_model_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _xdg = isolate_xdg_config(&dir);
        let _ws = isolate_workspace(&dir);

        let global = r#"
[[providers]]
name = "test"
base_url = "https://example.com/v1"
api_key = "test"

[[models]]
id = "test-model"
name = "Test Model"
provider = "test"

[agent.models]
performance = "test-model"
"#;
        std::fs::create_dir_all(crate::config::xdg_catus_dir()).unwrap();
        std::fs::write(crate::config::xdg_config_path(), global).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        // The scope snapshot lists the global model entry.
        let scopes = app.config_scopes();
        assert_eq!(scopes[1].models.len(), 1);
        assert_eq!(scopes[0].models.len(), 0);

        // Adding a model writes the entry and refreshes the runtime list.
        app.upsert_model_in(
            crate::config::ConfigScope::Global,
            crate::config::ModelEntry {
                id: "m2".to_string(),
                name: "Model Two".to_string(),
                context_window: 8192,
                provider: "test".to_string(),
            },
        )
        .unwrap();
        assert!(app.models.iter().any(|m| m.id == "m2"));
        let saved = std::fs::read_to_string(crate::config::xdg_config_path()).unwrap();
        assert!(saved.contains("id = \"m2\""));

        // Updating the same id in the workspace scope writes an override and
        // keeps the id effective.
        app.upsert_model_in(
            crate::config::ConfigScope::Workspace,
            crate::config::ModelEntry {
                id: "m2".to_string(),
                name: "Workspace Two".to_string(),
                context_window: 0,
                provider: "test".to_string(),
            },
        )
        .unwrap();
        let scopes = app.config_scopes();
        assert!(scopes[0].models.iter().any(|m| m.id == "m2"));
        let saved = std::fs::read_to_string(crate::config::workspace_config_path()).unwrap();
        assert!(saved.contains("name = \"Workspace Two\""));

        // Unknown providers are rejected without touching the file.
        let before = std::fs::read_to_string(crate::config::workspace_config_path()).unwrap();
        assert!(
            app.upsert_model_in(
                crate::config::ConfigScope::Workspace,
                crate::config::ModelEntry {
                    id: "m3".to_string(),
                    name: String::new(),
                    context_window: 0,
                    provider: "nosuch".to_string(),
                },
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(crate::config::workspace_config_path()).unwrap(),
            before
        );

        // Removing a model not defined in the scope fails.
        assert!(
            app.remove_model_in(crate::config::ConfigScope::Workspace, "test-model")
                .is_err()
        );

        // Removing the performance-tier model is rejected.
        assert!(
            app.remove_model_in(crate::config::ConfigScope::Global, "test-model")
                .is_err()
        );

        // Removing a non-tier model works; when it is the current model the
        // runtime falls back to the performance-tier model.
        app.set_model("m2").unwrap();
        assert_eq!(app.current_model.id, "m2");
        let msg = app
            .remove_model_in(crate::config::ConfigScope::Global, "m2")
            .unwrap();
        // The workspace still defines m2, so it remains effective.
        assert!(msg.contains("remains effective"));
        assert!(app.models.iter().any(|m| m.id == "m2"));
        // After the workspace override is removed too, m2 disappears and the
        // current model falls back.
        app.remove_model_in(crate::config::ConfigScope::Workspace, "m2")
            .unwrap();
        assert!(!app.models.iter().any(|m| m.id == "m2"));
        assert_eq!(app.current_model.id, "test-model");
        assert_eq!(
            app.subagents.parent_current_model_for_test().id,
            "test-model"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_recall_dispatch_withholds_and_injects() {
        let dir = std::env::temp_dir().join(format!("catus_memory_recall_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let config = memory_test_config(&dir, false);
        let mut app = App::new(config);
        assert!(app.memory.available, "memory should be available");
        assert!(app.memory.warnings.is_empty());

        // Submitting a user message dispatches the recall pass and withholds
        // the main stream.
        app.submit_user_message("how do I configure the release profile?".to_string());
        assert!(app.awaiting_memory_recall());
        assert_eq!(app.status, AppStatus::RunningTool);

        // A recall=true completion injects the memory as a system message and
        // clears the pending state.
        let id = app.memory.pending_recall.clone().unwrap();
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id: id.clone(),
            result: r#"{"recall": true, "memory": "user prefers release.profile opt-level=3"}"#
                .to_string(),
        });
        assert!(!app.awaiting_memory_recall());
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_system() && m.content.contains("opt-level=3"))
        );

        // A stale completion event is a no-op, and later turns of the
        // session do not recall again (the session-start memory stays in
        // the conversation).
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id,
            result: r#"{"recall": false}"#.to_string(),
        });
        assert!(!app.awaiting_memory_recall());
        let recalled = app
            .messages
            .iter()
            .filter(|m| m.is_system() && m.content.contains("Recalled memory"))
            .count();
        app.submit_user_message("hi".to_string());
        assert!(!app.awaiting_memory_recall());
        assert!(app.memory.pending_recall.is_none());
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| m.is_system() && m.content.contains("Recalled memory"))
                .count(),
            recalled
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_recall_failure_degrades_to_no_memory() {
        let dir = std::env::temp_dir().join(format!("catus_memory_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let config = memory_test_config(&dir, false);
        let mut app = App::new(config);
        app.submit_user_message("hello".to_string());
        assert!(app.awaiting_memory_recall());

        let id = app.memory.pending_recall.clone().unwrap();
        app.handle_subagent_event(crate::subagent::SubagentEvent::Error {
            id,
            error: "llm unavailable".to_string(),
        });
        assert!(!app.awaiting_memory_recall());
        assert!(
            !app.messages
                .iter()
                .any(|m| m.is_system() && m.content.contains("Recalled memory"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_write_queues_while_pass_running() {
        let dir = std::env::temp_dir().join(format!("catus_memory_queue_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let config = memory_test_config(&dir, true);
        let mut app = App::new(config);
        app.messages.push(Message::user("remember I like rust"));
        app.messages.push(Message::assistant("noted"));

        app.maybe_dispatch_memory_write();
        let first = app.memory.pending_write.clone().expect("first write");
        // A write requested while the first pass runs is queued, not skipped.
        app.maybe_dispatch_memory_write();
        assert!(app.memory.write_queued);
        assert_eq!(app.memory.pending_write.as_deref(), Some(first.as_str()));

        // Completing the running pass dispatches the queued one.
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id: first,
            result: r#"{"written": false}"#.to_string(),
        });
        assert!(!app.memory.write_queued);
        let second = app.memory.pending_write.clone().expect("queued write");
        assert!(app.subagents.get(&second).is_some());

        // The queued pass completes normally.
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id: second,
            result: r#"{"written": true, "summary": "likes rust"}"#.to_string(),
        });
        assert!(app.memory.pending_write.is_none());
        assert!(!app.memory.write_queued);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("memory updated: likes rust"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_write_pass_runs_after_turn_completion() {
        let dir = std::env::temp_dir().join(format!("catus_memory_write_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let config = memory_test_config(&dir, true);
        let mut app = App::new(config);

        // No user message yet: nothing to summarize.
        app.maybe_dispatch_memory_write();
        assert!(app.memory.pending_write.is_none());

        app.messages.push(Message::user("remember I like rust"));
        app.messages.push(Message::assistant("noted"));
        app.maybe_dispatch_memory_write();
        let id = app.memory.pending_write.clone().expect("write dispatched");
        // The write pass forks the main agent: the task carries the memory
        // agent's instructions and the store path, and the context mode is
        // `fork` (the runner clones the main conversation prefix).
        let sub = app.subagents.get(&id).unwrap();
        assert_eq!(sub.mode, crate::subagent::SubagentContextMode::Fork);
        assert!(sub.task.contains("memory-book"));
        // The memory agent's definition body travels with the task (fork
        // mode does not inject it as a system message).
        assert!(sub.task.contains("Memory agent body."));

        // A written=false result clears the pass without an event line.
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id: id.clone(),
            result: r#"{"written": false}"#.to_string(),
        });
        assert!(app.memory.pending_write.is_none());

        // A written=true result reports what was recorded.
        app.maybe_dispatch_memory_write();
        let id = app.memory.pending_write.clone().unwrap();
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id,
            result: r#"{"written": true, "summary": "likes rust"}"#.to_string(),
        });
        assert!(app.memory.pending_write.is_none());
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("memory updated: likes rust"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_disabled_config_never_dispatches() {
        let dir = std::env::temp_dir().join(format!("catus_memory_off_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let mut config = memory_test_config(&dir, true);
        config.agent.memory.enabled = false;
        let mut app = App::new(config);
        assert!(!app.memory.available);

        app.submit_user_message("hello".to_string());
        assert!(!app.awaiting_memory_recall());
        app.maybe_dispatch_memory_write();
        assert!(app.memory.pending_write.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build an app for subagent-injection tests, with the spawned runners'
    /// LLM endpoint unreachable so background tasks fail fast.
    fn subagent_test_app(dir: &std::path::Path) -> App {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        write_agent_files(dir);
        let _xdg = isolate_xdg_config(dir);

        let mut config = test_config_with_history_dir(dir);
        config.providers[0].base_url = "http://127.0.0.1:9".to_string();
        App::new(config)
    }

    fn test_subagent_definition() -> AgentDefinition {
        AgentDefinition {
            name: "coder".to_string(),
            description: "test agent".to_string(),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: None,
            skills: Vec::new(),
            role: None,
            body: "Do things.".to_string(),
            source_path: std::path::PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn async_subagent_completion_injects_notice_not_result() {
        let dir = std::env::temp_dir().join(format!("catus_task_notice_{}", std::process::id()));
        let mut app = subagent_test_app(&dir);
        let definition = test_subagent_definition();
        let id = app.subagents.spawn(
            &definition,
            "do things".to_string(),
            crate::subagent::SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            SkillRegistry::new(),
            Some("call-task".to_string()),
        );
        app.subagents.get_mut(&id).unwrap().state = crate::subagent::SubagentState::Completed;
        app.subagents.get_mut(&id).unwrap().result = Some("the secret result body".to_string());

        let resumed = app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id: id.clone(),
            result: "the secret result body".to_string(),
        });
        assert!(resumed, "the parent turn should resume on completion");

        let notice = app
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-task"))
            .expect("completion notice injected under the dispatch call id");
        assert!(notice.content.contains("completed"));
        assert!(notice.content.contains(&format!("\"id\": \"{}\"", id)));
        assert!(notice.content.contains("\"action\": \"result\""));
        // The result body itself must not leak into the conversation.
        assert!(!notice.content.contains("the secret result body"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn async_subagent_failure_injects_notice_not_error() {
        let dir = std::env::temp_dir().join(format!("catus_task_fnotice_{}", std::process::id()));
        let mut app = subagent_test_app(&dir);
        let definition = test_subagent_definition();
        let id = app.subagents.spawn(
            &definition,
            "do things".to_string(),
            crate::subagent::SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            SkillRegistry::new(),
            Some("call-task".to_string()),
        );
        app.subagents.get_mut(&id).unwrap().state = crate::subagent::SubagentState::Error;
        app.subagents.get_mut(&id).unwrap().error = Some("llm unavailable".to_string());

        app.handle_subagent_event(crate::subagent::SubagentEvent::Error {
            id: id.clone(),
            error: "llm unavailable".to_string(),
        });

        let notice = app
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-task"))
            .expect("failure notice injected under the dispatch call id");
        assert!(notice.content.contains("failed"));
        assert!(notice.content.contains("\"action\": \"result\""));
        assert!(!notice.content.contains("llm unavailable"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tasksync_completion_gets_no_notice() {
        let dir = std::env::temp_dir().join(format!("catus_task_sync_{}", std::process::id()));
        let mut app = subagent_test_app(&dir);
        let definition = test_subagent_definition();
        let (id, _rx) = app.subagents.spawn_sync(
            &definition,
            "do things".to_string(),
            crate::subagent::SubagentContextMode::Create,
            Vec::new(),
            ShellState::new(),
            Vec::new(),
            SkillRegistry::new(),
            Some("call-sync".to_string()),
        );
        app.subagents.get_mut(&id).unwrap().state = crate::subagent::SubagentState::Completed;
        app.subagents.get_mut(&id).unwrap().result = Some("sync body".to_string());

        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id,
            result: "sync body".to_string(),
        });

        // taskSync delivers the result through its own tool return; no extra
        // notice message may be injected under the same call id.
        assert!(
            app.messages
                .iter()
                .all(|m| m.tool_call_id.as_deref() != Some("call-sync"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_session_toggle_is_persisted_and_restored() {
        let dir = std::env::temp_dir().join(format!("catus_memory_toggle_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_agent_files(&dir);
        let _xdg = isolate_xdg_config(&dir);

        let config = memory_test_config(&dir, true);
        let mut app = App::new(config);
        app.submit_user_message("remember this".to_string());
        // Drain the recall pass before toggling (one pass at a time).
        let id = app.memory.pending_recall.clone().unwrap();
        app.handle_subagent_event(crate::subagent::SubagentEvent::Completed {
            id,
            result: r#"{"recall": false}"#.to_string(),
        });

        // Turning memory off persists the toggle in the session state.
        let msg = app.set_memory_enabled(false);
        assert!(msg.contains("disabled"));
        assert!(
            app.persist_state()
                .contains_key(crate::history::STATE_MEMORY_ENABLED)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

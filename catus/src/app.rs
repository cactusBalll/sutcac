//! Application state for the catus Agent TUI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::ShellState;
use tokio::sync::mpsc;

use crate::agents::{AgentDefinition, AgentRegistry};
use crate::config::{AppConfig, TierModels};
use crate::llm::{LlmClient, LlmError, Model, StreamEvent, Usage};
use crate::mcp::{McpManager, mcp_tools};
use crate::message::{Message, Role};
use crate::skills::SkillRegistry;
use crate::subagent::SubagentManager;
use crate::tool::{
    AskAnswer, AskPermissionTool, AskQuestion, AskUserTool, EditTool, GRANT_SESSION, ShellTool,
    SkillTool, TaskSyncTool, TaskTool, Tool, ToolCall, ToolContext, ToolResult, Toolbox,
    parse_ask_permission_tags,
};

pub mod chat_state;
pub mod command;
pub mod commands;
pub mod input_state;
pub mod overlay_state;

pub use chat_state::ChatState;
pub use input_state::{InputState, MAX_CANDIDATES};
pub use overlay_state::{Overlay, OverlayState};

/// Current high-level state of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Idle,
    Streaming,
    RunningTool,
    Error,
}

/// Result of pressing Enter in the input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEnterResult {
    /// The input was a slash command and has been handled.
    Handled,
    /// The input was submitted as a user message; the caller should start
    /// streaming the assistant reply.
    Submitted,
    /// The input was empty; nothing happened.
    Empty,
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
    /// A tool call that is paused waiting for the user to answer questions
    /// in the ask overlay. The turn resumes via `complete_interaction` or
    /// `cancel_interaction` once the overlay closes.
    pending_interaction: Option<(ToolCall, Vec<AskQuestion>)>,
    /// SQLite-backed session history, when `[agent].history_path` is set.
    pub history_store: Option<crate::history::SessionStore>,
    /// Database row id of the session currently being continued, if any.
    pub current_session_id: Option<i64>,
    /// Name for the not-yet-persisted current session (timestamp-based).
    session_name: String,
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
    /// Currently observed subagent in the TUI, if any.
    pub current_subagent_view: Option<String>,
    /// Discovered Agent Skills registry.
    pub skill_registry: SkillRegistry,
    /// Names of skills currently active in the conversation.
    pub active_skills: Vec<String>,
    /// Connected MCP servers, if any.
    pub mcp_manager: Option<Arc<McpManager>>,
    /// All tools available to the LLM: built-in plus MCP-converted.
    pub toolbox: Toolbox,
    /// Input-line state (cursor, history, completion candidates).
    pub input_state: InputState,
    /// Chat viewport state (scroll, auto-scroll).
    pub chat_state: ChatState,
    /// Modal overlay state.
    pub overlay_state: OverlayState,
}

/// How long transient status-bar messages remain visible before clearing.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Generate the per-conversation session ID: stable for the lifetime of one
/// process (one conversation in the TUI).
fn new_session_id() -> String {
    format!(
        "catus-{}-{:08x}",
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
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
                log::warn!("no usable model configuration ({}); using placeholder", e);
                vec![Model::default()]
            }
        };
        let current_model = models.first().cloned().unwrap_or_default();
        let tier_models = config.resolve_tier_models(&models).unwrap_or_else(|e| {
            // Tests and minimal setups run without a tier configuration; real
            // runs are rejected earlier by config validation in main.
            log::warn!(
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

        let mut search_paths = SkillRegistry::default_paths();
        if let Some(extra) = &config.agent.skill_paths {
            search_paths.extend(extra.iter().cloned());
        }
        let skill_registry = SkillRegistry::discover(&search_paths).unwrap_or_else(|e| {
            log::warn!("failed to discover skills: {}", e);
            SkillRegistry::new()
        });

        let max_tool_rounds = config.agent.max_tool_rounds;

        // Composition root: register the built-in tools; MCP tools are added
        // in `connect_mcp` as servers come online.
        let mut toolbox = Toolbox::default();
        toolbox.register(std::sync::Arc::new(ShellTool));
        toolbox.register(std::sync::Arc::new(EditTool));
        toolbox.register(std::sync::Arc::new(SkillTool));
        toolbox.register(std::sync::Arc::new(AskUserTool));
        toolbox.register(std::sync::Arc::new(AskPermissionTool));
        // Task dispatch tools are only available to the main agent.
        toolbox.register(std::sync::Arc::new(TaskTool));
        toolbox.register(std::sync::Arc::new(TaskSyncTool));

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

        let (history_store, session_name) = match &config.agent.history_path {
            Some(dir) => match crate::history::SessionStore::open(dir) {
                Ok(store) => (Some(store), new_session_name()),
                Err(e) => {
                    log::warn!("failed to open history database: {}", e);
                    (None, String::new())
                }
            },
            None => (None, String::new()),
        };

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
            current_subagent_view: None,
            skill_registry,
            active_skills: Vec::new(),
            mcp_manager: None,
            toolbox,
            status: AppStatus::Idle,
            status_message: String::new(),
            max_tool_rounds,
            pending_tool_calls: Vec::new(),
            tool_rounds_this_turn: 0,
            pending_interaction: None,
            history_store,
            current_session_id: None,
            session_name,
            usage: Usage::default(),
            request_count: 0,
            should_quit: false,
            status_message_clear_at: None,
            config_path: AppConfig::find_config_file(),
            input_state: InputState::new(),
            chat_state: ChatState::new(),
            overlay_state: OverlayState::new(),
        }
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
            log::warn!("failed to discover agents: {}", e);
            AgentRegistry::new()
        });

        let main = registry.get("main").cloned();
        if let Some(main) = main {
            return (registry, main, true);
        }

        log::warn!("main.md agent definition not found; using empty placeholder");
        let placeholder = AgentDefinition {
            name: "main".to_string(),
            description: "Default main agent".to_string(),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: config.shell.as_ref().and_then(|s| s.perm_mode.clone()),
            skills: Vec::new(),
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
            log::warn!("no mcp servers connected");
            self.mcp_manager = None;
        } else {
            log::info!("{} mcp server(s) connected", manager.len());
            let manager = Arc::new(manager);
            for tool in mcp_tools(manager.clone()).await {
                log::info!("registered mcp tool '{}'", tool.name());
                self.toolbox.register(Arc::new(tool));
            }
            self.mcp_manager = Some(manager.clone());
            // Re-apply the main agent tool filter so MCP tools are included in
            // the subagent parent snapshot if allowed.
            let parent_toolbox =
                if self.main_agent.allowed_tools.is_empty() || self.main_agent.inherits_tools() {
                    self.toolbox.clone()
                } else {
                    self.toolbox.filter(&self.main_agent.explicit_tools())
                };
            self.subagents
                .update_parent_toolbox(parent_toolbox, Some(manager));
        }
        warnings
    }

    /// Take the current input and append it as a user message.
    pub fn submit_user_message(&mut self) -> Option<String> {
        let text = self.input_state.take_input()?;
        self.messages.push(Message::user(text.clone()));
        self.input_state.record_history(&text);

        self.tool_rounds_this_turn = 0;
        self.chat_state.scroll_to_bottom();
        self.persist_session();
        Some(text)
    }

    /// Handle pressing Enter in the input line: slash commands take precedence;
    /// otherwise submit the user message and signal that streaming should start.
    pub async fn on_enter(&mut self) -> OnEnterResult {
        let input = self.input_state.input.trim().to_string();
        if input.is_empty() {
            return OnEnterResult::Empty;
        }

        if self.handle_command(&input).await {
            self.input_state.clear();
            OnEnterResult::Handled
        } else if self.submit_user_message().is_some() {
            OnEnterResult::Submitted
        } else {
            OnEnterResult::Empty
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
        log::info!("pending tool call added: {} -> {}", call.id, call.name);
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
        self.chat_state.scroll_to_bottom();

        if let Some(last) = self.messages.last() {
            log::info!(
                "assistant finished: content_len={} had_tool_calls={} pending_tool_calls={}",
                last.content.len(),
                last.had_tool_calls,
                self.pending_tool_calls.len()
            );
        }

        // Skill activation is driven by `use_skill` tool calls handled in
        // `run_pending_tool`, not by text markers.

        if self.has_empty_assistant_placeholder() {
            log::warn!("assistant response was empty; dropping placeholder message");
            self.messages.pop();
            self.messages.push(Message::event(
                "Assistant returned an empty response".to_string(),
            ));
            self.chat_state.scroll_to_bottom();
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
        log::info!(
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

    /// Switch the TUI chat view to a subagent's conversation.
    pub fn watch_subagent(&mut self, id: &str) -> Result<String, Box<dyn std::error::Error>> {
        if self.subagents.get(id).is_none() {
            return Err(format!("subagent not found: {}", id).into());
        }
        self.current_subagent_view = Some(id.to_string());
        Ok(format!("watching subagent {}", id))
    }

    /// Switch the TUI chat view back to the main agent.
    pub fn watch_main_agent(&mut self) {
        self.current_subagent_view = None;
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
    pub async fn mcp_server_list(&self) -> String {
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
            let defs = manager.all_tool_definitions().await;
            if !defs.is_empty() {
                lines.push("available mcp tools:".to_string());
                for def in defs {
                    lines.push(format!("- {}", def.function.name));
                }
            } else {
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

    /// Return the editable config fields as (key, current_value) pairs.
    pub fn config_fields(&self) -> Vec<(String, String)> {
        let mut fields = vec![
            (
                "agent.max_tool_rounds".to_string(),
                self.config.agent.max_tool_rounds.to_string(),
            ),
            (
                "agent.log_level".to_string(),
                self.config.agent.log_level.clone(),
            ),
        ];
        if let Some(shell) = &self.config.shell {
            fields.push((
                "shell.perm_mode".to_string(),
                shell.perm_mode.clone().unwrap_or_default(),
            ));
        }
        fields
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
        self.persist_session();
        Ok(msg)
    }

    /// Human-readable list of configured models, marking the current one.
    pub fn model_names_list(&self) -> String {
        if self.models.is_empty() {
            return "no models configured".to_string();
        }
        self.models
            .iter()
            .map(|m| {
                if m.id == self.current_model.id {
                    format!("{}* (current)", m.display_name())
                } else {
                    m.display_name().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
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

        match key {
            "agent.max_tool_rounds" => {
                self.config.agent.max_tool_rounds = value.parse()?;
                self.max_tool_rounds = self.config.agent.max_tool_rounds;
            }
            "agent.log_level" => self.config.agent.log_level = value.to_string(),
            "shell.perm_mode" => {
                let shell = self.config.shell.get_or_insert_with(ShellConfig::default);
                shell.perm_mode = Some(value.to_string());
                let (permissions, audit_logger) = self
                    .config
                    .shell
                    .clone()
                    .map(|s| (s.permission_policy(), s.audit_logger()))
                    .unwrap_or_else(|| {
                        let default = ShellConfig::default();
                        (default.permission_policy(), default.audit_logger())
                    });
                self.shell_state.set_permission_policy(permissions);
                self.shell_state.set_audit_logger(audit_logger);
            }
            _ => return Err(format!("unknown config field: {}", key).into()),
        }

        self.config.save(&path)?;
        Ok(format!("saved {} to {}", key, path.display()))
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
        log::warn!("{}", text);
        self.messages.push(Message::event(text));
        self.chat_state.scroll_to_bottom();
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
                if policy.read_paths.is_empty() && policy.write_paths.is_empty() {
                    "none".to_string()
                } else {
                    format!(
                        "read: {} dir(s), write: {} dir(s)",
                        policy.read_paths.len(),
                        policy.write_paths.len()
                    )
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
        log::info!("session permissions granted via /permission: {}", tags);
    }

    /// Revoke previously granted session permission tags from the main
    /// agent's shell policy. `tags` is a comma-separated list.
    pub fn revoke_session_permissions(&mut self, tags: &str) {
        self.shell_state.permissions.revoke_grants(tags);
        log::info!("session permissions revoked via /permission: {}", tags);
    }

    /// Reset the main agent's shell permission policy to the configured
    /// default (drops all session grants).
    pub fn reset_session_permissions(&mut self) {
        let (permissions, audit_logger) = self
            .config
            .shell
            .clone()
            .map(|s| (s.permission_policy(), s.audit_logger()))
            .unwrap_or_else(|| {
                let default = ShellConfig::default();
                (default.permission_policy(), default.audit_logger())
            });
        self.shell_state.set_permission_policy(permissions);
        self.shell_state.set_audit_logger(audit_logger);
        log::info!("session permissions reset via /permission");
    }

    /// Switch the main agent's shell permission policy to allow-all for the
    /// rest of the session. Path restrictions are kept.
    pub fn set_session_permissions_allow_all(&mut self) {
        self.shell_state.permissions.mode = sutcac_sh::permissions::PermissionMode::AllowAll;
        self.shell_state.permissions.session_grants =
            sutcac_sh::permissions::PermissionSet::empty();
        log::info!("session permissions switched to allow_all via /auto");
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
                log::info!("running tool: {}", description);
                self.messages
                    .push(Message::event(format!("tool: {}", description)));
                self.chat_state.scroll_to_bottom();

                let mut ctx = ToolContext {
                    shell_state: &mut self.shell_state,
                    skill_registry: &mut self.skill_registry,
                    active_skills: &mut self.active_skills,
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
                log::warn!("unknown tool call: {}", call.name);
                ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("catus: unknown tool '{}'", call.name),
                    interaction: None,
                }
            }
        };

        if let Some(request) = result.interaction {
            // The tool is asking the user questions: pause the turn, open the
            // ask overlay, and resume via `complete_interaction` once the
            // overlay closes. No result message is pushed yet.
            log::info!("tool '{}' is waiting for user input", call.name);
            self.pending_interaction = Some((call, request.questions.clone()));
            self.overlay_state.open_ask(request.questions);
            self.chat_state.scroll_to_bottom();
            return None;
        }

        let message = result.to_message();
        self.messages
            .push(Message::tool(message.clone(), call.id.clone()));
        self.pending_tool_calls.remove(0);
        self.tool_rounds_this_turn += 1;

        self.status = AppStatus::Idle;
        self.status_message.clear();
        self.chat_state.scroll_to_bottom();
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
            let tags = parse_ask_permission_tags(&call.arguments);
            let choice = answers.first().map(|a| match &a.answer {
                crate::tool::Answer::One(label) => label.as_str(),
                crate::tool::Answer::Many(labels) => {
                    labels.first().map(String::as_str).unwrap_or_default()
                }
            });
            let (status, stdout) = match (tags, choice) {
                (Some(tags), Some(GRANT_SESSION)) => {
                    for tag in &tags {
                        self.shell_state.permissions.grant_tag(tag);
                    }
                    log::info!("session permissions granted: {}", tags.join(", "));
                    (
                        0,
                        format!(
                            "granted: permission(s) \"{}\" for this session",
                            tags.join(", ")
                        ),
                    )
                }
                (tags, _) => (
                    1,
                    format!(
                        "denied: the user did not grant permission(s) {}",
                        tags.map(|t| format!("\"{}\"", t.join(", ")))
                            .unwrap_or_else(|| "(unknown)".to_string())
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
            log::warn!("failed to serialize ask_user answers: {}", e);
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
        self.chat_state.scroll_to_bottom();
        self.persist_session();
        true
    }

    /// List available session names from the history database, newest first.
    pub fn list_session_names(&self) -> Vec<String> {
        let Some(store) = self.history_store.as_ref() else {
            return Vec::new();
        };
        match store.list_sessions() {
            Ok(sessions) => sessions.into_iter().map(|s| s.name).collect(),
            Err(e) => {
                log::warn!("failed to list sessions: {}", e);
                Vec::new()
            }
        }
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
            STATE_PENDING_INTERACTION, STATE_PENDING_TOOL_CALLS, STATE_SHELL_CWD,
            STATE_SHELL_EXPORTED, STATE_SHELL_VARS,
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
                match store.create_session(&name, &self.session_id, &self.current_model.id) {
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
                    log::warn!("failed to create history session row; skipping persist");
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
        };
        let subagents = self.subagents.snapshot();
        let state = self.persist_state();
        let store = self.history_store.as_ref().unwrap();
        if let Err(e) = store.save_meta(&meta) {
            log::warn!("failed to save session metadata: {}", e);
        }
        if let Err(e) =
            store.replace_messages(session, crate::history::MAIN_AGENT_ID, &self.messages)
        {
            log::warn!("failed to save conversation messages: {}", e);
        }
        if let Err(e) = store.replace_subagents(session, &subagents) {
            log::warn!("failed to save subagent state: {}", e);
        }
        if let Err(e) = store.replace_state(session, &state) {
            log::warn!("failed to save session state: {}", e);
        }
    }

    /// Process a subagent event: update managed state, inject completion
    /// results into the parent conversation, and resume the parent turn when
    /// an asynchronous subagent finishes.
    ///
    /// Returns `true` when the parent agent should start a new LLM stream to
    /// react to an asynchronous `task` result.
    pub fn handle_subagent_event(&mut self, event: crate::subagent::SubagentEvent) -> bool {
        use crate::subagent::SubagentEvent;
        let mut should_resume = false;
        match &event {
            SubagentEvent::Started { id } => {
                log::info!("subagent {} started", id);
                self.add_event_message(format!("subagent {} started", id));
            }
            SubagentEvent::StateChanged { id, state } => {
                log::info!("subagent {} state changed to {:?}", id, state);
            }
            SubagentEvent::Message { id, message } => {
                log::debug!("subagent {} message: {:?}", id, message.role);
            }
            SubagentEvent::Completed { id, result } => {
                log::info!("subagent {} completed", id);
                self.add_event_message(format!(
                    "subagent {} completed ({} chars)",
                    id,
                    result.len()
                ));
                // Inject the result as a tool response if the parent is waiting
                // for this subagent.
                if let Some(sub) = self.subagents.get(id) {
                    if sub.parent_call_id.is_some() {
                        let call_id = sub.parent_call_id.clone().unwrap();
                        self.messages.push(Message::tool(
                            format!("status=0\nstdout=```\n{}\n```\nstderr=```\n\n```", result),
                            call_id,
                        ));
                        should_resume = self.status == AppStatus::Idle;
                    }
                }
            }
            SubagentEvent::Error { id, error } => {
                log::error!("subagent {} error: {}", id, error);
                self.add_event_message(format!("subagent {} error: {}", id, error));
                if let Some(sub) = self.subagents.get(id) {
                    if sub.parent_call_id.is_some() {
                        let call_id = sub.parent_call_id.clone().unwrap();
                        self.messages.push(Message::tool(
                            format!("status=1\nstdout=```\n\n```\nstderr=```\n{}\n```", error),
                            call_id,
                        ));
                        should_resume = self.status == AppStatus::Idle;
                    }
                }
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
        if self.history_store.is_none() {
            return Err("history_path is not configured".into());
        }
        let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.history_names_list());
        };
        let store = self.history_store.as_ref().unwrap();
        let id = store
            .find_session(name)?
            .ok_or_else(|| format!("history not found: {}", name))?;
        let snapshot = store
            .load_session(id)?
            .ok_or_else(|| format!("history not found: {}", name))?;

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
            log::warn!(
                "saved model '{}' is not configured; keeping the current model",
                snapshot.meta.model_id
            );
        }
        self.client = LlmClient::new(
            self.current_model.provider.clone(),
            self.current_model.clone(),
            &self.session_id,
        );

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

        // A paused ask_user interaction reopens its overlay; the turn
        // continues once the user answers.
        self.pending_interaction = snapshot
            .state
            .get(crate::history::STATE_PENDING_INTERACTION)
            .and_then(|j| serde_json::from_str(j).ok());
        if let Some((_, questions)) = &self.pending_interaction {
            self.overlay_state.open_ask(questions.clone());
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
        self.chat_state.scroll_to_bottom();
        Ok("session resumed".to_string())
    }

    /// Handle a TUI slash command. Returns `true` if the input was a command
    /// and should not be sent to the LLM.
    pub async fn handle_command(&mut self, input: &str) -> bool {
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

    /// Start an async LLM stream and send events through `event_tx`.
    ///
    /// The stream task signals completion through `done_tx`.
    pub async fn start_llm_stream(
        &mut self,
        event_tx: mpsc::Sender<StreamEvent>,
        done_tx: mpsc::Sender<Result<(), LlmError>>,
    ) {
        // Snapshot the conversation *before* adding the assistant placeholder so
        // the API request never contains an empty assistant message.
        let messages = self.messages.clone();
        let tools = self.toolbox.definitions();
        self.start_assistant_message();
        let client = self.client.clone();

        tokio::spawn(async move {
            let result = client.stream_chat(&messages, &tools, event_tx).await;
            let _ = done_tx.send(result).await;
        });
    }

    /// Handle the completion of an LLM stream, running any pending tool calls
    /// and scheduling follow-up requests.
    pub async fn handle_llm_done(
        &mut self,
        result: Result<(), LlmError>,
        event_tx: &mpsc::Sender<StreamEvent>,
        done_tx: &mpsc::Sender<Result<(), LlmError>>,
    ) {
        match result {
            Ok(()) => {
                self.finish_stream();
                if self.has_pending_tool_call() {
                    log::info!(
                        "{} pending tool call(s); running tool",
                        self.pending_tool_calls_count()
                    );
                    self.run_pending_tool().await;
                    if self.pending_interaction.is_some() {
                        // The tool turned into an interactive question; the
                        // overlay collects the answer and the turn resumes
                        // via AppAction::StartStream once it closes.
                        return;
                    }
                    self.start_llm_stream(event_tx.clone(), done_tx.clone())
                        .await;
                } else if self.pending_tool_calls_count() > 0 {
                    let count = self.pending_tool_calls_count();
                    if self.is_tool_round_limit_reached() {
                        let msg = format!(
                            "Reached max tool rounds ({}) for this turn; {} pending tool call(s) ignored.",
                            self.max_tool_rounds(),
                            count
                        );
                        log::warn!("{}", msg);
                        self.add_event_message(msg);
                    }
                    self.clear_pending_tool_calls();
                } else {
                    log::info!("no pending tool call; turn complete");
                }
                self.persist_session();
            }
            Err(e) => {
                // Remove the empty assistant placeholder so a failed request does
                // not leave an invalid assistant message in the conversation.
                if self.has_empty_assistant_placeholder() {
                    self.messages.pop();
                }
                log::error!("llm stream error: {}", e);
                self.add_event_message(format!("LLM request failed: {}", e));
                self.set_error("LLM request failed".to_string());
                self.persist_session();
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
                history_path: Some(dir.to_path_buf()),
                log_path: None,
                log_level: "info".to_string(),
                skill_paths: None,
                auto_include_skills: false,
                agent_paths: None,
                models: crate::config::TierModelConfig {
                    performance: "test-model".to_string(),
                    efficient: None,
                },
            },
            shell: None,
            mcp: None,
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
    fn seed_session(
        dir: &std::path::Path,
        name: &str,
        messages: &[Message],
    ) -> crate::history::SessionStore {
        let store = crate::history::SessionStore::open(dir).unwrap();
        let id = store
            .create_session(name, "catus-seed", "test-model")
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
        let id = store.find_session("session").unwrap().unwrap();

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

        let id = store.find_session("existing").unwrap().unwrap();
        let snap = store.load_session(id).unwrap().unwrap();
        assert!(snap.messages.iter().any(|m| m.content == "new"));

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
        let names = store.list_sessions().unwrap();
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
            store.list_sessions().unwrap()[0].name.clone()
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
    fn input_history_recalls_newest_first() {
        let dir = std::env::temp_dir().join(format!("catus_hist_order_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        submit_message(&mut app, "first");
        submit_message(&mut app, "second");
        submit_message(&mut app, "third");

        assert_eq!(
            app.input_state.input_history,
            vec!["third", "second", "first"]
        );

        // Type a new draft line, then use Up/Down to recall history and restore it.
        app.input_state.input = "draft".to_string();
        app.input_state.history_previous();
        assert_eq!(app.input_state.input, "third");
        app.input_state.history_previous();
        assert_eq!(app.input_state.input, "second");
        app.input_state.history_next();
        assert_eq!(app.input_state.input, "third");
        app.input_state.history_next();
        assert_eq!(app.input_state.input, "draft");
        assert!(app.input_state.input_history_index.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consecutive_duplicate_inputs_are_not_stored_twice() {
        let dir = std::env::temp_dir().join(format!("catus_hist_dedup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        submit_message(&mut app, "same");
        submit_message(&mut app, "same");

        assert_eq!(app.input_state.input_history, vec!["same"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_command_completion_offers_resume() {
        let dir = std::env::temp_dir().join(format!("catus_slash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.input = "/res".to_string();
        app.input_state.recompute_candidates();

        assert_eq!(app.input_state.candidates, vec!["/resume"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_completion_filters_by_prefix_case_insensitively() {
        let dir = std::env::temp_dir().join(format!("catus_hist_complete_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        submit_message(&mut app, "Hello World");
        submit_message(&mut app, "hello there");
        submit_message(&mut app, "goodbye");

        app.input_state.input = "HEL".to_string();
        app.input_state.recompute_candidates();

        assert_eq!(
            app.input_state.candidates,
            vec!["hello there", "Hello World"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_cycles_through_candidates() {
        let dir = std::env::temp_dir().join(format!("catus_cycle_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.candidates =
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];

        app.input_state.cycle_candidate(1);
        assert_eq!(app.input_state.input, "alpha");
        assert_eq!(app.input_state.selected_candidate, Some(0));

        app.input_state.cycle_candidate(1);
        assert_eq!(app.input_state.input, "beta");

        app.input_state.cycle_candidate(-1);
        assert_eq!(app.input_state.input, "alpha");

        app.input_state.cycle_candidate(-1);
        assert_eq!(app.input_state.input, "gamma"); // wrap backward

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_resets_history_recall() {
        let dir = std::env::temp_dir().join(format!("catus_type_reset_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        submit_message(&mut app, "base");
        app.input_state.history_previous();
        assert!(app.input_state.input_history_index.is_some());

        app.input_state.push_char('x');
        assert!(app.input_state.input_history_index.is_none());
        assert_eq!(app.input_state.input, "basex");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_moves_and_inserts_at_cursor() {
        let dir = std::env::temp_dir().join(format!("catus_cursor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.push_char('a');
        app.input_state.push_char('b');
        app.input_state.push_char('c');
        assert_eq!(app.input_state.input, "abc");
        assert_eq!(app.input_state.cursor, 3);

        app.input_state.move_cursor_left();
        app.input_state.move_cursor_left();
        assert_eq!(app.input_state.cursor, 1);

        app.input_state.push_char('x');
        assert_eq!(app.input_state.input, "axbc");
        assert_eq!(app.input_state.cursor, 2);

        app.input_state.backspace();
        assert_eq!(app.input_state.input, "abc");
        assert_eq!(app.input_state.cursor, 1);

        app.input_state.move_cursor_home();
        assert_eq!(app.input_state.cursor, 0);
        app.input_state.move_cursor_end();
        assert_eq!(app.input_state.cursor, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn slash_resume_command_is_recorded_in_history() {
        let dir = std::env::temp_dir().join(format!("catus_cmd_hist_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_session(&dir, "foo", &[]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.input = "/resume foo".to_string();
        app.handle_command("/resume foo").await;

        assert_eq!(app.input_state.input_history, vec!["/resume foo"]);
        app.input_state.history_previous();
        assert_eq!(app.input_state.input, "/resume foo");

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
        assert!(app.handle_command("/status").await);
        assert_eq!(app.overlay_state.overlay, Overlay::Status);
        assert!(app.overlay_state.is_active());
        assert_eq!(app.usage.prompt_tokens, 19);

        // Esc closes it.
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Esc);
        assert!(!app.overlay_state.is_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bare_resume_opens_picker_and_enter_loads_history() {
        let dir = std::env::temp_dir().join(format!("catus_resume_overlay_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_session(&dir, "alpha", &[Message::user("hi")]);
        seed_session(&dir, "beta", &[]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/resume").await);
        let items = match &app.overlay_state.overlay {
            Overlay::Resume { items, selected } => {
                assert_eq!(items.len(), 2);
                assert_eq!(*selected, 0);
                items.clone()
            }
            other => panic!("expected resume overlay, got {:?}", other),
        };

        // Arrow keys move the selection with wrap-around.
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Down);
        assert_eq!(
            app.overlay_state.overlay,
            Overlay::Resume { items, selected: 1 }
        );
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Down);
        match &app.overlay_state.overlay {
            Overlay::Resume { selected, .. } => assert_eq!(*selected, 0),
            other => panic!("expected resume overlay, got {:?}", other),
        }

        // Enter yields the selected session name for the event loop to load.
        let action =
            crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Enter);
        match action {
            crate::ui::OverlayAction::LoadHistory(name) => {
                let store = crate::history::SessionStore::open(&dir).unwrap();
                assert!(store.find_session(&name).unwrap().is_some());
            }
            other => panic!("expected LoadHistory, got {:?}", other),
        }
        assert!(!app.overlay_state.is_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_picker_esc_closes_without_loading() {
        let dir = std::env::temp_dir().join(format!("catus_resume_esc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_session(&dir, "a", &[]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        let items = app.list_session_names();
        app.overlay_state.open_resume(items);
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Esc);
        assert_eq!(app.overlay_state.overlay, Overlay::None);
        assert!(app.current_session_id.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_with_name_argument_loads_directly_without_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_resume_arg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_session(&dir, "foo", &[]);

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/resume foo").await);
        assert_eq!(app.overlay_state.overlay, Overlay::None);
        assert_eq!(app.status_message, "session resumed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn config_command_opens_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_config_overlay_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/config").await);
        assert!(matches!(
            app.overlay_state.overlay,
            Overlay::Config { selected: 0 }
        ));

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
        assert!(!keys.iter().any(|k| k.starts_with("api.")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_overlay_enter_prefills_edit_command() {
        let dir = std::env::temp_dir().join(format!("catus_config_enter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.overlay_state.open_config();
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Enter);
        assert!(app.input_state.input.starts_with("/config set "));
        assert_eq!(app.overlay_state.overlay, Overlay::None);

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

        let msg = app.set_model("Other").unwrap();
        assert!(msg.contains("Other"));
        assert_eq!(app.current_model.id, "other-model");
        assert_eq!(app.client.session_id(), app.session_id);

        assert!(app.set_model("nosuch").is_err());
        // Failed switches keep the previous model.
        assert_eq!(app.current_model.id, "other-model");

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

    #[test]
    fn slash_command_completion_offers_status() {
        let dir = std::env::temp_dir().join(format!("catus_slash_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.input = "/st".to_string();
        app.input_state.recompute_candidates();
        assert_eq!(app.input_state.candidates, vec!["/status"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_command_completion_offers_help_and_exit() {
        let dir = std::env::temp_dir().join(format!("catus_slash_help_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.input = "/".to_string();
        app.input_state.recompute_candidates();
        assert!(app.input_state.candidates.contains(&"/help".to_string()));
        assert!(app.input_state.candidates.contains(&"/exit".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_command_completion_offers_mcp_and_subcommands() {
        let dir = std::env::temp_dir().join(format!("catus_slash_mcp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input_state.input = "/mc".to_string();
        app.input_state.recompute_candidates();
        assert_eq!(
            app.input_state.candidates,
            vec!["/mcp", "/mcp list", "/mcp status"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mcp_status_command_shows_connection_counts() {
        let dir = std::env::temp_dir().join(format!("catus_mcp_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/mcp status").await);
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
        assert!(app.handle_command("/help").await);
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
        assert!(app.handle_command("/help mcp list").await);
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
        assert!(app.handle_command("/help nosuch").await);
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
        assert!(app.handle_command("/exit").await);
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

    /// Helper that submits a user message directly without going through the TUI.
    fn submit_message(app: &mut App, text: &str) {
        app.input_state.input = text.to_string();
        app.submit_user_message();
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
        assert!(app.overlay_state.is_active());
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
            prompt: "Grant shell permission(s) \"NETWORK, WRITE\"?".to_string(),
            answer: crate::tool::Answer::One(GRANT_SESSION.to_string()),
        }];
        assert!(app.complete_interaction(answers));
        assert!(!app.has_pending_interaction());
        let last = app.messages.last().expect("tool result message");
        assert!(
            last.content
                .contains("granted: permission(s) \"NETWORK, WRITE\" for this session")
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
}

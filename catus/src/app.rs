//! Application state for the catus Agent TUI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::ShellState;
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::llm::{LlmClient, LlmError, Model, StreamEvent, Usage};
use crate::mcp::{McpManager, mcp_tools};
use crate::message::{Message, Role};
use crate::skills::SkillRegistry;
use crate::tool::{
    AskAnswer, AskQuestion, AskUserTool, ShellTool, SkillTool, Tool, ToolCall, ToolContext,
    ToolResult, Toolbox,
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
    /// Path of the history file currently being continued, if any.
    current_history_file: Option<std::path::PathBuf>,
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

        let (permissions, audit_logger) = config
            .shell
            .clone()
            .map(|s| (s.permission_policy(), s.audit_logger()))
            .unwrap_or_else(|| {
                let default = ShellConfig::default();
                (default.permission_policy(), default.audit_logger())
            });

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

        let system_prompt = Self::build_system_prompt(&config, &skill_registry);
        let max_tool_rounds = config.agent.max_tool_rounds;

        // Composition root: register the built-in tools; MCP tools are added
        // in `connect_mcp` as servers come online.
        let mut toolbox = Toolbox::default();
        toolbox.register(Box::new(ShellTool));
        toolbox.register(Box::new(SkillTool));
        toolbox.register(Box::new(AskUserTool));

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
            current_history_file: None,
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

    /// Build the initial system prompt, optionally appending the skill catalog.
    pub fn build_system_prompt(config: &AppConfig, registry: &SkillRegistry) -> String {
        let mut prompt = config.agent.system_prompt.clone();
        if config.agent.auto_include_skills && !registry.is_empty() {
            prompt.push_str("\n\nThe following Agent Skills are available. ");
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
                self.toolbox.register(Box::new(tool));
            }
            self.mcp_manager = Some(manager);
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
        true
    }

    /// Load conversation history from a JSON file and append it after the
    /// existing system prompt. Leading system messages in the file are skipped
    /// so the configured system prompt remains authoritative.
    pub fn load_history(
        &mut self,
        path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !path.exists() {
            return Ok(());
        }
        let contents = std::fs::read_to_string(path)?;
        let mut loaded: Vec<Message> = serde_json::from_str(&contents)?;

        // Drop any leading system messages from the loaded file.
        while let Some(first) = loaded.first() {
            if first.is_system() {
                loaded.remove(0);
            } else {
                break;
            }
        }

        // Drop invalid assistant placeholders created by older versions that
        // left empty assistant messages with no tool calls in history.
        loaded
            .retain(|m| !(m.role == Role::Assistant && m.content.is_empty() && !m.had_tool_calls));

        // Make sure the first message is the configured system prompt.
        if self
            .messages
            .first()
            .map(|m| !m.is_system())
            .unwrap_or(true)
        {
            self.messages.insert(0, Message::system(String::new()));
        }
        self.messages.extend(loaded);
        self.chat_state.scroll_to_bottom();
        Ok(())
    }

    /// Save the conversation history (including the system prompt) to a JSON
    /// file. Missing parent directories are created automatically.
    pub fn save_history(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(&self.messages)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Return the configured history directory, if any.
    fn history_dir(&self) -> Option<&std::path::Path> {
        self.config.agent.history_path.as_deref()
    }

    /// List available history JSON files in the configured history directory,
    /// sorted from newest to oldest by modification time.
    pub fn list_history_files(&self) -> Vec<std::path::PathBuf> {
        let dir = match self.history_dir() {
            Some(d) => d,
            None => return Vec::new(),
        };
        let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("json"))
                        .unwrap_or(false)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort_by(|a, b| {
            let ta = std::fs::metadata(a)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let tb = std::fs::metadata(b)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            tb.cmp(&ta)
        });
        files
    }

    /// Return a human-readable list of available history names for the status
    /// bar. Names are the file stems of JSON files in the history directory.
    pub fn history_names_list(&self) -> String {
        let names: Vec<String> = self
            .list_history_files()
            .iter()
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .collect();
        if names.is_empty() {
            "no saved histories".to_string()
        } else {
            format!("available: {}", names.join(", "))
        }
    }

    /// Resume a saved conversation. With no `name`, returns the list of
    /// available histories. With a name, loads `<history_dir>/<name>.json`,
    /// resets the current conversation to the configured system prompt plus the
    /// saved messages, and records the file as the current history file.
    pub fn resume_history(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let dir = self.history_dir().ok_or("history_path is not configured")?;

        let path = match name {
            None => {
                return Ok(self.history_names_list());
            }
            Some(n) => {
                let n = n.trim();
                let file_name = if n.ends_with(".json") {
                    n.to_string()
                } else {
                    format!("{}.json", n)
                };
                dir.join(file_name)
            }
        };

        if !path.exists() {
            return Err(format!("history file not found: {}", path.display()).into());
        }

        // Reset to the configured system prompt, then load the saved messages.
        let system_prompt = self.config.agent.system_prompt.clone();
        self.messages = vec![Message::system(system_prompt)];
        self.load_history(&path)?;
        self.current_history_file = Some(path);
        self.chat_state.scroll_to_bottom();
        Ok("history loaded".to_string())
    }

    /// Save the current session to the configured history directory. If a
    /// history file has been loaded with `/resume`, overwrite that file;
    /// otherwise create a new timestamped file.
    pub fn save_session_history(
        &self,
    ) -> Result<Option<std::path::PathBuf>, Box<dyn std::error::Error>> {
        let dir = match self.history_dir() {
            Some(d) => d,
            None => return Ok(None),
        };
        std::fs::create_dir_all(dir)?;

        let path = match &self.current_history_file {
            Some(p) => p.clone(),
            None => {
                let now = chrono::Local::now();
                let file_name = format!("history_{}.json", now.format("%Y-%m-%dT%H_%M_%S"));
                dir.join(file_name)
            }
        };

        self.save_history(&path)?;
        Ok(Some(path))
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
                system_prompt: "test prompt".to_string(),
                max_tool_rounds: 5,
                history_path: Some(dir.to_path_buf()),
                log_path: None,
                log_level: "info".to_string(),
                skill_paths: None,
                auto_include_skills: false,
            },
            shell: None,
            mcp: None,
        }
    }

    #[tokio::test]
    async fn resume_lists_available_histories() {
        let dir = std::env::temp_dir().join(format!("catus_resume_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let history = dir.join("alpha.json");
        let messages = vec![Message::user("hello")];
        std::fs::write(&history, serde_json::to_string(&messages).unwrap()).unwrap();

        let app = App::new(test_config_with_history_dir(&dir));
        let list = app.history_names_list();
        assert!(
            list.contains("alpha"),
            "list should contain alpha: {}",
            list
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_loads_history_and_sets_current_file() {
        let dir = std::env::temp_dir().join(format!("catus_resume_load_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let history = dir.join("session.json");
        let messages = vec![Message::user("previous"), Message::assistant("ok")];
        std::fs::write(&history, serde_json::to_string(&messages).unwrap()).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.current_history_file.is_none());

        app.resume_history(Some("session")).unwrap();
        assert_eq!(app.current_history_file, Some(history.clone()));
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

    #[test]
    fn save_session_history_overwrites_loaded_file() {
        let dir = std::env::temp_dir().join(format!("catus_resume_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let history = dir.join("existing.json");
        let messages = vec![Message::user("old")];
        std::fs::write(&history, serde_json::to_string(&messages).unwrap()).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.resume_history(Some("existing")).unwrap();
        app.messages.push(Message::user("new"));

        let saved = app.save_session_history().unwrap();
        assert_eq!(saved, Some(history.clone()));

        let loaded: Vec<Message> =
            serde_json::from_str(&std::fs::read_to_string(&history).unwrap()).unwrap();
        assert!(loaded.iter().any(|m| m.content == "new"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_session_history_creates_timestamped_file_for_new_session() {
        let dir = std::env::temp_dir().join(format!("catus_resume_new_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let app = App::new(test_config_with_history_dir(&dir));
        let saved = app.save_session_history().unwrap();
        assert!(saved.is_some());
        let saved = saved.unwrap();
        assert_eq!(saved.parent().unwrap(), dir);
        assert!(saved.exists());

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

    #[test]
    fn load_history_filters_empty_assistant_placeholders() {
        let dir = std::env::temp_dir().join(format!("catus_load_filter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let history = dir.join("bad.json");
        let messages = vec![
            Message::system("old".to_string()),
            Message::user("hello".to_string()),
            Message::assistant(String::new()),
            Message::event("LLM request failed".to_string()),
        ];
        std::fs::write(&history, serde_json::to_string(&messages).unwrap()).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.load_history(&history).unwrap();

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
        std::fs::write(dir.join("foo.json"), "[]").unwrap();

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
        std::fs::write(
            dir.join("alpha.json"),
            serde_json::to_string(&vec![Message::user("hi")]).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("beta.json"), "[]").unwrap();

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

        // Enter loads the selected history and closes the picker.
        let action =
            crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Enter);
        match action {
            crate::ui::OverlayAction::LoadHistory(name) => {
                let path = dir.join(format!("{}.json", name));
                assert!(path.exists());
            }
            other => panic!("expected LoadHistory, got {:?}", other),
        }
        assert!(!app.overlay_state.is_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_picker_esc_closes_without_loading() {
        let dir = std::env::temp_dir().join(format!("catus_resume_esc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), "[]").unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        let items = app
            .list_history_files()
            .iter()
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .collect();
        app.overlay_state.open_resume(items);
        crate::ui::overlay::handle_overlay_key(&mut app, crossterm::event::KeyCode::Esc);
        assert_eq!(app.overlay_state.overlay, Overlay::None);
        assert!(app.current_history_file.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_with_name_argument_loads_directly_without_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_resume_arg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foo.json"), "[]").unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/resume foo").await);
        assert_eq!(app.overlay_state.overlay, Overlay::None);
        assert_eq!(app.status_message, "history loaded");

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
system_prompt = "test prompt"
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
    fn completing_without_pending_interaction_is_a_noop() {
        let mut app = App::new(AppConfig::default());
        assert!(!app.complete_interaction(Vec::new()));
        assert!(!app.cancel_interaction());
        assert!(app.messages.iter().all(|m| m.role != Role::Tool));
    }
}

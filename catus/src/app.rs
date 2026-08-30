//! Application state for the catus Agent TUI.

use std::time::{Duration, Instant};

use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::ShellState;

use crossterm::event::KeyCode;

use crate::config::AppConfig;
use crate::llm::{LlmClient, Usage};
use crate::message::{Message, Role};
use crate::skills::SkillRegistry;
use crate::tool::{ToolCall, ToolResult, execute_shell_command};

/// Current high-level state of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Idle,
    Streaming,
    RunningTool,
    Error,
}

/// Modal page shown on top of the chat view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// No overlay; keys go to the input line.
    None,
    /// Interactive history picker. `items` are history file stems (newest
    /// first), `selected` is the highlighted entry.
    Resume { items: Vec<String>, selected: usize },
    /// Token usage details.
    Status,
    /// Config editor.
    Config { selected: usize },
    /// Skill picker.
    Skills { items: Vec<String>, selected: usize },
}

impl Overlay {
    pub fn is_active(&self) -> bool {
        !matches!(self, Overlay::None)
    }
}

/// Result of handling a key press while an overlay is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayResult {
    /// The key was consumed by the overlay; nothing else to do.
    Consumed,
    /// The overlay was closed without an action.
    Closed,
    /// The resume picker confirmed a history name to load.
    LoadHistory(String),
    /// The skill picker confirmed a skill name to activate.
    ActivateSkill(String),
}

/// Mutable application state shared between the TUI and async workers.
pub struct App {
    pub config: AppConfig,
    pub client: LlmClient,
    pub shell_state: ShellState,
    pub messages: Vec<Message>,
    pub input: String,
    /// Byte index of the cursor inside `input`. Always aligned to a UTF-8
    /// character boundary.
    pub cursor: usize,
    pub status: AppStatus,
    pub status_message: String,
    pub scroll: usize,
    pub auto_scroll: bool,
    pub max_tool_rounds: usize,
    /// Submitted user inputs in the current session, newest first.
    pub input_history: Vec<String>,
    /// Index into `input_history` when recalling a previous input.
    /// `None` means the user is editing a fresh line.
    pub input_history_index: Option<usize>,
    /// The line being typed before history recall started, restored by Down.
    pub draft_input: String,
    /// Current completion candidates shown below the input box.
    pub candidates: Vec<String>,
    /// Currently selected candidate index, if any.
    pub selected_candidate: Option<usize>,
    pending_tool_calls: Vec<ToolCall>,
    tool_rounds_this_turn: usize,
    /// Path of the history file currently being continued, if any.
    current_history_file: Option<std::path::PathBuf>,
    /// Cumulative token usage across all completed LLM requests.
    pub usage: Usage,
    /// Number of completed LLM requests in this session.
    pub request_count: usize,
    /// Modal overlay currently displayed on top of the chat view.
    pub overlay: Overlay,
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
}

/// Built-in TUI slash commands offered by command completion.
const SLASH_COMMANDS: &[&str] = &["/config", "/exit", "/help", "/resume", "/skill", "/status"];

/// Maximum number of completion candidates shown at once.
pub const MAX_CANDIDATES: usize = 8;

/// How long transient status-bar messages remain visible before clearing.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Return the previous UTF-8 character boundary before `idx`.
fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut pos = idx - 1;
    while !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Return the next UTF-8 character boundary at or after `idx`.
fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut pos = idx + 1;
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let client = LlmClient::new(
            config.api.base_url.clone(),
            config.api.api_key.clone(),
            config.api.model.clone(),
        );

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

        Self {
            config,
            client,
            shell_state,
            messages: vec![Message::system(system_prompt)],
            skill_registry,
            active_skills: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: AppStatus::Idle,
            status_message: String::new(),
            scroll: 0,
            auto_scroll: true,
            max_tool_rounds,
            input_history: Vec::new(),
            input_history_index: None,
            draft_input: String::new(),
            candidates: Vec::new(),
            selected_candidate: None,
            pending_tool_calls: Vec::new(),
            tool_rounds_this_turn: 0,
            current_history_file: None,
            usage: Usage::default(),
            request_count: 0,
            overlay: Overlay::None,
            should_quit: false,
            status_message_clear_at: None,
            config_path: AppConfig::find_config_file(),
        }
    }

    /// Build the initial system prompt, optionally appending the skill catalog.
    pub fn build_system_prompt(config: &AppConfig, registry: &SkillRegistry) -> String {
        let mut prompt = config.agent.system_prompt.clone();
        if config.agent.auto_include_skills && !registry.is_empty() {
            prompt.push_str("\n\nThe following Agent Skills are available. ");
            prompt.push_str("When a task matches a skill's description, activate it ");
            prompt.push_str("by saying 'use_skill:<name>' at the start of your reply, ");
            prompt.push_str("then follow the skill's instructions.\n\n");
            for (name, description) in registry.names_and_descriptions() {
                prompt.push_str(&format!("- {}: {}\n", name, description));
            }
        }
        prompt
    }

    pub fn push_char(&mut self, c: char) {
        self.input_history_index = None;
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.recompute_candidates();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.input_history_index = None;
        let prev = prev_char_boundary(&self.input, self.cursor);
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.recompute_candidates();
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.input, self.cursor);
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.input, self.cursor);
    }

    pub fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.input_history_index = None;
        self.draft_input.clear();
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Take the current input and append it as a user message.
    pub fn submit_user_message(&mut self) -> Option<String> {
        let text = self.input.trim();
        if text.is_empty() {
            return None;
        }
        let text = text.to_string();
        self.messages.push(Message::user(text.clone()));
        self.record_input_history(&text);

        self.input.clear();
        self.cursor = 0;
        self.input_history_index = None;
        self.draft_input.clear();
        self.selected_candidate = None;
        self.recompute_candidates();
        self.tool_rounds_this_turn = 0;
        self.scroll_to_bottom();
        Some(text)
    }

    /// Remember a submitted line for Up/Down recall.
    pub fn record_input_history(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.input_history.first().map(|s| s.as_str()) != Some(text) {
            self.input_history.insert(0, text.to_string());
        }
    }

    /// Recompute the completion candidate list based on the current input.
    fn recompute_candidates(&mut self) {
        self.candidates.clear();
        self.selected_candidate = None;

        if self.input.is_empty() {
            return;
        }

        if self.input.starts_with('/') {
            for &cmd in SLASH_COMMANDS {
                if cmd.starts_with(&self.input) && !self.candidates.contains(&cmd.to_string()) {
                    self.candidates.push(cmd.to_string());
                }
            }
        } else {
            let prefix = self.input.to_lowercase();
            for entry in &self.input_history {
                if entry.to_lowercase().starts_with(&prefix) && !self.candidates.contains(entry) {
                    self.candidates.push(entry.clone());
                    if self.candidates.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
        }
    }

    /// Recall the next older input from the session history (bound to Up).
    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.input_history_index {
            None => {
                self.draft_input = self.input.clone();
                self.input_history_index = Some(0);
            }
            Some(i) if i + 1 < self.input_history.len() => {
                self.input_history_index = Some(i + 1);
            }
            Some(_) => {}
        }

        if let Some(i) = self.input_history_index {
            self.input = self.input_history[i].clone();
            self.cursor = self.input.len();
        }
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Recall the next newer input, restoring the draft line at the top (bound to Down).
    pub fn history_next(&mut self) {
        match self.input_history_index {
            None => {}
            Some(0) => {
                self.input_history_index = None;
                self.input = self.draft_input.clone();
            }
            Some(i) => {
                self.input_history_index = Some(i - 1);
                self.input = self.input_history[i - 1].clone();
            }
        }
        self.cursor = self.input.len();
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Cycle through completion candidates by `delta` positions and fill the
    /// input box with the selected candidate. Wraps around at both ends.
    pub fn cycle_candidate(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }

        let idx = match self.selected_candidate {
            None if delta >= 0 => 0usize,
            None => self.candidates.len() - 1,
            Some(i) => {
                let len = self.candidates.len() as isize;
                let next = (i as isize + delta).rem_euclid(len);
                next as usize
            }
        };

        self.selected_candidate = Some(idx);
        self.input = self.candidates[idx].clone();
        self.cursor = self.input.len();
        self.input_history_index = None;
        // Candidates stay valid because the new input matches the prefix.
    }

    /// Clear the active candidate selection without changing the input.
    pub fn clear_candidate_selection(&mut self) {
        self.selected_candidate = None;
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
        self.scroll_to_bottom();

        if let Some(last) = self.messages.last() {
            log::info!(
                "assistant finished: content_len={} had_tool_calls={} pending_tool_calls={}",
                last.content.len(),
                last.had_tool_calls,
                self.pending_tool_calls.len()
            );
        }

        // Check for a skill activation marker in the assistant's text.
        if let Some(name) = self.take_skill_activation_marker() {
            match self.activate_skill(&name) {
                Ok(msg) => self.set_transient_message(msg),
                Err(e) => self.set_error(e.to_string()),
            }
        }

        if self.has_empty_assistant_placeholder() {
            log::warn!("assistant response was empty; dropping placeholder message");
            self.messages.pop();
            self.messages.push(Message::event(
                "Assistant returned an empty response".to_string(),
            ));
            self.scroll_to_bottom();
        }
    }

    /// If the most recent assistant message starts with `use_skill:<name>`,
    /// remove that prefix and return the skill name.
    pub fn take_skill_activation_marker(&mut self) -> Option<String> {
        let content = self.messages.last().and_then(|last| {
            if last.role == Role::Assistant {
                Some(last.content.clone())
            } else {
                None
            }
        })?;

        let trimmed = content.trim_start();
        let prefix = "use_skill:";
        if !trimmed.starts_with(prefix) {
            return None;
        }
        let rest = &trimmed[prefix.len()..];
        let name = rest
            .split_whitespace()
            .next()
            .unwrap_or(rest)
            .trim()
            .to_string();
        if name.is_empty() {
            return None;
        }

        // Remove the marker from the message content.
        let marker_end = content.find(prefix).unwrap_or(0) + prefix.len() + rest.len()
            - rest.trim_start().len()
            + name.len();
        let after_marker = content[marker_end..].trim_start().to_string();

        if let Some(last) = self.messages.last_mut() {
            last.content = after_marker;
        }

        // Ignore re-activation of a skill that is already active.
        if self.active_skills.contains(&name) {
            return None;
        }

        Some(name)
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

    /// Whether a modal overlay is currently displayed.
    pub fn overlay_active(&self) -> bool {
        self.overlay.is_active()
    }

    /// Open the history picker overlay with available history names.
    pub fn open_resume_overlay(&mut self) {
        let items = self
            .list_history_files()
            .iter()
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .collect();
        self.overlay = Overlay::Resume { items, selected: 0 };
    }

    /// Open the token usage overlay.
    pub fn open_status_overlay(&mut self) {
        self.overlay = Overlay::Status;
    }

    /// Open the config editor overlay.
    pub fn open_config_overlay(&mut self) {
        self.overlay = Overlay::Config { selected: 0 };
    }

    /// Open the skill picker overlay.
    pub fn open_skills_overlay(&mut self) {
        let items = self.skill_registry.iter().map(|s| s.name.clone()).collect();
        self.overlay = Overlay::Skills { items, selected: 0 };
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
            ("api.base_url".to_string(), self.config.api.base_url.clone()),
            ("api.api_key".to_string(), self.config.api.api_key.clone()),
            ("api.model".to_string(), self.config.api.model.clone()),
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
            "api.base_url" => self.config.api.base_url = value.to_string(),
            "api.api_key" => self.config.api.api_key = value.to_string(),
            "api.model" => self.config.api.model = value.to_string(),
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

        // API changes need a refreshed client.
        if key.starts_with("api.") {
            self.client = LlmClient::new(
                self.config.api.base_url.clone(),
                self.config.api.api_key.clone(),
                self.config.api.model.clone(),
            );
        }

        self.config.save(&path)?;
        Ok(format!("saved {} to {}", key, path.display()))
    }

    /// Close any active overlay.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Handle a key press while an overlay is active. Keys never reach the
    /// input line while an overlay is open.
    pub fn handle_overlay_key(&mut self, code: KeyCode) -> OverlayResult {
        match self.overlay.clone() {
            Overlay::None => OverlayResult::Consumed,
            Overlay::Status => match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.close_overlay();
                    OverlayResult::Closed
                }
                _ => OverlayResult::Consumed,
            },
            Overlay::Resume { items, selected } => match code {
                KeyCode::Up => {
                    let next = if items.is_empty() {
                        0
                    } else {
                        (selected + items.len() - 1) % items.len()
                    };
                    self.overlay = Overlay::Resume {
                        items,
                        selected: next,
                    };
                    OverlayResult::Consumed
                }
                KeyCode::Down => {
                    let next = if items.is_empty() {
                        0
                    } else {
                        (selected + 1) % items.len()
                    };
                    self.overlay = Overlay::Resume {
                        items,
                        selected: next,
                    };
                    OverlayResult::Consumed
                }
                KeyCode::Enter => {
                    let chosen = items.get(selected).cloned();
                    self.close_overlay();
                    match chosen {
                        Some(name) => OverlayResult::LoadHistory(name),
                        None => OverlayResult::Closed,
                    }
                }
                KeyCode::Esc => {
                    self.close_overlay();
                    OverlayResult::Closed
                }
                _ => OverlayResult::Consumed,
            },
            Overlay::Skills { items, selected } => match code {
                KeyCode::Up => {
                    let next = if items.is_empty() {
                        0
                    } else {
                        (selected + items.len() - 1) % items.len()
                    };
                    self.overlay = Overlay::Skills {
                        items,
                        selected: next,
                    };
                    OverlayResult::Consumed
                }
                KeyCode::Down => {
                    let next = if items.is_empty() {
                        0
                    } else {
                        (selected + 1) % items.len()
                    };
                    self.overlay = Overlay::Skills {
                        items,
                        selected: next,
                    };
                    OverlayResult::Consumed
                }
                KeyCode::Enter => {
                    let chosen = items.get(selected).cloned();
                    self.close_overlay();
                    match chosen {
                        Some(name) => OverlayResult::ActivateSkill(name),
                        None => OverlayResult::Closed,
                    }
                }
                KeyCode::Esc => {
                    self.close_overlay();
                    OverlayResult::Closed
                }
                _ => OverlayResult::Consumed,
            },
            Overlay::Config { selected } => {
                let fields = self.config_fields();
                match code {
                    KeyCode::Up => {
                        let next = if fields.is_empty() {
                            0
                        } else {
                            (selected + fields.len() - 1) % fields.len()
                        };
                        self.overlay = Overlay::Config { selected: next };
                        OverlayResult::Consumed
                    }
                    KeyCode::Down => {
                        let next = if fields.is_empty() {
                            0
                        } else {
                            (selected + 1) % fields.len()
                        };
                        self.overlay = Overlay::Config { selected: next };
                        OverlayResult::Consumed
                    }
                    KeyCode::Enter => {
                        if let Some((key, value)) = fields.get(selected) {
                            self.input = format!("/config set {} {}", key, value);
                            self.cursor = self.input.len();
                            self.recompute_candidates();
                        }
                        self.close_overlay();
                        OverlayResult::Closed
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.close_overlay();
                        OverlayResult::Closed
                    }
                    _ => OverlayResult::Consumed,
                }
            }
        }
    }

    /// Move the resume picker selection with the mouse wheel. Returns true if
    /// the scroll was consumed by an overlay.
    pub fn handle_overlay_scroll(&mut self, up: bool) -> bool {
        match &self.overlay {
            Overlay::Resume { .. } | Overlay::Config { .. } | Overlay::Skills { .. } => {
                let _ = self.handle_overlay_key(if up { KeyCode::Up } else { KeyCode::Down });
                true
            }
            _ => false,
        }
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
        self.scroll_to_bottom();
    }

    /// Echo a shell command into the history area.
    pub fn echo_shell_command(&mut self, command: &str) {
        log::info!("running shell command: {}", command);
        self.messages
            .push(Message::event(format!("shell: {}", command)));
        self.scroll_to_bottom();
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
    pub fn run_pending_tool(&mut self) -> Option<String> {
        let call = self.pending_tool_calls.first()?.clone();

        let Some(command) = call.shell_command() else {
            // Malformed arguments: report back to the model and consume the
            // call instead of leaving it pending forever.
            log::warn!("malformed tool call: {}", call.arguments);
            let message = format!(
                "status=2\nstdout=```\n\n```\nstderr=```\ncatus: tool call format error: arguments must be a single JSON object {{\"command\": \"<shell command>\"}} with exactly one string \"command\" field; got: {}\n```",
                call.arguments
            );
            self.messages
                .push(Message::tool(message.clone(), call.id.clone()));
            self.pending_tool_calls.remove(0);
            self.scroll_to_bottom();
            return Some(message);
        };

        self.status = AppStatus::RunningTool;
        self.status_message = format!("Running: {}", command);
        self.echo_shell_command(&command);

        let output = execute_shell_command(&command, &mut self.shell_state);
        log::info!(
            "shell command finished: status={} stdout_len={} stderr_len={}",
            output.status,
            output.stdout.len(),
            output.stderr.len()
        );
        let result = ToolResult {
            call: call.clone(),
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        };
        let message = result.to_message();
        self.messages
            .push(Message::tool(message.clone(), call.id.clone()));
        self.pending_tool_calls.remove(0);
        self.tool_rounds_this_turn += 1;

        self.status = AppStatus::Idle;
        self.status_message.clear();
        self.scroll_to_bottom();

        Some(message)
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        if self.scroll == 0 {
            self.auto_scroll = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.auto_scroll = true;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = usize::MAX;
        self.auto_scroll = false;
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
        self.scroll_to_bottom();
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
        self.scroll_to_bottom();
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
    pub fn handle_command(&mut self, input: &str) -> bool {
        if !input.starts_with('/') {
            return false;
        }

        let rest = input[1..].trim();
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();

        match cmd {
            "help" => {
                let help_text = format!(
                    "Commands:\n\
                     {cmds}\n\
                     Keys: Enter send, Tab/↑↓ complete, PgUp/PgDn scroll, Ctrl+C quit",
                    cmds = SLASH_COMMANDS.join(", ")
                );
                self.add_event_message(help_text);
                self.set_transient_message("Help displayed");
                self.record_input_history(input);
            }
            "exit" => {
                self.should_quit = true;
            }
            "config" => {
                let arg = arg.map(str::trim).filter(|s| !s.is_empty());
                match arg {
                    Some(args) => {
                        let mut set_parts = args.splitn(3, ' ');
                        let sub = set_parts.next().unwrap_or("");
                        if sub == "set" {
                            let key = set_parts.next().unwrap_or("");
                            let value = set_parts.next().unwrap_or("");
                            if key.is_empty() {
                                self.set_error("usage: /config set <key> <value>");
                            } else {
                                match self.set_config_field(key, value) {
                                    Ok(msg) => {
                                        self.status = AppStatus::Idle;
                                        self.set_transient_message(msg);
                                    }
                                    Err(e) => self.set_error(e.to_string()),
                                }
                            }
                        } else {
                            self.set_error(format!(
                                "unknown /config subcommand: {}. Try /config set <key> <value>",
                                sub
                            ));
                        }
                        self.record_input_history(input);
                    }
                    None => {
                        self.status = AppStatus::Idle;
                        self.open_config_overlay();
                    }
                }
            }
            "resume" => {
                let arg = arg.map(str::trim).filter(|s| !s.is_empty());
                match arg {
                    // `/resume <name>` loads the history directly.
                    Some(name) => match self.resume_history(Some(name)) {
                        Ok(msg) => {
                            self.status = AppStatus::Idle;
                            self.set_transient_message(msg);
                        }
                        Err(e) => self.set_error(e.to_string()),
                    },
                    // Bare `/resume` opens the interactive history picker.
                    None => {
                        self.status = AppStatus::Idle;
                        self.open_resume_overlay();
                    }
                }
                self.record_input_history(input);
            }
            "status" => {
                self.status = AppStatus::Idle;
                self.open_status_overlay();
                self.record_input_history(input);
            }
            "skill" => {
                self.status = AppStatus::Idle;
                let arg = arg.map(str::trim).filter(|s| !s.is_empty());
                match arg {
                    Some(args) => {
                        let mut parts = args.splitn(2, ' ');
                        let sub = parts.next().unwrap_or("");
                        let sub_arg = parts.next();
                        match sub {
                            "list" => {
                                self.add_event_message(self.skill_names_list());
                                self.set_transient_message("Skills listed");
                            }
                            "use" => match sub_arg {
                                Some(name) => match self.activate_skill(name.trim()) {
                                    Ok(msg) => self.set_transient_message(msg),
                                    Err(e) => self.set_error(e.to_string()),
                                },
                                None => self.set_error("usage: /skill use <name>"),
                            },
                            _ => self.set_error(format!(
                                "unknown /skill subcommand: {}. Try /skill list or /skill use <name>",
                                sub
                            )),
                        }
                        self.record_input_history(input);
                    }
                    None => {
                        self.open_skills_overlay();
                    }
                }
            }
            _ => self.set_error(format!("unknown command: /{}", cmd)),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config_with_history_dir(dir: &std::path::Path) -> AppConfig {
        AppConfig {
            api: crate::config::ApiConfig {
                base_url: "https://example.com".to_string(),
                api_key: "test".to_string(),
                model: "test".to_string(),
            },
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
        }
    }

    #[test]
    fn resume_lists_available_histories() {
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

        assert_eq!(app.input_history, vec!["third", "second", "first"]);

        // Type a new draft line, then use Up/Down to recall history and restore it.
        app.input = "draft".to_string();
        app.history_previous();
        assert_eq!(app.input, "third");
        app.history_previous();
        assert_eq!(app.input, "second");
        app.history_next();
        assert_eq!(app.input, "third");
        app.history_next();
        assert_eq!(app.input, "draft");
        assert!(app.input_history_index.is_none());

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

        assert_eq!(app.input_history, vec!["same"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_command_completion_offers_resume() {
        let dir = std::env::temp_dir().join(format!("catus_slash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input = "/res".to_string();
        app.recompute_candidates();

        assert_eq!(app.candidates, vec!["/resume"]);

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

        app.input = "HEL".to_string();
        app.recompute_candidates();

        assert_eq!(app.candidates, vec!["hello there", "Hello World"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_cycles_through_candidates() {
        let dir = std::env::temp_dir().join(format!("catus_cycle_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.candidates = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];

        app.cycle_candidate(1);
        assert_eq!(app.input, "alpha");
        assert_eq!(app.selected_candidate, Some(0));

        app.cycle_candidate(1);
        assert_eq!(app.input, "beta");

        app.cycle_candidate(-1);
        assert_eq!(app.input, "alpha");

        app.cycle_candidate(-1);
        assert_eq!(app.input, "gamma"); // wrap backward

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_resets_history_recall() {
        let dir = std::env::temp_dir().join(format!("catus_type_reset_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        submit_message(&mut app, "base");
        app.history_previous();
        assert!(app.input_history_index.is_some());

        app.push_char('x');
        assert!(app.input_history_index.is_none());
        assert_eq!(app.input, "basex");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_moves_and_inserts_at_cursor() {
        let dir = std::env::temp_dir().join(format!("catus_cursor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.push_char('a');
        app.push_char('b');
        app.push_char('c');
        assert_eq!(app.input, "abc");
        assert_eq!(app.cursor, 3);

        app.move_cursor_left();
        app.move_cursor_left();
        assert_eq!(app.cursor, 1);

        app.push_char('x');
        assert_eq!(app.input, "axbc");
        assert_eq!(app.cursor, 2);

        app.backspace();
        assert_eq!(app.input, "abc");
        assert_eq!(app.cursor, 1);

        app.move_cursor_home();
        assert_eq!(app.cursor, 0);
        app.move_cursor_end();
        assert_eq!(app.cursor, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_resume_command_is_recorded_in_history() {
        let dir = std::env::temp_dir().join(format!("catus_cmd_hist_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foo.json"), "[]").unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input = "/resume foo".to_string();
        app.handle_command("/resume foo");

        assert_eq!(app.input_history, vec!["/resume foo"]);
        app.history_previous();
        assert_eq!(app.input, "/resume foo");

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

    #[test]
    fn status_command_opens_overlay_with_usage() {
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
        assert!(app.handle_command("/status"));
        assert_eq!(app.overlay, Overlay::Status);
        assert!(app.overlay_active());
        assert_eq!(app.usage.prompt_tokens, 19);

        // Esc closes it.
        assert_eq!(app.handle_overlay_key(KeyCode::Esc), OverlayResult::Closed);
        assert!(!app.overlay_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_resume_opens_picker_and_enter_loads_history() {
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
        assert!(app.handle_command("/resume"));
        let items = match &app.overlay {
            Overlay::Resume { items, selected } => {
                assert_eq!(items.len(), 2);
                assert_eq!(*selected, 0);
                items.clone()
            }
            other => panic!("expected resume overlay, got {:?}", other),
        };

        // Arrow keys move the selection with wrap-around.
        app.handle_overlay_key(KeyCode::Down);
        assert_eq!(app.overlay, Overlay::Resume { items, selected: 1 });
        app.handle_overlay_key(KeyCode::Down);
        match &app.overlay {
            Overlay::Resume { selected, .. } => assert_eq!(*selected, 0),
            other => panic!("expected resume overlay, got {:?}", other),
        }

        // Enter loads the selected history and closes the picker.
        let result = app.handle_overlay_key(KeyCode::Enter);
        match result {
            OverlayResult::LoadHistory(name) => {
                let path = dir.join(format!("{}.json", name));
                assert!(path.exists());
            }
            other => panic!("expected LoadHistory, got {:?}", other),
        }
        assert!(!app.overlay_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_picker_esc_closes_without_loading() {
        let dir = std::env::temp_dir().join(format!("catus_resume_esc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), "[]").unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.open_resume_overlay();
        assert_eq!(app.handle_overlay_key(KeyCode::Esc), OverlayResult::Closed);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.current_history_file.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_with_name_argument_loads_directly_without_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_resume_arg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foo.json"), "[]").unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/resume foo"));
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.status_message, "history loaded");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_command_opens_overlay() {
        let dir = std::env::temp_dir().join(format!("catus_config_overlay_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/config"));
        assert!(matches!(app.overlay, Overlay::Config { selected: 0 }));

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
        assert!(keys.contains(&"api.base_url"));
        assert!(keys.contains(&"api.model"));
        assert!(keys.contains(&"agent.max_tool_rounds"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_overlay_enter_prefills_edit_command() {
        let dir = std::env::temp_dir().join(format!("catus_config_enter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.open_config_overlay();
        app.handle_overlay_key(KeyCode::Enter);
        assert!(app.input.starts_with("/config set "));
        assert_eq!(app.overlay, Overlay::None);

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
[api]
base_url = "https://example.com/v1"
api_key = "test"
model = "test-model"

[agent]
system_prompt = "test prompt"
max_tool_rounds = 5
log_level = "info"
"#;
        std::fs::write(&config_path, initial).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.config_path = Some(config_path.clone());
        app.set_config_field("api.model", "gpt-4o")
            .expect("set_config_field should succeed");
        assert_eq!(app.config.api.model, "gpt-4o");

        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("gpt-4o"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_pending_tool_reports_malformed_arguments() {
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

        let result = app.run_pending_tool();
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
        app.input = "/st".to_string();
        app.recompute_candidates();
        assert_eq!(app.candidates, vec!["/status"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_command_completion_offers_help_and_exit() {
        let dir = std::env::temp_dir().join(format!("catus_slash_help_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.input = "/".to_string();
        app.recompute_candidates();
        assert!(app.candidates.contains(&"/help".to_string()));
        assert!(app.candidates.contains(&"/exit".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_command_shows_help_in_history() {
        let dir = std::env::temp_dir().join(format!("catus_help_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/help"));
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("/help"))
        );
        assert!(app.status_message.contains("Help"));
        assert!(app.status_message_clear_at.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_command_requests_quit() {
        let dir = std::env::temp_dir().join(format!("catus_exit_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        assert!(app.handle_command("/exit"));
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

    #[test]
    fn skill_activation_marker_is_extracted_and_removed() {
        let dir = std::env::temp_dir().join(format!("catus_skill_marker_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.messages.push(Message::assistant(
            "use_skill:my-skill\n\ndo it".to_string(),
        ));

        let name = app.take_skill_activation_marker();
        assert_eq!(name, Some("my-skill".to_string()));
        assert_eq!(app.messages.last().unwrap().content, "do it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_skill_activation_marker_is_ignored_when_already_active() {
        let dir = std::env::temp_dir().join(format!("catus_skill_repeat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(test_config_with_history_dir(&dir));
        app.active_skills.push("my-skill".to_string());
        app.messages.push(Message::assistant(
            "use_skill:my-skill\n\ndo it".to_string(),
        ));

        let name = app.take_skill_activation_marker();
        assert_eq!(name, None);
        assert_eq!(app.messages.last().unwrap().content, "do it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper that submits a user message directly without going through the TUI.
    fn submit_message(app: &mut App, text: &str) {
        app.input = text.to_string();
        app.submit_user_message();
    }
}

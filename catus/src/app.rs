//! Application state for the catus Agent TUI.

use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::ShellState;

use crate::config::AppConfig;
use crate::llm::LlmClient;
use crate::message::{Message, Role};
use crate::tool::{ToolCall, ToolResult, execute_shell_command};

/// Current high-level state of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Idle,
    Streaming,
    RunningTool,
    Error,
}

/// Mutable application state shared between the TUI and async workers.
pub struct App {
    pub config: AppConfig,
    pub client: LlmClient,
    pub shell_state: ShellState,
    pub messages: Vec<Message>,
    pub input: String,
    pub status: AppStatus,
    pub status_message: String,
    pub scroll: usize,
    pub auto_scroll: bool,
    pub max_tool_rounds: usize,
    pending_tool_calls: Vec<ToolCall>,
    tool_rounds_this_turn: usize,
    /// Path of the history file currently being continued, if any.
    current_history_file: Option<std::path::PathBuf>,
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

        let system_prompt = config.agent.system_prompt.clone();
        let max_tool_rounds = config.agent.max_tool_rounds;

        Self {
            config,
            client,
            shell_state,
            messages: vec![Message::system(system_prompt)],
            input: String::new(),
            status: AppStatus::Idle,
            status_message: String::new(),
            scroll: 0,
            auto_scroll: true,
            max_tool_rounds,
            pending_tool_calls: Vec::new(),
            tool_rounds_this_turn: 0,
            current_history_file: None,
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn backspace(&mut self) {
        self.input.pop();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    /// Take the current input and append it as a user message.
    pub fn submit_user_message(&mut self) -> Option<String> {
        let text = self.input.trim();
        if text.is_empty() {
            return None;
        }
        let text = text.to_string();
        self.messages.push(Message::user(text.clone()));
        self.input.clear();
        self.tool_rounds_this_turn = 0;
        self.scroll_to_bottom();
        Some(text)
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

        if self.has_empty_assistant_placeholder() {
            log::warn!("assistant response was empty; dropping placeholder message");
            self.messages.pop();
            self.messages.push(Message::event(
                "Assistant returned an empty response".to_string(),
            ));
            self.scroll_to_bottom();
        }
    }

    /// Return true if the most recent message is an empty assistant placeholder
    /// (no content and no tool calls).
    pub fn has_empty_assistant_placeholder(&self) -> bool {
        self.messages
            .last()
            .map(|last| {
                last.role == Role::Assistant && last.content.is_empty() && !last.had_tool_calls
            })
            .unwrap_or(false)
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = AppStatus::Error;
        self.status_message = msg.into();
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
        let command = call.shell_command()?;

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
            "resume" => {
                let result = self.resume_history(arg);
                match result {
                    Ok(msg) => {
                        self.status = AppStatus::Idle;
                        self.status_message = msg;
                    }
                    Err(e) => self.set_error(e.to_string()),
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
}

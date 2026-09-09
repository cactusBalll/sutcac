//! Extensible slash-command interface and registry.
//!
//! Adding a new slash command now requires only two steps:
//!
//! 1. Create a small struct in [`crate::app::commands`] and implement the
//!    [`SlashCommand`] trait for it.
//! 2. Register the struct in [`crate::app::commands::BUILT_IN_REGISTRY`].
//!
//! The command name, completion candidates, help text, and execution are all
//! declared in the same place, so the command cannot accidentally drift out of
//! sync with completion or help.
//!
//! Commands are frontend-neutral: they mutate core session state and return
//! a [`CommandOutcome`]. Presentation intents (which modal page a graphical
//! frontend may want to open) are expressed through [`UiRequest`] instead of
//! reaching into any specific UI layer.

use futures::future::BoxFuture;

use crate::app::App;

/// A presentation intent produced by a slash command.
///
/// Frontends decide how to realize it: the TUI opens the matching modal
/// overlay, a Web frontend may push a page, a headless frontend ignores it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "page", rename_all = "snake_case")]
pub enum UiRequest {
    /// Show the session status page.
    ShowStatus,
    /// Show the config editor.
    ShowConfig,
    /// Show the saved-session picker with the given names (newest first).
    ShowResumePicker { items: Vec<String> },
    /// Show the skill picker with the given skill names.
    ShowSkills { items: Vec<String> },
    /// Show the model picker. `selected` is the highlighted entry.
    ShowModels { items: Vec<String>, selected: usize },
    /// Show the agent picker.
    ShowAgents { items: Vec<String> },
    /// Show the running-subagent picker.
    ShowSubagents { items: Vec<String> },
    /// Open the subagent monitor page, focused on the given subagent id.
    WatchSubagent { id: String },
    /// Show the subagent close picker with the given subagent ids.
    CloseSubagentPicker { items: Vec<String> },
}

/// Result of executing a slash command, returned to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CommandOutcome {
    /// Whether the input was treated as a command (even an unknown one).
    pub handled: bool,
    /// Whether the frontend should record the full input line in its
    /// input history.
    pub record_history: bool,
    /// Presentation intent for frontends with modal pages.
    pub ui: Option<UiRequest>,
}

impl CommandOutcome {
    /// A handled command with no presentation intent.
    pub fn handled() -> Self {
        Self {
            handled: true,
            record_history: false,
            ui: None,
        }
    }

    /// An input that was not a command.
    pub fn not_handled() -> Self {
        Self {
            handled: false,
            record_history: false,
            ui: None,
        }
    }
}

/// Error produced while executing a slash command.
#[derive(Debug)]
pub struct CommandError(pub String);

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CommandError {}

impl From<String> for CommandError {
    fn from(s: String) -> Self {
        CommandError(s)
    }
}

impl From<&str> for CommandError {
    fn from(s: &str) -> Self {
        CommandError(s.to_string())
    }
}

impl From<Box<dyn std::error::Error>> for CommandError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        CommandError(e.to_string())
    }
}

/// A slash command such as `/help` or `/mcp list`.
///
/// Each command is a stateless object held by [`CommandRegistry`]. Execution is
/// asynchronous because some commands need to await MCP server responses.
///
/// `execute` returns `Ok(Some(request))` when the frontend should realize a
/// presentation intent (e.g. open a picker page); plain informational
/// commands return `Ok(None)`.
pub trait SlashCommand: Send + Sync {
    /// Primary command name without the leading slash, e.g. `"help"`.
    fn name(&self) -> &'static str;

    /// Alternative names that also invoke this command.
    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    /// One-line description shown in the general `/help` listing.
    fn description(&self) -> &'static str;

    /// Usage line shown in the general `/help` listing, e.g.
    /// `"/config [set <key> <value>]"`.
    fn usage(&self) -> &'static str;

    /// Static subcommand names offered for completion, e.g. `["list", "status"]`.
    fn subcommands(&self) -> &[&'static str] {
        &[]
    }

    /// Return help text for this command. `subcommand` is the subcommand the
    /// user asked about, if any.
    ///
    /// The default implementation formats the usage line and description;
    /// commands may override it for richer, subcommand-specific help.
    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some(sub) if self.subcommands().contains(&sub) => {
                format!("Usage: /{} {}\n{}", self.name(), sub, self.description())
            }
            Some(sub) => format!("Unknown subcommand '{}' for /{}", sub, self.name()),
            None => format!("Usage: {}\n{}", self.usage(), self.description()),
        }
    }

    /// Whether the full command input should be recorded in session history
    /// after successful execution.
    fn record_history(&self, _args: Option<&str>) -> bool {
        true
    }

    /// Execute the command. `args` is the remainder of the input line after the
    /// command name (and the separating space).
    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<UiRequest>, CommandError>>;
}

/// Registry of slash commands.
pub struct CommandRegistry {
    commands: Vec<Box<dyn SlashCommand>>,
}

impl CommandRegistry {
    /// Build a registry from a list of commands.
    pub fn new(commands: Vec<Box<dyn SlashCommand>>) -> Self {
        Self { commands }
    }

    /// Find a command by primary name or alias.
    pub fn find_command(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands.iter().find_map(|c| {
            if c.name() == name || c.aliases().contains(&name) {
                Some(c.as_ref())
            } else {
                None
            }
        })
    }

    /// Iterate over all registered commands.
    pub fn all_commands(&self) -> &[Box<dyn SlashCommand>] {
        &self.commands
    }

    /// Return the general help text listing every registered command.
    pub fn general_help(&self) -> String {
        let mut lines = vec!["Commands:".to_string()];
        for cmd in &self.commands {
            lines.push(format!("  {} - {}", cmd.usage(), cmd.description()));
        }
        lines.push("Keys: Enter send, Tab/↑↓ complete, PgUp/PgDn scroll, Ctrl+C quit".to_string());
        lines.join("\n")
    }

    /// Return completion candidates matching `prefix`.
    ///
    /// Candidates include the base command form (`/mcp`) and any static
    /// subcommands (`/mcp list`, `/mcp status`). The `/help` command is special:
    /// it can take another command name (and that command's subcommands) as an
    /// argument, so candidates like `/help mcp list` are also generated.
    pub fn completion_candidates(&self, prefix: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        if !prefix.starts_with('/') {
            return candidates;
        }

        // Snapshot the names/subcommands first so we do not borrow `self`
        // recursively while generating `/help ...` forms below.
        let command_data: Vec<(&str, &[&str])> = self
            .commands
            .iter()
            .map(|c| (c.name(), c.subcommands()))
            .collect();

        for (name, subs) in &command_data {
            let base = format!("/{}", name);
            if base.starts_with(prefix) && !candidates.contains(&base) {
                candidates.push(base);
            }
            for sub in *subs {
                let full = format!("/{} {}", name, sub);
                if full.starts_with(prefix) && !candidates.contains(&full) {
                    candidates.push(full);
                }
            }
        }

        // /help accepts any other command (and its subcommands) as an argument.
        if self.find_command("help").is_some() {
            let help_base = "/help".to_string();
            if help_base.starts_with(prefix) && !candidates.contains(&help_base) {
                candidates.push(help_base);
            }
            for (name, subs) in &command_data {
                let full = format!("/help {}", name);
                if full.starts_with(prefix) && !candidates.contains(&full) {
                    candidates.push(full);
                }
                for sub in *subs {
                    let full = format!("/help {} {}", name, sub);
                    if full.starts_with(prefix) && !candidates.contains(&full) {
                        candidates.push(full);
                    }
                }
            }
        }

        candidates.sort();
        candidates.dedup();
        candidates
    }

    /// Parse `input` and dispatch it to the matching command, if any.
    /// Returns the outcome describing how the input was handled; errors are
    /// also surfaced through the app's error status for display frontends.
    pub async fn handle_command(&self, app: &mut App, input: &str) -> CommandOutcome {
        if !input.starts_with('/') {
            return CommandOutcome::not_handled();
        }

        let rest = input[1..].trim();
        let mut parts = rest.splitn(2, ' ');
        let cmd_name = parts.next().unwrap_or("");
        let args = parts.next();

        match self.find_command(cmd_name) {
            Some(cmd) => match cmd.execute(app, args).await {
                Ok(ui) => CommandOutcome {
                    handled: true,
                    record_history: cmd.record_history(args),
                    ui,
                },
                Err(CommandError(msg)) => {
                    app.set_error(msg);
                    CommandOutcome {
                        handled: true,
                        record_history: false,
                        ui: None,
                    }
                }
            },
            None => {
                if !cmd_name.is_empty() {
                    app.set_error(format!("unknown command: /{}", cmd_name));
                } else {
                    app.set_error("unknown command: /".to_string());
                }
                CommandOutcome {
                    handled: true,
                    record_history: false,
                    ui: None,
                }
            }
        }
    }
}

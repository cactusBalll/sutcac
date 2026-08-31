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

use futures::future::BoxFuture;

use crate::app::App;

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
    ) -> BoxFuture<'a, Result<(), CommandError>>;
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
    /// Returns `true` when the input started with `/` and was treated as a
    /// command, even if the command itself is unknown.
    pub async fn handle_command(&self, app: &mut App, input: &str) -> bool {
        if !input.starts_with('/') {
            return false;
        }

        let rest = input[1..].trim();
        let mut parts = rest.splitn(2, ' ');
        let cmd_name = parts.next().unwrap_or("");
        let args = parts.next();

        match self.find_command(cmd_name) {
            Some(cmd) => match cmd.execute(app, args).await {
                Ok(()) => {
                    if cmd.record_history(args) {
                        app.input_state.record_history(input);
                    }
                    true
                }
                Err(CommandError(msg)) => {
                    app.set_error(msg);
                    true
                }
            },
            None => {
                if !cmd_name.is_empty() {
                    app.set_error(format!("unknown command: /{}", cmd_name));
                } else {
                    app.set_error("unknown command: /".to_string());
                }
                true
            }
        }
    }
}

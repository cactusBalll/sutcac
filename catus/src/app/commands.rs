//! Built-in TUI slash commands.
//!
//! Each command is implemented as a small struct implementing the
//! [`SlashCommand`] trait defined in [`crate::app::command`]. To add a new
//! command, implement the trait and register the struct in
//! [`BUILT_IN_REGISTRY`].

use std::sync::LazyLock;

use futures::future::BoxFuture;

use crate::app::App;
use crate::app::AppStatus;
use crate::app::command::{CommandError, CommandRegistry, SlashCommand};

/// Show help for slash commands.
pub struct HelpCommand;

impl SlashCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "Show help for slash commands"
    }

    fn usage(&self) -> &'static str {
        "/help [command] [subcommand]"
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            let text = match args.map(str::trim).filter(|s| !s.is_empty()) {
                Some(args) => {
                    let mut parts = args.splitn(2, ' ');
                    let cmd_name = parts.next().unwrap_or("");
                    let sub = parts.next();
                    match BUILT_IN_REGISTRY.find_command(cmd_name) {
                        Some(cmd) => cmd.help(sub),
                        None => format!("unknown command: /{}", cmd_name),
                    }
                }
                None => BUILT_IN_REGISTRY.general_help(),
            };
            app.add_event_message(text);
            app.set_transient_message("Help displayed");
            Ok(())
        })
    }
}

/// Quit the application.
pub struct ExitCommand;

impl SlashCommand for ExitCommand {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn description(&self) -> &'static str {
        "Quit catus"
    }

    fn usage(&self) -> &'static str {
        "/exit"
    }

    fn record_history(&self, _args: Option<&str>) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        _args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.should_quit = true;
            Ok(())
        })
    }
}

/// Edit configuration fields.
pub struct ConfigCommand;

impl SlashCommand for ConfigCommand {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "Edit configuration"
    }

    fn usage(&self) -> &'static str {
        "/config [set <key> <value>]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["set"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("set") => "Usage: /config set <key> <value>\n\
                Set a configuration field and save it to the config file."
                .to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /config", sub),
            None => format!("Usage: {}\n{}", self.usage(), self.description()),
        }
    }

    fn record_history(&self, args: Option<&str>) -> bool {
        args.is_some()
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            match args.map(str::trim).filter(|s| !s.is_empty()) {
                Some(args) => {
                    let mut set_parts = args.splitn(3, ' ');
                    let sub = set_parts.next().unwrap_or("");
                    if sub == "set" {
                        let key = set_parts.next().unwrap_or("");
                        let value = set_parts.next().unwrap_or("");
                        if key.is_empty() {
                            return Err("usage: /config set <key> <value>".into());
                        }
                        let msg = app.set_config_field(key, value)?;
                        app.set_transient_message(msg);
                    } else {
                        return Err(format!(
                            "unknown /config subcommand: {}. Try /config set <key> <value>",
                            sub
                        )
                        .into());
                    }
                }
                None => app.overlay_state.open_config(),
            }
            Ok(())
        })
    }
}

/// Resume a saved conversation.
pub struct ResumeCommand;

impl SlashCommand for ResumeCommand {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn description(&self) -> &'static str {
        "Resume a saved conversation"
    }

    fn usage(&self) -> &'static str {
        "/resume [name]"
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            match args.map(str::trim).filter(|s| !s.is_empty()) {
                Some(name) => {
                    let msg = app.resume_history(Some(name))?;
                    app.set_transient_message(msg);
                }
                None => {
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
                }
            }
            Ok(())
        })
    }
}

/// Show token usage and session status.
pub struct StatusCommand;

impl SlashCommand for StatusCommand {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show token usage and session status"
    }

    fn usage(&self) -> &'static str {
        "/status"
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        _args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            app.overlay_state.open_status();
            Ok(())
        })
    }
}

/// Manage Agent Skills.
pub struct SkillCommand;

impl SlashCommand for SkillCommand {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Manage Agent Skills"
    }

    fn usage(&self) -> &'static str {
        "/skill [list | use <name>]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["list", "use"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("list") => "Usage: /skill list\nList discovered Agent Skills.".to_string(),
            Some("use") => "Usage: /skill use <name>\nActivate a skill by name.".to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /skill", sub),
            None => format!("Usage: {}\n{}", self.usage(), self.description()),
        }
    }

    fn record_history(&self, args: Option<&str>) -> bool {
        args.is_some()
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            match args.map(str::trim).filter(|s| !s.is_empty()) {
                Some(args) => {
                    let mut parts = args.splitn(2, ' ');
                    let sub = parts.next().unwrap_or("");
                    let sub_arg = parts.next();
                    match sub {
                        "list" => {
                            app.add_event_message(app.skill_names_list());
                            app.set_transient_message("Skills listed");
                        }
                        "use" => match sub_arg {
                            Some(name) => {
                                let msg = app.activate_skill(name.trim())?;
                                app.set_transient_message(msg);
                            }
                            None => return Err("usage: /skill use <name>".into()),
                        },
                        _ => {
                            return Err(format!(
                                "unknown /skill subcommand: {}. Try /skill list or /skill use <name>",
                                sub
                            )
                            .into());
                        }
                    }
                }
                None => {
                    let items = app.skill_registry.iter().map(|s| s.name.clone()).collect();
                    app.overlay_state.open_skills(items);
                }
            }
            Ok(())
        })
    }
}

/// Show MCP servers and tools.
pub struct McpCommand;

impl SlashCommand for McpCommand {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn description(&self) -> &'static str {
        "Show MCP servers and tools"
    }

    fn usage(&self) -> &'static str {
        "/mcp [list | status]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["list", "status"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("list") => {
                "Usage: /mcp list\nList configured MCP servers and available tools.".to_string()
            }
            Some("status") => "Usage: /mcp status\nShow MCP server connection status.".to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /mcp", sub),
            None => format!("Usage: {}\n{}", self.usage(), self.description()),
        }
    }

    fn record_history(&self, args: Option<&str>) -> bool {
        args.is_some()
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            match args.map(str::trim).filter(|s| !s.is_empty()) {
                Some(args) => {
                    let mut parts = args.splitn(2, ' ');
                    let sub = parts.next().unwrap_or("");
                    match sub {
                        "list" => {
                            app.add_event_message(app.mcp_server_list().await);
                            app.set_transient_message("MCP servers listed");
                        }
                        "status" => {
                            app.add_event_message(app.mcp_status_message());
                            app.set_transient_message("MCP status listed");
                        }
                        _ => {
                            return Err(format!(
                                "unknown /mcp subcommand: {}. Try /mcp list or /mcp status",
                                sub
                            )
                            .into());
                        }
                    }
                }
                None => {
                    app.add_event_message(app.mcp_status_message());
                    app.set_transient_message("MCP status listed");
                }
            }
            Ok(())
        })
    }
}

/// Built-in slash commands available in the TUI.
///
/// The registry is initialized lazily on first access.
pub static BUILT_IN_REGISTRY: LazyLock<CommandRegistry> = LazyLock::new(|| {
    CommandRegistry::new(vec![
        Box::new(HelpCommand),
        Box::new(ExitCommand),
        Box::new(ConfigCommand),
        Box::new(ResumeCommand),
        Box::new(StatusCommand),
        Box::new(SkillCommand),
        Box::new(McpCommand),
    ])
});

/// Handle a TUI slash command. Returns `true` if the input started with `/`
/// and has been handled as a command.
pub async fn handle_command(app: &mut App, input: &str) -> bool {
    BUILT_IN_REGISTRY.handle_command(app, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_expected_commands() {
        let names: Vec<&str> = BUILT_IN_REGISTRY
            .all_commands()
            .iter()
            .map(|c| c.name())
            .collect();
        assert!(names.contains(&"help"));
        assert!(names.contains(&"mcp"));
        assert!(names.contains(&"skill"));
    }

    #[test]
    fn completion_candidates_include_parameterized_forms() {
        let candidates = BUILT_IN_REGISTRY.completion_candidates("/mcp");
        assert!(candidates.contains(&"/mcp".to_string()));
        assert!(candidates.contains(&"/mcp list".to_string()));
        assert!(candidates.contains(&"/mcp status".to_string()));
    }

    #[test]
    fn completion_candidates_include_help_forms() {
        let candidates = BUILT_IN_REGISTRY.completion_candidates("/help ");
        assert!(candidates.contains(&"/help mcp".to_string()));
        assert!(candidates.contains(&"/help mcp list".to_string()));
        assert!(candidates.contains(&"/help config set".to_string()));
    }

    #[test]
    fn command_help_supports_subcommands() {
        let mcp = BUILT_IN_REGISTRY.find_command("mcp").unwrap();
        assert!(mcp.help(Some("list")).contains("/mcp list"));
        assert!(mcp.help(Some("status")).contains("/mcp status"));
        assert!(mcp.help(Some("nosuch")).contains("Unknown subcommand"));
    }

    #[test]
    fn help_command_uses_command_help() {
        let help = BUILT_IN_REGISTRY.find_command("help").unwrap();
        let text = help.help(None);
        assert!(text.contains("/help [command] [subcommand]"));
    }
}

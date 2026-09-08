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
                    let msg = app.resume_history(Some(name)).await?;
                    app.set_transient_message(msg);
                }
                None => {
                    let items = app.list_session_names();
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

/// Manage Agent definitions and subagents.
pub struct AgentCommand;

impl SlashCommand for AgentCommand {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn aliases(&self) -> &[&'static str] {
        &["agents"]
    }

    fn description(&self) -> &'static str {
        "Manage Agent definitions and running subagents"
    }

    fn usage(&self) -> &'static str {
        "/agent [list | status | use <name> <task> | watch <id>]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["list", "status", "use", "watch", "close"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("list") => "Usage: /agent list\nList discovered Agent definitions.".to_string(),
            Some("status") => "Usage: /agent status\nList running subagents and their state.".to_string(),
            Some("use") => "Usage: /agent use <name> <task> [fork|create]\nDispatch a task to a subagent manually.".to_string(),
            Some("watch") => "Usage: /agent watch <id>\nSwitch the chat view to a subagent's conversation.".to_string(),
            Some("close") => "Usage: /agent close <id>\nRemove a completed subagent from the status list.".to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /agent", sub),
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
                    let mut parts = args.splitn(4, ' ');
                    let sub = parts.next().unwrap_or("");
                    match sub {
                        "list" => {
                            app.add_event_message(app.agent_names_list());
                            app.set_transient_message("Agents listed");
                        }
                        "status" => {
                            app.add_event_message(app.subagent_status_list());
                            app.set_transient_message("Subagent status listed");
                        }
                        "use" => {
                            let name = parts
                                .next()
                                .ok_or("usage: /agent use <name> <task> [fork|create]")?;
                            let rest: Vec<&str> = parts.collect();
                            if rest.is_empty() {
                                return Err("usage: /agent use <name> <task> [fork|create]".into());
                            }
                            let mode_str = rest
                                .last()
                                .copied()
                                .filter(|s| *s == "fork" || *s == "create");
                            let task = if mode_str.is_some() {
                                rest[..rest.len() - 1].join(" ")
                            } else {
                                rest.join(" ")
                            };
                            let mode = mode_str
                                .unwrap_or("create")
                                .parse::<crate::subagent::SubagentContextMode>()?;
                            let msg = app.spawn_subagent(name, &task, mode)?;
                            app.set_transient_message(msg);
                        }
                        "watch" => match parts.next() {
                            Some(id) => {
                                let msg = app.watch_subagent(id)?;
                                app.set_transient_message(msg);
                            }
                            None => {
                                let items: Vec<String> =
                                    app.subagents.list().iter().map(|s| s.id.clone()).collect();
                                app.overlay_state.open_subagent_status(items);
                            }
                        },
                        "close" => {
                            let id = parts.next().ok_or("usage: /agent close <id>")?;
                            if app.subagents.remove(id) {
                                if app.current_subagent_view.as_deref() == Some(id) {
                                    app.watch_main_agent();
                                }
                                app.set_transient_message(format!("closed subagent {}", id));
                            } else {
                                return Err(format!("subagent not found: {}", id).into());
                            }
                        }
                        _ => {
                            return Err(format!(
                                "unknown /agent subcommand: {}. Try /agent list, status, use, watch, or close",
                                sub
                            )
                            .into());
                        }
                    }
                }
                None => {
                    let items: Vec<String> =
                        app.agent_registry.iter().map(|a| a.name.clone()).collect();
                    app.overlay_state.open_agents(items);
                }
            }
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

/// Switch the active model.
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "model"
    }

    fn description(&self) -> &'static str {
        "Switch the active model"
    }

    fn usage(&self) -> &'static str {
        "/model [name]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["list"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("list") => "Usage: /model list\nList configured models.".to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /model", sub),
            None => "Usage: /model [name]\n\
                With no argument, open the model picker. With a name, switch\nto the configured \
                model directly."
                .to_string(),
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
                    if args == "list" {
                        app.add_event_message(app.model_names_list());
                        app.set_transient_message("Models listed");
                    } else {
                        let msg = app.set_model(args)?;
                        app.set_transient_message(msg);
                    }
                }
                None => {
                    let items: Vec<String> = app.models.iter().map(|m| m.id.clone()).collect();
                    let selected = app
                        .models
                        .iter()
                        .position(|m| m.id == app.current_model.id)
                        .unwrap_or(0);
                    app.overlay_state.open_model(items, selected);
                }
            }
            Ok(())
        })
    }
}

/// Show or adjust the current session's shell permissions.
pub struct PermissionCommand;

impl SlashCommand for PermissionCommand {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn description(&self) -> &'static str {
        "Show or adjust the current session's shell permissions"
    }

    fn usage(&self) -> &'static str {
        "/permission [grant <tags> | revoke <tags> | reset]"
    }

    fn subcommands(&self) -> &[&'static str] {
        &["grant", "revoke", "reset"]
    }

    fn help(&self, subcommand: Option<&str>) -> String {
        match subcommand {
            Some("grant") => "Usage: /permission grant <tags>\n\
                 Grant permission tags to the main agent's shell for this session.\n\
                 Tags are case-insensitive and may be comma-separated, e.g. `network,write`."
                .to_string(),
            Some("revoke") => "Usage: /permission revoke <tags>\n\
                 Revoke previously granted session permission tags (comma-separated)."
                .to_string(),
            Some("reset") => "Usage: /permission reset\n\
                 Reset the session policy to the configured default, dropping all session grants."
                .to_string(),
            Some(sub) => format!("Unknown subcommand '{}' for /permission", sub),
            None => format!(
                "Usage: {}\n\n\
                 Without arguments, shows the current permission policy.\n\
                 Grants apply to the main agent only; subagents always use the permissions\n\
                 declared in their agent definition.",
                self.usage()
            ),
        }
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
                    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());
                    match (sub, rest) {
                        ("grant", Some(tags)) => {
                            app.grant_session_permissions(tags);
                            app.add_event_message(format!(
                                "granted session permissions: {}",
                                tags.to_ascii_uppercase()
                            ));
                            app.set_transient_message("session permissions granted");
                        }
                        ("revoke", Some(tags)) => {
                            app.revoke_session_permissions(tags);
                            app.add_event_message(format!(
                                "revoked session permissions: {}",
                                tags.to_ascii_uppercase()
                            ));
                            app.set_transient_message("session permissions revoked");
                        }
                        ("reset", None) => {
                            app.reset_session_permissions();
                            app.add_event_message(
                                "session permissions reset to the configured default".to_string(),
                            );
                            app.set_transient_message("session permissions reset");
                        }
                        ("grant", None) | ("revoke", None) => {
                            return Err(format!("usage: /permission {} <tags>", sub).into());
                        }
                        ("reset", Some(_)) => {
                            return Err("usage: /permission reset".into());
                        }
                        _ => {
                            return Err(format!(
                                "unknown /permission subcommand: {}. Try /permission grant|revoke|reset",
                                sub
                            )
                            .into());
                        }
                    }
                }
                None => {
                    app.add_event_message(app.permission_summary());
                    app.set_transient_message("session permissions listed");
                }
            }
            Ok(())
        })
    }
}

/// Switch the current session's shell permissions to allow-all.
pub struct AutoCommand;

impl SlashCommand for AutoCommand {
    fn name(&self) -> &'static str {
        "auto"
    }

    fn description(&self) -> &'static str {
        "Switch the current session's shell permissions to allow_all"
    }

    fn usage(&self) -> &'static str {
        "/auto"
    }

    fn help(&self, _subcommand: Option<&str>) -> String {
        "Usage: /auto\n\n\
         Sets the main agent's session policy to allow_all and clears session grants.\n\
         Path restrictions from the config are kept. Subagents always use the permissions\n\
         declared in their agent definition."
            .to_string()
    }

    fn execute<'a>(
        &'a self,
        app: &'a mut App,
        _args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), CommandError>> {
        Box::pin(async move {
            app.status = AppStatus::Idle;
            app.set_session_permissions_allow_all();
            app.add_event_message(
                "session permissions switched to allow_all (path restrictions kept)".to_string(),
            );
            app.set_transient_message("session permissions: allow_all");
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
                            app.add_event_message(app.mcp_server_list());
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
        Box::new(ModelCommand),
        Box::new(AgentCommand),
        Box::new(SkillCommand),
        Box::new(PermissionCommand),
        Box::new(AutoCommand),
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
        assert!(names.contains(&"model"));
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

    #[tokio::test]
    async fn model_command_lists_and_switches_models() {
        let mut config = crate::config::AppConfig {
            providers: vec![crate::llm::Provider {
                name: "test".to_string(),
                base_url: "https://example.com".to_string(),
                api_key: "test".to_string(),
                session_header: None,
            }],
            models: vec![
                crate::config::ModelEntry {
                    id: "model-a".to_string(),
                    name: "Model A".to_string(),
                    context_window: 4096,
                    provider: "test".to_string(),
                },
                crate::config::ModelEntry {
                    id: "model-b".to_string(),
                    name: "Model B".to_string(),
                    context_window: 8192,
                    provider: "test".to_string(),
                },
            ],
            ..Default::default()
        };
        config.agent.auto_include_skills = false;
        let mut app = App::new(config);
        assert_eq!(app.current_model.id, "model-a");

        // Bare /model opens the picker with the current model selected.
        assert!(app.handle_command("/model").await);
        assert!(matches!(
            app.overlay_state.overlay,
            crate::app::Overlay::Model { .. }
        ));

        assert!(app.handle_command("/model list").await);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_event() && m.content.contains("Model B"))
        );

        assert!(app.handle_command("/model Model B").await);
        assert_eq!(app.current_model.id, "model-b");
        assert!(app.status_message.contains("Model B"));

        assert!(app.handle_command("/model nosuch").await);
        assert!(
            app.status_message.contains("model not found"),
            "unexpected status: {}",
            app.status_message
        );
        assert_eq!(app.current_model.id, "model-b");
    }

    #[tokio::test]
    async fn permission_command_shows_and_adjusts_session_policy() {
        use crate::config::AppConfig;
        use sutcac_sh::permissions::{Permission, PermissionSet};

        fn custom_network() -> PermissionSet {
            let mut set = PermissionSet::empty();
            set.insert(Permission::Custom("NETWORK".to_string()));
            set
        }

        let mut app = App::new(AppConfig::default());
        app.shell_state.permissions =
            sutcac_sh::permissions::PermissionPolicy::parse("deny:network,write").unwrap();

        // Bare /permission shows the current policy.
        assert!(app.handle_command("/permission").await);
        assert!(
            app.messages
                .iter()
                .any(|m| m.content.contains("mode: deny:NETWORK,WRITE"))
        );

        // Grant tags for the session.
        assert!(app.handle_command("/permission grant network,write").await);
        assert!(app.shell_state.permissions.check(&custom_network()).is_ok());
        assert!(
            app.shell_state
                .permissions
                .check(&PermissionSet::write())
                .is_ok()
        );
        assert!(app.messages.iter().any(|m| {
            m.content
                .contains("granted session permissions: NETWORK,WRITE")
        }));

        // Revoke them again.
        assert!(app.handle_command("/permission revoke network,write").await);
        assert!(
            app.shell_state
                .permissions
                .check(&custom_network())
                .is_err()
        );
        assert!(
            app.shell_state
                .permissions
                .check(&PermissionSet::write())
                .is_err()
        );

        // Missing argument is an error.
        assert!(app.handle_command("/permission grant").await);
        assert!(
            app.status_message
                .contains("usage: /permission grant <tags>")
        );

        // Unknown subcommand is an error.
        assert!(app.handle_command("/permission bogus").await);
        assert!(
            app.status_message
                .contains("unknown /permission subcommand")
        );
    }

    #[tokio::test]
    async fn auto_command_switches_session_policy_to_allow_all() {
        use crate::config::AppConfig;
        use sutcac_sh::permissions::{Permission, PermissionSet};

        let mut app = App::new(AppConfig::default());
        app.shell_state.permissions =
            sutcac_sh::permissions::PermissionPolicy::parse("deny:network").unwrap();
        app.shell_state.permissions.grant_tag("network");

        assert!(app.handle_command("/auto").await);
        assert!(app.messages.iter().any(|m| m.content.contains("allow_all")));
        let mut network = PermissionSet::empty();
        network.insert(Permission::Custom("NETWORK".to_string()));
        assert!(app.shell_state.permissions.check(&network).is_ok());
        // Session grants are cleared along with the mode switch.
        assert!(app.shell_state.permissions.session_grants.is_empty());
    }
}

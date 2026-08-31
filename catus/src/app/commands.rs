//! Built-in TUI slash commands.

use crate::app::{App, AppStatus};

/// Built-in TUI slash commands offered by command completion.
pub const SLASH_COMMANDS: &[&str] = &[
    "/config", "/exit", "/help", "/mcp", "/resume", "/skill", "/status",
];

/// Handle a TUI slash command. Returns `true` if the input was a command
/// and should not be sent to the LLM.
pub async fn handle_command(app: &mut App, input: &str) -> bool {
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
            app.add_event_message(help_text);
            app.set_transient_message("Help displayed");
            app.input_state.record_history(input);
        }
        "exit" => {
            app.should_quit = true;
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
                            app.set_error("usage: /config set <key> <value>");
                        } else {
                            match app.set_config_field(key, value) {
                                Ok(msg) => {
                                    app.status = AppStatus::Idle;
                                    app.set_transient_message(msg);
                                }
                                Err(e) => app.set_error(e.to_string()),
                            }
                        }
                    } else {
                        app.set_error(format!(
                            "unknown /config subcommand: {}. Try /config set <key> <value>",
                            sub
                        ));
                    }
                    app.input_state.record_history(input);
                }
                None => {
                    app.status = AppStatus::Idle;
                    app.overlay_state.open_config();
                }
            }
        }
        "resume" => {
            let arg = arg.map(str::trim).filter(|s| !s.is_empty());
            match arg {
                Some(name) => match app.resume_history(Some(name)) {
                    Ok(msg) => {
                        app.status = AppStatus::Idle;
                        app.set_transient_message(msg);
                    }
                    Err(e) => app.set_error(e.to_string()),
                },
                None => {
                    app.status = AppStatus::Idle;
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
            app.input_state.record_history(input);
        }
        "status" => {
            app.status = AppStatus::Idle;
            app.overlay_state.open_status();
            app.input_state.record_history(input);
        }
        "skill" => {
            app.status = AppStatus::Idle;
            let arg = arg.map(str::trim).filter(|s| !s.is_empty());
            match arg {
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
                            Some(name) => match app.activate_skill(name.trim()) {
                                Ok(msg) => app.set_transient_message(msg),
                                Err(e) => app.set_error(e.to_string()),
                            },
                            None => app.set_error("usage: /skill use <name>"),
                        },
                        _ => app.set_error(format!(
                            "unknown /skill subcommand: {}. Try /skill list or /skill use <name>",
                            sub
                        )),
                    }
                    app.input_state.record_history(input);
                }
                None => {
                    let items = app.skill_registry.iter().map(|s| s.name.clone()).collect();
                    app.overlay_state.open_skills(items);
                }
            }
        }
        "mcp" => {
            app.status = AppStatus::Idle;
            let arg = arg.map(str::trim).filter(|s| !s.is_empty());
            match arg {
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
                        _ => app.set_error(format!(
                            "unknown /mcp subcommand: {}. Try /mcp list or /mcp status",
                            sub
                        )),
                    }
                    app.input_state.record_history(input);
                }
                None => {
                    app.add_event_message(app.mcp_status_message());
                    app.set_transient_message("MCP status listed");
                }
            }
        }
        _ => app.set_error(format!("unknown command: /{}", cmd)),
    }
    true
}

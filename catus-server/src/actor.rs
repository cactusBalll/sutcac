//! The runtime actor: the single owner of [`Runtime`].
//!
//! This mirrors `catus-web/src-tauri/src/actor.rs`: one task owns the
//! `Runtime`, frontend actions arrive as [`Command`]s on an mpsc channel,
//! and runtime events/snapshots are broadcast to WebSocket clients.
//! The Tauri bridge differs only in how events leave the process (Tauri
//! `emit` vs the axum broadcast channel).

use std::fs::OpenOptions;

use log::LevelFilter;
use simplelog::{Config, WriteLogger};
use tokio::sync::{broadcast, mpsc, oneshot};

use catus_core::app::{AppSnapshot, InputLineOutcome, RuntimeEvent};
use catus_core::bootstrap::{bootstrap_runtime, load_config};
use catus_core::config::AppConfig;
use catus_core::message::Message;
use catus_core::runtime::Runtime;
use catus_core::tool::AskAnswer;

use crate::BroadcastEvent;

/// A frontend action, executed by the actor against the owned `Runtime`.
#[derive(Debug)]
pub enum Command {
    /// Submit one input line (message or slash command).
    SendInput {
        text: String,
        reply: oneshot::Sender<InputLineOutcome>,
    },
    /// Answer the paused interaction and resume the turn.
    CompleteInteraction {
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<bool>,
    },
    /// Cancel the paused interaction and resume the turn.
    CancelInteraction { reply: oneshot::Sender<bool> },
    /// A picker/monitor action copied from the TUI's `handle_overlay_result`.
    OverlayAction {
        action: String,
        value: String,
        reply: oneshot::Sender<String>,
    },
    /// Pull the full serializable session state.
    GetSnapshot { reply: oneshot::Sender<AppSnapshot> },
    /// Pull one subagent's messages for the monitor page.
    GetSubagentMessages {
        id: String,
        reply: oneshot::Sender<Vec<Message>>,
    },
    /// Set a config field in one scope (workspace or global).
    SetConfigField {
        scope: catus_core::config::ConfigScope,
        key: String,
        value: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Remove a config field from one scope.
    RemoveConfigField {
        scope: catus_core::config::ConfigScope,
        key: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Persist the session and shut down the process.
    Shutdown,
}

/// Run the actor until the command channel closes, the session quits, or
/// startup fails. Runtime events and snapshots are broadcast on `events`.
pub async fn run(
    mut cmd_rx: mpsc::Receiver<Command>,
    events: broadcast::Sender<BroadcastEvent>,
) -> Result<(), String> {
    let config: AppConfig = load_config()?;
    init_logger(&config.effective_log_path(), config.effective_log_level());

    let mut runtime = bootstrap_runtime(config, false).await?;

    // Initial state so the UI can render before any interaction.
    emit_snapshot(&events, runtime.app.snapshot());

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                match command {
                    None => break,
                    Some(cmd) => {
                        if handle_command(&events, &mut runtime, cmd).await {
                            break;
                        }
                    }
                }
            }
            event = runtime.next_event() => {
                match event {
                    None => break,
                    Some(event) => handle_runtime_event(&events, &mut runtime, event).await,
                }
            }
        }
    }

    runtime.app.await_memory_passes().await;
    runtime.app.persist_session();
    Ok(())
}

fn init_logger(path: &std::path::Path, level: LevelFilter) {
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = WriteLogger::init(level, Config::default(), file);
    }
}

/// Expand the `@agent_name <task>` shorthand, mirroring the TUI input layer.
fn expand_agent_shorthand(input: &str) -> String {
    let input = input.trim();
    let Some(rest) = input.strip_prefix('@') else {
        return input.to_string();
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return input.to_string();
    }
    let mut parts = rest.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let task = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        return input.to_string();
    }
    format!("/agent use {} {} create", name, task)
}

/// Execute one frontend command. Returns `true` when the actor should stop.
async fn handle_command(
    events: &broadcast::Sender<BroadcastEvent>,
    runtime: &mut Runtime,
    command: Command,
) -> bool {
    match command {
        Command::SendInput { text, reply } => {
            let input = expand_agent_shorthand(&text);
            let outcome = runtime.app.handle_input_line(&input).await;
            let quit = runtime.app.should_quit;
            match &outcome {
                InputLineOutcome::Submitted => {
                    runtime.maybe_resume_stream().await;
                }
                // Commands mutate status/error messages; refresh the UI.
                InputLineOutcome::Handled(_) => {
                    emit_snapshot(events, runtime.app.snapshot());
                }
                InputLineOutcome::Empty => {}
            }
            let _ = reply.send(outcome);
            if quit {
                return true;
            }
        }
        Command::CompleteInteraction { answers, reply } => {
            let resumed = runtime.app.complete_interaction(answers);
            if resumed {
                runtime.maybe_resume_stream().await;
            }
            let _ = reply.send(resumed);
        }
        Command::CancelInteraction { reply } => {
            let resumed = runtime.app.cancel_interaction();
            if resumed {
                runtime.maybe_resume_stream().await;
            }
            let _ = reply.send(resumed);
        }
        Command::OverlayAction {
            action,
            value,
            reply,
        } => {
            let message = apply_overlay_action(runtime, &action, &value).await;
            emit_snapshot(events, runtime.app.snapshot());
            let _ = reply.send(message);
        }
        Command::GetSnapshot { reply } => {
            let _ = reply.send(runtime.app.snapshot());
        }
        Command::GetSubagentMessages { id, reply } => {
            let messages = runtime
                .app
                .subagents
                .list()
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.messages.clone())
                .unwrap_or_default();
            let _ = reply.send(messages);
        }
        Command::SetConfigField {
            scope,
            key,
            value,
            reply,
        } => {
            let result = runtime.app.set_config_field_in(scope, &key, &value);
            match &result {
                Ok(msg) => runtime.app.set_transient_message(msg),
                Err(e) => runtime.app.set_error(&e.to_string()),
            }
            emit_snapshot(events, runtime.app.snapshot());
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        Command::RemoveConfigField { scope, key, reply } => {
            let result = runtime.app.remove_config_field_in(scope, &key);
            match &result {
                Ok(msg) => runtime.app.set_transient_message(msg),
                Err(e) => runtime.app.set_error(&e.to_string()),
            }
            emit_snapshot(events, runtime.app.snapshot());
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        Command::Shutdown => {
            return true;
        }
    }
    false
}

/// Apply a picker action (the web analogue of the TUI's
/// `handle_overlay_result`). Returns the status message for the UI.
async fn apply_overlay_action(runtime: &mut Runtime, action: &str, value: &str) -> String {
    match action {
        "resume_history" => match runtime.app.resume_history(Some(value)).await {
            Ok(msg) => {
                runtime.app.status = catus_core::app::AppStatus::Idle;
                runtime.app.set_transient_message(msg.as_str());
                msg
            }
            Err(e) => {
                runtime.app.set_error(e.to_string());
                e.to_string()
            }
        },
        "activate_skill" => match runtime.app.activate_skill(value) {
            Ok(msg) => {
                runtime.app.status = catus_core::app::AppStatus::Idle;
                runtime.app.set_transient_message(msg.as_str());
                msg
            }
            Err(e) => {
                runtime.app.set_error(e.to_string());
                e.to_string()
            }
        },
        "switch_model" => match runtime.app.set_model(value) {
            Ok(msg) => {
                runtime.app.status = catus_core::app::AppStatus::Idle;
                runtime.app.set_transient_message(msg.as_str());
                msg
            }
            Err(e) => {
                runtime.app.set_error(e.to_string());
                e.to_string()
            }
        },
        "close_subagent" => match runtime.app.close_subagent(value) {
            Ok(msg) => {
                runtime.app.set_transient_message(msg.as_str());
                msg
            }
            Err(e) => {
                runtime.app.set_error(e.to_string());
                e.to_string()
            }
        },
        // Pure view change: the web frontend opens its own monitor page.
        "watch_subagent" => format!("watching subagent {}", value),
        _ => {
            let msg = format!("unknown overlay action: {}", action);
            runtime.app.set_error(msg.as_str());
            msg
        }
    }
}

/// Forward a runtime event to the clients; refresh the snapshot after
/// structural changes.
async fn handle_runtime_event(
    events: &broadcast::Sender<BroadcastEvent>,
    runtime: &mut Runtime,
    event: RuntimeEvent,
) {
    let _ = events.send(BroadcastEvent::RuntimeEvent(event.clone()));

    match &event {
        RuntimeEvent::MessagesChanged => {
            emit_snapshot(events, runtime.app.snapshot());
        }
        RuntimeEvent::TurnComplete => {
            emit_snapshot(events, runtime.app.snapshot());
        }
        RuntimeEvent::SubagentEvent(_) => {
            // Subagent messages are shown on the monitor page; refresh the
            // summaries cheaply so the subagent picker stays current.
            emit_snapshot(events, runtime.app.snapshot());
        }
        _ => {}
    }
}

fn emit_snapshot(events: &broadcast::Sender<BroadcastEvent>, snapshot: AppSnapshot) {
    let _ = events.send(BroadcastEvent::Snapshot(snapshot));
}

#[cfg(test)]
mod tests {
    use super::expand_agent_shorthand;

    #[test]
    fn keeps_plain_input() {
        assert_eq!(expand_agent_shorthand("hello world"), "hello world");
        assert_eq!(expand_agent_shorthand("  /status  "), "/status");
    }

    #[test]
    fn expands_agent_shorthand() {
        assert_eq!(
            expand_agent_shorthand("@coder fix the bug"),
            "/agent use coder fix the bug create"
        );
        assert_eq!(expand_agent_shorthand("@coder"), "/agent use coder  create");
    }

    #[test]
    fn keeps_incomplete_shorthand() {
        assert_eq!(expand_agent_shorthand("@"), "@");
        assert_eq!(
            expand_agent_shorthand("@ coder"),
            "/agent use coder  create"
        );
    }
}

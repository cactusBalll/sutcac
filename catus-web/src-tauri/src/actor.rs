//! The runtime actor: the single owner of [`Runtime`].
//!
//! The web frontend never touches `App` directly. All frontend actions arrive
//! as [`Command`]s on an mpsc channel; runtime events are serialized and
//! emitted to the webview as Tauri events. This mirrors the TUI's event loop
//! (a `tokio::select!` over UI input and `Runtime::next_event`) with the UI
//! input replaced by the command channel.
//!
//! Emitted events:
//! - `runtime-event`: a serialized [`RuntimeEvent`] (`{type: "stream_text", ...}`).
//! - `snapshot`: a full [`AppSnapshot`] after structural state changes.
//! - `app-quit`: the session requested a shutdown (`/exit`).

use std::fs::OpenOptions;

use log::LevelFilter;
use serde_json::json;
use simplelog::{Config, WriteLogger};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot};

use catus_core::app::{AppSnapshot, InputLineOutcome, RuntimeEvent};
use catus_core::bootstrap::{bootstrap_runtime, load_config};
use catus_core::config::AppConfig;
use catus_core::message::Message;
use catus_core::runtime::Runtime;
use catus_core::tool::AskAnswer;

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
/// startup fails. All Tauri event emission happens here.
pub async fn run(app: AppHandle, mut cmd_rx: mpsc::Receiver<Command>) -> Result<(), String> {
    let config: AppConfig = load_config()?;
    init_logger(&config.effective_log_path(), config.effective_log_level());

    let mut runtime = bootstrap_runtime(config, false).await?;

    // Initial state so the UI can render before any interaction.
    let snapshot = runtime.app.snapshot();
    emit_snapshot(&app, snapshot);

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                match command {
                    None => break,
                    Some(cmd) => {
                        if handle_command(&app, &mut runtime, cmd).await {
                            break;
                        }
                    }
                }
            }
            event = runtime.next_event() => {
                match event {
                    None => break,
                    Some(event) => handle_runtime_event(&app, &mut runtime, event).await,
                }
            }
        }
    }

    runtime.app.await_memory_passes().await;
    runtime.app.persist_session();
    // Close the whole application: the actor is the only session owner, so
    // once it stops there is nothing left to serve the webview.
    app.exit(0);
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
async fn handle_command(app: &AppHandle, runtime: &mut Runtime, command: Command) -> bool {
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
                    emit_snapshot(app, runtime.app.snapshot());
                }
                InputLineOutcome::Empty => {}
            }
            let _ = reply.send(outcome);
            if quit {
                finish(app);
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
            emit_snapshot(app, runtime.app.snapshot());
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
            emit_snapshot(app, runtime.app.snapshot());
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        Command::RemoveConfigField { scope, key, reply } => {
            let result = runtime.app.remove_config_field_in(scope, &key);
            match &result {
                Ok(msg) => runtime.app.set_transient_message(msg),
                Err(e) => runtime.app.set_error(&e.to_string()),
            }
            emit_snapshot(app, runtime.app.snapshot());
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        Command::Shutdown => {
            finish(app);
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

/// Forward a runtime event to the webview; refresh the snapshot after
/// structural changes.
async fn handle_runtime_event(app: &AppHandle, runtime: &mut Runtime, event: RuntimeEvent) {
    let payload = match serde_json::to_value(&event) {
        Ok(value) => value,
        Err(e) => {
            log::error!("failed to serialize runtime event: {}", e);
            return;
        }
    };
    let _ = app.emit("runtime-event", payload);

    match &event {
        RuntimeEvent::MessagesChanged => {
            emit_snapshot(app, runtime.app.snapshot());
        }
        RuntimeEvent::TurnComplete => {
            emit_snapshot(app, runtime.app.snapshot());
        }
        RuntimeEvent::SubagentEvent(_) => {
            // Subagent messages are shown on the monitor page; refresh the
            // summaries cheaply so the subagent picker stays current.
            emit_snapshot(app, runtime.app.snapshot());
        }
        _ => {}
    }
}

fn emit_snapshot(app: &AppHandle, snapshot: AppSnapshot) {
    if let Ok(value) = serde_json::to_value(&snapshot) {
        let _ = app.emit("snapshot", value);
    }
}

/// Persist the session and notify the webview before the actor stops.
fn finish(app: &AppHandle) {
    // Persistence is performed by the actor loop epilogue; here we only
    // notify the UI so it can close the window.
    let _ = app.emit("app-quit", json!({}));
}

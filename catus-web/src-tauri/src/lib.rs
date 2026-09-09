//! catus-web: the Tauri + Vue web frontend for catus-core.
//!
//! The Rust side is a thin bridge: Tauri commands forward frontend actions to
//! the [`actor`] (the single `Runtime` owner), and runtime events/snapshots
//! are emitted back to the webview as Tauri events.

mod actor;

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};

use actor::Command;
use catus_core::app::{AppSnapshot, InputLineOutcome};
use catus_core::message::Message;
use catus_core::tool::AskAnswer;

/// Slash-command completion candidates for the current input prefix,
/// served directly from the static `BUILT_IN_REGISTRY` (no actor round-trip).
#[tauri::command]
async fn completion_candidates(input: String) -> Result<Vec<String>, String> {
    Ok(catus_core::app::commands::BUILT_IN_REGISTRY.completion_candidates(&input))
}

/// Set a config field in one scope (`workspace` or `global`). Returns the
/// status message.
#[tauri::command]
async fn set_config_field(
    state: State<'_, mpsc::Sender<Command>>,
    scope: catus_core::config::ConfigScope,
    key: String,
    value: String,
) -> Result<String, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::SetConfigField {
            scope,
            key,
            value,
            reply: tx,
        },
    )
    .await?;
    rx.await.unwrap_or(Err("actor stopped".to_string()))
}

/// Remove a config field from one scope. Returns the status message.
#[tauri::command]
async fn remove_config_field(
    state: State<'_, mpsc::Sender<Command>>,
    scope: catus_core::config::ConfigScope,
    key: String,
) -> Result<String, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::RemoveConfigField {
            scope,
            key,
            reply: tx,
        },
    )
    .await?;
    rx.await.unwrap_or(Err("actor stopped".to_string()))
}

/// Submit one input line (a user message or a slash command).
#[tauri::command]
async fn send_input(
    state: State<'_, mpsc::Sender<Command>>,
    text: String,
) -> Result<InputLineOutcome, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::SendInput { text, reply: tx }).await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Answer the paused `ask_user`/`ask_permission` interaction and resume the
/// turn. Returns whether an interaction was actually completed.
#[tauri::command]
async fn complete_interaction(
    state: State<'_, mpsc::Sender<Command>>,
    answers: Vec<AskAnswer>,
) -> Result<bool, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::CompleteInteraction { answers, reply: tx }).await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Cancel the paused interaction, reporting "user cancelled" to the model.
#[tauri::command]
async fn cancel_interaction(state: State<'_, mpsc::Sender<Command>>) -> Result<bool, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::CancelInteraction { reply: tx }).await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Apply a picker action (`resume_history`, `activate_skill`, `switch_model`,
/// `close_subagent`, `watch_subagent`). Returns the status message.
#[tauri::command]
async fn overlay_action(
    state: State<'_, mpsc::Sender<Command>>,
    action: String,
    value: String,
) -> Result<String, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::OverlayAction {
            action,
            value,
            reply: tx,
        },
    )
    .await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Pull the full serializable session state.
#[tauri::command]
async fn get_snapshot(state: State<'_, mpsc::Sender<Command>>) -> Result<AppSnapshot, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::GetSnapshot { reply: tx }).await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Pull one subagent's messages for the monitor page.
#[tauri::command]
async fn get_subagent_messages(
    state: State<'_, mpsc::Sender<Command>>,
    id: String,
) -> Result<Vec<Message>, String> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::GetSubagentMessages { id, reply: tx }).await?;
    rx.await.map_err(|_| "actor stopped".to_string())
}

/// Persist the session and quit the application (the `/exit` analogue).
#[tauri::command]
async fn quit_app(state: State<'_, mpsc::Sender<Command>>, app: AppHandle) -> Result<(), String> {
    state
        .send(Command::Shutdown)
        .await
        .map_err(|_| "actor stopped".to_string())?;
    let _ = app;
    Ok(())
}

async fn dispatch(
    state: &State<'_, mpsc::Sender<Command>>,
    command: Command,
) -> Result<(), String> {
    state
        .send(command)
        .await
        .map_err(|_| "runtime actor stopped".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let (tx, rx) = tauri::async_runtime::channel::<Command>(64);
            app.manage(tx);
            tauri::async_runtime::spawn(async move {
                if let Err(e) = actor::run(handle.clone(), rx).await {
                    log::error!("startup failed: {}", e);
                    eprintln!("catus-web: {}", e);
                    let _ = handle.emit("startup-error", e);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_input,
            complete_interaction,
            cancel_interaction,
            overlay_action,
            get_snapshot,
            get_subagent_messages,
            quit_app,
            completion_candidates,
            set_config_field,
            remove_config_field
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

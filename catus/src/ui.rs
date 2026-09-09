//! TUI layer.
//!
//! Split into three concerns:
//! - [`chat`]: the always-visible chat view (history, input, candidates,
//!   status bar) and its scroll keys.
//! - [`overlay`]: modal pages rendered on top of the chat view (history
//!   picker, token usage details) and their navigation keys.
//! - [`input`]: input-line editing keys.
//! - [`actions`]: outcomes returned to the main event loop.
//! - [`state`]: the TUI-owned view state ([`UiState`]).
//!
//! Rendering functions are pure views over [`catus_core::app::App`] (the
//! core runtime session state) plus [`UiState`]; interaction logic lives in
//! both: keys map to core semantic actions, and view-local state (scroll,
//! overlays, input editing) is handled here. The chat view may normalize
//! `UiState::chat_state.scroll` so that it always stays within the available
//! content range.

pub mod actions;
pub mod chat;
pub mod chat_state;
pub mod input;
pub mod input_state;
pub mod overlay;
pub mod overlay_state;
pub mod state;

pub use actions::AppAction;
pub use chat_state::ChatState;
pub use input_state::{InputState, MAX_CANDIDATES};
pub use overlay::OverlayAction;
pub use overlay_state::{Overlay, OverlayState};
pub use state::UiState;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;

use catus_core::app::{App, AppStatus, InputLineOutcome};

/// Draw the full UI into the provided frame: the chat view plus any active
/// overlay on top of it.
pub fn draw(frame: &mut Frame, app: &mut App, ui: &mut UiState) {
    chat::draw_chat(frame, app, ui);
    if ui.overlay_state.is_active() {
        overlay::draw_overlay(frame, app, ui);
    }
}

/// Handle a single key event and return an action for the event loop.
///
/// This is the single entry point for all keyboard input. It enforces the
/// precedence: Ctrl+C always quits, overlays consume keys while open, and
/// finally the input line / chat view get the remaining keys.
pub async fn handle_key_event(app: &mut App, ui: &mut UiState, key: KeyEvent) -> AppAction {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return AppAction::None;
    }

    // Ctrl+C always quits, even with an overlay open.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return AppAction::Quit;
    }

    // Modal overlays swallow all other keys first.
    if ui.overlay_state.is_active() {
        let action = overlay::handle_overlay_key(app, ui, key.code);
        return handle_overlay_result(app, ui, action).await;
    }

    // Enter is special: it may run a slash command or submit a message.
    if key.code == KeyCode::Enter {
        // @agent_name <task> is a shorthand for /agent use <name> <task>.
        let input = ui.input_state.input.trim().to_string();
        if let Some(rest) = input.strip_prefix('@') {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                let mut parts = rest.splitn(2, ' ');
                let name = parts.next().unwrap_or("");
                let task = parts.next().unwrap_or("").trim();
                ui.input_state.input = format!("/agent use {} {} create", name, task);
                ui.input_state.cursor = ui.input_state.input.len();
                ui.input_state.recompute_candidates();
            }
        }

        let submitted_input = ui.input_state.input.trim().to_string();
        match app.handle_input_line(&submitted_input).await {
            InputLineOutcome::Submitted => {
                ui.input_state.clear();
                ui.input_state.record_history(&submitted_input);
                return AppAction::StartStream;
            }
            InputLineOutcome::Handled(outcome) => {
                ui.input_state.clear();
                if outcome.record_history {
                    ui.input_state.record_history(&submitted_input);
                }
                if let Some(request) = outcome.ui {
                    ui.apply_request(request);
                }
            }
            InputLineOutcome::Empty => return AppAction::None,
        }
        // Slash commands such as /exit may request a clean shutdown.
        if app.should_quit {
            return AppAction::Quit;
        }
        return AppAction::None;
    }

    // Input-line keys take precedence over chat scroll keys.
    if input::handle_input_key(&mut ui.input_state, key.code) {
        return AppAction::None;
    }

    if chat::handle_chat_key(ui, key.code) {
        return AppAction::None;
    }

    AppAction::None
}

/// Apply the result of an overlay key press to the app and view state.
async fn handle_overlay_result(
    app: &mut App,
    ui: &mut UiState,
    result: OverlayAction,
) -> AppAction {
    match result {
        OverlayAction::LoadHistory(name) => match app.resume_history(Some(&name)).await {
            Ok(msg) => {
                app.status = AppStatus::Idle;
                app.set_transient_message(msg);
                AppAction::None
            }
            Err(e) => AppAction::SetError(e.to_string()),
        },
        OverlayAction::ActivateSkill(name) => match app.activate_skill(&name) {
            Ok(msg) => {
                app.status = AppStatus::Idle;
                app.set_transient_message(msg);
                AppAction::None
            }
            Err(e) => AppAction::SetError(e.to_string()),
        },
        OverlayAction::SwitchModel(id) => match app.set_model(&id) {
            Ok(msg) => {
                app.status = AppStatus::Idle;
                app.set_transient_message(msg);
                AppAction::None
            }
            Err(e) => AppAction::SetError(e.to_string()),
        },
        // The ask overlay finished: append the tool result and resume the
        // paused LLM turn.
        OverlayAction::Answered(answers) => {
            if app.complete_interaction(answers) {
                AppAction::StartStream
            } else {
                AppAction::None
            }
        }
        OverlayAction::CancelInteraction => {
            if app.cancel_interaction() {
                AppAction::StartStream
            } else {
                AppAction::None
            }
        }
        OverlayAction::ActivateAgent(name) => {
            ui.input_state.input = format!("/agent use {} ", name);
            ui.input_state.cursor = ui.input_state.input.len();
            ui.input_state.recompute_candidates();
            AppAction::None
        }
        OverlayAction::WatchSubagent(id) => match app.watch_subagent(&id) {
            Ok(msg) => {
                app.status = AppStatus::Idle;
                app.set_transient_message(msg);
                AppAction::None
            }
            Err(e) => AppAction::SetError(e.to_string()),
        },
        OverlayAction::Closed | OverlayAction::Consumed => AppAction::None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::*;
    use catus_core::config::AppConfig;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    #[tokio::test]
    async fn enter_exit_command_returns_quit_action() {
        let mut app = App::new(AppConfig::default());
        let mut ui = UiState::new();
        ui.input_state.input = "/exit".to_string();
        let action = handle_key_event(&mut app, &mut ui, key(KeyCode::Enter)).await;
        assert_eq!(action, AppAction::Quit);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn enter_submits_message_and_records_history() {
        let mut app = App::new(AppConfig::default());
        let mut ui = UiState::new();
        ui.input_state.input = "hello".to_string();
        let action = handle_key_event(&mut app, &mut ui, key(KeyCode::Enter)).await;
        assert_eq!(action, AppAction::StartStream);
        assert!(ui.input_state.input.is_empty());
        assert_eq!(ui.input_state.input_history, vec!["hello"]);
        assert!(
            app.messages
                .iter()
                .any(|m| m.is_user() && m.content == "hello")
        );
    }
}

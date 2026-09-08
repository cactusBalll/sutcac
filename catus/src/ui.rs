//! TUI layer.
//!
//! Split into three concerns:
//! - [`chat`]: the always-visible chat view (history, input, candidates,
//!   status bar) and its scroll keys.
//! - [`overlay`]: modal pages rendered on top of the chat view (history
//!   picker, token usage details) and their navigation keys.
//! - [`input`]: input-line editing keys.
//! - [`actions`]: outcomes returned to the main event loop.
//!
//! Rendering functions are mostly pure views over [`App`]; interaction logic
//! lives in `crate::app`. The chat view may normalize `App::chat_state.scroll`
//! so that it always stays within the available content range.

pub mod actions;
pub mod chat;
pub mod input;
pub mod overlay;

pub use actions::AppAction;
pub use overlay::OverlayAction;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;

use crate::app::{App, AppStatus, OnEnterResult};

/// Draw the full UI into the provided frame: the chat view plus any active
/// overlay on top of it.
pub fn draw(frame: &mut Frame, app: &mut App) {
    chat::draw_chat(frame, app);
    if app.overlay_state.is_active() {
        overlay::draw_overlay(frame, &*app);
    }
}

/// Handle a single key event and return an action for the event loop.
///
/// This is the single entry point for all keyboard input. It enforces the
/// precedence: Ctrl+C always quits, overlays consume keys while open, and
/// finally the input line / chat view get the remaining keys.
pub async fn handle_key_event(app: &mut App, key: KeyEvent) -> AppAction {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return AppAction::None;
    }

    // Ctrl+C always quits, even with an overlay open.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return AppAction::Quit;
    }

    // Modal overlays swallow all other keys first.
    if app.overlay_state.is_active() {
        let action = overlay::handle_overlay_key(app, key.code);
        return handle_overlay_result(app, action).await;
    }

    // Enter is special: it may run a slash command or submit a message.
    if key.code == KeyCode::Enter {
        // @agent_name <task> is a shorthand for /agent use <name> <task>.
        let input = app.input_state.input.trim();
        if let Some(rest) = input.strip_prefix('@') {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                let mut parts = rest.splitn(2, ' ');
                let name = parts.next().unwrap_or("");
                let task = parts.next().unwrap_or("").trim();
                app.input_state.input = format!("/agent use {} {} create", name, task);
                app.input_state.cursor = app.input_state.input.len();
                app.input_state.recompute_candidates();
            }
        }

        match app.on_enter().await {
            OnEnterResult::Submitted => return AppAction::StartStream,
            OnEnterResult::Handled | OnEnterResult::Empty => {}
        }
        // Slash commands such as /exit may request a clean shutdown.
        if app.should_quit {
            return AppAction::Quit;
        }
        return AppAction::None;
    }

    // Input-line keys take precedence over chat scroll keys.
    if input::handle_input_key(app, key.code) {
        return AppAction::None;
    }

    if chat::handle_chat_key(app, key.code) {
        return AppAction::None;
    }

    AppAction::None
}

/// Apply the result of an overlay key press to `App`.
async fn handle_overlay_result(app: &mut App, result: OverlayAction) -> AppAction {
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
            app.input_state.input = format!("/agent use {} ", name);
            app.input_state.cursor = app.input_state.input.len();
            app.input_state.recompute_candidates();
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
    use crate::config::AppConfig;

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
        app.input_state.input = "/exit".to_string();
        let action = handle_key_event(&mut app, key(KeyCode::Enter)).await;
        assert_eq!(action, AppAction::Quit);
        assert!(app.should_quit);
    }
}

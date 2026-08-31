//! Input-line key handling.

use crossterm::event::KeyCode;

use crate::app::App;

/// Handle a key event that targets the input line or its completion
/// candidates. Returns `true` if the key was consumed.
pub fn handle_input_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Char(c) => {
            app.input_state.push_char(c);
            true
        }
        KeyCode::Backspace => {
            app.input_state.backspace();
            true
        }
        KeyCode::Left => {
            app.input_state.move_cursor_left();
            true
        }
        KeyCode::Right => {
            app.input_state.move_cursor_right();
            true
        }
        KeyCode::Tab => {
            app.input_state.cycle_candidate(1);
            true
        }
        KeyCode::BackTab => {
            app.input_state.cycle_candidate(-1);
            true
        }
        KeyCode::Up => {
            if app.input_state.input.starts_with('/') && !app.input_state.candidates.is_empty() {
                app.input_state.cycle_candidate(-1);
            } else {
                app.input_state.history_previous();
            }
            true
        }
        KeyCode::Down => {
            if app.input_state.input.starts_with('/') && !app.input_state.candidates.is_empty() {
                app.input_state.cycle_candidate(1);
            } else {
                app.input_state.history_next();
            }
            true
        }
        KeyCode::Esc => {
            // Esc only dismisses completion candidates or overlays;
            // use /exit or Ctrl+C to quit.
            if app.input_state.selected_candidate.is_some() {
                app.input_state.clear_candidate_selection();
            }
            true
        }
        _ => false,
    }
}

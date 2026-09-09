//! Input-line key handling.

use crossterm::event::KeyCode;

use crate::ui::InputState;

/// Handle a key event that targets the input line or its completion
/// candidates. Returns `true` if the key was consumed.
pub fn handle_input_key(input_state: &mut InputState, code: KeyCode) -> bool {
    match code {
        KeyCode::Char(c) => {
            input_state.push_char(c);
            true
        }
        KeyCode::Backspace => {
            input_state.backspace();
            true
        }
        KeyCode::Left => {
            input_state.move_cursor_left();
            true
        }
        KeyCode::Right => {
            input_state.move_cursor_right();
            true
        }
        KeyCode::Tab => {
            input_state.cycle_candidate(1);
            true
        }
        KeyCode::BackTab => {
            input_state.cycle_candidate(-1);
            true
        }
        KeyCode::Up => {
            if input_state.input.starts_with('/') && !input_state.candidates.is_empty() {
                input_state.cycle_candidate(-1);
            } else {
                input_state.history_previous();
            }
            true
        }
        KeyCode::Down => {
            if input_state.input.starts_with('/') && !input_state.candidates.is_empty() {
                input_state.cycle_candidate(1);
            } else {
                input_state.history_next();
            }
            true
        }
        KeyCode::Esc => {
            // Esc only dismisses completion candidates or overlays;
            // use /exit or Ctrl+C to quit.
            if input_state.selected_candidate.is_some() {
                input_state.clear_candidate_selection();
            }
            true
        }
        _ => false,
    }
}

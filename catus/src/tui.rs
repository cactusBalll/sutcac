//! Terminal lifecycle management for the catus TUI.
//!
//! Rendering lives in [`crate::ui`]; this module owns raw-mode setup and
//! teardown so the terminal is always restored, even on error or panic. It
//! also emits the OSC escape sequences that surface the app status in the
//! terminal tab title and ring the bell when a turn finishes.

use std::io::Write;

use catus_core::app::AppStatus;

/// Initialize the terminal into raw mode and return a `Terminal`.
///
/// Mouse capture is enabled so the scroll wheel can be handled by the
/// application instead of being translated to Up/Down arrow keys by the
/// terminal. Most terminals still allow text selection while holding Shift.
pub fn init_terminal() -> Result<
    ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    Box<dyn std::error::Error>,
> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = ratatui::Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Restore the default tab/window title (OSC 0 with an empty payload) and
    // hide the taskbar progress so the app status does not linger after catus
    // exits.
    write_stdout(b"\x1b]0;\x07\x1b]9;4;0;0\x07");
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Tab/window title shown for each [`AppStatus`].
pub fn title_for_status(status: AppStatus) -> &'static str {
    match status {
        AppStatus::Idle => "catus · 空闲",
        AppStatus::Streaming => "catus · 正在输出",
        AppStatus::RunningTool => "catus · 正在输出",
        AppStatus::Error => "catus · 错误",
    }
}

/// Set the terminal tab/window title via OSC 0 (BEL-terminated).
pub fn set_tab_title(title: &str) {
    write_stdout(&title_sequence(title));
}

/// OSC 0 sequence setting the tab/window title to `title`.
fn title_sequence(title: &str) -> Vec<u8> {
    format!("\x1b]0;{title}\x07").into_bytes()
}

/// Ring the terminal bell once (BEL) to draw attention to a finished turn.
pub fn alert() {
    write_stdout(b"\x07");
}

/// ConEmu-style taskbar progress states (OSC 9;4).
const PROGRESS_HIDDEN: u8 = 0;
const PROGRESS_INDETERMINATE: u8 = 3;

/// Show an indeterminate, looping taskbar progress animation while a turn is
/// in flight.
pub fn show_indeterminate_progress() {
    write_stdout(&taskbar_progress_sequence(PROGRESS_INDETERMINATE, 0));
}

/// Hide the taskbar progress indicator.
pub fn hide_taskbar_progress() {
    write_stdout(&taskbar_progress_sequence(PROGRESS_HIDDEN, 0));
}

/// OSC 9;4 taskbar progress sequence: `state` selects hidden/normal/error/
/// indeterminate/warning, `value` is the 0-100 percentage for normal state.
fn taskbar_progress_sequence(state: u8, value: u8) -> Vec<u8> {
    format!("\x1b]9;4;{state};{value}\x07").into_bytes()
}

/// Write bytes to stdout and flush; escape sequences that configure the
/// surrounding terminal must not sit buffered inside the TUI's draw cycle.
fn write_stdout(bytes: &[u8]) {
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Clear the terminal screen and reset the cursor to the top-left corner.
pub fn clear_terminal(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> Result<(), Box<dyn std::error::Error>> {
    terminal.clear()?;
    Ok(())
}

/// A RAII guard that restores the terminal when dropped. This guarantees the
/// terminal is left in a usable state even if `run_tui_mode` returns an error
/// or panics.
pub struct TerminalGuard {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    pub fn new(
        terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Self {
        Self { terminal }
    }

    pub fn terminal(
        &mut self,
    ) -> &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal(&mut self.terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_reflects_busy_and_idle_states() {
        assert_eq!(title_for_status(AppStatus::Streaming), "catus · 正在输出");
        assert_eq!(title_for_status(AppStatus::RunningTool), "catus · 正在输出");
        assert_eq!(title_for_status(AppStatus::Idle), "catus · 空闲");
        assert_eq!(title_for_status(AppStatus::Error), "catus · 错误");
    }

    #[test]
    fn tab_title_uses_osc0_with_bel_terminator() {
        assert_eq!(title_sequence("catus"), b"\x1b]0;catus\x07".to_vec());
        assert_eq!(
            title_sequence(title_for_status(AppStatus::Idle)),
            "\x1b]0;catus · 空闲\x07".as_bytes().to_vec()
        );
    }

    #[test]
    fn taskbar_progress_sequences_match_conemu_format() {
        assert_eq!(
            taskbar_progress_sequence(PROGRESS_INDETERMINATE, 0),
            b"\x1b]9;4;3;0\x07".to_vec()
        );
        assert_eq!(
            taskbar_progress_sequence(PROGRESS_HIDDEN, 0),
            b"\x1b]9;4;0;0\x07".to_vec()
        );
    }
}

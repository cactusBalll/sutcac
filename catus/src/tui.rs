//! Terminal lifecycle management for the catus TUI.
//!
//! Rendering lives in [`crate::ui`]; this module owns raw-mode setup and
//! teardown so the terminal is always restored, even on error or panic.

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
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
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

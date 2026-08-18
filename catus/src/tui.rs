//! Terminal UI helpers for catus.
//!
//! Renders an inline chat layout: conversation history fills the terminal,
//! and the user types on a single-line prompt at the bottom.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::app::{App, AppStatus};
use crate::message::{Message, Role};

const PROMPT: &str = "> ";
const PROMPT_WIDTH: u16 = 2;

/// Draw the full UI into the provided frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_history(frame, app, chunks[0]);
    render_input(frame, app, chunks[1]);
    render_status(frame, app, chunks[2]);
}

fn render_history(frame: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app.messages.iter().flat_map(message_to_lines).collect();

    let total_lines = lines.len();
    let visible_lines = area.height as usize;
    let max_scroll = total_lines.saturating_sub(visible_lines.max(1));

    // `app.scroll` is an offset from the bottom (0 = latest message).
    let bottom_offset = app.scroll.min(max_scroll);
    let top_scroll = max_scroll.saturating_sub(bottom_offset);

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((top_scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let max_width = area.width.saturating_sub(PROMPT_WIDTH) as usize;
    let input_width = unicode_width::UnicodeWidthStr::width(app.input.as_str());

    // If the input is wider than the available space, show only the tail that
    // fits so the cursor stays at the end of the visible text.
    let visible_input = if input_width <= max_width {
        app.input.as_str()
    } else {
        truncate_to_width(&app.input, max_width)
    };

    let input = Paragraph::new(Text::from(Line::from(vec![
        Span::styled(
            PROMPT,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible_input),
    ])));

    frame.render_widget(input, area);

    let cursor_x =
        area.x + PROMPT_WIDTH + unicode_width::UnicodeWidthStr::width(visible_input) as u16;
    let cursor_y = area.y;
    frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), cursor_y));
}

/// Return the trailing substring of `s` whose display width is at most `max_width`.
fn truncate_to_width(s: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut seen = 0;
    let mut start_byte = 0;
    for (i, c) in s.char_indices().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if seen + w > max_width {
            // Skip the first character that would overflow; start after it.
            start_byte = i + c.len_utf8();
            break;
        }
        seen += w;
    }
    &s[start_byte..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate_to_width("hello", 3), "llo");
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_wide_chars() {
        // Each CJK character is typically width 2.
        assert_eq!(truncate_to_width("你好世界", 3), "界");
        assert_eq!(truncate_to_width("你好世界", 4), "世界");
        assert_eq!(truncate_to_width("你好世界", 8), "你好世界");
    }

    #[test]
    fn parse_tool_result_content() {
        let content = "status=0\nstdout=```\nhello\nworld\n```\nstderr=```\n```";
        let (status, stdout) = parse_tool_content(content).unwrap();
        assert_eq!(status, 0);
        assert_eq!(stdout, "hello\nworld");
    }

    #[test]
    fn truncate_stdout_limits_lines() {
        let input = "line\n".repeat(25);
        let out = truncate_stdout(&input);
        assert!(out.contains("... (5 lines truncated)"));
        assert_eq!(out.lines().count(), 21); // 20 kept + 1 marker
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let (label, color) = match app.status {
        AppStatus::Idle => ("idle", Color::Gray),
        AppStatus::Streaming => ("streaming", Color::Yellow),
        AppStatus::RunningTool => ("running tool", Color::Cyan),
        AppStatus::Error => ("error", Color::Red),
    };

    let mut spans = vec![Span::styled(
        format!("[{}] ", label),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];

    if !app.status_message.is_empty() {
        spans.push(Span::raw(app.status_message.clone()));
    } else {
        spans.push(Span::styled(
            "Enter: send | ↑/↓/PgUp/PgDn scroll | Home: top | End: bottom | Ctrl+L: clear | Esc: quit",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let line = Line::from(spans).alignment(Alignment::Left);
    frame.render_widget(Paragraph::new(line), area);
}

/// Maximum number of stdout lines shown for a shell tool result.
const MAX_TOOL_STDOUT_LINES: usize = 20;

fn message_to_lines(msg: &Message) -> Vec<Line<'static>> {
    match msg.role {
        Role::System => Vec::new(),
        Role::User => render_highlighted("YOU", Color::Blue, &msg.content),
        Role::Assistant => {
            if msg.content.trim().is_empty() {
                return Vec::new();
            }
            let style = if msg.had_tool_calls {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            };
            render_with_style("AI ", style, &msg.content)
        }
        Role::Tool => render_tool_output(&msg.content),
        Role::Event => render_event(&msg.content),
    }
}

fn render_highlighted(prefix: &str, color: Color, content: &str) -> Vec<Line<'static>> {
    let prefix_span = Span::styled(
        format!("{}: ", prefix),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let text_style = Style::default().add_modifier(Modifier::BOLD);

    content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    prefix_span.clone(),
                    Span::styled(line.to_string(), text_style),
                ])
            } else {
                Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(line.to_string(), text_style),
                ])
            }
        })
        .collect()
}

fn render_with_style(prefix: &str, style: Style, content: &str) -> Vec<Line<'static>> {
    let prefix_span = Span::styled(format!("{}: ", prefix), style);

    content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    prefix_span.clone(),
                    Span::styled(line.to_string(), style),
                ])
            } else {
                Line::from(vec![
                    Span::styled("    ", style),
                    Span::styled(line.to_string(), style),
                ])
            }
        })
        .collect()
}

fn render_event(content: &str) -> Vec<Line<'static>> {
    let style = Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);
    content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled("! ", style),
                    Span::styled(line.to_string(), style),
                ])
            } else {
                Line::from(vec![
                    Span::styled("  ", style),
                    Span::styled(line.to_string(), style),
                ])
            }
        })
        .collect()
}

fn render_tool_output(content: &str) -> Vec<Line<'static>> {
    let (status, stdout) = parse_tool_content(content).unwrap_or((1, content.to_string()));
    let dot_color = if status == 0 {
        Color::Green
    } else {
        Color::Red
    };
    let dot = Span::styled("● ", Style::default().fg(dot_color));
    let dim = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::DIM);

    let stdout = if stdout.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncate_stdout(&stdout)
    };

    stdout
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![dot.clone(), Span::styled(line.to_string(), dim)])
            } else {
                Line::from(vec![
                    Span::styled("  ", dim),
                    Span::styled(line.to_string(), dim),
                ])
            }
        })
        .collect()
}

/// Parse the formatted tool result message created by `ToolResult::to_message`.
fn parse_tool_content(content: &str) -> Option<(i32, String)> {
    let first_line = content.lines().next()?;
    let status: i32 = first_line.strip_prefix("status=")?.parse().ok()?;

    let marker = "stdout=```\n";
    let start = content.find(marker)? + marker.len();
    let end = content[start..].find("\n```")?;
    let stdout = content[start..start + end].to_string();

    Some((status, stdout))
}

/// Truncate stdout to `MAX_TOOL_STDOUT_LINES` and append an ellipsis marker.
fn truncate_stdout(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= MAX_TOOL_STDOUT_LINES {
        stdout.to_string()
    } else {
        let mut result = lines[..MAX_TOOL_STDOUT_LINES].join("\n");
        result.push_str(&format!(
            "\n... ({} lines truncated)",
            lines.len() - MAX_TOOL_STDOUT_LINES
        ));
        result
    }
}

/// Initialize the terminal into raw mode and return a `Terminal`.
///
/// Mouse capture is intentionally disabled so the terminal's native text
/// selection and copying continue to work.
pub fn init_terminal() -> Result<
    ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    Box<dyn std::error::Error>,
> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
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
        crossterm::terminal::LeaveAlternateScreen
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

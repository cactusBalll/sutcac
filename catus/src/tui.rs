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
    let candidate_height = if app.candidates.is_empty() { 0 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(candidate_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_history(frame, app, chunks[0]);
    if candidate_height > 0 {
        render_candidates(frame, app, chunks[1]);
    }
    render_input(frame, app, chunks[2]);
    render_status(frame, app, chunks[3]);
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
    let (visible_input, cursor_offset) = visible_input_window(&app.input, app.cursor, max_width);

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

    let cursor_x = area.x + PROMPT_WIDTH + cursor_offset as u16;
    let cursor_y = area.y;
    frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), cursor_y));
}

/// Return the visible substring of `s` for a one-line input field and the
/// display width from the start of that substring to the cursor.
///
/// The window is chosen so the cursor is always visible and as much surrounding
/// text as possible is shown within `max_width`.
fn visible_input_window(s: &str, cursor: usize, max_width: usize) -> (&str, usize) {
    if s.is_empty() || max_width == 0 {
        return ("", 0);
    }

    let total_width = unicode_width::UnicodeWidthStr::width(s);
    let prefix_to_cursor = unicode_width::UnicodeWidthStr::width(&s[..cursor]);

    if total_width <= max_width {
        return (s, prefix_to_cursor);
    }

    let chars: Vec<(usize, usize)> = s
        .char_indices()
        .map(|(i, c)| (i, unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)))
        .collect();

    let mut prefix = vec![0usize; chars.len() + 1];
    for (i, (_, w)) in chars.iter().enumerate() {
        prefix[i + 1] = prefix[i] + w;
    }

    let cursor_char = chars
        .iter()
        .position(|(b, _)| *b >= cursor)
        .unwrap_or(chars.len());

    // Find the latest start that still leaves the cursor visible.
    let mut start_char = cursor_char;
    for s_idx in (0..cursor_char).rev() {
        if prefix[cursor_char] - prefix[s_idx] <= max_width {
            start_char = s_idx;
        } else {
            break;
        }
    }

    // Extend the window to the right as far as it fits.
    let mut end = cursor_char + 1;
    while end <= chars.len() && prefix[end] - prefix[start_char] <= max_width {
        end += 1;
    }
    let end_char = end - 1;

    let start_byte = chars[start_char].0;
    let end_byte = if end_char == chars.len() {
        s.len()
    } else {
        chars[end_char].0
    };

    let visible = &s[start_byte..end_byte];
    let cursor_offset = prefix[cursor_char] - prefix[start_char];
    (visible, cursor_offset)
}

fn render_candidates(frame: &mut Frame, app: &App, area: Rect) {
    let line = build_candidate_line(app, area.width as usize);
    frame.render_widget(Paragraph::new(line), area);
}

/// Build the one-line candidate strip, truncating with an ellipsis if the
/// candidates do not fit in the available width.
fn build_candidate_line(app: &App, max_width: usize) -> Line<'static> {
    let normal_style = Style::default().fg(Color::Cyan);
    let selected_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span> = Vec::new();
    let mut used_width: usize = 0;
    let separator_width = 1usize; // space between candidates

    for (i, candidate) in app.candidates.iter().enumerate() {
        let style = if app.selected_candidate == Some(i) {
            selected_style
        } else {
            normal_style
        };
        let width = unicode_width::UnicodeWidthStr::width(candidate.as_str());

        if used_width + width > max_width && !spans.is_empty() {
            // Not enough room; truncate with an ellipsis.
            let ellipsis = Span::styled("...", Style::default().fg(Color::DarkGray));
            spans.push(ellipsis);
            break;
        }

        spans.push(Span::styled(candidate.clone(), style));
        used_width += width;

        if i + 1 < app.candidates.len() {
            if used_width + separator_width > max_width {
                // No room for another separator + candidate.
                break;
            }
            spans.push(Span::raw(" "));
            used_width += separator_width;
        }
    }

    Line::from(spans)
}

/// Return the trailing substring of `s` whose display width is at most `max_width`.
#[allow(dead_code)]
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
    fn format_token_counts_compactly() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(980), "980");
        assert_eq!(format_tokens(1_234), "1.23k");
        assert_eq!(format_tokens(32_100), "32.1k");
        assert_eq!(format_tokens(1_500_000), "1.5m");
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

    #[test]
    fn candidate_line_lists_all_items_when_roomy() {
        let app = app_with_candidates(&["ls", "cat", "pwd"]);
        let line = build_candidate_line(&app, 80);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("ls"));
        assert!(text.contains("cat"));
        assert!(text.contains("pwd"));
    }

    #[test]
    fn candidate_line_truncates_when_narrow() {
        let app = app_with_candidates(&["alpha", "beta", "gamma"]);
        let line = build_candidate_line(&app, 8);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        // "alpha" (5) + space (1) = 6; "beta" would not fit in 8, so it is replaced by "..."
        assert!(text.contains("alpha"));
        assert!(!text.contains("beta"));
        assert!(!text.contains("gamma"));
        assert!(text.contains("..."));
    }

    #[test]
    fn candidate_line_highlights_selected_item() {
        let mut app = app_with_candidates(&["foo", "bar"]);
        app.selected_candidate = Some(1);
        let line = build_candidate_line(&app, 80);
        assert_eq!(line.spans.len(), 3); // foo + space + bar
        assert!(
            line.spans[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn visible_window_shows_all_when_input_fits() {
        let (visible, offset) = visible_input_window("hello", 3, 20);
        assert_eq!(visible, "hello");
        assert_eq!(offset, 3);
    }

    #[test]
    fn visible_window_scrolls_to_keep_cursor_visible() {
        let s = "0123456789";
        let cursor = s.find('7').unwrap();
        let (visible, offset) = visible_input_window(s, cursor, 4);
        // Should show a window of width <= 4 containing the cursor before '7'.
        assert_eq!(visible, "3456");
        assert_eq!(offset, 4);
    }

    #[test]
    fn visible_window_handles_wide_characters() {
        let s = "你好世界";
        let cursor = s.find('世').unwrap();
        let (visible, offset) = visible_input_window(s, cursor, 4);
        // The window keeps the cursor at the right edge and shows the text
        // before it that fits: "你好" is width 4, cursor sits after it.
        assert_eq!(visible, "你好");
        assert_eq!(offset, 4);
    }

    fn app_with_candidates(candidates: &[&str]) -> crate::app::App {
        use crate::app::App;
        use crate::config::{AgentConfig, ApiConfig, AppConfig};
        let mut app = App::new(AppConfig {
            api: ApiConfig {
                base_url: "https://example.com".to_string(),
                api_key: "test".to_string(),
                model: "test".to_string(),
            },
            agent: AgentConfig {
                system_prompt: "test".to_string(),
                max_tool_rounds: 5,
                history_path: None,
                log_path: None,
                log_level: "info".to_string(),
            },
            shell: None,
        });
        app.candidates = candidates.iter().map(|s| s.to_string()).collect();
        app
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
            "Enter: send | Tab: complete | ↑/↓ history | PgUp/PgDn scroll | Home: top | End: bottom | Ctrl+L: clear | Esc: quit",
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right-aligned token usage: current context size and session total.
    if app.usage.prompt_tokens > 0 {
        let info = format!(
            "ctx {} tok | total {}",
            format_tokens(app.usage.context_tokens()),
            format_tokens(app.usage.total_tokens),
        );
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let needed = info.chars().count() + 2; // at least two spaces of padding
        let pad = (area.width as usize)
            .saturating_sub(used)
            .saturating_sub(needed);
        spans.push(Span::raw(" ".repeat(pad + 2)));
        spans.push(Span::styled(info, Style::default().fg(Color::Cyan)));
    }

    let line = Line::from(spans).alignment(Alignment::Left);
    frame.render_widget(Paragraph::new(line), area);
}

/// Compact token count: `980`, `1.2k`, `12.3k`, `1.5m`.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Maximum number of stdout lines shown for a shell tool result.
const MAX_TOOL_STDOUT_LINES: usize = 20;

fn message_to_lines(msg: &Message) -> Vec<Line<'static>> {
    match msg.role {
        Role::System => Vec::new(),
        Role::User => render_highlighted("YOU", Color::Blue, &msg.content),
        Role::Assistant => {
            let mut lines = Vec::new();
            if !msg.reasoning_content.trim().is_empty() {
                lines.extend(render_reasoning(&msg.reasoning_content));
            }
            if !msg.content.trim().is_empty() {
                let style = if msg.had_tool_calls {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                };
                lines.extend(render_with_style("AI ", style, &msg.content));
            }
            lines
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

fn render_reasoning(content: &str) -> Vec<Line<'static>> {
    let style = Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);
    content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled("thinking: ", style),
                    Span::styled(line.to_string(), style),
                ])
            } else {
                Line::from(vec![
                    Span::styled("           ", style),
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

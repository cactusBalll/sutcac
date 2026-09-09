//! Chat view rendering: conversation history, input line, candidate strip,
//! and the status bar.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use crate::ui::{MAX_CANDIDATES, UiState};
use catus_core::app::{App, AppStatus};
use catus_core::message::{Message, Role};
use crossterm::event::KeyCode;

const PROMPT: &str = "> ";
const PROMPT_WIDTH: u16 = 2;

/// Draw the chat layout into the provided frame.
pub fn draw_chat(frame: &mut Frame, app: &mut App, ui: &mut UiState) {
    let area = frame.area();
    let candidate_height = ui.input_state.candidates.len().min(MAX_CANDIDATES) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(candidate_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_history(frame, app, ui, chunks[0]);
    if candidate_height > 0 {
        render_candidates(frame, ui, chunks[1]);
    }
    render_input(frame, ui, chunks[2]);
    render_status(frame, app, ui, chunks[3]);
}

fn render_history(frame: &mut Frame, app: &mut App, ui: &mut UiState, area: Rect) {
    let messages: Vec<&Message> = app.messages.iter().collect();
    let lines: Vec<Line> = messages.into_iter().flat_map(message_to_lines).collect();

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total_wrapped_lines = paragraph.line_count(area.width);
    let visible_lines = area.height as usize;

    // `UiState::chat_state.scroll` is an offset from the bottom (0 = latest
    // message). Clamp it here so `scroll_to_top()` cannot leave it at
    // usize::MAX, which would make subsequent scroll-down operations
    // ineffective.
    let (bottom_offset, top_scroll) =
        compute_history_scroll(total_wrapped_lines, visible_lines, ui.chat_state.scroll);
    ui.chat_state.scroll = bottom_offset;

    let paragraph = paragraph.scroll((top_scroll, 0));

    frame.render_widget(paragraph, area);
}

/// Compute the clamped bottom scroll offset and the corresponding Paragraph
/// top-scroll value.
///
/// `total_wrapped_lines` is the number of visual lines the wrapped history
/// occupies; `visible_lines` is the height of the history area. The returned
/// `bottom_offset` is always within `[0, max_scroll]` and can be stored back
/// into `App::scroll` to keep the value bounded.
pub(crate) fn compute_history_scroll(
    total_wrapped_lines: usize,
    visible_lines: usize,
    scroll: usize,
) -> (usize, u16) {
    let max_scroll = total_wrapped_lines.saturating_sub(visible_lines.max(1));
    let bottom_offset = scroll.min(max_scroll);
    let top_scroll = max_scroll
        .saturating_sub(bottom_offset)
        .min(u16::MAX as usize) as u16;
    (bottom_offset, top_scroll)
}

fn render_input(frame: &mut Frame, ui: &UiState, area: Rect) {
    let max_width = area.width.saturating_sub(PROMPT_WIDTH) as usize;
    let (visible_input, cursor_offset) =
        visible_input_window(&ui.input_state.input, ui.input_state.cursor, max_width);

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

/// Compute the scroll offset for a candidate list so that `selected` is
/// visible inside a window of `visible_count` rows. The selection is centered
/// when possible and clamped at the top and bottom edges.
fn candidate_scroll_offset(total: usize, selected: usize, visible_count: usize) -> usize {
    if total <= visible_count {
        return 0;
    }
    let half = visible_count / 2;
    if selected <= half {
        0
    } else if selected + visible_count - half >= total {
        total - visible_count
    } else {
        selected - half
    }
}

fn render_candidates(frame: &mut Frame, ui: &UiState, area: Rect) {
    let normal_style = Style::default().fg(Color::Cyan);
    let selected_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::REVERSED);

    let total = ui.input_state.candidates.len();
    if total == 0 {
        return;
    }

    let visible_count = area.height as usize;
    let selected = ui
        .input_state
        .selected_candidate
        .unwrap_or(0)
        .min(total - 1);
    let offset = candidate_scroll_offset(total, selected, visible_count);

    let items: Vec<ListItem> = ui
        .input_state
        .candidates
        .iter()
        .skip(offset)
        .take(visible_count)
        .enumerate()
        .map(|(i, candidate)| {
            let absolute_i = offset + i;
            let style = if ui.input_state.selected_candidate == Some(absolute_i) {
                selected_style
            } else {
                normal_style
            };
            ListItem::new(Line::styled(candidate.clone(), style))
        })
        .collect();

    let list = List::new(items).highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(ui.input_state.selected_candidate.map(|i| i - offset));
    frame.render_stateful_widget(list, area, &mut state);
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

fn render_status(frame: &mut Frame, app: &mut App, ui: &UiState, area: Rect) {
    app.maybe_clear_status_message();

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
        let navigate = if ui.input_state.input.starts_with('/') {
            "↑↓:cmd"
        } else {
            "↑↓:hist"
        };
        spans.push(Span::styled(
            format!(
                "Enter:send Tab:complete {} PgUp/PgDn:scroll /help Ctrl+C:quit",
                navigate
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right-aligned compact subagent count and token usage; details live in
    // the /agent status and /status pages.
    let running = app.subagents.running_count();
    let info = if app.usage.prompt_tokens > 0 {
        format!(
            "subs {} | ctx {} tok | total {}",
            running,
            format_tokens(app.usage.context_tokens()),
            format_tokens(app.usage.total_tokens),
        )
    } else if running > 0 {
        format!("subs {}", running)
    } else {
        String::new()
    };
    if !info.is_empty() {
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
pub(crate) fn format_tokens(tokens: u64) -> String {
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

/// Handle a key event that scrolls the chat history. Returns `true` if the
/// key was consumed.
pub fn handle_chat_key(ui: &mut UiState, code: KeyCode) -> bool {
    match code {
        KeyCode::Home => {
            ui.chat_state.scroll_to_top();
            true
        }
        KeyCode::End => {
            ui.chat_state.scroll_to_bottom();
            true
        }
        KeyCode::PageUp => {
            ui.chat_state.scroll_up(10);
            true
        }
        KeyCode::PageDown => {
            ui.chat_state.scroll_down(10);
            true
        }
        _ => false,
    }
}

pub(crate) fn message_to_lines(msg: &Message) -> Vec<Line<'static>> {
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

    #[test]
    fn scroll_to_top_is_clamped_to_max_scroll() {
        let (clamped, top_scroll) = compute_history_scroll(100, 20, usize::MAX);
        assert_eq!(clamped, 80);
        assert_eq!(top_scroll, 0);
    }

    #[test]
    fn scroll_at_bottom_uses_max_top_scroll() {
        let (clamped, top_scroll) = compute_history_scroll(100, 20, 0);
        assert_eq!(clamped, 0);
        assert_eq!(top_scroll, 80);
    }

    #[test]
    fn scroll_fits_when_content_shorter_than_area() {
        let (clamped, top_scroll) = compute_history_scroll(10, 20, 5);
        assert_eq!(clamped, 0);
        assert_eq!(top_scroll, 0);
    }

    #[test]
    fn candidate_scroll_offset_clamps_and_centers() {
        // Short list: no scrolling needed.
        assert_eq!(candidate_scroll_offset(5, 2, 8), 0);
        // Long list: selection near top stays at top.
        assert_eq!(candidate_scroll_offset(20, 1, 8), 0);
        // Selection centered in the middle.
        assert_eq!(candidate_scroll_offset(20, 10, 8), 10 - 4);
        // Selection near bottom clamps to remaining rows.
        assert_eq!(candidate_scroll_offset(20, 18, 8), 12);
        // Last item keeps the window at the bottom.
        assert_eq!(candidate_scroll_offset(20, 19, 8), 12);
    }
}

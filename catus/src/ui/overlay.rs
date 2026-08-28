//! Modal overlay pages rendered on top of the chat view.
//!
//! Two pages exist:
//! - [`Overlay::Resume`](crate::app::Overlay::Resume): interactive history
//!   picker backed by a ratatui [`List`].
//! - [`Overlay::Status`](crate::app::Overlay::Status): token usage details.
//!
//! Key handling for overlays lives in `crate::app::App::handle_overlay_key`;
//! this module only renders.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table},
};

use crate::app::{App, Overlay};

/// Render the active overlay centered over the chat view.
pub fn draw_overlay(frame: &mut Frame, app: &App) {
    let popup = centered_rect(60, 70, frame.area());
    frame.render_widget(Clear, popup);

    match &app.overlay {
        Overlay::None => {}
        Overlay::Resume { items, selected } => draw_resume(frame, items, *selected, popup),
        Overlay::Status => draw_status(frame, app, popup),
        Overlay::Config { selected } => draw_config(frame, app, *selected, popup),
    }
}

fn draw_resume(frame: &mut Frame, items: &[String], selected: usize, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Resume ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::styled(
            "(no saved histories)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        items
            .iter()
            .map(|name| ListItem::new(Line::from(name.clone())))
            .collect()
    };

    let list = List::new(list_items)
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, rows[0], &mut state);

    let help = Paragraph::new(Line::styled(
        "↑/↓ select · Enter load · Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Status ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let usage_rows: Vec<Row> = vec![
        label_value_row("model", &app.config.api.model),
        label_value_row("requests", &app.request_count.to_string()),
        label_value_row("prompt tokens", &app.usage.prompt_tokens.to_string()),
        label_value_row("  cached", &app.usage.cached_tokens.to_string()),
        label_value_row(
            "completion tokens",
            &app.usage.completion_tokens.to_string(),
        ),
        label_value_row("total tokens", &app.usage.total_tokens.to_string()),
        label_value_row(
            "context (last prompt)",
            &app.usage.context_tokens().to_string(),
        ),
    ];

    let table = Table::new(usage_rows, [Constraint::Length(22), Constraint::Fill(1)]);
    frame.render_widget(table, rows[0]);

    let help = Paragraph::new(Line::styled(
        "Esc/q close",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn draw_config(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Config ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let fields = app.config_fields();
    let items: Vec<ListItem> = fields
        .iter()
        .enumerate()
        .map(|(i, (key, value))| {
            let text = format!("{}: {}", key, value);
            let style = if i == selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Cyan)
            };
            ListItem::new(Line::styled(text, style))
        })
        .collect();

    let list = List::new(items).highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, rows[0], &mut state);

    let help = Paragraph::new(Line::styled(
        "↑/↓ select · Enter edit · Esc/q close",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn label_value_row(label: &str, value: &str) -> Row<'static> {
    Row::new(vec![
        Line::styled(
            format!("{}: ", label),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(value.to_string()),
    ])
}

/// Return a rectangle centered inside `area` covering `percent_x` ×
/// `percent_y` of it, clamped to the available space.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(((100 - percent_y) / 2).min(49)),
            Constraint::Percentage(percent_y.min(100)),
            Constraint::Percentage(((100 - percent_y) / 2).min(49)),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(((100 - percent_x) / 2).min(49)),
            Constraint::Percentage(percent_x.min(100)),
            Constraint::Percentage(((100 - percent_x) / 2).min(49)),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_covers_full_area_at_100_percent() {
        let area = Rect::new(0, 0, 80, 24);
        let rect = centered_rect(100, 100, area);
        assert_eq!(rect.width, 80);
        assert_eq!(rect.height, 24);
    }

    #[test]
    fn centered_rect_is_inside_area_and_smaller() {
        let area = Rect::new(0, 0, 80, 24);
        let rect = centered_rect(60, 50, area);
        assert!(rect.width < area.width);
        assert!(rect.height < area.height);
        assert!(rect.x >= area.x);
        assert!(rect.y >= area.y);
        assert!(rect.right() <= area.right());
        assert!(rect.bottom() <= area.bottom());

        // Horizontally centered.
        let left_gap = rect.x - area.x;
        let right_gap = area.right() - rect.right();
        assert!((left_gap as i32 - right_gap as i32).abs() <= 1);
    }

    #[test]
    fn label_value_row_pairs_columns() {
        let row = label_value_row("model", "gpt-test");
        let expected = Row::new(vec![
            Line::styled(
                "model: ".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("gpt-test".to_string()),
        ]);
        assert_eq!(format!("{:?}", row), format!("{:?}", expected));
    }
}

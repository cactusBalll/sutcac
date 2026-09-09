//! Modal overlay pages rendered on top of the chat view.
//!
//! Pages:
//! - [`Overlay::Resume`]: interactive history picker backed by a ratatui
//!   [`List`].
//! - [`Overlay::Status`]: token usage details.
//! - [`Overlay::Config`]: editable config fields.
//! - [`Overlay::Skills`]: skill picker.
//! - [`Overlay::Model`]: model picker.
//! - [`Overlay::Ask`]: question dialog opened by the `ask_user` tool.
//!
//! This module renders overlays and handles keyboard navigation for them.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table},
};

use crate::ui::{Overlay, UiState};
use catus_core::app::App;
use catus_core::tool::{AskAnswer, AskQuestion, collect_answer};

/// Result of handling a key press while an overlay is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    /// The key was consumed by the overlay; nothing else to do.
    Consumed,
    /// The overlay was closed without an action.
    Closed,
    /// The resume picker confirmed a history name to load.
    LoadHistory(String),
    /// The skill picker confirmed a skill name to activate.
    ActivateSkill(String),
    /// The model picker confirmed a model id to switch to.
    SwitchModel(String),
    /// The ask overlay collected answers for every question.
    Answered(Vec<AskAnswer>),
    /// The ask overlay was dismissed without answering.
    CancelInteraction,
    /// The agent picker confirmed an agent name to dispatch.
    ActivateAgent(String),
    /// The subagent status picker confirmed a subagent id to watch.
    WatchSubagent(String),
}

/// Handle a key press while an overlay is active. Keys never reach the
/// input line while an overlay is open.
pub fn handle_overlay_key(app: &mut App, ui: &mut UiState, code: KeyCode) -> OverlayAction {
    match ui.overlay_state.overlay.clone() {
        Overlay::None => OverlayAction::Consumed,
        Overlay::Status => match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
        Overlay::Resume { items, selected } => match code {
            KeyCode::Up => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + items.len() - 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Resume {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Down => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Resume {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Enter => {
                let chosen = items.get(selected).cloned();
                ui.overlay_state.close();
                match chosen {
                    Some(name) => OverlayAction::LoadHistory(name),
                    None => OverlayAction::Closed,
                }
            }
            KeyCode::Esc => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
        Overlay::Skills { items, selected } => match code {
            KeyCode::Up => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + items.len() - 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Skills {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Down => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Skills {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Enter => {
                let chosen = items.get(selected).cloned();
                ui.overlay_state.close();
                match chosen {
                    Some(name) => OverlayAction::ActivateSkill(name),
                    None => OverlayAction::Closed,
                }
            }
            KeyCode::Esc => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
        Overlay::Model { items, selected } => match code {
            KeyCode::Up => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + items.len() - 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Model {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Down => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Model {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Enter => {
                let chosen = items.get(selected).cloned();
                ui.overlay_state.close();
                match chosen {
                    Some(id) => OverlayAction::SwitchModel(id),
                    None => OverlayAction::Closed,
                }
            }
            KeyCode::Esc => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
        Overlay::Ask {
            questions,
            current,
            focus,
            mut selections,
            mut other,
            mut answers,
        } => {
            let Some(question) = questions.get(current).cloned() else {
                ui.overlay_state.close();
                return OverlayAction::CancelInteraction;
            };
            // Focus rows: `0..options.len()` are the options, the row after
            // them is the pseudo-option "Other".
            let rows = question.options.len() + 1;
            let other_row = question.options.len();
            let current_selections: &[bool] =
                selections.get(current).map(Vec::as_slice).unwrap_or(&[]);
            match code {
                KeyCode::Up => {
                    ui.overlay_state.overlay = Overlay::Ask {
                        questions,
                        current,
                        focus: (focus + rows - 1) % rows,
                        selections,
                        other,
                        answers,
                    };
                    OverlayAction::Consumed
                }
                KeyCode::Down => {
                    ui.overlay_state.overlay = Overlay::Ask {
                        questions,
                        current,
                        focus: (focus + 1) % rows,
                        selections,
                        other,
                        answers,
                    };
                    OverlayAction::Consumed
                }
                KeyCode::Char(c) if focus == other_row => {
                    other.push(c);
                    ui.overlay_state.overlay = Overlay::Ask {
                        questions,
                        current,
                        focus,
                        selections,
                        other,
                        answers,
                    };
                    OverlayAction::Consumed
                }
                KeyCode::Backspace if focus == other_row => {
                    other.pop();
                    ui.overlay_state.overlay = Overlay::Ask {
                        questions,
                        current,
                        focus,
                        selections,
                        other,
                        answers,
                    };
                    OverlayAction::Consumed
                }
                KeyCode::Char(' ') if question.multi_select && focus < other_row => {
                    if let Some(slot) = selections
                        .get_mut(current)
                        .and_then(|sel| sel.get_mut(focus))
                    {
                        *slot = !*slot;
                    }
                    ui.overlay_state.overlay = Overlay::Ask {
                        questions,
                        current,
                        focus,
                        selections,
                        other,
                        answers,
                    };
                    OverlayAction::Consumed
                }
                KeyCode::Enter => {
                    match collect_answer(&question, current_selections, focus, &other) {
                        Some(answer) => {
                            answers.push(answer);
                            if current + 1 < questions.len() {
                                ui.overlay_state.overlay = Overlay::Ask {
                                    questions,
                                    current: current + 1,
                                    focus: 0,
                                    selections,
                                    other: String::new(),
                                    answers,
                                };
                                OverlayAction::Consumed
                            } else {
                                ui.overlay_state.close();
                                OverlayAction::Answered(answers)
                            }
                        }
                        // Nothing answered yet; wait for a selection.
                        None => OverlayAction::Consumed,
                    }
                }
                KeyCode::Esc => {
                    ui.overlay_state.close();
                    OverlayAction::CancelInteraction
                }
                _ => OverlayAction::Consumed,
            }
        }
        Overlay::Config { selected } => {
            let fields = app.config_fields();
            match code {
                KeyCode::Up => {
                    let next = if fields.is_empty() {
                        0
                    } else {
                        (selected + fields.len() - 1) % fields.len()
                    };
                    ui.overlay_state.overlay = Overlay::Config { selected: next };
                    OverlayAction::Consumed
                }
                KeyCode::Down => {
                    let next = if fields.is_empty() {
                        0
                    } else {
                        (selected + 1) % fields.len()
                    };
                    ui.overlay_state.overlay = Overlay::Config { selected: next };
                    OverlayAction::Consumed
                }
                KeyCode::Enter => {
                    if let Some((key, value)) = fields.get(selected) {
                        ui.input_state.input = format!("/config set {} {}", key, value);
                        ui.input_state.cursor = ui.input_state.input.len();
                        ui.input_state.recompute_candidates();
                    }
                    ui.overlay_state.close();
                    OverlayAction::Closed
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    ui.overlay_state.close();
                    OverlayAction::Closed
                }
                _ => OverlayAction::Consumed,
            }
        }
        Overlay::Agents { items, selected } => match code {
            KeyCode::Up => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + items.len() - 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Agents {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Down => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::Agents {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Enter => {
                let chosen = items.get(selected).cloned();
                ui.overlay_state.close();
                match chosen {
                    Some(name) => OverlayAction::ActivateAgent(name),
                    None => OverlayAction::Closed,
                }
            }
            KeyCode::Esc => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
        Overlay::SubagentStatus { items, selected } => match code {
            KeyCode::Up => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + items.len() - 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::SubagentStatus {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Down => {
                let next = if items.is_empty() {
                    0
                } else {
                    (selected + 1) % items.len()
                };
                ui.overlay_state.overlay = Overlay::SubagentStatus {
                    items,
                    selected: next,
                };
                OverlayAction::Consumed
            }
            KeyCode::Enter => {
                let chosen = items.get(selected).cloned();
                ui.overlay_state.close();
                match chosen {
                    Some(id) => OverlayAction::WatchSubagent(id),
                    None => OverlayAction::Closed,
                }
            }
            KeyCode::Esc => {
                ui.overlay_state.close();
                OverlayAction::Closed
            }
            _ => OverlayAction::Consumed,
        },
    }
}

/// Move the overlay selection with the mouse wheel. Returns true if the
/// scroll was consumed by an overlay.
pub fn handle_overlay_scroll(app: &mut App, ui: &mut UiState, up: bool) -> bool {
    match &ui.overlay_state.overlay {
        Overlay::Resume { .. }
        | Overlay::Config { .. }
        | Overlay::Skills { .. }
        | Overlay::Model { .. }
        | Overlay::Agents { .. }
        | Overlay::SubagentStatus { .. }
        | Overlay::Ask { .. } => {
            let _ = handle_overlay_key(app, ui, if up { KeyCode::Up } else { KeyCode::Down });
            true
        }
        _ => false,
    }
}

/// Render the active overlay centered over the chat view.
pub fn draw_overlay(frame: &mut Frame, app: &App, ui: &UiState) {
    let popup = centered_rect(60, 70, frame.area());
    frame.render_widget(Clear, popup);

    match &ui.overlay_state.overlay {
        Overlay::None => {}
        Overlay::Resume { items, selected } => draw_resume(frame, items, *selected, popup),
        Overlay::Status => draw_status(frame, app, popup),
        Overlay::Config { selected } => draw_config(frame, app, *selected, popup),
        Overlay::Skills { items, selected } => draw_skills(frame, items, *selected, popup),
        Overlay::Model { items, selected } => draw_model(frame, app, items, *selected, popup),
        Overlay::Agents { items, selected } => draw_agents(frame, app, items, *selected, popup),
        Overlay::SubagentStatus { items, selected } => {
            draw_subagent_status(frame, app, items, *selected, popup)
        }
        Overlay::Ask {
            questions,
            current,
            focus,
            selections,
            other,
            ..
        } => draw_ask(frame, questions, *current, *focus, selections, other, popup),
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

    let context_window = if app.current_model.context_window > 0 {
        app.current_model.context_window.to_string()
    } else {
        "unknown".to_string()
    };
    let usage_rows: Vec<Row> = vec![
        label_value_row("model", app.current_model.display_name()),
        label_value_row("provider", &app.current_model.provider.name),
        label_value_row("context window", &context_window),
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

fn draw_skills(frame: &mut Frame, items: &[String], selected: usize, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Skills ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::styled(
            "(no skills discovered)",
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
        "↑/↓ select · Enter activate · Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn draw_model(frame: &mut Frame, app: &App, items: &[String], selected: usize, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Model ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::styled(
            "(no models configured)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        items
            .iter()
            .map(|id| {
                let text = match app.models.iter().find(|m| m.id == *id) {
                    Some(model) => {
                        let ctx = if model.context_window > 0 {
                            model.context_window.to_string()
                        } else {
                            "unknown".to_string()
                        };
                        let mut text = format!(
                            "{} ({} · {} · ctx {})",
                            model.display_name(),
                            model.id,
                            model.provider.name,
                            ctx
                        );
                        if model.id == app.current_model.id {
                            text.push_str("  ← current");
                        }
                        text
                    }
                    None => id.clone(),
                };
                ListItem::new(Line::from(text))
            })
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
        "↑/↓ select · Enter switch · Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn draw_agents(frame: &mut Frame, app: &App, items: &[String], selected: usize, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Agents ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::styled(
            "(no agents discovered)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        items
            .iter()
            .map(|name| {
                let text = match app.agent_registry.get(name) {
                    Some(agent) => format!("{} — {}", agent.name, agent.description),
                    None => name.clone(),
                };
                ListItem::new(Line::from(text))
            })
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
        "↑/↓ select · Enter dispatch · Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[1]);
}

fn draw_subagent_status(
    frame: &mut Frame,
    app: &App,
    items: &[String],
    selected: usize,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Subagent Status ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::styled(
            "(no active subagents)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        items
            .iter()
            .map(|id| {
                let text = match app.subagents.get(id) {
                    Some(s) => format!("{} [{}] {}", s.id, s.state.as_str(), s.name),
                    None => id.clone(),
                };
                ListItem::new(Line::from(text))
            })
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
        "↑/↓ select · Enter watch · Esc cancel",
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

/// Render the ask-user question dialog: the current question's prompt, its
/// options (with checkbox markers for multi-select), the "Other" free-text
/// row, and a context-sensitive help footer.
fn draw_ask(
    frame: &mut Frame,
    questions: &[AskQuestion],
    current: usize,
    focus: usize,
    selections: &[Vec<bool>],
    other: &str,
    area: Rect,
) {
    let Some(question) = questions.get(current) else {
        return;
    };
    let title = format!(" {} ({}/{}) ", question.title, current + 1, questions.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let prompt = Paragraph::new(question.prompt.as_str())
        .style(Style::default().add_modifier(Modifier::BOLD))
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(prompt, rows[0]);

    let current_selections = selections.get(current).cloned().unwrap_or_default();
    let marker = |checked: bool| if checked { "[x] " } else { "[ ] " };
    let mut items: Vec<ListItem> = question
        .options
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let prefix = if question.multi_select {
                marker(current_selections.get(i).copied().unwrap_or(false))
            } else {
                ""
            };
            let mut spans = vec![Span::raw(format!("{}{}", prefix, option.label))];
            if let Some(description) = &option.description {
                spans.push(Span::styled(
                    format!("  — {}", description),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let other_row = question.options.len();
    let other_prefix = if question.multi_select {
        marker(!other.trim().is_empty())
    } else {
        ""
    };
    let other_style = if focus == other_row {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    items.push(ListItem::new(Line::styled(
        format!("{}Other: {}", other_prefix, other),
        other_style,
    )));

    let list = List::new(items).highlight_symbol("▶ ").highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    state.select(Some(focus.min(other_row)));
    frame.render_stateful_widget(list, rows[1], &mut state);

    let help = if question.multi_select {
        "↑/↓ move · Space toggle · Enter confirm · Esc cancel"
    } else {
        "↑/↓ move · Enter choose · Esc cancel"
    };
    let help = Paragraph::new(Line::styled(help, Style::default().fg(Color::DarkGray)))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help, rows[2]);
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

    mod ask {
        use super::*;
        use crate::ui::Overlay;
        use catus_core::config::AppConfig;
        use catus_core::tool::{Answer, AskAnswer, AskQuestion};

        fn question(multi_select: bool) -> AskQuestion {
            serde_json::from_str(&format!(
                r#"{{"prompt":"Pick","title":"Pick","options":[{{"label":"a"}},{{"label":"b"}}],"multiSelect":{}}}"#,
                multi_select
            ))
            .unwrap()
        }

        fn app_with_ask(questions: Vec<AskQuestion>) -> (App, UiState) {
            let mut app = App::new(AppConfig::default());
            let mut ui = UiState::new();
            ui.overlay_state.open_ask(questions);
            (app, ui)
        }

        #[test]
        fn single_select_enter_picks_focused_option() {
            let (mut app, mut ui) = app_with_ask(vec![question(false)]);
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Down),
                OverlayAction::Consumed
            );
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Answered(vec![AskAnswer {
                    prompt: "Pick".to_string(),
                    answer: Answer::One("b".to_string()),
                }])
            );
            assert!(!ui.overlay_state.is_active());
        }

        #[test]
        fn single_select_other_row_uses_typed_text() {
            let (mut app, mut ui) = app_with_ask(vec![question(false)]);
            // Focus row 2 is the pseudo-option "Other" (after options a, b).
            handle_overlay_key(&mut app, &mut ui, KeyCode::Down);
            handle_overlay_key(&mut app, &mut ui, KeyCode::Down);
            handle_overlay_key(&mut app, &mut ui, KeyCode::Char('x'));
            handle_overlay_key(&mut app, &mut ui, KeyCode::Char('y'));
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Answered(vec![AskAnswer {
                    prompt: "Pick".to_string(),
                    answer: Answer::One("xy".to_string()),
                }])
            );
        }

        #[test]
        fn multi_select_toggles_and_collects() {
            let (mut app, mut ui) = app_with_ask(vec![question(true)]);
            // Check option "a" with Space, then confirm.
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Char(' ')),
                OverlayAction::Consumed
            );
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Answered(vec![AskAnswer {
                    prompt: "Pick".to_string(),
                    answer: Answer::Many(vec!["a".to_string()]),
                }])
            );
        }

        #[test]
        fn multi_select_requires_at_least_one_answer() {
            let (mut app, mut ui) = app_with_ask(vec![question(true)]);
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Consumed,
                "Enter without any selection must not submit"
            );
            assert!(ui.overlay_state.is_active());
        }

        #[test]
        fn multiple_questions_advance_then_answer() {
            let (mut app, mut ui) = app_with_ask(vec![question(false), question(false)]);
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Consumed,
                "first Enter advances to the next question"
            );
            match &ui.overlay_state.overlay {
                Overlay::Ask { current, .. } => assert_eq!(*current, 1),
                other => panic!("expected ask overlay, got {:?}", other),
            }
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::Answered(vec![
                    AskAnswer {
                        prompt: "Pick".to_string(),
                        answer: Answer::One("a".to_string()),
                    },
                    AskAnswer {
                        prompt: "Pick".to_string(),
                        answer: Answer::One("a".to_string()),
                    },
                ])
            );
        }

        #[test]
        fn esc_cancels_the_interaction() {
            let (mut app, mut ui) = app_with_ask(vec![question(false)]);
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Esc),
                OverlayAction::CancelInteraction
            );
            assert!(!ui.overlay_state.is_active());
        }
    }

    mod model_picker {
        use super::*;
        use crate::ui::Overlay;
        use catus_core::config::{AppConfig, ModelEntry};
        use catus_core::llm::Provider;

        fn app_with_models() -> (App, UiState) {
            let mut config = AppConfig {
                providers: vec![Provider {
                    name: "test".to_string(),
                    base_url: "https://example.com".to_string(),
                    api_key: "test".to_string(),
                    session_header: None,
                }],
                models: vec![
                    ModelEntry {
                        id: "model-a".to_string(),
                        name: "Model A".to_string(),
                        context_window: 4096,
                        provider: "test".to_string(),
                    },
                    ModelEntry {
                        id: "model-b".to_string(),
                        name: "Model B".to_string(),
                        context_window: 8192,
                        provider: "test".to_string(),
                    },
                ],
                ..Default::default()
            };
            config.agent.auto_include_skills = false;
            let mut app = App::new(config);
            let items: Vec<String> = app.models.iter().map(|m| m.id.clone()).collect();
            let selected = app
                .models
                .iter()
                .position(|m| m.id == app.current_model.id)
                .unwrap_or(0);
            let mut ui = UiState::new();
            ui.overlay_state.open_model(items, selected);
            (app, ui)
        }

        #[test]
        fn enter_confirms_selected_model_id() {
            let (mut app, mut ui) = app_with_models();
            handle_overlay_key(&mut app, &mut ui, KeyCode::Down);
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Enter),
                OverlayAction::SwitchModel("model-b".to_string())
            );
            assert!(!ui.overlay_state.is_active());
        }

        #[test]
        fn selection_wraps_and_esc_closes() {
            let (mut app, mut ui) = app_with_models();
            handle_overlay_key(&mut app, &mut ui, KeyCode::Up);
            match &ui.overlay_state.overlay {
                Overlay::Model { selected, .. } => assert_eq!(*selected, 1),
                other => panic!("expected model overlay, got {:?}", other),
            }
            assert_eq!(
                handle_overlay_key(&mut app, &mut ui, KeyCode::Esc),
                OverlayAction::Closed
            );
            assert_eq!(ui.overlay_state.overlay, Overlay::None);
        }
    }

    mod resume_picker {
        use super::*;
        use crate::ui::Overlay;
        use catus_core::app::App;
        use catus_core::config::AppConfig;
        use catus_core::history::SessionStore;
        use catus_core::message::Message;

        fn test_app_with_history_dir(dir: &std::path::Path) -> App {
            let config = AppConfig {
                agent: catus_core::config::AgentConfig {
                    history_path: Some(dir.to_path_buf()),
                    ..Default::default()
                },
                ..Default::default()
            };
            App::new(config)
        }

        fn seed_session(dir: &std::path::Path, name: &str, messages: &[Message]) {
            let store = SessionStore::open(dir).unwrap();
            let id = store
                .create_session(name, "catus-seed", "test-model")
                .unwrap();
            store
                .replace_messages(id, catus_core::history::MAIN_AGENT_ID, messages)
                .unwrap();
        }

        #[tokio::test]
        async fn picker_navigation_and_enter_loads_history() {
            let dir =
                std::env::temp_dir().join(format!("catus_resume_overlay_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            seed_session(&dir, "alpha", &[Message::user("hi")]);
            seed_session(&dir, "beta", &[]);

            let mut app = test_app_with_history_dir(&dir);
            let mut ui = UiState::new();
            let items = app.list_session_names();
            ui.overlay_state.open_resume(items);

            // Arrow keys move the selection with wrap-around.
            handle_overlay_key(&mut app, &mut ui, KeyCode::Down);
            match &ui.overlay_state.overlay {
                Overlay::Resume { items, selected } => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(*selected, 1);
                }
                other => panic!("expected resume overlay, got {:?}", other),
            }
            handle_overlay_key(&mut app, &mut ui, KeyCode::Down);
            match &ui.overlay_state.overlay {
                Overlay::Resume { selected, .. } => assert_eq!(*selected, 0),
                other => panic!("expected resume overlay, got {:?}", other),
            }

            // Enter yields the selected session name for the event loop to load.
            let action = handle_overlay_key(&mut app, &mut ui, KeyCode::Enter);
            match action {
                OverlayAction::LoadHistory(name) => {
                    let store = SessionStore::open(&dir).unwrap();
                    assert!(store.find_session(&name).unwrap().is_some());
                }
                other => panic!("expected LoadHistory, got {:?}", other),
            }
            assert!(!ui.overlay_state.is_active());

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn esc_closes_without_loading() {
            let dir = std::env::temp_dir().join(format!("catus_resume_esc_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            seed_session(&dir, "a", &[]);

            let mut app = test_app_with_history_dir(&dir);
            let mut ui = UiState::new();
            let items = app.list_session_names();
            ui.overlay_state.open_resume(items);
            handle_overlay_key(&mut app, &mut ui, KeyCode::Esc);
            assert_eq!(ui.overlay_state.overlay, Overlay::None);
            assert!(app.current_session_id.is_none());

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    mod config_overlay {
        use super::*;
        use crate::ui::Overlay;
        use catus_core::config::AppConfig;

        #[test]
        fn enter_prefills_edit_command() {
            let dir =
                std::env::temp_dir().join(format!("catus_config_enter_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            let mut app = App::new(AppConfig::default());
            let mut ui = UiState::new();
            ui.overlay_state.open_config();
            handle_overlay_key(&mut app, &mut ui, KeyCode::Enter);
            assert!(ui.input_state.input.starts_with("/config set "));
            assert_eq!(ui.overlay_state.overlay, Overlay::None);

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

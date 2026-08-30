//! catus: an Agent tool prototype with OpenAI-compatible API and TUI.

use std::fs::OpenOptions;
use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use log::LevelFilter;
use simplelog::{Config, WriteLogger};
use tokio::sync::mpsc;

use catus::app::{App, AppStatus, OverlayResult};
use catus::config::AppConfig;
use catus::llm::{LlmClient, LlmError, StreamEvent};
use catus::message::Message;
use catus::tool::{ToolResult, execute_shell_command};
use catus::tui;
use catus::ui;

enum UiEvent {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if let Some(prompt) = parse_test_arg(&args) {
        let config = load_config()?;
        init_logger(&config.effective_log_path(), config.effective_log_level());
        return run_test_mode(prompt, config).await;
    }

    let config = load_config()?;
    init_logger(&config.effective_log_path(), config.effective_log_level());
    run_tui_mode(config).await
}

fn parse_test_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--test" {
            return iter.next().cloned();
        }
    }
    None
}

fn init_logger(path: &Path, level: LevelFilter) {
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = WriteLogger::init(level, Config::default(), file);
    }
}

async fn run_test_mode(
    prompt: String,
    config: AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(config);
    let client = app.client.clone();
    let max_rounds = app.max_tool_rounds;

    println!("USER: {}", prompt);
    app.messages.push(Message::user(prompt));

    for round in 0..max_rounds {
        match client.chat(&app.messages).await {
            Ok(reply) => {
                if !reply.content.is_empty() {
                    println!("ASSISTANT: {}", reply.content);
                }
                if !reply.reasoning_content.is_empty() {
                    println!("REASONING: {}", reply.reasoning_content);
                }
                if let Some(usage) = &reply.usage {
                    println!(
                        "USAGE: prompt={} completion={} total={} cached={}",
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.total_tokens,
                        usage.cached_tokens
                    );
                }

                if !reply.content.is_empty() || !reply.reasoning_content.is_empty() {
                    let mut msg = Message::assistant(reply.content);
                    msg.reasoning_content = reply.reasoning_content;
                    app.messages.push(msg);
                }

                // Handle skill activation markers in the assistant reply.
                if let Some(name) = app.take_skill_activation_marker() {
                    match app.activate_skill(&name) {
                        Ok(msg) => {
                            app.status = catus::app::AppStatus::Idle;
                            app.status_message = msg.clone();
                            println!("SKILL ACTIVATED: {}", msg);
                            continue;
                        }
                        Err(e) => {
                            eprintln!("catus: failed to activate skill '{}': {}", name, e);
                            std::process::exit(1);
                        }
                    }
                }

                if let Some(call) = reply.tool_calls.first() {
                    let command = match call.shell_command() {
                        Some(cmd) => cmd,
                        None => {
                            log::error!("malformed tool call arguments: {}", call.arguments);
                            eprintln!(
                                "catus: tool call format error: expected {{\"command\": \"...\"}}, got: {}",
                                call.arguments
                            );
                            std::process::exit(1);
                        }
                    };
                    log::info!("tool call {} -> {}", call.id, command);
                    println!("TOOL CALL: {} -> {}", call.id, command);
                    let output = execute_shell_command(&command, &mut app.shell_state);
                    let result = ToolResult {
                        call: call.clone(),
                        status: output.status,
                        stdout: output.stdout,
                        stderr: output.stderr,
                    };
                    println!(
                        "TOOL RESULT: status={}\nstdout=```\n{}\n```\nstderr=```\n{}\n```",
                        result.status, result.stdout, result.stderr
                    );
                    app.messages
                        .push(Message::tool(result.to_message(), call.id.clone()));
                } else {
                    println!(
                        "[no tool call, conversation complete after {} round(s)]",
                        round + 1
                    );
                    break;
                }
            }
            Err(e) => {
                log::error!("llm error: {}", e);
                eprintln!("catus: llm error: {}", e);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

async fn run_tui_mode(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let app = App::new(config);
    let client = app.client.clone();

    let mut guard = tui::TerminalGuard::new(tui::init_terminal()?);
    let terminal = guard.terminal();
    terminal.clear()?;

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(128);
    let (done_tx, mut done_rx) = mpsc::channel::<Result<(), LlmError>>(1);

    // Spawn a blocking task to read crossterm events.
    tokio::task::spawn_blocking(move || {
        loop {
            match crossterm::event::read() {
                Ok(Event::Key(key)) => {
                    if ui_tx.blocking_send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Mouse(mouse)) => {
                    if ui_tx.blocking_send(UiEvent::Mouse(mouse)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    let _ = ui_tx.blocking_send(UiEvent::Resize);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let mut app = app;
    // History is no longer loaded automatically on startup; use `/resume`.

    let mut should_quit = false;

    while !should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        tokio::select! {
            Some(event) = ui_rx.recv() => {
                match event {
                    UiEvent::Key(key) => {
                        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                            continue;
                        }

                        // Ctrl+C always quits, even with an overlay open.
                        if key.code == KeyCode::Char('c')
                            && key.modifiers == KeyModifiers::CONTROL
                        {
                            should_quit = true;
                            continue;
                        }

                        // Modal overlays swallow all other keys first.
                        if app.overlay_active() {
                            match app.handle_overlay_key(key.code) {
                                OverlayResult::LoadHistory(name) => {
                                    match app.resume_history(Some(&name)) {
                                        Ok(msg) => {
                                            app.status = AppStatus::Idle;
                                            app.status_message = msg;
                                        }
                                        Err(e) => app.set_error(e.to_string()),
                                    }
                                }
                                OverlayResult::ActivateSkill(name) => {
                                    match app.activate_skill(&name) {
                                        Ok(msg) => {
                                            app.status = AppStatus::Idle;
                                            app.status_message = msg;
                                        }
                                        Err(e) => app.set_error(e.to_string()),
                                    }
                                }
                                OverlayResult::Closed | OverlayResult::Consumed => {}
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Home => {
                                app.scroll_to_top();
                            }
                            KeyCode::End => {
                                app.scroll_to_bottom();
                            }
                            KeyCode::Esc => {
                                // Esc only dismisses completion candidates or overlays;
                                // use /exit or Ctrl+C to quit.
                                if app.selected_candidate.is_some() {
                                    app.clear_candidate_selection();
                                }
                            }
                            KeyCode::Enter => {
                                let input = app.input.trim().to_string();
                                if app.handle_command(&input) {
                                    app.clear_input();
                                } else if app.submit_user_message().is_some() {
                                    start_llm_stream(&mut app, &client, &event_tx, &done_tx);
                                }
                            }
                            KeyCode::Char(c) => {
                                app.push_char(c);
                            }
                            KeyCode::Backspace => {
                                app.backspace();
                            }
                            KeyCode::Left => {
                                app.move_cursor_left();
                            }
                            KeyCode::Right => {
                                app.move_cursor_right();
                            }
                            KeyCode::Tab => {
                                app.cycle_candidate(1);
                            }
                            KeyCode::BackTab => {
                                app.cycle_candidate(-1);
                            }
                            KeyCode::Up => {
                                if app.input.starts_with('/') && !app.candidates.is_empty() {
                                    app.cycle_candidate(-1);
                                } else {
                                    app.history_previous();
                                }
                            }
                            KeyCode::Down => {
                                if app.input.starts_with('/') && !app.candidates.is_empty() {
                                    app.cycle_candidate(1);
                                } else {
                                    app.history_next();
                                }
                            }
                            KeyCode::PageUp => {
                                app.scroll_up(10);
                            }
                            KeyCode::PageDown => {
                                app.scroll_down(10);
                            }
                            _ => {}
                        }

                        if app.should_quit {
                            should_quit = true;
                        }
                    }
                    UiEvent::Mouse(mouse) => {
                        match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => {
                                if !app.handle_overlay_scroll(true) {
                                    app.scroll_up(3);
                                }
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                if !app.handle_overlay_scroll(false) {
                                    app.scroll_down(3);
                                }
                            }
                            _ => {}
                        }
                    }
                    UiEvent::Resize => {
                        terminal.autoresize()?;
                        terminal.clear()?;
                    }
                }
            }
            Some(event) = event_rx.recv() => {
                let is_content_chunk =
                    matches!(event, StreamEvent::Text(_) | StreamEvent::Reasoning(_));
                handle_stream_event(event, &mut app);
                if app.auto_scroll && is_content_chunk {
                    app.scroll_to_bottom();
                }
            }
            Some(result) = done_rx.recv() => {
                // The stream task sends every StreamEvent *before* signalling
                // done, but tokio::select! may pick the done branch first even
                // when queued events are still waiting. Drain them so tool
                // calls and text chunks are never applied after completion.
                while let Ok(event) = event_rx.try_recv() {
                    handle_stream_event(event, &mut app);
                }
                handle_llm_done(result, &mut app, &client, &event_tx, &done_tx);
            }
        }
    }

    if app.config.agent.history_path.is_some() {
        if let Err(e) = app.save_session_history() {
            log::error!("failed to save history: {}", e);
        }
    }

    Ok(())
}

fn handle_stream_event(event: StreamEvent, app: &mut App) {
    match event {
        StreamEvent::Text(text) => app.append_stream_text(&text),
        StreamEvent::Reasoning(text) => app.append_stream_reasoning(&text),
        StreamEvent::ToolCall(call) => app.add_tool_call(call),
        StreamEvent::Usage(usage) => app.record_usage(&usage),
    }
}

fn load_config() -> Result<AppConfig, Box<dyn std::error::Error>> {
    let config = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("catus: failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    if config.api.api_key.is_empty() {
        eprintln!(
            "catus: api.api_key is empty; set it in .sutcac/config.toml or ~/.config/catus/config.toml"
        );
        std::process::exit(1);
    }

    Ok(config)
}

fn start_llm_stream(
    app: &mut App,
    client: &LlmClient,
    event_tx: &mpsc::Sender<StreamEvent>,
    done_tx: &mpsc::Sender<Result<(), LlmError>>,
) {
    // Snapshot the conversation *before* adding the assistant placeholder so
    // the API request never contains an empty assistant message.
    let messages = app.messages.clone();
    app.start_assistant_message();
    let client = client.clone();
    let event_tx = event_tx.clone();
    let done_tx = done_tx.clone();

    tokio::spawn(async move {
        let result = client.stream_chat(&messages, event_tx).await;
        let _ = done_tx.send(result).await;
    });
}

fn handle_llm_done(
    result: Result<(), LlmError>,
    app: &mut App,
    client: &LlmClient,
    event_tx: &mpsc::Sender<StreamEvent>,
    done_tx: &mpsc::Sender<Result<(), LlmError>>,
) {
    match result {
        Ok(()) => {
            app.finish_stream();
            if app.has_pending_tool_call() {
                log::info!(
                    "{} pending tool call(s); running tool",
                    app.pending_tool_calls_count()
                );
                app.run_pending_tool();
                start_llm_stream(app, client, event_tx, done_tx);
            } else if app.pending_tool_calls_count() > 0 {
                let count = app.pending_tool_calls_count();
                if app.is_tool_round_limit_reached() {
                    let msg = format!(
                        "Reached max tool rounds ({}) for this turn; {} pending tool call(s) ignored.",
                        app.max_tool_rounds(),
                        count
                    );
                    log::warn!("{}", msg);
                    app.add_event_message(msg);
                }
                app.clear_pending_tool_calls();
            } else {
                log::info!("no pending tool call; turn complete");
            }
        }
        Err(e) => {
            // Remove the empty assistant placeholder so a failed request does
            // not leave an invalid assistant message in the conversation.
            if app.has_empty_assistant_placeholder() {
                app.messages.pop();
            }
            log::error!("llm stream error: {}", e);
            app.add_event_message(format!("LLM request failed: {}", e));
            app.set_error("LLM request failed".to_string());
        }
    }
}

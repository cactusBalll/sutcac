//! catus: an Agent tool prototype with OpenAI-compatible API and TUI.

use std::fs::OpenOptions;
use std::path::Path;

use crossterm::event::Event;
use log::LevelFilter;
use simplelog::{Config, WriteLogger};
use tokio::sync::mpsc;

use catus::app::App;
use catus::config::AppConfig;
use catus::llm::{LlmError, StreamEvent};
use catus::message::Message;
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

async fn run_test_mode(
    prompt: String,
    config: AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(config);
    let client = app.client.clone();
    let max_rounds = app.max_tool_rounds;

    // Connect to configured MCP servers before starting the conversation.
    let mcp_warnings = app.connect_mcp().await;
    for warning in &mcp_warnings {
        eprintln!("catus: mcp warning: {}", warning);
    }

    println!("USER: {}", prompt);
    app.messages.push(Message::user(prompt));

    for round in 0..max_rounds {
        let tools = app.toolbox.definitions();
        match client.chat(&app.messages, &tools).await {
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

                // Always push an assistant message so tool calls have a parent
                // message, matching the TUI flow.
                let mut msg = Message::assistant(reply.content);
                msg.reasoning_content = reply.reasoning_content;
                app.messages.push(msg);

                // Skill activation happens through `use_skill` tool calls,
                // handled by run_pending_tool below.

                for call in reply.tool_calls {
                    app.add_tool_call(call);
                }

                if app.has_pending_tool_call() {
                    while app.has_pending_tool_call() {
                        if let Some(message) = app.run_pending_tool().await {
                            println!("TOOL RESULT:\n{}", message);
                        }
                    }
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
    let mut app = App::new(config);
    let mcp_warnings = app.connect_mcp().await;
    for warning in &mcp_warnings {
        log::warn!("mcp warning: {}", warning);
    }

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

    let mut should_quit = false;

    while !should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        tokio::select! {
            Some(event) = ui_rx.recv() => {
                match event {
                    UiEvent::Key(key) => {
                        match ui::handle_key_event(&mut app, key).await {
                            ui::AppAction::None => {}
                            ui::AppAction::Quit => should_quit = true,
                            ui::AppAction::StartStream => {
                                app.start_llm_stream(event_tx.clone(), done_tx.clone()).await;
                            }
                            ui::AppAction::SetError(msg) => app.set_error(msg),
                        }
                    }
                    UiEvent::Mouse(mouse) => {
                        match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => {
                                if !ui::overlay::handle_overlay_scroll(&mut app, true) {
                                    app.chat_state.scroll_up(3);
                                }
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                if !ui::overlay::handle_overlay_scroll(&mut app, false) {
                                    app.chat_state.scroll_down(3);
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
                app.handle_stream_event(event);
                if app.chat_state.auto_scroll && is_content_chunk {
                    app.chat_state.scroll_to_bottom();
                }
            }
            Some(result) = done_rx.recv() => {
                // The stream task sends every StreamEvent *before* signalling
                // done, but tokio::select! may pick the done branch first even
                // when queued events are still waiting. Drain them so tool
                // calls and text chunks are never applied after completion.
                while let Ok(event) = event_rx.try_recv() {
                    app.handle_stream_event(event);
                }
                app.handle_llm_done(result, &event_tx, &done_tx).await;
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

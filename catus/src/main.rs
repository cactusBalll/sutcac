//! catus: an Agent tool prototype with OpenAI-compatible API and TUI.

use std::fs::OpenOptions;
use std::path::Path;

use clap::Parser;
use crossterm::event::Event;
use log::LevelFilter;
use simplelog::{Config, WriteLogger};
use tokio::sync::mpsc;

use catus::app::{App, AppStatus};
use catus::config::AppConfig;
use catus::llm::{LlmError, StreamEvent};
use catus::message::Message;
use catus::resources;
use catus::tui;
use catus::ui;

/// catus: an Agent TUI with an OpenAI-compatible API.
#[derive(Debug, Parser)]
#[command(name = "catus", version)]
struct Cli {
    /// Headless one-shot mode: run this prompt without the TUI and exit.
    #[arg(long, value_name = "PROMPT")]
    test: Option<String>,
    /// Dump the default config template, agents and skills into ./.sutcac/
    /// (existing files are never overwritten) and exit.
    #[arg(long)]
    install_project_config: bool,
}

enum UiEvent {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.install_project_config {
        let root = std::env::current_dir()?;
        match resources::install_project_config(&root) {
            Ok(report) => {
                println!(
                    "catus: installed project config into {}",
                    report.target_dir.display()
                );
                for (relative, action) in &report.files {
                    let verb = match action {
                        resources::InstallAction::Written => "written",
                        resources::InstallAction::Skipped => "kept",
                    };
                    println!("catus:   {verb}: .sutcac/{relative}");
                }
                if report.skipped() > 0 {
                    println!("catus: existing files were kept; edit them instead of overwriting");
                }
                println!("catus: fill in api_key in .sutcac/config.toml, then run catus again");
            }
            Err(e) => {
                eprintln!("catus: failed to install project config: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(prompt) = cli.test {
        let config = load_config()?;
        init_logger(&config.effective_log_path(), config.effective_log_level());
        return run_test_mode(prompt, config).await;
    }

    let config = load_config()?;
    init_logger(&config.effective_log_path(), config.effective_log_level());
    run_tui_mode(config).await
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

    if let Err(e) = config.resolve_models() {
        eprintln!("catus: invalid model configuration: {}", e);
        eprintln!("catus: configure at least one [[providers]] entry and one [[models]] entry");
        std::process::exit(1);
    }

    if let Err(e) = config.validate_tier_models() {
        eprintln!("catus: invalid tier model configuration: {}", e);
        eprintln!(
            "catus: set [agent.models] performance to a configured [[models]] id or name; efficient is optional"
        );
        std::process::exit(1);
    }

    if config.providers.iter().all(|p| p.api_key.is_empty()) {
        eprintln!(
            "catus: all provider api_keys are empty; set api_key in .sutcac/config.toml or ~/.config/catus/config.toml"
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
    if !app.main_agent_from_file {
        eprintln!("catus: main agent definition not found; create .sutcac/agents/main.md");
        std::process::exit(1);
    }
    for warning in &app.memory.warnings {
        eprintln!("catus: memory warning: {}", warning);
    }
    let client = app.client.clone();
    let max_rounds = app.max_tool_rounds;

    // Connect to configured MCP servers before starting the conversation.
    let mcp_warnings = app.connect_mcp().await;
    for warning in &mcp_warnings {
        eprintln!("catus: mcp warning: {}", warning);
    }

    println!("USER: {}", prompt);
    app.messages.push(Message::user(prompt));

    // Memory recall pass before the first LLM request, mirroring the TUI
    // flow (simple tasks skip memory inside the pass itself).
    let user_prompt = app
        .messages
        .iter()
        .rev()
        .find(|m| m.role == catus::message::Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default();
    app.maybe_dispatch_memory_recall(&user_prompt);
    app.await_memory_passes().await;

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
                        if app.has_pending_interaction() {
                            // The ask overlay needs the interactive TUI;
                            // report the question as cancelled so the model
                            // can continue without a user.
                            app.cancel_interaction();
                            println!(
                                "TOOL RESULT:\nuser cancelled (ask_user requires the interactive TUI)"
                            );
                            break;
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

    // Let the memory write pass summarize and record the turn before exit.
    app.maybe_dispatch_memory_write();
    app.await_memory_passes().await;

    Ok(())
}

async fn run_tui_mode(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(config);
    if !app.main_agent_from_file {
        eprintln!("catus: main agent definition not found; create .sutcac/agents/main.md");
        std::process::exit(1);
    }
    for warning in &app.memory.warnings {
        log::warn!("memory warning: {}", warning);
    }
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

    // Track the status shown in the terminal tab title and taskbar progress so
    // they are rewritten only on real transitions. The bell rings when a busy
    // turn settles to Idle; intermediate Idle blips between tool rounds never
    // become visible here.
    let mut last_status = app.status;
    tui::set_tab_title(tui::title_for_status(app.status));

    while !should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        let status = app.status;
        if status != last_status {
            let was_busy = matches!(last_status, AppStatus::Streaming | AppStatus::RunningTool);
            let is_busy = matches!(status, AppStatus::Streaming | AppStatus::RunningTool);
            if is_busy && !was_busy {
                tui::show_indeterminate_progress();
            } else if was_busy && !is_busy {
                tui::hide_taskbar_progress();
            }
            if was_busy && status == AppStatus::Idle {
                tui::alert();
            }
            tui::set_tab_title(tui::title_for_status(status));
            last_status = status;
        }

        tokio::select! {
            Some(event) = ui_rx.recv() => {
                match event {
                    UiEvent::Key(key) => {
                        match ui::handle_key_event(&mut app, key).await {
                            ui::AppAction::None => {}
                            ui::AppAction::Quit => should_quit = true,
                            ui::AppAction::StartStream => {
                                if app.awaiting_memory_recall() {
                                    // The user's message is still waiting for
                                    // the memory recall pass; the main stream
                                    // starts when the pass completes.
                                } else {
                                    app.start_llm_stream(event_tx.clone(), done_tx.clone())
                                        .await;
                                }
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
            Some(event) = app.subagents.event_rx.recv() => {
                if app.handle_subagent_event(event) {
                    app.start_llm_stream(event_tx.clone(), done_tx.clone()).await;
                }
            }
        }
    }

    // Final snapshot; incremental saves already cover most of the session.
    app.persist_session();

    Ok(())
}

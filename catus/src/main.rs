//! catus: an Agent tool prototype with OpenAI-compatible API and TUI.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use crossterm::event::Event;
use tokio::sync::mpsc;

use catus::tui;
use catus::ui::{self, UiState};
use catus_core::app::{AppStatus, RuntimeEvent};
use catus_core::bootstrap::{bootstrap_runtime, load_config};
use catus_core::config::AppConfig;

/// catus: an Agent TUI with an OpenAI-compatible API.
#[derive(Debug, Parser)]
#[command(name = "catus", version)]
struct Cli {
    /// Headless one-shot mode: run this prompt without the TUI and exit.
    #[arg(long, value_name = "PROMPT")]
    test: Option<String>,
    /// Working directory for this session: the workspace config
    /// (`<dir>/.sutcac/config.toml`), workspace agents/skills, and the
    /// recorded session history cwd are all resolved against it.
    #[arg(short = 'w', long, value_name = "DIR")]
    workspace: Option<PathBuf>,
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

    if let Some(dir) = &cli.workspace {
        if let Err(e) = enter_workspace(dir) {
            eprintln!("catus: {}", e);
            std::process::exit(1);
        }
    }

    if cli.install_project_config {
        let root = std::env::current_dir()?;
        match catus_core::resources::install_project_config(&root) {
            Ok(report) => {
                println!(
                    "catus: installed project config into {}",
                    report.target_dir.display()
                );
                for (relative, action) in &report.files {
                    let verb = match action {
                        catus_core::resources::InstallAction::Written => "written",
                        catus_core::resources::InstallAction::Skipped => "kept",
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
        let config = load_config().unwrap_or_else(exit_on_startup_error);
        let _log_guard = catus_core::logging::init_logging(
            &config.effective_log_path(),
            config.effective_log_level(),
        );
        return run_test_mode(prompt, config).await;
    }

    let config = load_config().unwrap_or_else(exit_on_startup_error);
    let _log_guard = catus_core::logging::init_logging(
        &config.effective_log_path(),
        config.effective_log_level(),
    );
    run_tui_mode(config).await
}

/// Switch the process into the requested workspace directory. Must run before
/// config loading so the workspace config (`<dir>/.sutcac/config.toml`),
/// workspace agents/skills, and the recorded session cwd all resolve there.
fn enter_workspace(dir: &Path) -> Result<(), String> {
    let resolved = dir
        .canonicalize()
        .map_err(|e| format!("cannot use workspace '{}': {}", dir.display(), e))?;
    if !resolved.is_dir() {
        return Err(format!(
            "workspace '{}' is not a directory",
            resolved.display()
        ));
    }
    std::env::set_current_dir(&resolved)
        .map_err(|e| format!("cannot enter workspace '{}': {}", resolved.display(), e))
}

/// Print a startup error the way the process-local versions used to, then exit.
fn exit_on_startup_error<T>(e: String) -> T {
    eprintln!("catus: {}", e);
    std::process::exit(1)
}

/// Headless one-shot mode. Drives the same runtime event loop as the TUI:
/// prints assistant deltas, tool activity, and cancels interactive
/// questions (there is no user to answer them).
async fn run_test_mode(
    prompt: String,
    config: AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = bootstrap_runtime(config, true)
        .await
        .unwrap_or_else(exit_on_startup_error);

    println!("USER: {}", prompt);
    runtime.app.submit_user_message(prompt);
    // Start the main stream unless a memory recall pass is still running.
    runtime.maybe_resume_stream().await;

    let mut printed = runtime.app.messages.len();
    let mut assistant_opened = false;

    while let Some(event) = runtime.next_event().await {
        match event {
            RuntimeEvent::StreamText(text) => {
                if !assistant_opened {
                    println!();
                    print!("ASSISTANT: ");
                    assistant_opened = true;
                }
                print!("{}", text);
                std::io::stdout().flush()?;
            }
            RuntimeEvent::ToolCallAdded(call) => {
                if assistant_opened {
                    println!();
                    assistant_opened = false;
                }
                println!("TOOL CALL: {} {}", call.name, call.arguments);
            }
            RuntimeEvent::MessagesChanged => {
                for message in &runtime.app.messages[printed..] {
                    match message.role {
                        catus_core::message::Role::Tool => {
                            println!("TOOL RESULT:\n{}", message.content);
                        }
                        catus_core::message::Role::Event => {
                            println!("{}", message.content);
                        }
                        _ => {}
                    }
                }
                printed = runtime.app.messages.len();
            }
            RuntimeEvent::InteractionRequested(_) => {
                if assistant_opened {
                    println!();
                    assistant_opened = false;
                }
                // The ask overlay needs the interactive TUI; report the
                // question as cancelled so the model can continue without a
                // user.
                println!("TOOL RESULT:\nuser cancelled (ask_user requires the interactive TUI)");
                runtime.app.cancel_interaction();
                runtime.maybe_resume_stream().await;
            }
            RuntimeEvent::TurnComplete => break,
            _ => {}
        }
    }

    // Let the background memory write pass summarize the turn before exit.
    runtime.app.await_memory_passes().await;
    runtime.app.persist_session();

    Ok(())
}

async fn run_tui_mode(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = bootstrap_runtime(config, false)
        .await
        .unwrap_or_else(exit_on_startup_error);
    let mut ui = UiState::new();

    let mut guard = tui::TerminalGuard::new(tui::init_terminal()?);
    let terminal = guard.terminal();
    terminal.clear()?;

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(32);

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
    let mut last_status = runtime.app.status;
    tui::set_tab_title(tui::title_for_status(runtime.app.status));

    while !should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut runtime.app, &mut ui))?;

        let status = runtime.app.status;
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
                        match ui::handle_key_event(&mut runtime.app, &mut ui, key).await {
                            ui::AppAction::None => {}
                            ui::AppAction::Quit => should_quit = true,
                            ui::AppAction::StartStream => runtime.maybe_resume_stream().await,
                            ui::AppAction::SetError(msg) => runtime.app.set_error(msg),
                        }
                    }
                    UiEvent::Mouse(mouse) => {
                        match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => {
                                if !ui::overlay::handle_overlay_scroll(
                                    &mut runtime.app,
                                    &mut ui,
                                    true,
                                ) {
                                    ui.chat_state.scroll_up(3);
                                }
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                if !ui::overlay::handle_overlay_scroll(
                                    &mut runtime.app,
                                    &mut ui,
                                    false,
                                ) {
                                    ui.chat_state.scroll_down(3);
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
            Some(event) = runtime.next_event() => {
                match event {
                    RuntimeEvent::InteractionRequested(questions) => {
                        // The core paused the turn for user input; open the
                        // ask overlay to collect the answers.
                        ui.overlay_state.open_ask(questions);
                    }
                    RuntimeEvent::StreamText(_) | RuntimeEvent::StreamReasoning(_)
                    | RuntimeEvent::MessagesChanged => {
                        if ui.chat_state.auto_scroll {
                            ui.chat_state.scroll_to_bottom();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Final snapshot; incremental saves already cover most of the session.
    runtime.app.persist_session();

    Ok(())
}

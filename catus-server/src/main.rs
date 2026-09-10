//! catus-server: an axum + tower HTTP/WebSocket server frontend for
//! catus-core with an embedded Vue web UI.
//!
//! Layout:
//! - [`actor`]: the single `Runtime` owner (mirrors the Tauri bridge).
//! - [`api`]: REST command routes (one per Tauri command).
//! - [`ws`]: the `/ws` endpoint broadcasting runtime events and snapshots.
//! - [`assets`]: static hosting of the embedded `web/dist` UI bundle.
//!
//! Usage: `cargo run -p catus-server -- [-w <dir>] [--host 127.0.0.1]
//! [--port 3117] [--static-dir <dir>]`, then open the printed URL.

mod actor;
mod api;
mod assets;
mod state;
mod ws;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{broadcast, mpsc};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::actor::Command;
use crate::assets::StaticAssets;
use crate::state::{AppState, BroadcastEvent, SharedState};

use axum::extract::State;

/// How long the process lingers after the actor stops, so the final
/// `app-quit` frames reach connected clients.
const QUIT_GRACE: Duration = Duration::from_millis(300);

#[derive(Parser)]
#[command(
    name = "catus-server",
    about = "HTTP/WebSocket server frontend for catus-core with an embedded Vue UI"
)]
struct Args {
    /// Run against another working directory (workspace config, agents,
    /// skills, and the session cwd are resolved against it).
    #[arg(short = 'w', long)]
    workspace: Option<PathBuf>,
    /// Bind address. Local-only by default; the server carries no auth.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// TCP port for the web UI and API.
    #[arg(short, long, default_value_t = 3117)]
    port: u16,
    /// Serve the web UI from this directory instead of the embedded bundle.
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Some(dir) = &args.workspace {
        if let Err(e) = enter_workspace(dir) {
            eprintln!("catus-server: {}", e);
            std::process::exit(1);
        }
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
    let (events, _) = broadcast::channel::<BroadcastEvent>(256);
    let state: SharedState = AppState {
        cmd: cmd_tx,
        events: events.clone(),
        static_assets: StaticAssets::new(args.static_dir),
    }
    .into();

    // The actor owns the Runtime; its failure to boot is announced to
    // connected clients instead of tearing down the server.
    let actor_events = events.clone();
    tokio::spawn(async move {
        match actor::run(cmd_rx, actor_events.clone()).await {
            Ok(()) => {
                let _ = actor_events.send(BroadcastEvent::AppQuit);
                tokio::time::sleep(QUIT_GRACE).await;
                std::process::exit(0);
            }
            Err(e) => {
                log::error!("startup failed: {}", e);
                eprintln!("catus-server: {}", e);
                let _ = actor_events.send(BroadcastEvent::StartupError(e));
            }
        }
    });

    let app = build_router(state);
    let addr = format!("{}:{}", args.host, args.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("catus-server: cannot bind {}: {}", addr, e);
            std::process::exit(1);
        }
    };
    println!("catus-server: web UI at http://{}", addr);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("catus-server: {}", e);
        std::process::exit(1);
    }
}

/// REST commands + WebSocket events + the embedded SPA.
fn build_router(state: SharedState) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/api/input", post(api::send_input))
        .route("/api/interaction/complete", post(api::complete_interaction))
        .route("/api/interaction/cancel", post(api::cancel_interaction))
        .route("/api/overlay", post(api::overlay_action))
        .route("/api/snapshot", get(api::get_snapshot))
        .route("/api/config", post(api::set_config_field))
        .route("/api/config/remove", post(api::remove_config_field))
        .route("/api/models", post(api::upsert_model))
        .route("/api/models/remove", post(api::remove_model))
        .route("/api/mcp", post(api::upsert_mcp_server))
        .route("/api/mcp/remove", post(api::remove_mcp_server))
        .route("/api/completion", get(api::completion_candidates))
        .route("/api/sessions", get(api::list_sessions))
        .route("/api/skills/{name}/preview", get(api::skill_preview))
        .route("/api/agents/{name}", get(api::get_agent_detail))
        .route("/api/agents/{name}", post(api::save_agent))
        .route("/api/agents", post(api::create_agent))
        .route(
            "/api/subagents/{id}/messages",
            get(api::get_subagent_messages),
        )
        .route("/api/quit", post(api::quit_app))
        .route("/ws", get(ws::ws_handler))
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Serve the embedded Vue UI (or `--static-dir`) with an SPA fallback.
async fn static_handler(
    State(state): State<SharedState>,
    uri: axum::extract::OriginalUri,
) -> axum::response::Response {
    assets::serve(&state.static_assets, &uri.0).await
}

/// The workspace is selected by changing into the directory before the
/// runtime boots, mirroring the `catus` TUI binary.
fn enter_workspace(dir: &std::path::Path) -> Result<(), String> {
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

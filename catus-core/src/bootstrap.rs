//! Shared startup for all frontends: configuration loading/validation and
//! runtime bootstrap.
//!
//! Both the `catus` TUI and the `catus-web` Tauri frontend call
//! [`load_config`] then [`bootstrap_runtime`], so configuration validation
//! rules, the `main.md` requirement, startup-event draining, and MCP server
//! connection cannot drift between frontends.
//!
//! Unlike the previous binary-local versions, these functions return
//! `Result` instead of exiting the process: the web frontend must be able to
//! surface startup errors in its UI. Text frontends print the error and exit
//! themselves.

use crate::app::App;
use crate::config::{AppConfig, AppDirs};
use crate::runtime::Runtime;
use tracing::Instrument;

/// Load the effective configuration.
///
/// `AppConfig::load()` merges the XDG base config with the workspace config
/// (`./.sutcac/config.toml`), so the process must already be sitting in the
/// workspace directory. The production storage directories (session history,
/// memory store) are forced to the XDG base directory afterwards.
pub fn load_config() -> Result<AppConfig, String> {
    let mut config = AppConfig::load().map_err(|e| format!("failed to load config: {}", e))?;
    config.dirs = AppDirs::from_xdg();

    config
        .resolve_models()
        .map_err(|_| {
            "invalid model configuration; configure at least one [[providers]] entry and one [[models]] entry"
                .to_string()
        })?;

    config.validate_tier_models().map_err(|_| {
        "invalid tier model configuration; set [agent.models] performance to a configured [[models]] id or name; efficient is optional"
            .to_string()
    })?;

    if config.providers.iter().all(|p| p.api_key.is_empty()) {
        return Err("all provider api_keys are empty; set api_key in ~/.config/catus/config.toml or .sutcac/config.toml".to_string());
    }

    Ok(config)
}

/// Build the runtime and connect to the configured MCP servers.
///
/// Requires a `main.md` agent definition. Startup warnings (memory subsystem,
/// MCP) are reported through tracing when `log_warnings` is false and to stderr
/// otherwise; startup events queued during initialization are drained.
pub async fn bootstrap_runtime(config: AppConfig, log_warnings: bool) -> Result<Runtime, String> {
    let span = tracing::info_span!("bootstrap");
    let mut runtime = Runtime::new(App::new(config));
    if !runtime.app.main_agent_from_file {
        return Err("main agent definition not found; create .sutcac/agents/main.md".to_string());
    }
    for warning in &runtime.app.memory.warnings {
        if log_warnings {
            eprintln!("catus: memory warning: {}", warning);
        } else {
            tracing::warn!(parent: span.clone(), "memory warning: {}", warning);
        }
    }
    // Drain startup events queued during initialization.
    while runtime.app.take_event().is_some() {}

    // RAG pre-load: opening the index snapshot is cheap (the embedding model
    // itself stays lazy), so auto-injection works from the very first turn.
    runtime.app.preload_rag();

    let mcp_warnings = runtime.app.connect_mcp().instrument(span.clone()).await;
    for warning in &mcp_warnings {
        if log_warnings {
            eprintln!("catus: mcp warning: {}", warning);
        } else {
            tracing::warn!(parent: span.clone(), "mcp warning: {}", warning);
        }
    }
    tracing::info!(parent: span, "bootstrap complete");
    Ok(runtime)
}

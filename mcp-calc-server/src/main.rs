//! Simple calculator MCP server for testing catus MCP client support.
//!
//! Listens on stdio and exposes arithmetic tools (`sum`, `sub`, `mul`, `div`,
//! `modulo`). Run it directly or point catus at it via an `[[mcp.servers]]`
//! entry with `command = "/path/to/mcp-calc-server"`.
//!
//! With `--http <addr>` it instead serves the same tools over the Streamable
//! HTTP transport at `/mcp`, for testing catus's remote MCP server support.

use std::sync::Arc;

use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};

mod calculator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--http") {
        let addr = args
            .get(pos + 1)
            .ok_or("--http requires a listen address, e.g. --http 127.0.0.1:8000")?;
        return serve_http(addr).await;
    }

    let service = calculator::Calculator
        .serve(stdio())
        .await
        .inspect_err(|e| eprintln!("mcp-calc-server: serve error: {}", e))?;

    service.waiting().await?;
    Ok(())
}

/// Serve the calculator tools over Streamable HTTP at `/mcp`.
async fn serve_http(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(
        || Ok(calculator::Calculator),
        session_manager,
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().route_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("mcp-calc-server: listening on http://{}/mcp", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

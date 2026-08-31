//! Simple calculator MCP server for testing catus MCP client support.
//!
//! Listens on stdio and exposes arithmetic tools (`sum`, `sub`, `mul`, `div`,
//! `modulo`). Run it directly or point catus at it via an `[[mcp.servers]]`
//! entry with `command = "/path/to/mcp-calc-server"`.

use rmcp::{ServiceExt, transport::stdio};

mod calculator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = calculator::Calculator
        .serve(stdio())
        .await
        .inspect_err(|e| eprintln!("mcp-calc-server: serve error: {}", e))?;

    service.waiting().await?;
    Ok(())
}

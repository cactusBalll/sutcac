//! Integration test: catus MCP client talks to a remote Streamable HTTP server.
//!
//! Spawns mcp-calc-server in `--http` mode and exercises catus's `McpManager`
//! over the Streamable HTTP transport: connection, tool discovery, and tool
//! calls, including custom HTTP headers.

use std::collections::HashMap;
use std::process::{Child, Command as ProcessCommand};
use std::time::{Duration, Instant};

use catus_core::config::{McpServerConfig, McpTransport};
use catus_core::mcp::McpManager;

fn server_binary_path() -> &'static str {
    // Cargo sets this for integration tests of the package that defines the binary.
    env!("CARGO_BIN_EXE_mcp-calc-server")
}

/// Kills the spawned server on drop so a panicking test cannot leak it.
struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pick a free loopback port and spawn the calculator server in HTTP mode on
/// it. Returns the guard and the server's `/mcp` URL.
fn start_http_server() -> (ServerGuard, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let child = ProcessCommand::new(server_binary_path())
        .args(["--http", &format!("127.0.0.1:{}", port)])
        .spawn()
        .expect("spawn mcp-calc-server --http");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (
                ServerGuard { child },
                format!("http://127.0.0.1:{}/mcp", port),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mcp-calc-server did not start listening on port {}", port);
}

fn http_server_config(name: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        transport: McpTransport::StreamableHttp,
        url: Some(url.to_string()),
        // Exercises the custom-headers path end to end; the test server
        // accepts any header value.
        headers: HashMap::from_iter([(
            "Authorization".to_string(),
            "Bearer test-token".to_string(),
        )]),
        ..Default::default()
    }
}

#[tokio::test]
async fn connect_list_and_call_calculator_tools_over_http() {
    let (_server, url) = start_http_server();
    let config = http_server_config("calc", &url);

    let (manager, warnings) = McpManager::connect(&[config]).await;
    assert!(
        warnings.is_empty(),
        "unexpected connection warnings: {:?}",
        warnings
    );
    assert_eq!(manager.len(), 1, "calculator server should be connected");

    let tools = manager
        .tool_catalog("calc")
        .expect("calc catalog should be cached");
    assert!(
        tools.iter().any(|t| t.name == "sum"),
        "expected sum in {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let mut arguments = serde_json::Map::new();
    arguments.insert("a".to_string(), serde_json::json!(3));
    arguments.insert("b".to_string(), serde_json::json!(2));
    let result = manager
        .call_tool("calc", "sum", arguments)
        .await
        .expect("sum call should succeed");
    assert_eq!(result.status, 0);
    assert_eq!(result.stdout, "5");
}

#[tokio::test]
async fn unreachable_remote_server_produces_warning_without_blocking_others() {
    let (_server, url) = start_http_server();
    let good_config = http_server_config("calc", &url);
    let bad_config = http_server_config("ghost", "http://127.0.0.1:1/mcp");

    let (manager, warnings) = McpManager::connect(&[bad_config, good_config]).await;

    assert_eq!(manager.len(), 1, "good server should still connect");
    assert_eq!(
        warnings.len(),
        1,
        "unreachable server should produce a warning"
    );
    assert!(warnings[0].starts_with("ghost:"), "warning: {:?}", warnings);

    let tools = manager
        .tool_catalog("calc")
        .expect("calc catalog should be cached");
    assert!(
        tools.iter().any(|t| t.name == "sum"),
        "calc tools should still be available"
    );
}

//! Integration test: catus MCP client talks to mcp-calc-server.
//!
//! Spawns the calculator MCP server as a stdio child process and exercises
//! catus's `McpManager` end-to-end: connection, tool catalog discovery, and
//! tool calls.

use catus_core::config::McpServerConfig;
use catus_core::mcp::McpManager;

fn server_binary_path() -> &'static str {
    // Cargo sets this for integration tests of the package that defines the binary.
    env!("CARGO_BIN_EXE_mcp-calc-server")
}

fn calc_server_config() -> McpServerConfig {
    McpServerConfig {
        name: "calc".to_string(),
        command: server_binary_path().to_string(),
        ..Default::default()
    }
}

fn args(pairs: &[(&str, i64)]) -> serde_json::Map<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect()
}

#[tokio::test]
async fn connect_and_list_calculator_tools() {
    let config = calc_server_config();
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
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(names.contains(&"sum"), "expected sum in {:?}", names);
    assert!(names.contains(&"sub"), "expected sub in {:?}", names);
    assert!(names.contains(&"mul"), "expected mul in {:?}", names);
    assert!(names.contains(&"div"), "expected div in {:?}", names);
    assert!(names.contains(&"modulo"), "expected modulo in {:?}", names);

    // Each tool should advertise a JSON-schema parameter object.
    for tool in tools {
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&serde_json::json!("object"))
        );
    }
}

#[tokio::test]
async fn call_sum_and_sub() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let sum_result = manager
        .call_tool("calc", "sum", args(&[("a", 3), ("b", 2)]))
        .await
        .expect("sum call should succeed");
    assert_eq!(sum_result.status, 0);
    assert_eq!(sum_result.stdout, "5");
    assert!(sum_result.stderr.is_empty());

    let sub_result = manager
        .call_tool("calc", "sub", args(&[("a", 3), ("b", 2)]))
        .await
        .expect("sub call should succeed");
    assert_eq!(sub_result.status, 0);
    assert_eq!(sub_result.stdout, "1");
}

#[tokio::test]
async fn call_mul_and_div() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let mul_result = manager
        .call_tool("calc", "mul", args(&[("a", 4), ("b", 5)]))
        .await
        .expect("mul call should succeed");
    assert_eq!(mul_result.status, 0);
    assert_eq!(mul_result.stdout, "20");

    let div_result = manager
        .call_tool("calc", "div", args(&[("a", 7), ("b", 2)]))
        .await
        .expect("div call should succeed");
    assert_eq!(div_result.status, 0);
    assert_eq!(div_result.stdout, "3.5");
}

#[tokio::test]
async fn div_by_zero_returns_error() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let err = manager
        .call_tool("calc", "div", args(&[("a", 1), ("b", 0)]))
        .await
        .expect_err("division by zero should return an error");
    let text = err.to_string();
    assert!(
        text.to_lowercase().contains("division by zero"),
        "expected division-by-zero message, got: {}",
        text
    );
}

#[tokio::test]
async fn unknown_tool_is_rejected_locally() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let err = manager
        .call_tool("calc", "nope", serde_json::Map::new())
        .await
        .expect_err("unknown tool should be rejected without a server round-trip");
    assert!(err.to_string().contains("no tool 'nope'"), "error: {}", err);

    let err = manager
        .call_tool("ghost", "sum", serde_json::Map::new())
        .await
        .expect_err("unknown server should be rejected");
    assert!(
        err.to_string().contains("unknown mcp server"),
        "error: {}",
        err
    );
}

#[tokio::test]
async fn manager_survives_when_server_command_is_missing() {
    let bad_config = McpServerConfig {
        name: "missing".to_string(),
        command: "/no/such/binary".to_string(),
        ..Default::default()
    };
    let good_config = calc_server_config();

    let (manager, warnings) = McpManager::connect(&[bad_config, good_config]).await;

    assert_eq!(manager.len(), 1, "good server should still connect");
    assert_eq!(warnings.len(), 1, "missing server should produce a warning");

    // The good server is still usable.
    let tools = manager
        .tool_catalog("calc")
        .expect("calc catalog should be cached");
    assert!(
        tools.iter().any(|t| t.name == "sum"),
        "calc tools should still be available"
    );
}

#[tokio::test]
async fn disconnect_and_connect_one_reconnect() {
    let config = calc_server_config();
    let (manager, warnings) = McpManager::connect(&[config.clone()]).await;
    assert!(warnings.is_empty(), "unexpected warnings: {:?}", warnings);
    assert!(manager.is_connected("calc"));

    // Disconnect drops the connection and the cached catalog.
    assert!(manager.disconnect("calc"));
    assert!(!manager.disconnect("calc"), "second disconnect is a no-op");
    assert!(!manager.is_connected("calc"));
    assert!(manager.tool_catalog("calc").is_none());
    assert!(manager.is_empty());

    // `connect_one` restores the connection and the catalog (reconnect).
    manager
        .connect_one(&config)
        .await
        .expect("reconnect should succeed");
    assert!(manager.is_connected("calc"));
    let tools = manager
        .tool_catalog("calc")
        .expect("catalog should be cached after reconnect");
    assert!(tools.iter().any(|t| t.name == "sum"));

    // Tool calls work through the reconnected client.
    let sum_result = manager
        .call_tool("calc", "sum", args(&[("a", 1), ("b", 1)]))
        .await
        .expect("sum call should succeed after reconnect");
    assert_eq!(sum_result.stdout, "2");
}

//! Integration test: catus MCP client talks to mcp-calc-server.
//!
//! Spawns the calculator MCP server as a stdio child process and exercises
//! catus's `McpManager` end-to-end: connection, tool discovery, and tool calls.

use std::collections::HashMap;

use catus::config::McpServerConfig;
use catus::mcp::McpManager;
use catus::tool::ToolCall;

fn server_binary_path() -> &'static str {
    // Cargo sets this for integration tests of the package that defines the binary.
    env!("CARGO_BIN_EXE_mcp-calc-server")
}

fn calc_server_config() -> McpServerConfig {
    McpServerConfig {
        name: "calc".to_string(),
        command: server_binary_path().to_string(),
        args: Vec::new(),
        env: HashMap::new(),
    }
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

    let tools = manager.all_tool_definitions().await;
    let names: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();

    assert!(
        names.contains(&"calc__sum"),
        "expected calc__sum in {:?}",
        names
    );
    assert!(
        names.contains(&"calc__sub"),
        "expected calc__sub in {:?}",
        names
    );
    assert!(
        names.contains(&"calc__mul"),
        "expected calc__mul in {:?}",
        names
    );
    assert!(
        names.contains(&"calc__div"),
        "expected calc__div in {:?}",
        names
    );
    assert!(
        names.contains(&"calc__modulo"),
        "expected calc__modulo in {:?}",
        names
    );

    // Each tool should advertise a JSON-schema parameter object.
    for tool in &tools {
        assert_eq!(
            tool.function.parameters.get("type"),
            Some(&serde_json::json!("object"))
        );
    }
}

#[tokio::test]
async fn call_sum_and_sub() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let sum_call = ToolCall {
        id: "call_1".to_string(),
        name: "calc__sum".to_string(),
        arguments: r#"{"a": 3, "b": 2}"#.to_string(),
    };
    let sum_result = manager
        .call_tool(&sum_call)
        .await
        .expect("sum call should succeed");
    assert_eq!(sum_result.status, 0);
    assert_eq!(sum_result.stdout, "5");
    assert!(sum_result.stderr.is_empty());

    let sub_call = ToolCall {
        id: "call_2".to_string(),
        name: "calc__sub".to_string(),
        arguments: r#"{"a": 3, "b": 2}"#.to_string(),
    };
    let sub_result = manager
        .call_tool(&sub_call)
        .await
        .expect("sub call should succeed");
    assert_eq!(sub_result.stdout, "1");
}

#[tokio::test]
async fn call_mul_and_div() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let mul_call = ToolCall {
        id: "call_3".to_string(),
        name: "calc__mul".to_string(),
        arguments: r#"{"a": 4, "b": 5}"#.to_string(),
    };
    let mul_result = manager
        .call_tool(&mul_call)
        .await
        .expect("mul call should succeed");
    assert_eq!(mul_result.stdout, "20");

    let div_call = ToolCall {
        id: "call_4".to_string(),
        name: "calc__div".to_string(),
        arguments: r#"{"a": 7, "b": 2}"#.to_string(),
    };
    let div_result = manager
        .call_tool(&div_call)
        .await
        .expect("div call should succeed");
    assert_eq!(div_result.stdout, "3.5");
}

#[tokio::test]
async fn div_by_zero_returns_error() {
    let config = calc_server_config();
    let (manager, _) = McpManager::connect(&[config]).await;

    let call = ToolCall {
        id: "call_5".to_string(),
        name: "calc__div".to_string(),
        arguments: r#"{"a": 1, "b": 0}"#.to_string(),
    };
    let err = manager
        .call_tool(&call)
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
async fn manager_survives_when_server_command_is_missing() {
    let bad_config = McpServerConfig {
        name: "missing".to_string(),
        command: "/no/such/binary".to_string(),
        args: Vec::new(),
        env: HashMap::new(),
    };
    let good_config = calc_server_config();

    let (manager, warnings) = McpManager::connect(&[bad_config, good_config]).await;

    assert_eq!(manager.len(), 1, "good server should still connect");
    assert_eq!(warnings.len(), 1, "missing server should produce a warning");

    // The good server is still usable.
    let tools = manager.all_tool_definitions().await;
    assert!(
        tools.iter().any(|t| t.function.name == "calc__sum"),
        "calc tools should still be available"
    );
}

use nib::config::McpServerEntry;
use nib::integrations::mcp::McpManager;
use serde_json::json;
use std::collections::HashMap;

#[tokio::test]
async fn test_mcp_mock_server() {
    let mut servers = HashMap::new();
    servers.insert(
        "mock_server".to_string(),
        McpServerEntry {
            command: "python3".to_string(),
            args: vec!["tests/mock_mcp_server.py".to_string()],
            env: HashMap::new(),
        },
    );

    let mcp = McpManager::new(&servers).await.expect("Failed to initialize McpManager");

    // 1. Test tools/list
    let tools = mcp.list_tools().await.expect("Failed to list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "mock_server::say_hello");

    // 2. Test tools/call
    let res = mcp
        .call_tool(
            "mock_server::say_hello",
            json!({"name": "Nib"}),
        )
        .await
        .expect("Failed to call tool");

    let text = res["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "Hello, Nib!");
}

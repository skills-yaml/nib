use crate::config::load_nib_config;
use serde_json::{json, Value};
use std::io::{self, BufRead};
use std::path::Path;

pub async fn run_mcp_server(project_root: &Path) {
    let _cfg = load_nib_config(project_root);

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    // A simple MCP JSON-RPC stdio loop
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }

        let req: Result<Value, _> = serde_json::from_str(&line);
        if let Ok(req) = req {
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let params = req.get("params").cloned().unwrap_or(json!({}));

            match method {
                "initialize" => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {
                                "tools": {}
                            },
                            "serverInfo": {
                                "name": "nib-mcp",
                                "version": "0.1.0"
                            }
                        }
                    });
                    println!("{}", resp);
                }
                "tools/list" => {
                    let mcp_tools = vec![
                        json!({
                            "name": "nib_run",
                            "description": "Start a background nib agent to achieve a goal",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "goal": {
                                        "type": "string",
                                        "description": "The goal for the agent to achieve"
                                    }
                                },
                                "required": ["goal"]
                            }
                        }),
                        json!({
                            "name": "nib_get_status",
                            "description": "Query the status of an agent run using its session_id",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "session_id": {
                                        "type": "string",
                                        "description": "The session ID returned by nib_run"
                                    }
                                },
                                "required": ["session_id"]
                            }
                        }),
                    ];

                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": mcp_tools
                        }
                    });
                    println!("{}", resp);
                }
                "tools/call" => {
                    if let (Some(name), Some(args)) = (
                        params.get("name").and_then(|n| n.as_str()),
                        params.get("arguments"),
                    ) {
                        let text_content = match name {
                            "nib_run" => {
                                let goal =
                                    args.get("goal").and_then(|g| g.as_str()).unwrap_or("help");
                                let call_args = json!({"prompt": goal});
                                match crate::tools::delegation::spawn_subagent(
                                    &call_args,
                                    project_root,
                                ) {
                                    Ok(res) => res.to_string(),
                                    Err(e) => format!("Error: {}", e),
                                }
                            }
                            "nib_get_status" => {
                                let _session_id = args
                                    .get("session_id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("");
                                // For now, just return a generic status since TASK_MANAGER manages it
                                let tasks = crate::daemons::task::TASK_MANAGER.list_tasks();
                                json!({"tasks": tasks}).to_string()
                            }
                            _ => "Unknown tool".to_string(),
                        };

                        let is_error = text_content.starts_with("Error:");
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [
                                    {
                                        "type": "text",
                                        "text": text_content
                                    }
                                ],
                                "isError": is_error
                            }
                        });
                        println!("{}", resp);
                    } else {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32602,
                                "message": "Invalid params"
                            }
                        });
                        println!("{}", resp);
                    }
                }
                _ => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not found"
                        }
                    });
                    println!("{}", resp);
                }
            }
        }
    }
}

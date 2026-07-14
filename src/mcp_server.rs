use nib::config::load_nib_config;
use nib::tools::{ToolCall, ToolExecutor};
use serde_json::{json, Value};
use std::io::{self, BufRead};
use std::path::Path;

pub async fn run_mcp_server(project_root: &Path) {
    let cfg = load_nib_config(project_root);
    let mut executor =
        ToolExecutor::new(project_root.to_path_buf(), cfg.execution).with_auto_approve(false); // require human approval for dangerous tasks when called from MCP

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    let tools_schema = executor.get_tools_schema().await;

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
                    // map openAI schema to MCP tools schema
                    let mcp_tools: Vec<Value> = tools_schema
                        .iter()
                        .map(|t| {
                            let f = t.get("function").unwrap();
                            json!({
                                "name": f.get("name"),
                                "description": f.get("description"),
                                "inputSchema": f.get("parameters")
                            })
                        })
                        .collect();

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
                        let call = ToolCall {
                            tool_name: name.to_string(),
                            arguments: args.clone(),
                            session_id: None,
                            project_root: Some(project_root.to_path_buf()),
                        };
                        let result = executor.execute(call, None).await;

                        let is_error = !result.success;
                        let text_content = if is_error {
                            result
                                .error
                                .clone()
                                .unwrap_or_else(|| "Unknown error".to_string())
                        } else {
                            result.output.clone().unwrap_or_default().to_string()
                        };

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

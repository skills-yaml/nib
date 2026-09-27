use super::*;
#[cfg(unix)]
use serial_test::serial;

fn tool(name: impl Into<String>) -> Value {
    json!({
        "name": name.into(),
        "description": "test tool",
        "inputSchema": {"type": "object"}
    })
}

fn secret_matcher(values: &[String]) -> McpSecretMatcher {
    McpSecretMatcher::new(values).expect("bounded MCP secret matcher")
}

#[test]
fn hostile_oversized_external_schema_is_rejected() {
    let result = json!({
        "tools": [{
            "name": "hostile",
            "inputSchema": {
                "type": "object",
                "description": "x".repeat(MAX_EXTERNAL_TOOL_SCHEMA_BYTES)
            }
        }]
    });

    let error = parse_external_tools("fixture", &result, &secret_matcher(&[]))
        .expect_err("oversized external schema must fail closed");

    assert!(error.to_string().contains("input schema exceeds"));
}

#[test]
fn external_tool_count_name_and_description_limits_are_enforced() {
    let too_many = json!({
        "tools": (0..=MAX_EXTERNAL_TOOLS_PER_SERVER)
            .map(|index| tool(format!("tool_{index}")))
            .collect::<Vec<_>>()
    });
    assert!(
        parse_external_tools("fixture", &too_many, &secret_matcher(&[]))
            .expect_err("tool count must be bounded")
            .to_string()
            .contains("tool count")
    );

    let long_name = json!({"tools": [tool("x".repeat(MAX_EXTERNAL_TOOL_NAME_BYTES + 1))]});
    assert!(
        parse_external_tools("fixture", &long_name, &secret_matcher(&[]))
            .expect_err("tool name must be bounded")
            .to_string()
            .contains("tool 0 name")
    );

    let long_description = json!({
        "tools": [{
            "name": "described",
            "description": "x".repeat(MAX_EXTERNAL_TOOL_DESCRIPTION_BYTES + 1),
            "inputSchema": {"type": "object"}
        }]
    });
    assert!(
        parse_external_tools("fixture", &long_description, &secret_matcher(&[]))
            .expect_err("tool description must be bounded")
            .to_string()
            .contains("description exceeds")
    );
}

#[test]
fn successful_tool_metadata_rejects_configured_secret_spellings_atomically() {
    let name_secret = "configuredNameCredential";
    let quoted_secret = "quoted-\"credential\\value";
    let multiline_secret = "multiline-credential\nnext-line";
    let quoted_json = serde_json::to_string(quoted_secret).expect("quoted secret JSON");
    let escaped_quoted = quoted_json[1..quoted_json.len() - 1].to_string();
    let multiline_json = serde_json::to_string(multiline_secret).expect("multiline secret JSON");
    let mut properties = serde_json::Map::new();
    properties.insert(escaped_quoted.clone(), json!({"type": "string"}));
    let sensitive_values = secret_matcher(&[
        name_secret.to_string(),
        quoted_secret.to_string(),
        multiline_secret.to_string(),
    ]);
    let cases = [
        (
            "name",
            json!({"name": name_secret, "inputSchema": {"type": "object"}}),
        ),
        (
            "description",
            json!({
                "name": "description_secret",
                "description": quoted_secret,
                "inputSchema": {"type": "object"},
            }),
        ),
        (
            "multiline description",
            json!({
                "name": "multiline_secret",
                "description": multiline_secret,
                "inputSchema": {"type": "object"},
            }),
        ),
        (
            "schema key",
            json!({
                "name": "schema_key_secret",
                "inputSchema": {"type": "object", "properties": properties},
            }),
        ),
        (
            "schema value",
            json!({
                "name": "schema_value_secret",
                "inputSchema": {"type": "object", "description": quoted_json.clone()},
            }),
        ),
    ];

    for (surface, malicious_tool) in cases {
        let result = json!({
            "tools": [tool("accepted_prefix_must_not_escape"), malicious_tool]
        });
        let error = parse_external_tools("fixture", &result, &sensitive_values)
            .expect_err("secret-bearing metadata must reject the complete server list")
            .to_string();
        assert_eq!(
            error,
            format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
            "unexpected rejection for {surface}"
        );
    }

    let error = parse_external_tools(
        "fixture",
        &json!({"tools": [
            tool("accepted_prefix_must_not_escape"),
            {"name": name_secret, "inputSchema": {"type": "object"}},
        ]}),
        &sensitive_values,
    )
    .expect_err("secret-bearing metadata must reject the complete server list")
    .to_string();
    assert!(error.len() < 256, "metadata rejection must stay bounded");
    for offending in [
        name_secret,
        quoted_secret,
        multiline_secret,
        quoted_json.as_str(),
        escaped_quoted.as_str(),
        multiline_json.as_str(),
        "accepted_prefix_must_not_escape",
    ] {
        assert!(!error.contains(offending), "metadata escaped: {error}");
    }
}

#[test]
fn mcp_metadata_and_errors_reject_encoded_secrets_and_controls() {
    let secret = "active/credential".to_string();
    let matcher = secret_matcher(std::slice::from_ref(&secret));
    for spelling in [
        "active%2Fcredential",
        r"active\/credential",
        "YWN0aXZlL2NyZWRlbnRpYWw=",
        "YWN0aXZlL2NyZWRlbnRpYWw",
        "\u{1b}[2J",
    ] {
        let result = json!({
            "tools": [{
                "name": "encoded_secret",
                "description": format!("metadata={spelling}"),
                "inputSchema": {"type": "object"},
            }]
        });
        assert_eq!(
            parse_external_tools("fixture", &result, &matcher)
                .expect_err("unsafe MCP metadata must reject atomically")
                .to_string(),
            format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
        );
    }

    let error = matcher.redact(&format!(
        "remote={secret}; percent=active%2Fcredential; base64=YWN0aXZlL2NyZWRlbnRpYWw=; \u{1b}[31m"
    ));
    for forbidden in [
        secret.as_str(),
        "active%2Fcredential",
        "YWN0aXZlL2NyZWRlbnRpYWw=",
    ] {
        assert!(!error.contains(forbidden));
    }
    assert!(!error.contains('\u{1b}'));
}

#[test]
fn successful_tool_metadata_rejects_unconfigured_generic_secret_spelling() {
    for (surface, malicious_tool, spelling) in [
        (
            "name",
            json!({
                "name": "sk-unconfiguredMetadata123",
                "inputSchema": {"type": "object"},
            }),
            "sk-unconfiguredMetadata123",
        ),
        (
            "schema value",
            json!({
                "name": "generic_schema_secret",
                "inputSchema": {
                    "type": "object",
                    "description": "xai-unconfiguredMetadata123",
                },
            }),
            "xai-unconfiguredMetadata123",
        ),
        (
            "embedded name",
            json!({
                "name": "prefix_sk-secretvalue123",
                "inputSchema": {"type": "object"},
            }),
            "sk-secretvalue123",
        ),
        (
            "embedded schema value",
            json!({
                "name": "embedded_schema_secret",
                "inputSchema": {
                    "type": "object",
                    "description": "xxsk-secretvalue123",
                },
            }),
            "sk-secretvalue123",
        ),
    ] {
        let result = json!({"tools": [malicious_tool]});
        let error = parse_external_tools("fixture", &result, &secret_matcher(&[]))
            .expect_err("generic secret metadata must fail closed")
            .to_string();

        assert_eq!(
            error,
            format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
            "unexpected rejection for {surface}"
        );
        assert!(!error.contains(spelling));
    }
}

#[test]
fn sensitive_spelling_limits_fail_with_a_constant_error() {
    for result in [
        mcp_sensitive_spellings(&["first".to_string()], 1, usize::MAX),
        mcp_sensitive_spellings(&["first".to_string()], usize::MAX, 1),
    ] {
        let error = result.expect_err("secret spelling resources must be bounded");
        assert_eq!(
            error.to_string(),
            format!("invalid MCP configuration: {MCP_SECRET_BOUNDARY_LIMIT_ERROR}")
        );
    }
}

#[test]
fn final_namespaced_metadata_is_validated_before_exposure() {
    let tool = ExternalTool {
        name: "safe_tool".to_string(),
        description: "safe description".to_string(),
        input_schema: json!({"type": "object"}),
    };
    let configured = "configuredServerCredential".to_string();
    for (server_name, matcher) in [
        (
            configured.as_str(),
            secret_matcher(std::slice::from_ref(&configured)),
        ),
        ("prefix_sk-secretvalue123", secret_matcher(&[])),
    ] {
        let metadata = Value::Array(vec![exposed_external_tool(server_name, &tool)]);
        let error = validate_external_tool_metadata_boundary(&metadata, &matcher)
            .expect_err("namespaced secret must fail before exposure");
        assert_eq!(
            error.to_string(),
            format!("RPC error: {MCP_METADATA_SECRET_ERROR}")
        );
    }
}

#[tokio::test]
async fn timed_out_request_is_removed_from_pending_map() {
    let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
    let (request_tx, mut request_rx) = mpsc::channel(1);
    let (reply, response) = oneshot::channel();
    let mut pending_request =
        PendingRequestGuard::register(Arc::clone(&pending), request_tx, 7, reply);
    pending_request.mark_enqueued();

    assert!(matches!(
        await_pending_response(Instant::now(), response).await,
        PendingResponse::TimedOut
    ));

    drop(pending_request);
    assert!(lock_pending(&pending).is_empty());
    let cancellation = request_rx.recv().await.expect("cancellation notification");
    let cancellation: Value = serde_json::from_slice(&cancellation.bytes).unwrap();
    assert_eq!(cancellation["method"], "notifications/cancelled");
    assert_eq!(cancellation["params"]["requestId"], 7);
}

#[test]
fn request_dropped_before_enqueue_does_not_notify_server() {
    let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
    let (request_tx, mut request_rx) = mpsc::channel(1);
    let (reply, _response) = oneshot::channel();
    let pending_request = PendingRequestGuard::register(Arc::clone(&pending), request_tx, 9, reply);

    drop(pending_request);

    assert!(lock_pending(&pending).is_empty());
    assert!(request_rx.try_recv().is_err());
}

#[tokio::test]
async fn aborted_request_removes_pending_registration_and_notifies_server() {
    let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
    let (request_tx, mut request_rx) = mpsc::channel(2);
    let task_pending = Arc::clone(&pending);
    let task_request_tx = request_tx.clone();
    let (registered_tx, registered_rx) = oneshot::channel();
    let request = tokio::spawn(async move {
        let (reply, response) = oneshot::channel();
        let mut pending_request = PendingRequestGuard::register(
            Arc::clone(&task_pending),
            task_request_tx.clone(),
            11,
            reply,
        );
        task_request_tx
            .send(OutboundFrame::unacknowledged(b"request\n".to_vec()))
            .await
            .expect("request frame enqueued");
        pending_request.mark_enqueued();
        let _ = registered_tx.send(());
        await_pending_response(Instant::now() + Duration::from_secs(60), response).await
    });

    registered_rx.await.expect("request registered");
    assert_eq!(lock_pending(&pending).len(), 1);
    assert_eq!(
        request_rx.recv().await.expect("request frame").bytes,
        b"request\n"
    );
    request.abort();
    assert!(request.await.expect_err("request aborted").is_cancelled());

    assert!(lock_pending(&pending).is_empty());
    let cancellation = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
        .await
        .expect("cancellation notification timeout")
        .expect("cancellation notification");
    let cancellation: Value = serde_json::from_slice(&cancellation.bytes).unwrap();
    assert_eq!(cancellation["method"], "notifications/cancelled");
    assert_eq!(cancellation["params"]["requestId"], 11);
}

#[test]
fn manager_rejects_invalid_config_before_starting_processes() {
    let config = HashMap::from([(
        "bad::name".to_string(),
        McpServerEntry {
            command: "must-not-run".to_string(),
            ..McpServerEntry::default()
        },
    )]);

    let error = validate_server_config(&config).expect_err("invalid name must fail");

    assert!(matches!(error, McpError::InvalidConfiguration(_)));

    let invalid_environment = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: "must-not-run".to_string(),
            env: HashMap::from([("BAD-NAME".to_string(), "value".to_string())]),
            ..McpServerEntry::default()
        },
    )]);
    assert!(matches!(
        validate_server_config(&invalid_environment),
        Err(McpError::InvalidConfiguration(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn initialization_errors_redact_configured_secrets_and_child_stderr_is_not_inherited() {
    let secret = "mcp-\"quoted\\line\nnext";
    let json_spelling = serde_json::to_string(secret).expect("secret JSON spelling");
    let escaped_spelling = json_spelling[1..json_spelling.len() - 1].to_string();
    let debug_spelling = format!("{secret:?}");
    let response = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32000, "message": format!("escaped={escaped_spelling}")}
    })
    .to_string();
    let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "IFS= read -r request; printf '%s\\n' \"$MCP_API_TOKEN\" >&2; printf '%s\\n' '{response}'; sleep 1"
                    ),
                ],
                env: HashMap::from([("MCP_API_TOKEN".to_string(), secret.to_string())]),
                ..McpServerEntry::default()
            },
        )]);

    let error = match McpManager::new(&servers, &[secret.to_string()]).await {
        Ok(_) => panic!("fixture initialization must fail"),
        Err(error) => error,
    };
    let error = error.to_string();

    assert!(!error.contains(secret), "raw secret escaped: {error}");
    assert!(
        !error.contains(&escaped_spelling),
        "JSON-escaped secret leaked: {error}"
    );
    assert!(
        !error.contains(&json_spelling),
        "quoted JSON secret leaked: {error}"
    );
    assert!(
        !error.contains(&debug_spelling),
        "debug secret leaked: {error}"
    );
    assert!(
        error.contains("[REDACTED]"),
        "redaction marker missing: {error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn direct_client_start_normalizes_quoted_and_multiline_secret_spellings() {
    let secret = "direct-\"quoted\\line\nnext";
    let json_spelling = serde_json::to_string(secret).expect("secret JSON spelling");
    let escaped_spelling = json_spelling[1..json_spelling.len() - 1].to_string();
    let debug_spelling = format!("{secret:?}");
    let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32000,
                "message": format!("json={json_spelling}; escaped={escaped_spelling}; debug={debug_spelling}")
            }
        })
        .to_string();
    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!("IFS= read -r request; printf '%s\\n' '{response}'"),
        ],
        ..McpServerEntry::default()
    };

    let error = match McpServerClient::start(
        "fixture".to_string(),
        &entry,
        Arc::new(vec![secret.to_string()]),
    )
    .await
    {
        Ok(_) => panic!("fixture initialization must fail"),
        Err(error) => error.to_string(),
    };

    for spelling in [secret, &json_spelling, &escaped_spelling, &debug_spelling] {
        assert!(!error.contains(spelling), "secret spelling leaked: {error}");
    }
    assert!(error.contains("[REDACTED]"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn manager_construction_rejects_secret_bearing_successful_discovery_atomically() {
    let directory = tempfile::tempdir().expect("metadata boundary fixture");
    let pid_file = directory.path().join("metadata-boundary-pids");
    let name_secret = "managerMetadataCredential";
    let quoted_secret = "manager-\"quoted\\credential";
    let multiline_secret = "manager-multiline\ncredential";
    let quoted_json = serde_json::to_string(quoted_secret).expect("quoted secret JSON");
    let escaped_quoted = quoted_json[1..quoted_json.len() - 1].to_string();
    let mut properties = serde_json::Map::new();
    properties.insert(
        escaped_quoted.clone(),
        json!({
            "type": "string",
            "description": quoted_json,
            "default": "xai-genericMetadata123",
        }),
    );
    let response = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [
                tool("accepted_prefix_must_not_register"),
                {
                    "name": format!("tool_{name_secret}"),
                    "description": format!("{quoted_secret}; {multiline_secret}"),
                    "inputSchema": {
                        "type": "object",
                        "properties": properties,
                    },
                }
            ]
        }
    })
    .to_string();
    let servers = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "IFS= read -r initialize; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "IFS= read -r list_tools; ",
                    "printf '%s\\n' \"$MCP_TOOLS_RESPONSE\"; ",
                    "sleep 60"
                )
                .to_string(),
            ],
            env: HashMap::from([
                ("MCP_TOOLS_RESPONSE".to_string(), response),
                (
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]),
            ..McpServerEntry::default()
        },
    )]);

    let error = match McpManager::new(
        &servers,
        &[
            name_secret.to_string(),
            quoted_secret.to_string(),
            multiline_secret.to_string(),
        ],
    )
    .await
    {
        Ok(_) => panic!("secret-bearing discovery must reject manager construction"),
        Err(error) => error.to_string(),
    };

    assert_eq!(error, format!("RPC error: {MCP_METADATA_SECRET_ERROR}"));
    assert!(error.len() < 256, "metadata rejection must stay bounded");
    for offending in [
        name_secret,
        quoted_secret,
        multiline_secret,
        quoted_json.as_str(),
        escaped_quoted.as_str(),
        "accepted_prefix_must_not_register",
        "xai-genericMetadata123",
    ] {
        assert!(!error.contains(offending), "metadata escaped: {error}");
    }
    let pids = wait_for_mcp_fixture_pids(&pid_file).await;
    assert_mcp_fixture_pids_stop(&pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn initialized_notification_requires_supervisor_write_acknowledgement() {
    let directory = tempfile::tempdir().expect("initialized delivery fixture");
    let close_file = directory.path().join("close-stdout");
    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            concat!(
                "IFS= read -r initialize; ",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                "while [ ! -e \"$MCP_CLOSE_FILE\" ]; do sleep 0.01; done; ",
                "exec 1>&-; sleep 60"
            )
            .to_string(),
        ],
        env: HashMap::from([(
            "MCP_CLOSE_FILE".to_string(),
            close_file.to_string_lossy().into_owned(),
        )]),
        ..McpServerEntry::default()
    };
    let barrier = Arc::new(TransportWriteBarrier::default());
    barrier.arm_on_write(2);
    let start_barrier = Arc::clone(&barrier);
    let startup = tokio::spawn(async move {
        McpServerClient::start_with_write_barrier(
            "fixture".to_string(),
            &entry,
            Arc::new(Vec::new()),
            start_barrier,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_write_block())
        .await
        .expect("initialized notification reached write barrier");
    std::fs::write(&close_file, b"close").expect("trigger stdout failure");
    tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_fatal_enqueue())
        .await
        .expect("stdout failure reached supervisor");
    barrier.release_write();

    let result = tokio::time::timeout(Duration::from_secs(2), startup)
        .await
        .expect("startup failure is bounded")
        .expect("startup task");
    let error = match result {
        Ok(_) => panic!("undelivered initialized notification must fail startup"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("closed stdout"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn fatal_transport_reaps_server_descendants_and_rejects_late_requests() {
    let directory = tempfile::tempdir().expect("fatal transport fixture");
    let pid_file = directory.path().join("pids");
    let late_request_file = directory.path().join("late-request");
    let servers = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "IFS= read -r initialize; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "IFS= read -r list_tools; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\\n'; ",
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "exec 1>&-; ",
                    "if IFS= read -r late; then printf '%s' \"$late\" > \"$MCP_LATE_FILE\"; fi; ",
                    "wait"
                )
                .to_string(),
            ],
            env: HashMap::from([
                (
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_LATE_FILE".to_string(),
                    late_request_file.to_string_lossy().into_owned(),
                ),
            ]),
            ..McpServerEntry::default()
        },
    )]);

    let manager = McpManager::new(&servers, &[])
        .await
        .expect("fixture initializes before closing stdout");
    let ids = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(ids) = std::fs::read_to_string(&pid_file) {
                break ids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture process ids");
    let client = manager.servers.get("fixture").expect("fixture client");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if client.transport_error.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fatal transport state");
    tokio::time::timeout(Duration::from_secs(2), client.transport.completion.wait())
        .await
        .expect("fatal transport child reap");

    let error = manager
        .list_tools()
        .await
        .expect_err("closed transport must reject later requests");
    assert!(error.to_string().contains("closed stdout"), "{error}");
    assert!(
        !late_request_file.exists(),
        "late request reached failed server"
    );

    let pids = ids
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("numeric fixture pid"))
        .collect::<Vec<_>>();
    let terminated = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pids.iter().all(|pid| !mcp_test_process_is_active(*pid)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !terminated {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{}", pids[0])])
            .status();
    }
    assert!(terminated, "fatal MCP transport left descendants running");
}

#[cfg(unix)]
#[tokio::test]
async fn fatal_enqueue_wins_over_blocked_write_and_drains_pending_once() {
    let directory = tempfile::tempdir().expect("fatal enqueue fixture");
    let pid_file = directory.path().join("fatal-enqueue-pids");
    let close_file = directory.path().join("close-stdout");
    let late_request_file = directory.path().join("late-request");
    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            concat!(
                "IFS= read -r initialize; ",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                "IFS= read -r initialized; ",
                "sleep 60 >&- 2>&- & child=$!; ",
                "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                "while [ ! -e \"$MCP_CLOSE_FILE\" ]; do sleep 0.01; done; ",
                "exec 1>&-; ",
                "if IFS= read -r late; then printf '%s' \"$late\" > \"$MCP_LATE_FILE\"; fi; ",
                "wait"
            )
            .to_string(),
        ],
        env: HashMap::from([
            (
                "MCP_PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            ),
            (
                "MCP_CLOSE_FILE".to_string(),
                close_file.to_string_lossy().into_owned(),
            ),
            (
                "MCP_LATE_FILE".to_string(),
                late_request_file.to_string_lossy().into_owned(),
            ),
        ]),
        ..McpServerEntry::default()
    };
    let barrier = Arc::new(TransportWriteBarrier::default());
    let client = Arc::new(
        McpServerClient::start_with_write_barrier(
            "fixture".to_string(),
            &entry,
            Arc::new(Vec::new()),
            Arc::clone(&barrier),
        )
        .await
        .expect("fatal enqueue fixture initializes"),
    );
    let pids = wait_for_mcp_fixture_pids(&pid_file).await;
    barrier.arm();
    let request_client = Arc::clone(&client);
    let request =
        tokio::spawn(async move { request_client.request("tools/list", json!({})).await });
    tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_write_block())
        .await
        .expect("request reached pre-write barrier");

    std::fs::write(&close_file, b"close").expect("trigger fatal stdout closure");
    tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_fatal_enqueue())
        .await
        .expect("fatal transport signal enqueued");
    barrier.release_write();

    let error = request
        .await
        .expect("request task")
        .expect_err("fatal transport rejects blocked request");
    assert!(error.contains("closed stdout"), "{error}");
    tokio::time::timeout(Duration::from_secs(2), client.transport.completion.wait())
        .await
        .expect("fatal transport cleanup");
    assert!(lock_pending(&client.pending).is_empty());
    assert!(
        !late_request_file.exists(),
        "post-fatal frame was delivered to the server"
    );
    assert_mcp_fixture_pids_stop(&pids).await;
    client.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_manager_reaps_healthy_server_and_descendants() {
    let directory = tempfile::tempdir().expect("manager drop fixture");
    let pid_file = directory.path().join("manager-drop-pids");
    let servers = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "IFS= read -r initialize; ",
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "IFS= read -r list_tools; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\\n'; ",
                    "wait"
                )
                .to_string(),
            ],
            env: HashMap::from([(
                "MCP_PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            )]),
            ..McpServerEntry::default()
        },
    )]);

    let manager = McpManager::new(&servers, &[])
        .await
        .expect("healthy manager initializes");
    let pids = wait_for_mcp_fixture_pids(&pid_file).await;

    drop(manager);

    assert_mcp_fixture_pids_stop(&pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_client_start_joins_supervisor_and_reaps_server_tree() {
    let directory = tempfile::tempdir().expect("client startup cancellation fixture");
    let pid_file = directory.path().join("startup-cancel-pids");
    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            concat!(
                "sleep 60 >&- 2>&- & child=$!; ",
                "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                "IFS= read -r initialize; wait"
            )
            .to_string(),
        ],
        env: HashMap::from([(
            "MCP_PID_FILE".to_string(),
            pid_file.to_string_lossy().into_owned(),
        )]),
        ..McpServerEntry::default()
    };
    let startup = tokio::spawn(async move {
        McpServerClient::start("fixture".to_string(), &entry, Arc::new(Vec::new())).await
    });
    let pids = wait_for_mcp_fixture_pids(&pid_file).await;

    startup.abort();
    match startup.await {
        Err(error) if error.is_cancelled() => {}
        Err(error) => panic!("unexpected client startup task failure: {error}"),
        Ok(_) => panic!("client startup task completed instead of being cancelled"),
    }

    assert_mcp_fixture_pids_stop(&pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn second_server_init_failure_awaits_cleanup_of_every_started_tree() {
    let directory = tempfile::tempdir().expect("partial manager fixture");
    let first_pid_file = directory.path().join("first-pids");
    let second_pid_file = directory.path().join("second-pids");
    let server = |pid_file: &std::path::Path, response: &str| McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!(
                concat!(
                    "IFS= read -r initialize; ",
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "printf '%s\\n' '{}'; ",
                    "IFS= read -r initialized || true; ",
                    "IFS= read -r list_tools || true; ",
                    "printf '%s\\n' '{}'; ",
                    "wait"
                ),
                response, r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#
            ),
        ],
        env: HashMap::from([(
            "MCP_PID_FILE".to_string(),
            pid_file.to_string_lossy().into_owned(),
        )]),
        ..McpServerEntry::default()
    };
    let servers = HashMap::from([
        (
            "a_first".to_string(),
            server(&first_pid_file, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
        ),
        (
            "b_second".to_string(),
            server(
                &second_pid_file,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"no"}}"#,
            ),
        ),
    ]);

    let error = match McpManager::new(&servers, &[]).await {
        Ok(_) => panic!("second server initialization must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("initialization failed"),
        "{error}"
    );
    let first_pids = wait_for_mcp_fixture_pids(&first_pid_file).await;
    let second_pids = wait_for_mcp_fixture_pids(&second_pid_file).await;

    assert_mcp_fixture_pids_stop(&first_pids).await;
    assert_mcp_fixture_pids_stop(&second_pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn second_server_secret_metadata_rejects_atomically_and_cleans_every_tree() {
    let directory = tempfile::tempdir().expect("partial metadata manager fixture");
    let first_pid_file = directory.path().join("first-metadata-pids");
    let second_pid_file = directory.path().join("second-metadata-pids");
    let server = |pid_file: &std::path::Path, response: Value| McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            concat!(
                "IFS= read -r initialize; ",
                "sleep 60 >&- 2>&- & child=$!; ",
                "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                "IFS= read -r initialized; ",
                "IFS= read -r list_tools; ",
                "printf '%s\\n' \"$MCP_TOOLS_RESPONSE\"; ",
                "wait"
            )
            .to_string(),
        ],
        env: HashMap::from([
            (
                "MCP_PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            ),
            ("MCP_TOOLS_RESPONSE".to_string(), response.to_string()),
        ]),
        ..McpServerEntry::default()
    };
    let servers = HashMap::from([
        (
            "a_first".to_string(),
            server(
                &first_pid_file,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {"tools": [tool("safe_first_tool")]},
                }),
            ),
        ),
        (
            "b_second".to_string(),
            server(
                &second_pid_file,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "tools": [tool("prefix_sk-secretvalue123")],
                    },
                }),
            ),
        ),
    ]);

    let error = match McpManager::new(&servers, &[]).await {
        Ok(_) => panic!("secret-bearing second discovery must fail atomically"),
        Err(error) => error.to_string(),
    };
    assert_eq!(error, format!("RPC error: {MCP_METADATA_SECRET_ERROR}"));
    assert!(!error.contains("safe_first_tool"), "{error}");
    assert!(!error.contains("sk-secretvalue123"), "{error}");

    let first_pids = wait_for_mcp_fixture_pids(&first_pid_file).await;
    let second_pids = wait_for_mcp_fixture_pids(&second_pid_file).await;
    assert_mcp_fixture_pids_stop(&first_pids).await;
    assert_mcp_fixture_pids_stop(&second_pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_uses_validated_discovery_cache_and_shared_matcher() {
    let directory = tempfile::tempdir().expect("cached metadata fixture");
    let pid_file = directory.path().join("cached-metadata-pids");
    let changed_file = directory.path().join("changed-metadata-sent");
    let unexpected_request_file = directory.path().join("unexpected-tools-list-request");
    let safe = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"tools": [tool("safe_cached_tool")]},
    });
    let changed = json!({
        "jsonrpc": "2.0",
        "id": 900,
        "result": {"tools": [tool("prefix_sk-secretvalue123")]},
    });
    let servers = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "IFS= read -r initialize; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "IFS= read -r list_tools; ",
                    "printf '%s\\n' \"$MCP_SAFE_RESPONSE\"; ",
                    "printf '%s\\n' \"$MCP_CHANGED_RESPONSE\"; ",
                    ": > \"$MCP_CHANGED_FILE\"; ",
                    "if IFS= read -r unexpected; then ",
                    "printf '%s' \"$unexpected\" > \"$MCP_UNEXPECTED_REQUEST_FILE\"; ",
                    "fi; wait"
                )
                .to_string(),
            ],
            env: HashMap::from([
                ("MCP_SAFE_RESPONSE".to_string(), safe.to_string()),
                ("MCP_CHANGED_RESPONSE".to_string(), changed.to_string()),
                (
                    "MCP_CHANGED_FILE".to_string(),
                    changed_file.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_UNEXPECTED_REQUEST_FILE".to_string(),
                    unexpected_request_file.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]),
            ..McpServerEntry::default()
        },
    )]);

    let manager = McpManager::new(&servers, &[])
        .await
        .expect("safe discovery initializes manager");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !changed_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("changed discovery frame sent");

    let client = manager.servers.get("fixture").expect("fixture client");
    assert!(Arc::ptr_eq(&manager.secret_matcher, &client.secret_matcher));
    for _ in 0..2 {
        let tools = manager.list_tools().await.expect("cached discovery");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "fixture::safe_cached_tool");
        let serialized = tools[0].to_string();
        assert!(!serialized.contains("sk-secretvalue123"), "{serialized}");
    }
    assert!(manager
        .call_tool("fixture::prefix_sk-secretvalue123", json!({}))
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !unexpected_request_file.exists(),
        "runtime reissued tools/list instead of using its cache"
    );

    let pids = wait_for_mcp_fixture_pids(&pid_file).await;
    drop(manager);
    assert_mcp_fixture_pids_stop(&pids).await;
}

#[cfg(unix)]
async fn wait_for_mcp_fixture_pids(path: &std::path::Path) -> Vec<i32> {
    let ids = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(ids) = std::fs::read_to_string(path) {
                break ids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("MCP fixture process ids");
    ids.split_whitespace()
        .map(|value| value.parse::<i32>().expect("numeric MCP fixture pid"))
        .collect()
}

#[cfg(unix)]
async fn assert_mcp_fixture_pids_stop(pids: &[i32]) {
    let terminated = tokio::time::timeout(Duration::from_secs(2), async {
        while pids.iter().any(|pid| mcp_test_process_is_active(*pid)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !terminated {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{}", pids[0])])
            .status();
    }
    assert!(terminated, "MCP fixture process tree survived shutdown");
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn mcp_child_drops_ambient_secrets_and_keeps_configured_environment() {
    const AMBIENT: &str = "NIB_MCP_AMBIENT_TOKEN";
    let previous = std::env::var_os(AMBIENT);
    std::env::set_var(AMBIENT, "must-not-reach-child");
    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "printf '%s\\n%s' \"${NIB_MCP_AMBIENT_TOKEN-}\" \"${NIB_MCP_CONFIGURED-}\"".to_string(),
        ],
        env: HashMap::from([("NIB_MCP_CONFIGURED".to_string(), "configured".to_string())]),
        ..McpServerEntry::default()
    };
    let output = mcp_child_command(&entry).output().await;
    match previous {
        Some(value) => std::env::set_var(AMBIENT, value),
        None => std::env::remove_var(AMBIENT),
    }

    let output = output.expect("MCP child fixture");
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "\nconfigured");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_mcp_child_terminates_descendant_processes() {
    use tokio::io::AsyncBufReadExt;

    let entry = McpServerEntry {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "sleep 60 & child=$!; printf '%s %s\\n' \"$$\" \"$child\"; wait".to_string(),
        ],
        ..McpServerEntry::default()
    };
    let mut command = mcp_child_command(&entry);
    let mut child = crate::sandbox::spawn_managed_child(&mut command).expect("MCP child");
    let stdout = child.stdout.take().expect("MCP child stdout");
    let mut reader = BufReader::new(stdout);
    let mut ids = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut ids))
        .await
        .expect("MCP child ids timeout")
        .expect("MCP child ids");
    let mut ids = ids.split_whitespace().map(|value| {
        value
            .parse::<i32>()
            .expect("fixture process id must be numeric")
    });
    let process_group = ids.next().expect("process group leader id");
    let descendant = ids.next().expect("descendant process id");

    drop(reader);
    drop(child);
    let terminated = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !mcp_test_process_is_active(descendant) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !terminated {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{process_group}")])
            .status();
    }
    assert!(terminated, "descendant process survived MCP child drop");
}

#[cfg(unix)]
fn mcp_test_process_is_active(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            .is_some_and(|state| state != 'Z');
    }

    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

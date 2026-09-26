//! T043 split.

use super::*;

pub(crate) fn advertised_tools() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "nib_run",
            "description": "Start a linked nib agent run through the gated executor.",
            "inputSchema": nib_run_input_schema()
        }),
        json!({
            "name": "nib_get_status",
            "description": "Return the persisted status for one nib agent run.",
            "inputSchema": nib_status_input_schema()
        }),
    ];
    tools.extend(
        registry::list_tools()
            .into_iter()
            .filter(|metadata| metadata.mcp_exposable)
            .map(|metadata| {
                json!({
                    "name": metadata.name,
                    "description": metadata.description,
                    "inputSchema": metadata.input_schema,
                    "annotations": {
                        "permissionLevel": format!("{:?}", metadata.permission_level).to_lowercase(),
                        "requiresApproval": metadata.requires_approval,
                        "requiresWorktree": metadata.requires_worktree
                    }
                })
            }),
    );
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    tools
}

pub(crate) fn nib_run_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal": {"type": "string", "minLength": 1, "maxLength": 20000},
            "max_steps": {"type": "integer", "minimum": 1, "maximum": 100}
        },
        "required": ["goal"],
        "additionalProperties": false
    })
}

pub(crate) fn nib_status_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {"type": "string", "minLength": 1, "maxLength": 128}
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

pub(crate) fn parse_tool_call(params: &Value) -> Result<(String, Value), String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "tools/call requires a non-empty name".to_string())?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err("tools/call arguments must be an object".to_string());
    }
    Ok((name.to_string(), arguments))
}

pub(crate) fn prepare_tool_call(
    requested_name: String,
    mut arguments: Value,
) -> Result<PreparedToolCall, String> {
    let (executor_name, schema, requested_status_id) = match requested_name.as_str() {
        "nib_run" => ("spawn_subagent".to_string(), nib_run_input_schema(), None),
        "nib_get_status" => (
            "manage_subagents".to_string(),
            nib_status_input_schema(),
            arguments
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        name => {
            let metadata = registry::get_tool_metadata(name)
                .filter(|tool| tool.mcp_exposable)
                .ok_or_else(|| "tool is not advertised by nib MCP".to_string())?;
            (name.to_string(), metadata.input_schema.clone(), None)
        }
    };
    validate_mcp_tool_arguments(&requested_name, &schema, &arguments)?;

    match requested_name.as_str() {
        "nib_run" => {
            let goal = arguments
                .get("goal")
                .and_then(Value::as_str)
                .expect("nib_run schema requires a string goal");
            let mut mapped = json!({"prompt": goal});
            if let Some(max_steps) = arguments.get("max_steps").cloned() {
                mapped["max_steps"] = max_steps;
            }
            arguments = mapped;
        }
        "nib_get_status" => arguments = json!({"action": "list"}),
        _ => {}
    }

    Ok(PreparedToolCall {
        requested_name,
        executor_name,
        arguments,
        requested_status_id,
    })
}

pub(crate) fn validate_mcp_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid input schema for tool '{tool_name}': {error}"))?;
    let errors: Vec<_> = validator
        .iter_errors(arguments)
        .take(5)
        .map(|error| {
            let path = error.instance_path.to_string();
            let constraint = error.schema_path.to_string();
            let location = if path.is_empty() {
                "at the argument root".to_string()
            } else {
                format!("at {path}")
            };
            format!("{location}: failed schema constraint {constraint}")
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid arguments for tool '{tool_name}': {}",
            errors.join("; ")
        ))
    }
}

pub(crate) fn safe_mcp_validation_error(error: &str, sensitive_values: &[String]) -> String {
    crate::interactive::bounded_public_text(
        error,
        sensitive_values,
        MAX_MCP_VALIDATION_ERROR_BYTES,
        false,
    )
}

pub(crate) async fn call_tool(
    project_root: &Path,
    config: &NibConfig,
    prepared: PreparedToolCall,
    cancellation: Option<&crate::agent::CancellationSignal>,
    cancellation_audit_slot: &SharedCancellationAuditSlot,
) -> (ToolResult, Option<McpCancellationAuditGuard>) {
    let PreparedToolCall {
        requested_name,
        executor_name,
        arguments,
        requested_status_id,
    } = prepared;
    if requested_name == "nib_get_status" {
        let session_id = requested_status_id
            .as_deref()
            .expect("nib_get_status schema requires a session id");
        if let Err(error) = config.validate_public_session_id(session_id) {
            return (invalid_tool_result(&requested_name, error), None);
        }
    }
    let runtime = match run_mcp_session_io(|| resolve_mcp_runtime(project_root, config)) {
        Ok(runtime) => runtime,
        Err(error) => return (invalid_tool_result(&requested_name, error), None),
    };
    let audit_session_id = uuid::Uuid::new_v4().to_string();
    let cancellation_audit = Arc::new(McpCancellationAuditState {
        session_store: runtime.session_store.clone(),
        session_id: audit_session_id.clone(),
        tool_name: requested_name.clone(),
        cancellation_id: uuid::Uuid::new_v4().to_string(),
        status: StdMutex::new(McpCancellationAuditStatus::Pending),
        #[cfg(test)]
        injected_failures: std::sync::atomic::AtomicUsize::new(0),
        #[cfg(test)]
        injected_post_commit_failures: std::sync::atomic::AtomicUsize::new(0),
    });
    cancellation_audit_slot.set(Arc::clone(&cancellation_audit));
    let cancellation_audit_guard = McpCancellationAuditGuard {
        state: cancellation_audit,
        armed: true,
    };
    let audit_session = match run_mcp_session_io(|| {
        runtime
            .session_store
            .try_create_session_with_id(audit_session_id)
    }) {
        Ok(session) => session,
        Err(error) => {
            return (
                invalid_tool_result(
                    &requested_name,
                    format!("failed to create MCP audit session: {error}"),
                ),
                Some(cancellation_audit_guard),
            )
        }
    };

    let mut executor = ToolExecutor::new(runtime.project_root.clone(), config.execution.clone())
        .with_terminal_config(&config.terminal)
        .with_approvals_config(&config.approvals)
        .with_session_store(runtime.session_store)
        .with_environment(&runtime.environment)
        .with_sensitive_values(config.public_session_sensitive_values())
        .with_approval_handler(std::sync::Arc::new(DenyInteractiveApproval));
    if let Some(cancellation) = cancellation {
        executor = executor.with_cancellation(cancellation.clone());
    }
    let mut result = SessionStore::with_lock_policy(
        MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
        executor.execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: executor_name,
                arguments,
                session_id: Some(audit_session.id.clone()),
                project_root: Some(runtime.project_root),
            },
            Some(&audit_session.id),
        ),
    )
    .await;

    result.tool_name = requested_name.clone();
    if requested_name == "nib_get_status" && result.success {
        let session_id = requested_status_id.expect("nib_get_status schema requires a session id");
        let matching = result
            .output
            .as_ref()
            .and_then(|output| output.get("subagents"))
            .and_then(Value::as_array)
            .and_then(|subagents| {
                subagents.iter().find(|record| {
                    record.get("id").and_then(Value::as_str) == Some(session_id.as_str())
                        || record.get("child_session_id").and_then(Value::as_str)
                            == Some(session_id.as_str())
                })
            })
            .cloned();
        result.output = Some(
            matching.unwrap_or_else(|| json!({"session_id": session_id, "status": "not_found"})),
        );
    }
    (result, Some(cancellation_audit_guard))
}

pub(crate) fn run_mcp_session_io<T>(operation: impl FnOnce() -> T) -> T {
    let can_block_in_place = tokio::runtime::Handle::try_current().is_ok_and(|handle| {
        matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        )
    });
    if can_block_in_place {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

pub(crate) fn resolve_mcp_runtime(
    project_root: &Path,
    config: &NibConfig,
) -> Result<McpRuntime, String> {
    let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(project_root)
        .unwrap_or_else(|| profiles.default_profile());
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;

    let session_store = SessionStore::for_project(project_root)?
        .with_lock_timeout(MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT);
    if session_store.sessions_dir() != profile.sessions_dir() {
        return Err(format!(
            "selected profile changed while initializing MCP runtime: expected {}, got {}",
            profile.sessions_dir().display(),
            session_store.sessions_dir().display()
        ));
    }

    Ok(McpRuntime {
        project_root: profile.root_path().to_path_buf(),
        session_store,
        environment: profile.custom_env().clone(),
    })
}

pub(crate) fn invalid_tool_result(tool_name: &str, error: impl Into<String>) -> ToolResult {
    ToolResult {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool_name.to_string(),
        success: false,
        output: None,
        error: Some(error.into()),
        duration_seconds: 0.0,
        approval_granted: false,
        approval_source: Some("validation".to_string()),
    }
}

pub(crate) struct BoundedOutputWriter {
    pub(crate) bytes: Vec<u8>,
    pub(crate) limit: usize,
}

impl BoundedOutputWriter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
        }
    }
}

impl Write for BoundedOutputWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP tool output exceeds its serialized byte limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serialize_bounded_tool_output(output: &Value) -> Result<String, String> {
    let mut writer = BoundedOutputWriter::new(MAX_MCP_TOOL_OUTPUT_BYTES);
    serde_json::to_writer(&mut writer, output).map_err(|_| {
        format!("tool output exceeds the {MAX_MCP_TOOL_OUTPUT_BYTES}-byte serialized MCP limit")
    })?;
    String::from_utf8(writer.bytes)
        .map_err(|error| format!("tool output was not valid UTF-8 JSON: {error}"))
}

pub(crate) fn tool_result_content(result: ToolResult) -> Value {
    let (text, structured_content, is_error) = if result.success {
        match result.output {
            Some(output) => match serialize_bounded_tool_output(&output) {
                Ok(text) => (text, Some(output), false),
                Err(error) => (error, None, true),
            },
            None => ("null".to_string(), None, false),
        }
    } else {
        (
            result
                .error
                .unwrap_or_else(|| "tool execution failed".to_string()),
            None,
            true,
        )
    };
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured_content,
        "isError": is_error,
        "_meta": {
            "tool": result.tool_name,
            "approvalGranted": result.approval_granted,
            "approvalSource": result.approval_source,
            "durationSeconds": result.duration_seconds
        }
    })
}

pub(crate) fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

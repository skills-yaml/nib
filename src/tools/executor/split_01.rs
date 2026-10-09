//! T043 split.

use super::*;

impl ToolExecutor {
    pub fn new(project_root: PathBuf, execution_config: ExecutionConfig) -> Self {
        let project_root = project_root.canonicalize().unwrap_or(project_root);
        let resolved = resolve_execution_config(&project_root, execution_config);
        Self {
            skill_context_budget: 128_000,
            skill_catalog: None,
            active_skills: Vec::new(),
            question_outcome_invocation: None,
            session_store: None,
            implicit_session_id: None,
            approval_mode: ApprovalMode::Manual,
            project_root,
            auto_approve: false,
            execution_config: resolved.config,
            terminal_backend: TerminalConfig::default().backend,
            terminal_timeout_secs: TerminalConfig::default().timeout,
            approval_handler: Arc::new(StdinApprovalHandler),
            worktree_manager: None,
            project_read_fallback: false,
            prepared_worktree_for_batch: false,
            mcp_manager: None,
            policy_rules: resolved.policy_rules,
            policy_hooks: Vec::new(),
            after_tool_hooks: Vec::new(),
            environment: HashMap::new(),
            sensitive_values: Vec::new(),
            defer_background_start: false,
            terminal_output_callback: None,
            cancellation: None,
        }
    }

    pub fn effective_execution_posture(
        project_root: &Path,
        execution_config: ExecutionConfig,
        approvals_config: &ApprovalsConfig,
    ) -> EffectiveExecutionPosture {
        let project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let resolved = resolve_execution_config(&project_root, execution_config);
        let approval_mode = approval_mode_from_config(approvals_config);
        let sandbox_route = crate::sandbox::resolve_sandbox_execution_route(
            &resolved.config.provider,
            &resolved.config.default_profile,
            &resolved.config.boundaries,
        );
        let broad_or_off = approval_mode == ApprovalMode::Off
            || matches!(sandbox_route, crate::sandbox::SandboxExecutionRoute::Direct)
            || resolved.config.boundaries.network == "enabled";
        EffectiveExecutionPosture {
            configured_approval_preset: approvals_config.mode.clone(),
            effective_approval_mode: approval_mode_label(approval_mode),
            provider: resolved.config.provider,
            profile: resolved.config.default_profile,
            network: resolved.config.boundaries.network,
            mutation_plan_gate: resolved.config.plan_mode,
            mutation_owned_worktree_gate: true,
            instruction_posture: resolved.instruction_posture,
            sandbox_route,
            broad_or_off,
        }
    }

    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    pub fn with_approval_mode(mut self, approval_mode: ApprovalMode) -> Self {
        self.approval_mode = approval_mode;
        self
    }

    pub fn with_approvals_config(mut self, config: &ApprovalsConfig) -> Self {
        self.approval_mode = approval_mode_from_config(config);
        self
    }

    pub fn with_terminal_config(mut self, config: &TerminalConfig) -> Self {
        self.terminal_backend = config.backend.clone();
        self.terminal_timeout_secs = config.timeout.max(1);
        self
    }

    pub fn with_approval_handler(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        self.approval_handler = handler;
        self
    }

    pub fn with_session_store(mut self, session_store: SessionStore) -> Self {
        self.session_store = Some(session_store);
        self
    }

    pub fn with_environment(mut self, environment: &HashMap<String, String>) -> Self {
        self.environment = environment.clone();
        self
    }

    pub fn with_sensitive_values(mut self, values: impl IntoIterator<Item = String>) -> Self {
        self.sensitive_values.extend(values);
        normalize_sensitive_values(&mut self.sensitive_values);
        self
    }

    pub fn with_cancellation(mut self, cancellation: crate::agent::CancellationSignal) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Keep background terminal jobs paused until the caller has persisted the
    /// provisional observation that names the task.
    pub fn with_deferred_background_start(mut self, deferred: bool) -> Self {
        self.defer_background_start = deferred;
        self
    }

    pub fn with_terminal_output_callback(mut self, callback: core::TerminalOutputCallback) -> Self {
        self.terminal_output_callback = Some(callback);
        self
    }

    pub fn with_terminal_output_sender(
        self,
        sender: tokio::sync::mpsc::Sender<core::TerminalOutputEvent>,
    ) -> Self {
        self.with_terminal_output_callback(Arc::new(move |event| {
            let _ = sender.try_send(event);
        }))
    }

    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub fn with_policy_rules(mut self, rules: impl IntoIterator<Item = PolicyRule>) -> Self {
        self.policy_rules.extend(rules);
        self
    }

    pub fn with_policy_hook(mut self, hook: Arc<dyn ToolPolicyHook>) -> Self {
        self.policy_hooks.push(hook);
        self
    }

    pub fn with_after_tool_hooks(mut self, hooks: impl IntoIterator<Item = AfterToolHook>) -> Self {
        self.after_tool_hooks.extend(hooks);
        self
    }

    pub async fn get_tools_schema(&self) -> Vec<Value> {
        let mut schemas = tools_json_schema();
        if self.skill_catalog.is_some() {
            for name in ["load_skill", "read_skill_resource"] {
                let metadata = get_tool_metadata(name).expect("skill tool registered");
                schemas.push(json!({"type":"function", "function":{
                    "name": metadata.name, "description": metadata.description,
                    "parameters": metadata.input_schema,
                }}));
            }
        }
        if let Some(mcp) = &self.mcp_manager {
            if let Ok(mcp_tools) = mcp.list_tools().await {
                schemas.extend(mcp_tools.into_iter().map(|tool| {
                    json!({
                        "type": "function",
                        "function": tool,
                    })
                }));
            }
        }
        schemas
    }

    /// Runtime-only outcome publication. Model/MCP calls cannot supply human results.
    pub(crate) async fn execute_question_form(
        &mut self,
        call: ToolCall,
        session_id: Option<&str>,
    ) -> ToolResult {
        self.question_outcome_invocation = Some(call.invocation_id);
        let result = self.execute(call, session_id).await;
        result
    }

    pub async fn execute(&mut self, mut call: ToolCall, session_id: Option<&str>) -> ToolResult {
        let question_outcome_admitted = self.question_outcome_invocation.take()
            == Some(call.invocation_id)
            && call.tool_name == "ask_question";
        let requested_session = session_id
            .map(str::to_string)
            .or_else(|| call.session_id.clone());
        let has_authoritative_session = requested_session.is_some();
        let effective_session = match requested_session {
            Some(session_id) => session_id,
            None => {
                if let Some(session_id) = self.implicit_session_id.clone() {
                    return self
                        .execute_inner(
                            call,
                            Some(&session_id),
                            true,
                            false,
                            question_outcome_admitted,
                        )
                        .await;
                }
                if self.session_store.is_none() {
                    match SessionStore::for_project(&self.project_root) {
                        Ok(store) => self.session_store = Some(store),
                        Err(error) => {
                            return ToolResult {
                                invocation_id: call.invocation_id,
                                tool_name: call.tool_name,
                                success: false,
                                output: None,
                                error: Some(format!(
                                    "failed to resolve profile session store for mandatory audit: {error}"
                                )),
                                duration_seconds: 0.0,
                                approval_granted: false,
                                approval_source: Some("audit".to_string()),
                            };
                        }
                    }
                }
                let store = self
                    .session_store
                    .as_ref()
                    .expect("session store initialized above");
                let session = match store.try_create_session() {
                    Ok(session) => session,
                    Err(error) => {
                        return ToolResult {
                            invocation_id: call.invocation_id,
                            tool_name: call.tool_name,
                            success: false,
                            output: None,
                            error: Some(format!(
                                "failed to create mandatory tool audit session: {error}"
                            )),
                            duration_seconds: 0.0,
                            approval_granted: false,
                            approval_source: Some("audit".to_string()),
                        };
                    }
                };
                if let Err(error) = store.record_event(
                    &session.id,
                    "implicit_audit_session",
                    json!({"tool_name": self.redact_text(&call.tool_name)}),
                ) {
                    return ToolResult {
                        invocation_id: call.invocation_id,
                        tool_name: call.tool_name,
                        success: false,
                        output: None,
                        error: Some(format!(
                            "failed to initialize mandatory tool audit session: {error}"
                        )),
                        duration_seconds: 0.0,
                        approval_granted: false,
                        approval_source: Some("audit".to_string()),
                    };
                }
                self.implicit_session_id = Some(session.id.clone());
                session.id
            }
        };
        if has_authoritative_session {
            call.session_id = Some(effective_session.clone());
        }
        self.execute_inner(
            call,
            Some(&effective_session),
            true,
            has_authoritative_session,
            question_outcome_admitted,
        )
        .await
    }

    /// Reports whether this call will invoke the configured interactive
    /// approval handler. Policy denials and automatic classifier decisions do
    /// not claim that user approval is pending.
    pub fn requires_interactive_approval(&self, call: &ToolCall) -> bool {
        let metadata = get_tool_metadata(&call.tool_name);
        let is_mcp_tool = metadata.is_none() && call.tool_name.contains("::");
        if metadata.is_none() && !is_mcp_tool {
            return false;
        }
        let level = metadata
            .map(|value| value.permission_level)
            .unwrap_or(PermissionLevel::Network);
        let requires_approval = metadata
            .map(|value| value.requires_approval)
            .unwrap_or(true);
        let risk = if is_mcp_tool {
            ToolRisk::Network
        } else {
            classify_tool_call(call)
        };
        let Ok(effective_root) = self.resolve_scope(call) else {
            return false;
        };
        let evaluations = self.matching_policy_rules(call, &effective_root);

        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::Deny)
        {
            return false;
        }
        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::RequireApproval)
        {
            return true;
        }
        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::Allow)
        {
            return false;
        }
        if call.tool_name == "run_terminal" {
            let presentation = command_presentation(
                call,
                &normalized_encoded_sensitive_values(self.redaction_secrets()),
                "local",
                false,
            );
            if !presentation.offer_grant {
                return false;
            }
            if let Some(invocation) = crate::interaction_card::terminal_invocation(call) {
                if crate::interaction_card::matching_remembered_invocation(
                    &self.project_root,
                    &invocation,
                ) {
                    return false;
                }
            }
        }
        if matches!(level, PermissionLevel::ReadOnly | PermissionLevel::Plan)
            || risk == ToolRisk::ReadOnly
            || (call.tool_name == "run_terminal"
                && risk == ToolRisk::Safe
                && self.classifier_auto_approval_allowed(call, level, risk))
            || (!requires_approval && risk == ToolRisk::Safe)
        {
            return false;
        }
        !matches!(self.approval_mode, ApprovalMode::Policy | ApprovalMode::Off)
            && !self.auto_approve
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) async fn execute_inner(
        &mut self,
        call: ToolCall,
        session_id: Option<&str>,
        run_after_hooks: bool,
        has_authoritative_session: bool,
        question_outcome_admitted: bool,
    ) -> ToolResult {
        let start = Instant::now();
        let effective_session = session_id.or(call.session_id.as_deref());
        if let Some(session_id) = effective_session {
            if !valid_session_id(session_id) {
                return ToolResult {
                    invocation_id: call.invocation_id,
                    tool_name: call.tool_name.clone(),
                    success: false,
                    output: None,
                    error: Some("invalid session id".to_string()),
                    duration_seconds: start.elapsed().as_secs_f64(),
                    approval_granted: false,
                    approval_source: Some("policy".to_string()),
                };
            }
            if self.session_store.is_none() {
                match SessionStore::for_project(&self.project_root) {
                    Ok(store) => self.session_store = Some(store),
                    Err(error) => {
                        return ToolResult {
                            invocation_id: call.invocation_id,
                            tool_name: call.tool_name.clone(),
                            success: false,
                            output: None,
                            error: Some(format!(
                                "failed to resolve profile session store for audit: {error}"
                            )),
                            duration_seconds: start.elapsed().as_secs_f64(),
                            approval_granted: false,
                            approval_source: Some("audit".to_string()),
                        };
                    }
                }
            }
            if let Err(error) = self.record_attempt(&call, session_id) {
                return ToolResult {
                    invocation_id: call.invocation_id,
                    tool_name: call.tool_name.clone(),
                    success: false,
                    output: None,
                    error: Some(format!("failed to audit tool attempt: {error}")),
                    duration_seconds: start.elapsed().as_secs_f64(),
                    approval_granted: false,
                    approval_source: Some("audit".to_string()),
                };
            }
        }
        let metadata = get_tool_metadata(&call.tool_name);
        let is_mcp_tool = metadata.is_none() && call.tool_name.contains("::");
        if metadata.is_none() && !is_mcp_tool {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                format!("Unknown tool: {}", call.tool_name),
                ApprovalDecision::denied_by_policy("unknown tool"),
                PermissionLevel::Destructive,
                ToolRisk::RequiresApproval,
                None,
                None,
            );
        }

        let level = metadata
            .map(|value| value.permission_level)
            .unwrap_or(PermissionLevel::Network);
        let requires_approval = metadata
            .map(|value| value.requires_approval)
            .unwrap_or(true);
        let requires_worktree = metadata
            .map(|value| value.requires_worktree)
            .unwrap_or(false);
        let risk = if is_mcp_tool {
            ToolRisk::Network
        } else {
            classify_tool_call(&call)
        };
        let effective_execution_config = self.effective_execution_config(level, risk);
        let plan_id = self.resolve_plan_id(effective_session);

        let input_schema = if let Some(metadata) = metadata {
            Ok(metadata.input_schema.clone())
        } else {
            match &self.mcp_manager {
                Some(manager) => match manager.list_tools().await {
                    Ok(tools) => tools
                        .into_iter()
                        .find(|tool| {
                            tool.get("name").and_then(Value::as_str) == Some(&call.tool_name)
                        })
                        .and_then(|tool| tool.get("parameters").cloned())
                        .ok_or_else(|| {
                            format!(
                                "MCP tool is not advertised by its server: {}",
                                call.tool_name
                            )
                        }),
                    Err(error) => Err(format!(
                        "failed to load MCP input schema for {}: {error}",
                        call.tool_name
                    )),
                },
                None => Err("MCP tool called but manager is not initialized".to_string()),
            }
        };
        let input_schema = match input_schema {
            Ok(schema) => schema,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("tool schema unavailable"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };
        let mut validation_arguments = schema_validation_arguments(&call);
        if call.tool_name == "ask_question" && !question_outcome_admitted {
            for key in ["_question_outcome", "answer", "answer_error"] {
                if let Some(value) = call.arguments.get(key) {
                    validation_arguments[key] = value.clone();
                }
            }
        }
        if let Err(error) =
            validate_tool_arguments(&call.tool_name, &input_schema, &validation_arguments)
        {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                error,
                ApprovalDecision::denied_by_policy("invalid tool arguments"),
                level,
                risk,
                None,
                plan_id,
            );
        }

        let effective_root = match self.resolve_scope(&call) {
            Ok(root) => root,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("invalid scope"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };

        if (matches!(level, PermissionLevel::Network) || risk == ToolRisk::Network)
            && effective_execution_config.boundaries.network == "disabled"
        {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                "network access is disabled by the effective execution boundary".to_string(),
                ApprovalDecision::denied_by_policy("network boundary disabled"),
                level,
                risk,
                None,
                plan_id,
            );
        }

        if requires_worktree && self.execution_config.plan_mode {
            let plan_approved = effective_session
                .and_then(|id| self.session_store.as_ref()?.load_result(id).ok().flatten())
                .and_then(|session| session.plan)
                .is_some_and(|plan| {
                    plan.approved
                        && plan.has_identity()
                        && plan.is_structured()
                        && !plan.is_complete()
                });
            if !plan_approved {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    "mutating execution requires an approved persisted session plan with a valid identity and incomplete work"
                        .to_string(),
                    ApprovalDecision::denied_by_policy("missing approved plan gate"),
                    level,
                    risk,
                    None,
                    plan_id,
                );
            }
        }

        let approval = self
            .handle_approval(
                &call,
                level,
                risk,
                requires_approval,
                requires_worktree,
                &effective_root,
                &effective_execution_config,
                effective_session,
            )
            .await;
        if !approval.granted {
            let message = match approval.note.as_deref() {
                Some(reason)
                    if approval.source == "denied"
                        && reason != "User denied"
                        && !reason.is_empty() =>
                {
                    format!("Approval denied: {reason}")
                }
                _ => "Approval denied".to_string(),
            };
            return self.finish_failure(
                &call,
                effective_session,
                start,
                message,
                approval,
                level,
                risk,
                None,
                plan_id,
            );
        }

        let project_read = matches!(
            call.tool_name.as_str(),
            "read_file" | "list_directory" | "grep"
        );
        let worktree_result = if (call.tool_name == "git_status"
            && !self.prepared_worktree_for_batch)
            || (self.project_read_fallback && project_read)
        {
            Ok(None)
        } else {
            self.ensure_worktree(requires_worktree, &effective_root, effective_session)
                .await
        };
        let worktree = match worktree_result {
            Ok(worktree) => worktree,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("worktree isolation failed"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };

        let execution_root = worktree.as_deref().unwrap_or(&effective_root);
        let mut dispatch_arguments = call.arguments.clone();
        if matches!(
            call.tool_name.as_str(),
            "spawn_subagent" | "invoke_subagent"
        ) {
            if let Some(arguments) = dispatch_arguments.as_object_mut() {
                arguments.remove("_parent_session_id");
                arguments.remove("_audit_sessions_dir");
                if let Some(session) = effective_session {
                    arguments.insert(
                        "_parent_session_id".to_string(),
                        Value::String(session.to_string()),
                    );
                    if let Some(store) = self.session_store.as_ref() {
                        let audit_sessions_dir =
                            match crate::tools::delegation::serialize_subagent_audit_destination(
                                store,
                            ) {
                                Ok(path) => path,
                                Err(error) => {
                                    return self.finish_failure(
                                        &call,
                                        effective_session,
                                        start,
                                        error,
                                        approval,
                                        level,
                                        risk,
                                        worktree.as_deref(),
                                        plan_id,
                                    );
                                }
                            };
                        arguments.insert("_audit_sessions_dir".to_string(), audit_sessions_dir);
                    }
                }
            }
        }
        let background_terminal = call.tool_name == "run_terminal"
            && call
                .arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if call.tool_name == "schedule" || call.tool_name == "run_terminal" {
            if let Some(arguments) = dispatch_arguments.as_object_mut() {
                arguments.remove("_session_id");
                arguments.remove("_sessions_dir");
                if has_authoritative_session
                    && (call.tool_name == "schedule" || background_terminal)
                {
                    if let (Some(session), Some(store)) =
                        (effective_session, self.session_store.as_ref())
                    {
                        arguments.insert(
                            "_session_id".to_string(),
                            Value::String(session.to_string()),
                        );
                        arguments.insert(
                            "_sessions_dir".to_string(),
                            Value::String(store.sessions_dir().to_string_lossy().to_string()),
                        );
                    }
                }
            }
        }

        let terminal_capture_limit = (call.tool_name == "run_terminal" && !background_terminal)
            .then(|| core::terminal_output_limit(&dispatch_arguments).ok())
            .flatten();
        let (terminal_output_callback, terminal_projection) =
            self.redacted_terminal_output_projection(terminal_capture_limit);
        let outcome = if matches!(
            call.tool_name.as_str(),
            "load_skill" | "read_skill_resource"
        ) {
            self.execute_skill_tool(&call.tool_name, &dispatch_arguments, effective_session)
        } else if call.tool_name == "merge_subagent_worktree" {
            self.execute_subagent_merge(&dispatch_arguments, &effective_root, effective_session)
                .await
        } else if is_mcp_tool {
            match &self.mcp_manager {
                Some(mcp) => mcp
                    .call_tool(&call.tool_name, dispatch_arguments)
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("MCP tool called but manager is not initialized".to_string()),
            }
        } else {
            core::dispatch(
                &call.tool_name,
                call.invocation_id,
                &dispatch_arguments,
                execution_root,
                &effective_execution_config,
                &self.terminal_backend,
                self.terminal_timeout_secs,
                &self.environment,
                terminal_output_callback.as_ref(),
                self.cancellation.as_ref(),
            )
            .await
        };
        let outcome = outcome.map(|output| {
            self.project_authoritative_terminal_output(
                output,
                terminal_projection.as_ref(),
                terminal_capture_limit,
            )
        });
        let mut prepared_task = PreparedTaskGuard::from_output(
            background_terminal || call.tool_name == "schedule",
            outcome.as_ref().ok(),
        );

        let (success, output, error) = match outcome {
            Ok(output)
                if call.tool_name == "run_terminal"
                    && output.get("command_success").and_then(Value::as_bool) == Some(false) =>
            {
                let error = output
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("terminal command failed")
                    .to_string();
                (
                    false,
                    Some(self.redact_value(output)),
                    Some(self.redact_text(&error)),
                )
            }
            Ok(output) => (true, Some(self.redact_value(output)), None),
            Err(error) => (false, None, Some(self.redact_text(&error))),
        };
        let mut result = ToolResult {
            invocation_id: call.invocation_id,
            tool_name: call.tool_name.clone(),
            success,
            output,
            error,
            duration_seconds: start.elapsed().as_secs_f64(),
            approval_granted: true,
            approval_source: Some(approval.source.clone()),
        };
        if result.success && run_after_hooks {
            let hooks: Vec<_> = self
                .after_tool_hooks
                .iter()
                .filter(|hook| hook.tool_name == call.tool_name)
                .cloned()
                .collect();
            let mut hook_results = Vec::with_capacity(hooks.len());
            let hook_session_id = effective_session.map(str::to_string);
            for hook in hooks {
                let hook_call = ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "run_terminal".to_string(),
                    arguments: json!({
                        "command": hook.command,
                        "hook_source": hook.source,
                        "hook_for": call.tool_name,
                    }),
                    session_id: hook_session_id.clone(),
                    project_root: Some(effective_root.clone()),
                };
                let hook_result = Box::pin(self.execute_inner(
                    hook_call,
                    hook_session_id.as_deref(),
                    false,
                    has_authoritative_session,
                    false,
                ))
                .await;
                hook_results.push(json!({
                    "source": hook.source,
                    "command": self.redact_text(&hook.command),
                    "success": hook_result.success,
                    "error": hook_result.error,
                }));
                if !hook_result.success {
                    result.success = false;
                    result.error = Some(format!(
                        "after-tool hook from {} failed: {}",
                        hook.source,
                        hook_result.error.as_deref().unwrap_or("unknown error")
                    ));
                    break;
                }
            }
            if !hook_results.is_empty() {
                result.output = Some(match result.output.take() {
                    Some(Value::Object(mut output)) => {
                        output.insert("post_hooks".to_string(), Value::Array(hook_results));
                        Value::Object(output)
                    }
                    Some(output) => json!({"result": output, "post_hooks": hook_results}),
                    None => json!({"post_hooks": hook_results}),
                });
            }
        }
        result.duration_seconds = start.elapsed().as_secs_f64();
        self.sanitize_tool_result(&mut result);
        let record_succeeded = if let Err(error) = self.record(
            &call,
            &result,
            &approval,
            effective_session,
            worktree.as_deref(),
            level,
            risk,
            plan_id,
        ) {
            result.success = false;
            result.error = Some(format!("audit recording failed: {error}"));
            false
        } else {
            true
        };
        if prepared_task.is_armed() {
            if !record_succeeded || !result.success {
                if let Err(compensation_error) = prepared_task.fail(
                    "prepared task was not started because executor reconciliation failed"
                        .to_string(),
                ) {
                    result.success = false;
                    result.error = Some(match result.error.take() {
                        Some(existing) => format!(
                            "{existing}; failed to compensate prepared task: {compensation_error}"
                        ),
                        None => format!("failed to compensate prepared task: {compensation_error}"),
                    });
                }
            } else if self.defer_background_start {
                prepared_task.release();
            } else if let Err(error) = prepared_task.start() {
                result.success = false;
                result.error = Some(match result.error.take() {
                    Some(existing) => {
                        format!("{existing}; failed to start prepared task: {error}")
                    }
                    None => format!("failed to start prepared task: {error}"),
                });
            }
        }
        self.sanitize_tool_result(&mut result);
        result
    }

    pub(crate) async fn execute_subagent_merge(
        &self,
        arguments: &Value,
        project_root: &Path,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let subagent_id = arguments
            .get("subagent_id")
            .and_then(Value::as_str)
            .ok_or("missing subagent_id")?;
        let command = arguments
            .get("verification_command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or("verification_command is required before merge")?;
        let timeout = arguments
            .get("verification_timeout")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 3_600);
        let verification_target = crate::tools::delegation::prepare_subagent_verification_target(
            project_root,
            subagent_id,
            self.cancellation.as_ref(),
        )
        .await?;
        let worktree = verification_target.worktree_path;

        let terminal_config = TerminalConfig {
            backend: self.terminal_backend.clone(),
            timeout: self.terminal_timeout_secs,
        };
        let mut verifier = ToolExecutor::new(worktree.clone(), self.execution_config.clone())
            .with_auto_approve(self.auto_approve)
            .with_approval_mode(self.approval_mode)
            .with_terminal_config(&terminal_config)
            .with_approval_handler(self.approval_handler.clone())
            .with_environment(&self.environment)
            .with_sensitive_values(self.sensitive_values.clone());
        if let Some(store) = self.session_store.clone() {
            verifier = verifier.with_session_store(store);
        }
        verifier.policy_rules.extend(self.policy_rules.clone());
        verifier.policy_hooks = self.policy_hooks.clone();
        verifier.terminal_output_callback = self.terminal_output_callback.clone();

        let configured_provider = verifier.execution_config.provider.clone();
        let sandbox_profile = verifier.execution_config.default_profile.clone();
        let boundaries = verifier.execution_config.boundaries.clone();
        let verification = Box::pin(verifier.execute_inner(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({
                    "command": command,
                    "timeout": timeout,
                }),
                session_id: session_id.map(str::to_string),
                project_root: None,
            },
            session_id,
            false,
            session_id.is_some(),
            false,
        ))
        .await;
        let evidence = crate::tools::delegation::VerificationEvidence {
            tool_name: "run_terminal".to_string(),
            command: self.redact_text(command),
            worktree_path: worktree,
            success: verification.success,
            output: verification.output,
            error: verification.error,
            approval_granted: verification.approval_granted,
            approval_source: verification.approval_source,
            duration_seconds: verification.duration_seconds,
            configured_provider,
            sandbox_profile,
            boundaries,
            session_id: session_id.map(str::to_string),
            snapshot_commit: Some(verification_target.snapshot_commit),
            executed_at: Utc::now(),
        };
        crate::tools::delegation::merge_verified_subagent_worktree(
            arguments,
            project_root,
            evidence,
            self.cancellation.as_ref(),
        )
        .await
    }

    pub(crate) fn resolve_scope(&self, call: &ToolCall) -> Result<PathBuf, String> {
        let configured = self.project_root.canonicalize().map_err(|error| {
            format!(
                "configured project root {} is unavailable: {error}",
                self.project_root.display()
            )
        })?;
        let requested = call.project_root.as_deref().unwrap_or(&configured);
        let requested = requested.canonicalize().map_err(|error| {
            format!(
                "requested project root {} is unavailable: {error}",
                requested.display()
            )
        })?;
        if !requested.starts_with(&configured) {
            return Err(format!(
                "requested project root {} is outside configured root {}",
                requested.display(),
                configured.display()
            ));
        }
        Ok(requested)
    }

    #[cfg(test)]
    pub(crate) fn redacted_terminal_output_callback(&self) -> Option<core::TerminalOutputCallback> {
        self.redacted_terminal_output_projection(None).0
    }

    pub(crate) fn redacted_terminal_output_projection(
        &self,
        capture_limit: Option<usize>,
    ) -> (
        Option<core::TerminalOutputCallback>,
        Option<Arc<Mutex<RedactedTerminalProjection>>>,
    ) {
        let callback = self.terminal_output_callback.clone();
        if callback.is_none() && capture_limit.is_none() {
            return (None, None);
        }
        let secrets = normalized_encoded_sensitive_values(self.redaction_secrets());
        let projection = Arc::new(Mutex::new(RedactedTerminalProjection {
            redactor: StreamingTerminalRedactor::new(&secrets),
            capture: capture_limit.map(RedactedTerminalCapture::new),
        }));
        let callback_projection = Arc::clone(&projection);
        let projected_callback: core::TerminalOutputCallback = Arc::new(move |event| {
            let redacted = {
                let Ok(mut projection) = callback_projection.lock() else {
                    return;
                };
                let redacted = projection
                    .redactor
                    .push(event.stream, &event.chunk, event.eof);
                if let Some(capture) = projection.capture.as_mut() {
                    capture.push(event.stream, &redacted);
                }
                redacted
            };
            if redacted.is_empty() {
                return;
            }
            if let Some(callback) = &callback {
                callback(core::TerminalOutputEvent {
                    invocation_id: event.invocation_id,
                    tool_name: event.tool_name,
                    stream: event.stream,
                    chunk: redacted,
                    background_task_id: event.background_task_id,
                    eof: false,
                });
            }
        });
        (Some(projected_callback), Some(projection))
    }

    pub(crate) fn project_authoritative_terminal_output(
        &self,
        mut output: Value,
        projection: Option<&Arc<Mutex<RedactedTerminalProjection>>>,
        capture_limit: Option<usize>,
    ) -> Value {
        let (Some(projection), Some(limit), Some(object)) =
            (projection, capture_limit, output.as_object_mut())
        else {
            return output;
        };
        let snapshot = projection.lock().ok().and_then(|projection| {
            projection
                .capture
                .as_ref()
                .map(|capture| capture.snapshot())
        });
        let has_sensitive_values = !self.redaction_secrets().is_empty();
        let stdout_truncated = object
            .get("stdout_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let stderr_truncated = object
            .get("stderr_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let (stdout, stderr) = match snapshot {
            Some((stdout, stderr)) => (
                projected_terminal_stream(stdout, limit, stdout_truncated, has_sensitive_values),
                projected_terminal_stream(stderr, limit, stderr_truncated, has_sensitive_values),
            ),
            None => (
                bounded_redaction_marker(limit),
                bounded_redaction_marker(limit),
            ),
        };
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        object.insert("stdout_bytes_retained".to_string(), json!(stdout.len()));
        object.insert("stderr_bytes_retained".to_string(), json!(stderr.len()));
        object.insert("stdout".to_string(), Value::String(stdout));
        object.insert("stderr".to_string(), Value::String(stderr));
        if object.get("command_success").and_then(Value::as_bool) == Some(false) {
            object.insert(
                "error".to_string(),
                Value::String(projected_terminal_error(object)),
            );
        }
        output
    }

    pub(crate) fn redaction_secrets(&self) -> Vec<String> {
        let mut secrets = sensitive_environment_values(&self.environment);
        secrets.extend(self.sensitive_values.iter().cloned());
        normalize_sensitive_values(&mut secrets);
        secrets
    }

    pub(crate) fn redact_text(&self, text: &str) -> String {
        let redacted = redact_text_with_encoded_sensitive_values(text, self.redaction_secrets());
        crate::interactive::control_safe_text(&redacted, true)
    }

    pub(crate) fn redact_value(&self, value: Value) -> Value {
        let redacted = redact_value_with_encoded_sensitive_values(value, self.redaction_secrets());
        control_safe_public_value(redacted)
    }

    pub(crate) fn sanitize_tool_result(&self, result: &mut ToolResult) {
        result.tool_name = self.redact_text(&result.tool_name);
        result.output = result.output.take().map(|output| self.redact_value(output));
        result.error = result.error.take().map(|error| self.redact_text(&error));
        result.approval_source = result
            .approval_source
            .take()
            .map(|source| self.redact_text(&source));
    }

    pub(crate) async fn ensure_worktree(
        &mut self,
        required: bool,
        effective_root: &Path,
        session_id: Option<&str>,
    ) -> Result<Option<PathBuf>, String> {
        let Some(session_id) = session_id else {
            return if required {
                Err("mutating tools require a session id".to_string())
            } else {
                Ok(None)
            };
        };
        if self.project_root.join(".git").is_file() {
            return Ok(Some(effective_root.to_path_buf()));
        }
        if self.worktree_manager.is_none() {
            self.worktree_manager = Some(WorktreeManager::new(self.project_root.clone()));
        }
        let cancellation = self.cancellation.clone();
        let manager = self
            .worktree_manager
            .as_mut()
            .ok_or("worktree manager unavailable")?;
        let worktree_root = if required {
            manager
                .create_for_session_cancellable(session_id, cancellation.as_ref())
                .await?
        } else {
            let Some(existing) = manager.existing_for_session(session_id)? else {
                return Ok(None);
            };
            existing
        };
        let relative = effective_root
            .strip_prefix(&self.project_root)
            .map_err(|_| {
                format!(
                    "effective root {} is not under configured root {}",
                    effective_root.display(),
                    self.project_root.display()
                )
            })?;
        let target = worktree_root
            .join(relative)
            .canonicalize()
            .map_err(|error| format!("isolated execution root cannot be resolved: {error}"))?;
        if !target.starts_with(&worktree_root) {
            return Err("isolated execution root escaped its worktree".to_string());
        }
        Ok(Some(target))
    }

    /// Establishes the exact managed worktree that a later mutation will use so
    /// instruction discovery can be resolved against the same filesystem view.
    pub(crate) async fn prepare_session_worktree(
        &mut self,
        session_id: &str,
    ) -> Result<PathBuf, String> {
        let project_root = self.project_root.clone();
        self.ensure_worktree(true, &project_root, Some(session_id))
            .await?
            .ok_or_else(|| "managed session worktree was not established".to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_approval(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        requires_approval: bool,
        requires_worktree: bool,
        effective_root: &Path,
        effective_execution_config: &ExecutionConfig,
        session_id: Option<&str>,
    ) -> ApprovalDecision {
        let evaluations = self.matching_policy_rules(call, effective_root);

        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::Deny)
        {
            return ApprovalDecision::denied_by_policy(rule.reason.clone());
        }
        if let Some(decision) = self.approval_handler.approval_ceiling(call, level, risk) {
            return decision;
        }
        if let Some(decision) =
            self.refuse_hidden_or_closed_command(call, effective_execution_config)
        {
            return decision;
        }
        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::RequireApproval)
        {
            let mut context = self.approval_context(
                call,
                level,
                risk,
                effective_root,
                effective_execution_config,
                requires_worktree,
                session_id,
                &format!("project or tool policy requires approval: {}", rule.reason),
            );
            context.remember_exact = None;
            let mut decision = self.prompt_approval(call, level, context).await;
            decision.remember_command = None;
            if decision.note.as_deref() == Some("User denied") || decision.note.is_none() {
                decision.note = Some(rule.reason.clone());
            }
            return decision;
        }
        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::Allow)
        {
            return ApprovalDecision {
                granted: true,
                source: "policy".to_string(),
                note: Some(rule.reason.clone()),
                remember_command: None,
            };
        }
        if let Some(decision) = self.remembered_command_grant(call) {
            return decision;
        }

        if matches!(level, PermissionLevel::ReadOnly | PermissionLevel::Plan)
            || risk == ToolRisk::ReadOnly
        {
            return ApprovalDecision::granted_policy();
        }
        if call.tool_name == "run_terminal"
            && risk == ToolRisk::Safe
            && self.classifier_auto_approval_allowed(call, level, risk)
        {
            return ApprovalDecision::granted_classifier();
        }
        if !requires_approval && matches!(risk, ToolRisk::Safe) {
            return ApprovalDecision::granted_policy();
        }
        if self.approval_mode == ApprovalMode::Policy {
            return ApprovalDecision::denied_by_policy("no matching allow policy");
        }
        if self.approval_mode == ApprovalMode::Off {
            return ApprovalDecision::granted_yolo();
        }
        if self.auto_approve {
            return ApprovalDecision::granted_user();
        }
        let context = self.approval_context(
            call,
            level,
            risk,
            effective_root,
            effective_execution_config,
            requires_worktree,
            session_id,
            "effective tool metadata and risk classification require interactive approval",
        );
        self.prompt_approval(call, level, context).await
    }

    pub(crate) fn refuse_hidden_or_closed_command(
        &self,
        call: &ToolCall,
        config: &ExecutionConfig,
    ) -> Option<ApprovalDecision> {
        if call.tool_name != "run_terminal" {
            return None;
        }
        let presentation = command_presentation(
            call,
            &normalized_encoded_sensitive_values(self.redaction_secrets()),
            "local",
            false,
        );
        if !presentation.offer_grant {
            return Some(ApprovalDecision::denied_unshowable());
        }
        match crate::sandbox::resolve_sandbox_execution_route(
            &config.provider,
            &config.default_profile,
            &config.boundaries,
        ) {
            crate::sandbox::SandboxExecutionRoute::FailClosed(error) => {
                Some(ApprovalDecision::denied_by_policy(error))
            }
            crate::sandbox::SandboxExecutionRoute::Direct
            | crate::sandbox::SandboxExecutionRoute::Bwrap => None,
        }
    }

    pub(crate) fn remembered_command_grant(&self, call: &ToolCall) -> Option<ApprovalDecision> {
        let invocation = crate::interaction_card::terminal_invocation(call)?;
        if !crate::interaction_card::command_can_be_remembered(&invocation.command) {
            return None;
        }
        crate::interaction_card::matching_remembered_invocation(&self.project_root, &invocation)
            .then(|| ApprovalDecision {
                granted: true,
                source: "command_prefix".to_string(),
                note: Some("remembered command".to_string()),
                remember_command: None,
            })
    }

    pub(crate) async fn prompt_approval(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        mut context: ApprovalContext,
    ) -> ApprovalDecision {
        loop {
            let decision = self
                .approval_handler
                .handle_approval_with_context(call, level, &context)
                .await;
            let Some(command) = decision.remember_command.clone() else {
                return decision;
            };
            if !decision.granted || context.remember_exact.as_deref() != Some(command.as_str()) {
                let mut decision = decision;
                decision.remember_command = None;
                return decision;
            }
            let Some(invocation) = crate::interaction_card::terminal_invocation(call) else {
                return ApprovalDecision::denied_unshowable();
            };
            if invocation.command != command {
                return ApprovalDecision::denied_unshowable();
            }
            match crate::interaction_card::remember_invocation(&self.project_root, &invocation) {
                Ok(()) => {
                    return ApprovalDecision {
                        granted: true,
                        source: "command_prefix".to_string(),
                        note: Some("remembered command".to_string()),
                        remember_command: None,
                    };
                }
                Err(_) => {
                    context.input_error =
                        Some("Input error: the command prefix was not saved".to_string());
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn approval_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        effective_root: &Path,
        effective_execution_config: &ExecutionConfig,
        requires_worktree: bool,
        session_id: Option<&str>,
        reason: &str,
    ) -> ApprovalContext {
        let secrets = normalized_encoded_sensitive_values(self.redaction_secrets());
        let worktree = if requires_worktree && session_id.is_some() {
            "required; a session-owned managed worktree will be created or reused after approval"
        } else if requires_worktree {
            "required; execution will fail closed without an authoritative session"
        } else {
            "not required for this action"
        };
        let (display_subject, display_location) = normalized_approval_display(call, &secrets);
        let environment = match crate::sandbox::resolve_sandbox_execution_route(
            &effective_execution_config.provider,
            &effective_execution_config.default_profile,
            &effective_execution_config.boundaries,
        ) {
            crate::sandbox::SandboxExecutionRoute::Bwrap => "sandbox",
            crate::sandbox::SandboxExecutionRoute::Direct
            | crate::sandbox::SandboxExecutionRoute::FailClosed(_) => "local",
        };
        let command = command_presentation(call, &secrets, environment, true);
        ApprovalContext {
            action: normalized_approval_action(call, &secrets),
            display_subject,
            display_location,
            permission_and_risk: bounded_approval_field(
                &format!("{} / {}", permission_label(level), risk.as_str()),
                &secrets,
            ),
            target_scope: bounded_approval_field(&effective_root.display().to_string(), &secrets),
            network: bounded_approval_field(
                &effective_execution_config.boundaries.network,
                &secrets,
            ),
            worktree: bounded_approval_field(worktree, &secrets),
            reason: bounded_approval_field(reason, &secrets),
            choices: "approve once or deny".to_string(),
            details: approval_invocation_details(call, &secrets),
            command_environment: command.environment,
            shown_command: command.shown_command,
            command_extras: command.extras,
            remember_exact: command.remember_exact,
            offer_grant: command.offer_grant,
            input_error: None,
        }
    }

    pub(crate) fn matching_policy_rules(
        &self,
        call: &ToolCall,
        effective_root: &Path,
    ) -> Vec<PolicyRule> {
        let mut evaluations: Vec<PolicyRule> = self
            .policy_rules
            .iter()
            .filter(|rule| rule.matches(call))
            .cloned()
            .collect();
        evaluations.extend(
            self.policy_hooks
                .iter()
                .filter_map(|hook| hook.evaluate(call, effective_root))
                .filter(|rule| rule.matches(call)),
        );
        evaluations
    }

    /// Resolve the configured sandbox into the least-privileged envelope needed
    /// for the registry permission and the argument-aware classifier result.
    pub(crate) fn effective_execution_config(
        &self,
        level: PermissionLevel,
        risk: ToolRisk,
    ) -> ExecutionConfig {
        let mut config = self.execution_config.clone();
        let elevated = matches!(
            level,
            PermissionLevel::Destructive | PermissionLevel::Network
        ) || matches!(
            risk,
            ToolRisk::RequiresApproval | ToolRisk::Destructive | ToolRisk::Network
        );
        if elevated {
            if config.provider == "internal" {
                config.provider = "hybrid".to_string();
            }
            if config.default_profile == "internal" {
                config.default_profile = "restricted".to_string();
            }
        }
        if elevated
            && !matches!(level, PermissionLevel::Network)
            && risk != ToolRisk::Network
            && config.boundaries.network == "enabled"
        {
            config.boundaries.network = "restricted".to_string();
        }
        config
    }

    pub(crate) fn classifier_auto_approval_allowed(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
    ) -> bool {
        self.classifier_auto_approval_allowed_with_bwrap(
            call,
            level,
            risk,
            crate::sandbox::detect_capabilities().bwrap_available,
        )
    }

    pub(crate) fn classifier_auto_approval_allowed_with_bwrap(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        bwrap_available: bool,
    ) -> bool {
        let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
            return false;
        };
        if !safe_command_requires_isolation(command) {
            return true;
        }
        let config = self.effective_execution_config(level, risk);
        !matches!(config.provider.as_str(), "internal")
            && config.default_profile != "internal"
            && bwrap_available
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_failure(
        &self,
        call: &ToolCall,
        session_id: Option<&str>,
        start: Instant,
        error: String,
        approval: ApprovalDecision,
        level: PermissionLevel,
        risk: ToolRisk,
        worktree: Option<&Path>,
        plan_id: Option<String>,
    ) -> ToolResult {
        let mut result = ToolResult {
            invocation_id: call.invocation_id,
            tool_name: call.tool_name.clone(),
            success: false,
            output: None,
            error: Some(self.redact_text(&error)),
            duration_seconds: start.elapsed().as_secs_f64(),
            approval_granted: approval.granted,
            approval_source: Some(approval.source.clone()),
        };
        self.sanitize_tool_result(&mut result);
        if let Err(record_error) = self.record(
            call, &result, &approval, session_id, worktree, level, risk, plan_id,
        ) {
            result.error = Some(format!(
                "{}; audit recording failed: {record_error}",
                result.error.as_deref().unwrap_or("tool execution failed")
            ));
        }
        self.sanitize_tool_result(&mut result);
        result
    }

    pub(crate) fn resolve_plan_id(&self, session_id: Option<&str>) -> Option<String> {
        session_id.and_then(|session_id| {
            self.session_store
                .as_ref()?
                .load_result(session_id)
                .ok()??
                .plan
                .filter(|plan| plan.has_identity())
                .map(|plan| plan.id)
        })
    }

    pub(crate) fn record_attempt(&self, call: &ToolCall, session_id: &str) -> Result<(), String> {
        let store = self
            .session_store
            .as_ref()
            .ok_or("session store unavailable")?;
        store
            .record_event(
                session_id,
                "tool_attempted",
                json!({
                    "invocation_id": call.invocation_id,
                    "tool_name": self.redact_text(&call.tool_name),
                    "arguments": self.redact_value(call.arguments.clone()),
                }),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn record(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        approval: &ApprovalDecision,
        session_id: Option<&str>,
        worktree: Option<&Path>,
        level: PermissionLevel,
        risk: ToolRisk,
        plan_id: Option<String>,
    ) -> Result<(), String> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let Some(store) = &self.session_store else {
            return Err("session store unavailable".to_string());
        };

        let mut provider = None;
        let mut sandbox_profile = None;
        let mut bwrap_args: Option<Vec<String>> = None;
        let mut boundaries = None;
        if call.tool_name == "run_terminal"
            || matches!(level, PermissionLevel::Network)
            || risk == ToolRisk::Network
        {
            let effective_config = self.effective_execution_config(level, risk);
            provider = Some(effective_config.provider);
            sandbox_profile = Some(effective_config.default_profile);
            boundaries = Some(effective_config.boundaries);
        }
        if let Some(object) = result.output.as_ref().and_then(Value::as_object) {
            provider = object
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(provider);
            sandbox_profile = object
                .get("sandbox_profile")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(sandbox_profile);
            bwrap_args = object
                .get("bwrap_args")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            boundaries = object
                .get("boundaries")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .or(boundaries);
        }

        provider = provider.map(|value| self.redact_text(&value));
        sandbox_profile = sandbox_profile.map(|value| self.redact_text(&value));
        bwrap_args = bwrap_args.map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| self.redact_text(&argument))
                .collect()
        });
        boundaries = boundaries.map(|boundaries| crate::config::BoundaryConfig {
            allow_write: boundaries
                .allow_write
                .into_iter()
                .map(|path| self.redact_text(&path))
                .collect(),
            network: self.redact_text(&boundaries.network),
        });
        let plan_id = plan_id.map(|value| self.redact_text(&value));
        let worktree_path = worktree.map(|path| self.redact_text(&path.to_string_lossy()));
        let environment_keys = if call.tool_name == "run_terminal" {
            let mut keys: Vec<_> = self
                .environment
                .keys()
                .map(|key| self.redact_text(key))
                .collect();
            keys.sort();
            keys
        } else {
            Vec::new()
        };

        let record = ToolCallRecord {
            invocation_id: Some(call.invocation_id),
            id: Some(format!("tool-{}", Uuid::new_v4())),
            session_id: Some(session_id.to_string()),
            tool_name: Some(self.redact_text(&call.tool_name)),
            arguments: self.redact_value(call.arguments.clone()),
            result: Some(self.redact_value(json!({
                "success": result.success,
                "output": result.output.clone(),
                "error": result.error.clone(),
                "environment_keys": environment_keys,
                "approval": {
                    "granted": approval.granted,
                    "source": self.redact_text(&approval.source),
                    "note": approval.note.as_deref().map(|note| self.redact_text(note)),
                },
                "permission_level": permission_label(level),
                "risk": risk.as_str(),
            }))),
            error: result.error.as_deref().map(|error| self.redact_text(error)),
            duration_seconds: Some(result.duration_seconds),
            worktree_path,
            timestamp: Some(Utc::now()),
            provider,
            sandbox_profile,
            bwrap_args,
            boundaries,
            plan_id,
        };
        store
            .record_tool_call(record)
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn schema_validation_arguments(call: &ToolCall) -> Value {
    let mut arguments = call.arguments.clone();
    let Some(object) = arguments.as_object_mut() else {
        return arguments;
    };
    match call.tool_name.as_str() {
        "spawn_subagent" | "invoke_subagent" => {
            object.remove("_parent_session_id");
            object.remove("_audit_sessions_dir");
        }
        "schedule" | "run_terminal" => {
            object.remove("_session_id");
            object.remove("_sessions_dir");
            object.remove("hook_source");
            object.remove("hook_for");
        }
        "ask_question" => {
            object.remove("answer");
            object.remove("answer_error");
            object.remove("_question_outcome");
        }
        _ => {}
    }
    arguments
}

pub(crate) fn validate_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| format!("invalid input schema for tool '{tool_name}'"))?;
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
        if tool_name == "ask_question" {
            crate::interactive::parse_question_form(arguments)?;
        }
        Ok(())
    } else {
        const MAX_VALIDATION_ERROR_BYTES: usize = 8 * 1024;
        let mut error = format!(
            "invalid arguments for tool '{tool_name}': {}",
            errors.join("; ")
        );
        if error.len() > MAX_VALIDATION_ERROR_BYTES {
            let mut end = MAX_VALIDATION_ERROR_BYTES.saturating_sub(3);
            while end > 0 && !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
            error.push_str("...");
        }
        Err(error)
    }
}

pub(crate) fn validate_registered_tool_arguments(
    tool_name: &str,
    arguments: &Value,
) -> Result<(), String> {
    let metadata = crate::tools::registry::get_tool_metadata(tool_name)
        .ok_or_else(|| format!("tool is not registered: {tool_name}"))?;
    validate_tool_arguments(tool_name, &metadata.input_schema, arguments)
}

pub fn tools_json_schema() -> Vec<Value> {
    crate::tools::registry::list_tools()
        .into_iter()
        .filter(|metadata| metadata.mcp_exposable)
        .map(|metadata| {
            json!({
                "type": "function",
                "function": {
                    "name": metadata.name,
                    "description": metadata.description,
                    "parameters": metadata.input_schema,
                }
            })
        })
        .collect()
}

pub(crate) fn permission_label(level: PermissionLevel) -> &'static str {
    match level {
        PermissionLevel::ReadOnly => "read_only",
        PermissionLevel::Plan => "plan",
        PermissionLevel::Safe => "safe",
        PermissionLevel::Destructive => "destructive",
        PermissionLevel::Network => "network",
    }
}

pub(crate) fn bounded_approval_field(value: &str, secrets: &[String]) -> String {
    bounded_approval_field_with_limit(value, secrets, MAX_APPROVAL_FIELD_BYTES)
}

pub(crate) fn bounded_approval_field_with_limit(
    value: &str,
    secrets: &[String],
    limit: usize,
) -> String {
    let redacted = redact_text_with_encoded_secrets(value, secrets);
    let redacted = crate::interactive::control_safe_text(&redacted, true);
    let mut normalized = String::new();
    let mut truncated = false;
    for character in redacted.chars() {
        let escaped = match character {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            character if character.is_control() => format!("\\u{{{:x}}}", character as u32),
            character => character.to_string(),
        };
        if normalized.len().saturating_add(escaped.len()) > limit {
            truncated = true;
            break;
        }
        normalized.push_str(&escaped);
    }
    if truncated {
        let maximum = limit.saturating_sub(3);
        while normalized.len() > maximum {
            normalized.pop();
        }
        normalized.push_str("...");
    }
    if normalized.is_empty() {
        "(none)".to_string()
    } else {
        normalized
    }
}

pub(crate) fn approval_argument_string<'a>(call: &'a ToolCall, field: &str) -> Option<&'a str> {
    call.arguments.get(field).and_then(Value::as_str)
}

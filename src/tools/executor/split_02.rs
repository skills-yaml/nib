//! T043 split.

use super::*;

pub(crate) fn normalized_patch_targets(
    call: &ToolCall,
    secrets: &[String],
) -> (usize, Vec<String>) {
    let patch = approval_argument_string(call, "patch").unwrap_or_default();
    let mut targets = HashSet::new();
    let mut in_hunk = false;
    let mut pending_old_header = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            pending_old_header = false;
            continue;
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
            pending_old_header = false;
            continue;
        }
        let custom_path = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "));
        let unified_path = (!in_hunk && pending_old_header)
            .then(|| line.strip_prefix("+++ "))
            .flatten();
        if !in_hunk && line.starts_with("--- ") {
            pending_old_header = true;
            continue;
        }
        pending_old_header = false;
        let Some(path) = custom_path.or(unified_path) else {
            continue;
        };
        let path = path.trim().trim_start_matches("b/");
        if path != "/dev/null" {
            targets.insert(path.to_string());
        }
    }
    let target_count = targets.len();
    let mut targets = targets.into_iter().collect::<Vec<_>>();
    targets.sort();
    targets.truncate(4);
    let targets = targets
        .into_iter()
        .map(|path| bounded_approval_field_with_limit(&path, secrets, 40))
        .collect();
    (target_count, targets)
}

pub(crate) fn normalized_approval_display(
    call: &ToolCall,
    secrets: &[String],
) -> (String, Option<String>) {
    let path = || {
        ["path", "target_file", "file_path"]
            .into_iter()
            .find_map(|field| approval_argument_string(call, field))
    };
    let bounded = |value: &str, limit| bounded_approval_field_with_limit(value, secrets, limit);
    let display = match call.tool_name.as_str() {
        "run_terminal" => (
            bounded(
                approval_argument_string(call, "command").unwrap_or("(missing command)"),
                120,
            ),
            approval_argument_string(call, "cwd").map(|value| bounded(value, 48)),
        ),
        "read_file" | "list_directory" | "search_replace" => {
            (bounded(path().unwrap_or("(missing path)"), 120), None)
        }
        "apply_patch" => {
            let (target_count, targets) = normalized_patch_targets(call, secrets);
            let subject = if targets.is_empty() {
                "workspace files".to_string()
            } else {
                let omitted = target_count.saturating_sub(targets.len());
                let omission = if omitted == 0 {
                    String::new()
                } else {
                    format!(" (+{omitted} more)")
                };
                format!(
                    "{target_count} {}{omission}: {}",
                    if target_count == 1 { "file" } else { "files" },
                    targets.join(",")
                )
            };
            (bounded(&subject, 120), None)
        }
        "grep" => {
            let pattern = approval_argument_string(call, "pattern")
                .or_else(|| approval_argument_string(call, "query"));
            let subject = [pattern, path()]
                .into_iter()
                .flatten()
                .map(|value| bounded(value, 56))
                .collect::<Vec<_>>()
                .join(" in ");
            (
                if subject.is_empty() {
                    "(missing pattern)".to_string()
                } else {
                    bounded(&subject, 120)
                },
                None,
            )
        }
        "approve_plan" => (
            bounded(
                approval_argument_string(call, "goal")
                    .or_else(|| approval_argument_string(call, "plan_id"))
                    .unwrap_or("(missing plan)"),
                120,
            ),
            None,
        ),
        "merge_subagent_worktree" => (
            bounded(
                approval_argument_string(call, "subagent_id").unwrap_or("(missing subagent)"),
                96,
            ),
            None,
        ),
        other => {
            let value = [
                "command",
                "path",
                "query",
                "pattern",
                "url",
                "action",
                "target_file",
            ]
            .into_iter()
            .find_map(|field| approval_argument_string(call, field))
            .unwrap_or(other);
            (bounded(value, 120), None)
        }
    };
    display
}

pub(crate) fn approval_invocation_details(call: &ToolCall, secrets: &[String]) -> Vec<String> {
    if call.tool_name == "approve_plan" {
        return call
            .arguments
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, step)| {
                let text = step.as_str()?.trim();
                if text.is_empty() {
                    return None;
                }
                let bounded =
                    bounded_approval_field_with_limit(text, secrets, MAX_APPROVAL_LINE_BYTES);
                (!bounded.is_empty()).then(|| format!("{}. {bounded}", index + 1))
            })
            .collect();
    }
    let raw = match call.tool_name.as_str() {
        "run_terminal" => format!(
            "Command: {}\nWorking directory: {}\nBackground: {}",
            approval_argument_string(call, "command").unwrap_or("(missing command)"),
            approval_argument_string(call, "cwd").unwrap_or("."),
            call.arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        ),
        "apply_patch" => format!(
            "Patch:\n{}",
            approval_argument_string(call, "patch").unwrap_or("(missing patch)")
        ),
        _ => format!("Validated arguments: {}", call.arguments),
    };
    let safe = crate::interactive::control_safe_text(
        &redact_text_with_encoded_secrets_with_limit(
            &raw,
            secrets,
            MAX_APPROVAL_DETAIL_INPUT_BYTES,
        ),
        true,
    );
    if safe.len() <= MAX_APPROVAL_DETAIL_PAGE_BYTES {
        return vec![safe];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < safe.len() {
        let mut end = start
            .saturating_add(MAX_APPROVAL_DETAIL_PAGE_BYTES)
            .min(safe.len());
        while end > start && !safe.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(safe[start..end].to_string());
        start = end;
    }
    let page_count = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            format!(
                "[approval details page {}/{}; no content omitted]\n{chunk}",
                index + 1,
                page_count
            )
        })
        .collect()
}

pub(crate) fn normalized_approval_action(call: &ToolCall, secrets: &[String]) -> String {
    let summary = match call.tool_name.as_str() {
        "approve_plan" => {
            let plan_id = bounded_approval_field_with_limit(
                approval_argument_string(call, "plan_id").unwrap_or("(missing)"),
                secrets,
                64,
            );
            let goal = bounded_approval_field_with_limit(
                approval_argument_string(call, "goal").unwrap_or("(missing)"),
                secrets,
                80,
            );
            let step_count = call
                .arguments
                .get("steps")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("approve_plan plan_id={plan_id} steps={step_count} goal={goal}")
        }
        "run_terminal" => {
            let command = bounded_approval_field_with_limit(
                approval_argument_string(call, "command").unwrap_or("(missing command)"),
                secrets,
                120,
            );
            let cwd = bounded_approval_field_with_limit(
                approval_argument_string(call, "cwd").unwrap_or("."),
                secrets,
                48,
            );
            let background = call
                .arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!("run_terminal command={command} cwd={cwd} background={background}")
        }
        "apply_patch" => {
            let (target_count, targets) = normalized_patch_targets(call, secrets);
            let mode = if call
                .arguments
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "check"
            } else {
                "apply"
            };
            format!(
                "apply_patch mode={mode} files={target_count} targets={}",
                if targets.is_empty() {
                    "(unparsed)".to_string()
                } else {
                    targets.join(",")
                }
            )
        }
        "merge_subagent_worktree" => format!(
            "merge_subagent_worktree subagent_id={}",
            bounded_approval_field_with_limit(
                approval_argument_string(call, "subagent_id").unwrap_or("(missing)"),
                secrets,
                96,
            )
        ),
        "manage_subagents" | "manage_task" | "manage_memory" => {
            let action = bounded_approval_field_with_limit(
                approval_argument_string(call, "action").unwrap_or("(missing)"),
                secrets,
                48,
            );
            let target = ["subagent_id", "task_id", "namespace", "key"]
                .into_iter()
                .filter_map(|field| {
                    approval_argument_string(call, field).map(|value| {
                        format!(
                            " {field}={}",
                            bounded_approval_field_with_limit(value, secrets, 64)
                        )
                    })
                })
                .collect::<String>();
            format!("{} action={action}{target}", call.tool_name)
        }
        _ => bounded_approval_field(&call.tool_name, secrets),
    };
    bounded_approval_field(&summary, secrets)
}

pub(crate) fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

pub(crate) fn approval_mode_from_config(config: &ApprovalsConfig) -> ApprovalMode {
    permission_mode_from_name(&config.mode).unwrap_or(ApprovalMode::Manual)
}

/// Parses a configured or user-selected mode. Both the original config names
/// (`manual`, `smart`, `policy`, `off`) and the permission-mode names
/// (`ask`, `accept-edits`, `plan`, `auto`) are accepted (T080).
pub(crate) fn permission_mode_from_name(name: &str) -> Option<ApprovalMode> {
    match name.trim().to_ascii_lowercase().as_str() {
        "manual" | "ask" => Some(ApprovalMode::Manual),
        "smart" | "accept-edits" | "accept_edits" => Some(ApprovalMode::Smart),
        "policy" => Some(ApprovalMode::Policy),
        "off" | "auto" => Some(ApprovalMode::Off),
        "plan" => Some(ApprovalMode::Plan),
        _ => None,
    }
}

pub(crate) fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Manual => "manual",
        ApprovalMode::Smart => "smart",
        ApprovalMode::Policy => "policy",
        ApprovalMode::Off => "off",
        ApprovalMode::Plan => "plan",
    }
}

/// User-facing permission-mode name shown in the footer and by `/mode`.
pub(crate) fn permission_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Manual => "ask",
        ApprovalMode::Smart => "accept-edits",
        ApprovalMode::Policy => "policy",
        ApprovalMode::Off => "auto",
        ApprovalMode::Plan => "plan",
    }
}

/// Shift+Tab order: ask → accept-edits → plan → ask. `auto` and `policy` are
/// left for an explicit `/mode`, so cycling never silently drops prompts.
pub(crate) fn next_cycled_permission_mode(mode: ApprovalMode) -> ApprovalMode {
    match mode {
        ApprovalMode::Manual => ApprovalMode::Smart,
        ApprovalMode::Smart => ApprovalMode::Plan,
        ApprovalMode::Plan | ApprovalMode::Policy | ApprovalMode::Off => ApprovalMode::Manual,
    }
}

/// Tools whose only effect is editing files inside the workspace.
pub(crate) fn is_file_edit_tool(name: &str) -> bool {
    name == "apply_patch"
}

pub(crate) fn resolve_execution_config(
    project_root: &Path,
    mut execution_config: ExecutionConfig,
) -> ResolvedExecutionConfig {
    let configured = execution_config.clone();
    let mut policy_rules = load_instruction_policy_rules(project_root);
    let instruction_posture =
        match apply_instruction_execution_tightening(project_root, &mut execution_config) {
            Ok(()) if execution_config == configured => InstructionExecutionPosture::Configured,
            Ok(()) => InstructionExecutionPosture::Tightened,
            Err(error) => {
                fail_closed_execution_config(&mut execution_config);
                policy_rules.push(PolicyRule {
                    effect: PolicyEffect::Deny,
                    tool_name: "*".to_string(),
                    argument_contains: None,
                    reason: format!("invalid instruction execution directive: {error}"),
                });
                InstructionExecutionPosture::InvalidFailClosed
            }
        };
    ResolvedExecutionConfig {
        config: execution_config,
        policy_rules,
        instruction_posture,
    }
}

pub(crate) fn load_instruction_policy_rules(project_root: &Path) -> Vec<PolicyRule> {
    let files = instruction_policy_files(project_root);
    let mut rules = Vec::new();
    for file in files {
        let contents = match read_instruction_policy_file(&file) {
            Ok(contents) => contents,
            Err(error) => {
                rules.push(PolicyRule {
                    effect: PolicyEffect::Deny,
                    tool_name: "*".to_string(),
                    argument_contains: None,
                    reason: error,
                });
                continue;
            }
        };
        for (index, line) in contents.lines().enumerate() {
            let trimmed = line.trim().trim_start_matches(['-', '*']).trim();
            let Some(directive) = trimmed.strip_prefix("nib-policy:") else {
                continue;
            };
            let mut parts = directive.trim().splitn(3, char::is_whitespace);
            let effect = match parts.next().unwrap_or_default() {
                "allow" => PolicyEffect::Allow,
                "require-approval" => PolicyEffect::RequireApproval,
                "deny" => PolicyEffect::Deny,
                _ => continue,
            };
            let tool_name = parts.next().unwrap_or("*").to_string();
            let argument_contains = parts
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            rules.push(PolicyRule {
                effect,
                tool_name,
                argument_contains,
                reason: format!("{}:{}", file.display(), index + 1),
            });
        }
    }
    rules
}

pub(crate) fn read_instruction_policy_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect instruction file {}: {error}",
            path.display()
        )
    })?;
    validate_instruction_policy_metadata(path, &metadata)?;
    let file = std::fs::File::open(path).map_err(|error| {
        format!(
            "failed to read instruction file {}: {error}",
            path.display()
        )
    })?;
    validate_instruction_policy_metadata(
        path,
        &file.metadata().map_err(|error| error.to_string())?,
    )?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INSTRUCTION_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "failed to read instruction file {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_INSTRUCTION_POLICY_BYTES {
        return Err(format!(
            "instruction file {} exceeds the {MAX_INSTRUCTION_POLICY_BYTES}-byte policy limit",
            path.display()
        ));
    }
    validate_instruction_policy_metadata(
        path,
        &std::fs::symlink_metadata(path).map_err(|error| error.to_string())?,
    )?;
    String::from_utf8(bytes).map_err(|error| {
        format!(
            "failed to read instruction file {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn validate_instruction_policy_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "instruction file must be a regular local file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_INSTRUCTION_POLICY_BYTES {
        return Err(format!(
            "instruction file {} exceeds the {MAX_INSTRUCTION_POLICY_BYTES}-byte policy limit",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn instruction_policy_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for name in [
        "AGENTS.md",
        "AGENTS.local.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
    ] {
        let path = project_root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    let skills_root = project_root.join(".nib").join("skills");
    if let Ok(entries) = std::fs::read_dir(skills_root) {
        files.extend(
            entries
                .flatten()
                .take(256)
                .map(|entry| entry.path().join("SKILL.md"))
                .filter(|path| {
                    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
                }),
        );
    }
    files
}

pub(crate) fn fail_closed_execution_config(config: &mut ExecutionConfig) {
    config.provider = "bwrap".to_string();
    config.default_profile = "restricted".to_string();
    config.boundaries.network = "disabled".to_string();
    config.boundaries.allow_write.clear();
}

pub(crate) fn valid_boundary_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !matches!(name, "internal" | "restricted")
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

pub(crate) fn apply_named_boundary_profile(
    config: &mut ExecutionConfig,
    configured_boundaries: &crate::config::BoundaryConfig,
    name: &str,
) -> Result<(), String> {
    if !valid_boundary_profile_name(name) {
        return Err(format!(
            "invalid or reserved boundary profile name '{name}'"
        ));
    }
    let profile = config
        .boundary_profiles
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unknown boundary profile '{name}'"))?;
    boundary_profile_tightening_error(configured_boundaries, &profile).map_err(|reason| {
        format!("boundary profile '{name}' would weaken configured boundaries: {reason}")
    })?;

    config.default_profile = name.to_string();
    config.boundaries = profile;
    if config.boundaries.network == "disabled" {
        config.provider = "bwrap".to_string();
    } else if config.provider == "internal" {
        config.provider = "hybrid".to_string();
    }
    Ok(())
}

pub(crate) fn apply_instruction_execution_tightening(
    project_root: &Path,
    config: &mut ExecutionConfig,
) -> Result<(), String> {
    let configured_boundaries = config.boundaries.clone();
    let mut selected_profile: Option<String> = None;
    let mut require_bwrap = false;
    let mut disable_network = false;

    for file in instruction_policy_files(project_root) {
        let contents = match read_instruction_policy_file(&file) {
            Ok(contents) => contents,
            Err(error) => {
                fail_closed_execution_config(config);
                return Err(error);
            }
        };
        for line in contents.lines() {
            let trimmed = line.trim().trim_start_matches(['-', '*']).trim();
            if trimmed == "nib-sandbox: require-bwrap" {
                require_bwrap = true;
            } else if trimmed == "nib-boundary: disable-network" {
                require_bwrap = true;
                disable_network = true;
            } else if let Some(directive) = trimmed.strip_prefix("nib-boundary:") {
                let mut parts = directive.split_whitespace();
                if parts.next() != Some("profile") {
                    continue;
                }
                let name = parts.next().ok_or_else(|| {
                    format!(
                        "{} contains a boundary profile directive without a name",
                        file.display()
                    )
                })?;
                if parts.next().is_some() {
                    return Err(format!(
                        "{} contains an invalid boundary profile directive",
                        file.display()
                    ));
                }
                if selected_profile
                    .as_deref()
                    .is_some_and(|selected| selected != name)
                {
                    return Err(format!(
                        "conflicting boundary profiles '{}' and '{name}' were selected",
                        selected_profile.as_deref().unwrap_or_default()
                    ));
                }
                selected_profile = Some(name.to_string());
            }
        }
    }

    if let Some(name) = selected_profile {
        apply_named_boundary_profile(config, &configured_boundaries, &name)?;
    }
    if require_bwrap {
        config.provider = "bwrap".to_string();
        if config.default_profile == "internal" {
            config.default_profile = "restricted".to_string();
        }
    }
    if disable_network {
        config.provider = "bwrap".to_string();
        config.boundaries.network = "disabled".to_string();
    }
    Ok(())
}

pub(crate) fn redact_value(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            let mut redacted = Map::new();
            for (key, value) in std::mem::take(object) {
                let key_lower = key.to_ascii_lowercase();
                if ["api_key", "apikey", "token", "password", "secret"]
                    .iter()
                    .any(|sensitive| key_lower.contains(sensitive))
                {
                    redacted.insert(key, Value::String("[REDACTED]".to_string()));
                } else {
                    redacted.insert(key, redact_value(value));
                }
            }
            *object = redacted;
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value(std::mem::take(value));
            }
        }
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
    value
}

pub(crate) fn redact_value_with_environment(
    value: Value,
    environment: &HashMap<String, String>,
) -> Value {
    redact_value_with_secrets(value, &sensitive_environment_values(environment))
}

pub(crate) fn redact_value_with_encoded_sensitive_values(
    value: Value,
    values: impl IntoIterator<Item = String>,
) -> Value {
    let secrets = normalized_encoded_sensitive_values(values);
    redact_value_with_encoded_secrets(value, &secrets)
}

pub(crate) fn control_safe_public_value(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = control_safe_public_value(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = control_safe_public_value(std::mem::take(value));
            }
        }
        Value::String(text) => {
            *text = crate::interactive::control_safe_text(text, true);
        }
        _ => {}
    }
    value
}

pub(crate) fn redact_value_with_encoded_secrets(mut value: Value, secrets: &[String]) -> Value {
    value = redact_value(value);
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = redact_value_with_encoded_secrets(std::mem::take(value), secrets);
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value_with_encoded_secrets(std::mem::take(value), secrets);
            }
        }
        Value::String(text) => *text = redact_text_with_encoded_secrets(text, secrets),
        _ => {}
    }
    value
}

pub(crate) fn redact_value_with_secrets(mut value: Value, secrets: &[String]) -> Value {
    value = redact_value(value);
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = redact_value_with_secrets(std::mem::take(value), secrets);
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value_with_secrets(std::mem::take(value), secrets);
            }
        }
        Value::String(text) => *text = redact_text_with_secrets(text, secrets),
        _ => {}
    }
    value
}

pub(crate) fn redact_text(text: &str) -> String {
    GENERIC_SECRET_PATTERN
        .replace_all(text, "[REDACTED]")
        .into_owned()
}

pub(crate) fn contains_generic_secret(text: &str) -> bool {
    GENERIC_SECRET_PATTERN.is_match(text)
}

pub(crate) fn redact_text_with_environment(
    text: &str,
    environment: &HashMap<String, String>,
) -> String {
    redact_text_with_secrets(text, &sensitive_environment_values(environment))
}

pub(crate) fn redact_text_with_encoded_sensitive_values(
    text: &str,
    values: impl IntoIterator<Item = String>,
) -> String {
    let secrets = normalized_encoded_sensitive_values(values);
    redact_text_with_encoded_secrets(text, &secrets)
}

pub(crate) fn redact_text_with_secrets(text: &str, secrets: &[String]) -> String {
    let mut redacted = redact_text(text);
    for secret in secrets {
        redacted = redacted.replace(secret, "[REDACTED]");
    }
    redacted
}

pub(crate) fn redact_text_with_encoded_secrets(text: &str, secrets: &[String]) -> String {
    const MAX_ENCODED_REDACTION_INPUT_BYTES: usize = 64 * 1024;

    redact_text_with_encoded_secrets_with_limit(text, secrets, MAX_ENCODED_REDACTION_INPUT_BYTES)
}

pub(crate) fn redact_text_with_encoded_secrets_with_limit(
    text: &str,
    secrets: &[String],
    max_input_bytes: usize,
) -> String {
    if text.len() > max_input_bytes {
        return "[REDACTED]".to_string();
    }

    let Some(text_stages) = percent_decoded_byte_stages(text) else {
        return "[REDACTED]".to_string();
    };
    let mut secret_stages = Vec::new();
    for secret in secrets {
        let Some(stages) = percent_decoded_byte_stages(secret) else {
            return "[REDACTED]".to_string();
        };
        secret_stages.extend(
            stages
                .into_iter()
                .map(|stage| stage.bytes)
                .filter(|secret| !secret.is_empty()),
        );
    }
    let mut spans = Vec::new();
    for text_stage in &text_stages {
        for secret in &secret_stages {
            if secret.len() > text_stage.bytes.len() {
                continue;
            }
            for offset in 0..=text_stage.bytes.len() - secret.len() {
                if text_stage.bytes[offset..offset + secret.len()] == *secret.as_slice() {
                    let mut start = text_stage.origins[offset].0;
                    let mut end = text_stage.origins[offset + secret.len() - 1].1;
                    while !text.is_char_boundary(start) {
                        start = start.saturating_sub(1);
                    }
                    while end < text.len() && !text.is_char_boundary(end) {
                        end += 1;
                    }
                    spans.push((start, end));
                }
            }
        }
    }

    if spans.is_empty() {
        return redact_text_with_secrets(text, secrets);
    }
    spans.sort_unstable();
    let mut merged = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some((_, previous_end)) if start <= *previous_end => {
                *previous_end = (*previous_end).max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in merged {
        redacted.push_str(&text[cursor..start]);
        redacted.push_str("[REDACTED]");
        cursor = end;
    }
    redacted.push_str(&text[cursor..]);
    redact_text(&redacted)
}

#[derive(Clone)]
pub(crate) struct PercentDecodedBytes {
    pub(crate) bytes: Vec<u8>,
    pub(crate) origins: Vec<(usize, usize)>,
}

pub(crate) fn percent_decoded_byte_stages(value: &str) -> Option<Vec<PercentDecodedBytes>> {
    let mut stages = vec![PercentDecodedBytes {
        bytes: value.as_bytes().to_vec(),
        origins: (0..value.len()).map(|index| (index, index + 1)).collect(),
    }];
    let mut passes = 0;
    loop {
        let current = stages.last().expect("percent-decoding stage");
        let mut next = PercentDecodedBytes {
            bytes: Vec::with_capacity(current.bytes.len()),
            origins: Vec::with_capacity(current.origins.len()),
        };
        let mut index = 0;
        let mut decoded_escape = false;
        while index < current.bytes.len() {
            if current.bytes[index] == b'%' && index + 2 < current.bytes.len() {
                if let (Some(high), Some(low)) = (
                    percent_hex_value(current.bytes[index + 1]),
                    percent_hex_value(current.bytes[index + 2]),
                ) {
                    next.bytes.push((high << 4) | low);
                    next.origins
                        .push((current.origins[index].0, current.origins[index + 2].1));
                    index += 3;
                    decoded_escape = true;
                    continue;
                }
            }
            next.bytes.push(current.bytes[index]);
            next.origins.push(current.origins[index]);
            index += 1;
        }
        if !decoded_escape {
            return Some(stages);
        }
        if passes == MAX_PERCENT_DECODE_PASSES {
            return None;
        }
        stages.push(next);
        passes += 1;
    }
}

pub(crate) fn percent_hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn normalized_encoded_sensitive_values(
    values: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut secrets = values.into_iter().collect::<Vec<_>>();
    secrets.extend(
        secrets
            .iter()
            .map(|secret| secret.trim().to_string())
            .collect::<Vec<_>>(),
    );
    let json_variants = secrets
        .iter()
        .filter_map(|secret| serde_json::to_string(secret).ok())
        .filter_map(|quoted| {
            quoted
                .get(1..quoted.len().saturating_sub(1))
                .map(str::to_string)
        })
        .flat_map(|escaped| [escaped.clone(), escaped.replace('/', "\\/")])
        .collect::<Vec<_>>();
    secrets.extend(json_variants);
    let base64_variants = secrets
        .iter()
        .flat_map(|secret| {
            let standard = base64_secret_variant(secret.as_bytes(), false);
            let url_safe = base64_secret_variant(secret.as_bytes(), true);
            [
                standard.clone(),
                standard.trim_end_matches('=').to_string(),
                url_safe.clone(),
                url_safe.trim_end_matches('=').to_string(),
            ]
        })
        .collect::<Vec<_>>();
    secrets.extend(base64_variants);
    normalize_sensitive_values(&mut secrets);
    secrets
}

pub(crate) fn base64_secret_variant(bytes: &[u8], url_safe: bool) -> String {
    let alphabet = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(alphabet[usize::from(first >> 2)] as char);
        encoded.push(alphabet[usize::from(((first & 0x03) << 4) | (second >> 4))] as char);
        if chunk.len() > 1 {
            encoded.push(alphabet[usize::from(((second & 0x0f) << 2) | (third >> 6))] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[usize::from(third & 0x3f)] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

pub(crate) fn sensitive_environment_values(environment: &HashMap<String, String>) -> Vec<String> {
    let mut secrets: Vec<_> = environment
        .iter()
        .filter(|(key, value)| is_sensitive_environment_key(key) && !value.is_empty())
        .map(|(_, value)| value.clone())
        .collect();
    normalize_sensitive_values(&mut secrets);
    secrets
}

pub(crate) fn normalize_sensitive_values(secrets: &mut Vec<String>) {
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
}

pub(crate) fn is_sensitive_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "API_KEY",
        "APIKEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

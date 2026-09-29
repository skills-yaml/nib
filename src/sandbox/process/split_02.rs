//! T043 split.

use super::*;

pub(crate) fn validate_scope_fields(
    scope_id: &str,
    workload_kind: &str,
    execution_generation: u64,
) -> Result<(), String> {
    validate_scope_id(scope_id)?;
    if workload_kind.is_empty()
        || workload_kind.len() > 64
        || !workload_kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("managed-process workload kind is invalid".to_string());
    }
    if execution_generation == 0 {
        return Err("managed-process execution generation must be non-zero".to_string());
    }
    Ok(())
}

pub(crate) fn validate_scope_id(scope_id: &str) -> Result<(), String> {
    if scope_id.is_empty()
        || scope_id.len() > 160
        || !scope_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("managed-process scope identifier is invalid".to_string());
    }
    Ok(())
}

pub(crate) fn validate_record(record: &ProcessScopeRecord) -> Result<(), String> {
    if record.version != PROCESS_SCOPE_VERSION {
        return Err(format!(
            "unsupported managed-process scope version {}",
            record.version
        ));
    }
    validate_record_contents(record)
}

pub(crate) fn validate_record_contents(record: &ProcessScopeRecord) -> Result<(), String> {
    validate_scope_fields(
        &record.scope_id,
        &record.workload_kind,
        record.execution_generation,
    )?;
    validate_canonical_process_uuid(&record.cleanup_lease_id, "cleanup lease identifier")?;
    if let Some(nonce) = record.supervisor_registration_nonce.as_deref() {
        validate_canonical_process_uuid(nonce, "supervisor registration nonce")?;
        if record.workload_kind != "subagent" || record.launch_committed.is_none() {
            return Err(
                "managed-process supervisor registration nonce is not bound to a new subagent launch"
                    .to_string(),
            );
        }
    }
    validate_process_identity(&record.owner, "owner")?;
    if let Some(supervisor) = &record.supervisor {
        validate_process_identity(supervisor, "supervisor")?;
    }
    if let Some(direct_child) = &record.direct_child {
        validate_process_identity(direct_child, "direct child")?;
    }
    if let Some(reason) = &record.cleanup_reason {
        validate_process_cleanup_text(reason, "cleanup reason")?;
    }
    if record.status == ProcessScopeStatus::Complete {
        match (&record.cleanup_proof, &record.launch_abort_proof) {
            (Some(proof), None) => {
                validate_process_identity(&proof.direct_child, "cleanup proof direct child")?;
                validate_process_cleanup_text(&proof.outcome, "cleanup outcome")?;
                if proof.execution_generation != record.execution_generation
                    || proof.cleanup_lease_id != record.cleanup_lease_id
                    || proof.backend != record.backend
                    || Some(&proof.direct_child) != record.direct_child.as_ref()
                    || !proof.descendants_reaped
                    || record.launch_committed == Some(false)
                {
                    return Err(
                        "managed-process cleanup proof does not match its scope".to_string()
                    );
                }
            }
            (None, Some(proof)) => {
                validate_process_identity(&proof.supervisor, "launch-abort supervisor")?;
                if let Some(namespace_root) = &proof.namespace_root {
                    validate_process_identity(namespace_root, "launch-abort namespace root")?;
                }
                validate_process_cleanup_text(&proof.outcome, "launch-abort outcome")?;
                if proof.execution_generation != record.execution_generation
                    || proof.cleanup_lease_id != record.cleanup_lease_id
                    || proof.backend != record.backend
                    || Some(&proof.supervisor) != record.supervisor.as_ref()
                    || proof.namespace_root != record.direct_child
                    || proof.outcome != LAUNCH_ABORT_OUTCOME
                    || !proof.workload_never_launched
                    || record.launch_committed == Some(true)
                {
                    return Err(
                        "managed-process launch-abort proof does not match its scope".to_string(),
                    );
                }
            }
            (Some(_), Some(_)) => {
                return Err(
                    "completed managed-process scope carries conflicting completion proofs"
                        .to_string(),
                );
            }
            (None, None) => {
                return Err("completed managed-process scope has no completion proof".to_string());
            }
        }
    } else if record.cleanup_proof.is_some() || record.launch_abort_proof.is_some() {
        return Err("nonterminal managed-process scope carries a completion proof".to_string());
    }
    Ok(())
}

pub(crate) fn validate_canonical_process_uuid(value: &str, label: &str) -> Result<(), String> {
    let parsed =
        uuid::Uuid::parse_str(value).map_err(|_| format!("managed-process {label} is invalid"))?;
    if parsed.to_string() != value {
        return Err(format!("managed-process {label} is not canonical"));
    }
    Ok(())
}

pub(crate) fn validate_process_identity(
    identity: &ProcessIdentity,
    label: &str,
) -> Result<(), String> {
    if identity.pid == 0
        || identity.start_marker.is_empty()
        || identity.start_marker.len() > MAX_PROCESS_IDENTITY_MARKER_BYTES
    {
        return Err(format!("managed-process {label} identity is invalid"));
    }
    Ok(())
}

pub(crate) fn validate_process_cleanup_text(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_PROCESS_CLEANUP_TEXT_BYTES {
        return Err(format!(
            "managed-process {label} exceeds the {MAX_PROCESS_CLEANUP_TEXT_BYTES}-byte limit or is empty"
        ));
    }
    Ok(())
}

pub(crate) fn encode_process_state_bounded<T: Serialize>(
    value: &T,
    label: &str,
) -> Result<Vec<u8>, String> {
    let encoded = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("failed to encode managed-process {label}: {error}"))?;
    if encoded.len() as u64 > MAX_SCOPE_RECORD_BYTES {
        return Err(format!(
            "managed-process {label} exceeds the {MAX_SCOPE_RECORD_BYTES}-byte limit"
        ));
    }
    Ok(encoded)
}

pub(crate) fn maintain_process_scope_directory_until(
    directory: &crate::daemons::state::StableDirectory,
    reserve_new_scope: bool,
    deadline: Option<Instant>,
) -> Result<(), String> {
    maintain_process_scope_directory_with_limits_until(
        directory,
        reserve_new_scope,
        PROCESS_SCOPE_DIRECTORY_LIMITS,
        deadline,
    )
}

#[cfg(test)]
pub(crate) fn maintain_process_scope_directory_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    reserve_new_scope: bool,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    maintain_process_scope_directory_with_limits_until(directory, reserve_new_scope, limits, None)
}

pub(crate) fn maintain_process_scope_directory_with_limits_until(
    directory: &crate::daemons::state::StableDirectory,
    reserve_new_scope: bool,
    limits: ProcessScopeDirectoryLimits,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let usage = process_scope_directory_usage_with_limits_until(directory, limits, deadline)?;
    let reserved = usize::from(reserve_new_scope);
    if usage.records > limits.max_records.saturating_sub(reserved) {
        return Err(format!(
            "managed-process scope count exceeds the {}-record limit",
            limits.max_records
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn process_scope_directory_usage_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    limits: ProcessScopeDirectoryLimits,
) -> Result<ProcessScopeDirectoryUsage, String> {
    process_scope_directory_usage_with_limits_until(directory, limits, None)
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn process_scope_directory_usage_with_limits_until(
    directory: &crate::daemons::state::StableDirectory,
    limits: ProcessScopeDirectoryLimits,
    deadline: Option<Instant>,
) -> Result<ProcessScopeDirectoryUsage, String> {
    ensure_process_scope_deadline(deadline)?;
    recover_all_process_atomic_transactions_until(directory, limits, deadline)?;
    recover_stale_process_temporaries_until(
        directory,
        SCOPE_WRITE_PREFIX,
        ProcessAtomicKind::Scope,
        limits,
        deadline,
    )?;
    recover_stale_process_temporaries_until(
        directory,
        CLEANUP_LEASE_WRITE_PREFIX,
        ProcessAtomicKind::CleanupLease,
        limits,
        deadline,
    )?;

    let mut usage = ProcessScopeDirectoryUsage::default();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        usage.entries = usage
            .entries
            .checked_add(1)
            .ok_or_else(|| "managed-process state entry count overflowed".to_string())?;
        usage.name_bytes = usage
            .name_bytes
            .checked_add(name.as_encoded_bytes().len())
            .ok_or_else(|| "managed-process state filename byte count overflowed".to_string())?;
        let name_text = name
            .to_str()
            .ok_or_else(|| "managed-process state contains a non-UTF-8 filename".to_string())?;
        let path = directory.path().join(&name);
        let file = directory.open_read(&path)?;
        let length = file
            .metadata()
            .map_err(|error| {
                format!(
                    "failed to inspect managed-process state {}: {error}",
                    path.display()
                )
            })?
            .len();
        usage.bytes = usage
            .bytes
            .checked_add(length)
            .ok_or_else(|| "managed-process state aggregate byte count overflowed".to_string())?;
        if usage.bytes > limits.max_bytes {
            return Err(format!(
                "managed-process state exceeds the {}-byte aggregate limit",
                limits.max_bytes
            ));
        }

        if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            SCOPE_WRITE_PREFIX,
        ) || crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            CLEANUP_LEASE_WRITE_PREFIX,
        ) || is_process_deletion_quarantine(name_text)
            || (name_text.starts_with('.') && name_text.ends_with(".lock"))
        {
            return Ok(());
        }
        if let Some(scope_id) = name_text.strip_suffix(".json") {
            validate_scope_id(scope_id)?;
            let record: ProcessScopeRecord = read_bounded_json(&file, &path)?;
            if record.version == 1 {
                validate_record_contents(&record)?;
            } else {
                validate_record(&record)?;
            }
            if record.scope_id != scope_id {
                return Err(format!(
                    "managed-process scope filename does not match its record: {}",
                    path.display()
                ));
            }
            usage.records = usage
                .records
                .checked_add(1)
                .ok_or_else(|| "managed-process scope count overflowed".to_string())?;
            return Ok(());
        }
        if let Some(scope_id) = name_text.strip_suffix(CLEANUP_LEASE_SUFFIX) {
            validate_scope_id(scope_id)?;
            let lease: CleanupLeaseRecord = read_bounded_json(&file, &path)?;
            if lease.version == 1 {
                validate_cleanup_lease_contents(&lease)?;
            } else {
                validate_cleanup_lease_record(&lease)?;
            }
            if lease.scope_id != scope_id {
                return Err(format!(
                    "managed-process cleanup lease filename does not match its record: {}",
                    path.display()
                ));
            }
            return Ok(());
        }
        Err(format!(
            "managed-process state contains an unknown entry: {}",
            path.display()
        ))
    })?;
    Ok(usage)
}

#[cfg(test)]
pub(crate) fn ensure_process_scope_publication_budget(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    encoded_len: u64,
    kind: ProcessAtomicKind,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    ensure_process_scope_publication_budget_until(
        directory,
        target,
        encoded_len,
        kind,
        limits,
        None,
    )
}

pub(crate) fn ensure_process_scope_publication_budget_until(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    encoded_len: u64,
    kind: ProcessAtomicKind,
    limits: ProcessScopeDirectoryLimits,
    deadline: Option<Instant>,
) -> Result<(), String> {
    if encoded_len > MAX_SCOPE_RECORD_BYTES {
        return Err(format!(
            "managed-process publication exceeds the {MAX_SCOPE_RECORD_BYTES}-byte record limit"
        ));
    }
    let usage = process_scope_directory_usage_with_limits_until(directory, limits, deadline)?;
    let target_exists = directory.path_exists(target)?;
    let temporary = directory.deterministic_artifact_path(target, kind.write_prefix(), ".tmp")?;
    let transaction_peer = if target_exists {
        directory.deterministic_previous_artifact_path(target, kind.write_prefix())?
    } else {
        target.to_path_buf()
    };
    let adds_record = usize::from(!target_exists && matches!(kind, ProcessAtomicKind::Scope));
    let transaction_name_bytes = temporary
        .file_name()
        .ok_or("managed-process temporary publication has no filename")?
        .as_encoded_bytes()
        .len()
        .checked_add(
            transaction_peer
                .file_name()
                .ok_or("managed-process publication transaction peer has no filename")?
                .as_encoded_bytes()
                .len(),
        )
        .ok_or_else(|| "managed-process transaction filename byte count overflowed".to_string())?;
    let records = usage
        .records
        .checked_add(adds_record)
        .ok_or_else(|| "managed-process scope count overflowed".to_string())?;
    let entries = usage
        .entries
        .checked_add(2)
        .ok_or_else(|| "managed-process state entry count overflowed".to_string())?;
    let name_bytes = usage
        .name_bytes
        .checked_add(transaction_name_bytes)
        .ok_or_else(|| "managed-process state filename byte count overflowed".to_string())?;
    let transaction_bytes = encoded_len
        .checked_mul(2)
        .ok_or_else(|| "managed-process transaction byte count overflowed".to_string())?;
    let bytes = usage
        .bytes
        .checked_add(transaction_bytes)
        .ok_or_else(|| "managed-process state aggregate byte count overflowed".to_string())?;

    if records > limits.max_records {
        return Err(format!(
            "managed-process scope count exceeds the {}-record limit",
            limits.max_records
        ));
    }
    if entries > limits.max_entries {
        return Err(format!(
            "managed-process state exceeds the {}-entry limit",
            limits.max_entries
        ));
    }
    if name_bytes > limits.max_name_bytes {
        return Err(format!(
            "managed-process state exceeds the {}-byte filename limit",
            limits.max_name_bytes
        ));
    }
    if bytes > limits.max_bytes {
        return Err(format!(
            "managed-process state exceeds the {}-byte aggregate limit",
            limits.max_bytes
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum ProcessAtomicKind {
    Scope,
    CleanupLease,
}

impl ProcessAtomicKind {
    pub(crate) fn write_prefix(self) -> &'static str {
        match self {
            Self::Scope => SCOPE_WRITE_PREFIX,
            Self::CleanupLease => CLEANUP_LEASE_WRITE_PREFIX,
        }
    }
}

pub(crate) fn recover_stale_process_temporaries_until(
    directory: &crate::daemons::state::StableDirectory,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
    limits: ProcessScopeDirectoryLimits,
    deadline: Option<Instant>,
) -> Result<(), String> {
    ensure_process_scope_deadline(deadline)?;
    let mut temporary_names = Vec::new();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            temporary_prefix,
        ) && crate::daemons::state::StableDirectory::atomic_previous_target_name(
            &name,
            temporary_prefix,
        )
        .is_none()
        {
            temporary_names.push(name);
        }
        Ok(())
    })?;
    for name in temporary_names {
        let path = directory.path().join(name);
        if !directory.path_exists(&path)? {
            continue;
        }
        let file = directory.open_read_write(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect managed-process temporary state {}: {error}",
                    path.display()
                ));
            }
        }
        if process_atomic_payload_version(&file, &path, None, kind)? == 1
            && legacy_process_temporary_matches_payload(
                directory,
                &file,
                &path,
                temporary_prefix,
                kind,
            )?
        {
            continue;
        }
        ensure_process_scope_deadline(deadline)?;
        directory.remove_visible_file_if_matches_direct_with_guard(&path, &file, || {
            ensure_process_scope_deadline(deadline)
        })?;
    }
    ensure_process_scope_deadline(deadline)?;
    directory.sync_directory()
}

pub(crate) fn recover_all_process_atomic_transactions_until(
    directory: &crate::daemons::state::StableDirectory,
    limits: ProcessScopeDirectoryLimits,
    deadline: Option<Instant>,
) -> Result<(), String> {
    ensure_process_scope_deadline(deadline)?;
    let mut transactions = Vec::new();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        for (prefix, kind) in [
            (SCOPE_WRITE_PREFIX, ProcessAtomicKind::Scope),
            (CLEANUP_LEASE_WRITE_PREFIX, ProcessAtomicKind::CleanupLease),
        ] {
            if let Some(target) =
                crate::daemons::state::StableDirectory::atomic_previous_target_name(&name, prefix)
            {
                transactions.push((target, prefix, kind));
            }
        }
        Ok(())
    })?;
    transactions.sort_by(|left, right| left.0.cmp(&right.0));
    transactions.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    for (target, prefix, kind) in transactions {
        recover_process_atomic_transaction_until(
            directory,
            &directory.path().join(target),
            prefix,
            kind,
            deadline,
        )?;
    }
    Ok(())
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn recover_process_atomic_transaction_until(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
    deadline: Option<Instant>,
) -> Result<(), String> {
    ensure_process_scope_deadline(deadline)?;
    let temporary = directory.deterministic_artifact_path(target, temporary_prefix, ".tmp")?;
    let previous = directory.deterministic_previous_artifact_path(target, temporary_prefix)?;
    let temporary_file = if directory.path_exists(&temporary)? {
        let file = directory.open_read_write(&temporary)?;
        match file.try_lock() {
            Ok(()) => Some(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "managed-process atomic transaction is still owned by a live writer: {}",
                    target.display()
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect managed-process atomic writer {}: {error}",
                    temporary.display()
                ));
            }
        }
    } else {
        None
    };

    let target_file = directory
        .path_exists(target)?
        .then(|| directory.open_read(target))
        .transpose()?;
    let previous_file = directory
        .path_exists(&previous)?
        .then(|| directory.open_read(&previous))
        .transpose()?;
    let target_payload_file = target_file
        .as_ref()
        .map(|file| process_atomic_read_handle(file, temporary_file.as_ref()))
        .transpose()?;
    let previous_payload_file = previous_file
        .as_ref()
        .map(|file| process_atomic_read_handle(file, temporary_file.as_ref()))
        .transpose()?;
    if process_atomic_transaction_is_legacy(
        target,
        target_payload_file.map(|file| (file, target)),
        previous_payload_file.map(|file| (file, previous.as_path())),
        temporary_file
            .as_ref()
            .map(|file| (file, temporary.as_path())),
        kind,
    )? {
        return Ok(());
    }
    match (target_file.as_ref(), previous_file.as_ref()) {
        (None, Some(previous_file)) => {
            validate_process_atomic_payload(
                directory,
                target,
                previous_payload_file.expect("previous payload handle"),
                kind,
            )?;
            ensure_process_scope_deadline(deadline)?;
            directory.restore_visible_file_no_replace_if_matches_with_guard(
                &previous,
                previous_file,
                target,
                || ensure_process_scope_deadline(deadline),
            )?;
        }
        (Some(target_file), Some(previous_file)) => {
            validate_process_atomic_pair(
                directory,
                target,
                target_payload_file.expect("target payload handle"),
                &previous,
                previous_payload_file.expect("previous payload handle"),
                kind,
            )?;
            directory.verify_file_identity(target, target_file)?;
            ensure_process_scope_deadline(deadline)?;
            directory.remove_visible_file_if_matches_direct_with_guard(
                &previous,
                previous_file,
                || ensure_process_scope_deadline(deadline),
            )?;
        }
        (Some(target_file), None) => {
            validate_process_atomic_payload(
                directory,
                target,
                target_payload_file.expect("target payload handle"),
                kind,
            )?;
            directory.verify_file_identity(target, target_file)?;
        }
        (None, None) => {}
    }
    if let Some(temporary_file) = temporary_file {
        if directory.path_exists(&temporary)? {
            ensure_process_scope_deadline(deadline)?;
            directory.remove_visible_file_if_matches_direct_with_guard(
                &temporary,
                &temporary_file,
                || ensure_process_scope_deadline(deadline),
            )?;
        }
    }
    ensure_process_scope_deadline(deadline)?;
    directory.sync_directory()
}

pub(crate) fn process_atomic_read_handle<'a>(
    visible: &'a File,
    temporary: Option<&'a File>,
) -> Result<&'a File, String> {
    if let Some(temporary) = temporary {
        if crate::daemons::state::same_open_file_identity(visible, temporary)? {
            return Ok(temporary);
        }
    }
    Ok(visible)
}

pub(crate) fn process_atomic_transaction_is_legacy(
    target_path: &Path,
    target: Option<(&File, &Path)>,
    previous: Option<(&File, &Path)>,
    temporary: Option<(&File, &Path)>,
    kind: ProcessAtomicKind,
) -> Result<bool, String> {
    let mut has_legacy = false;
    let mut has_current_or_unknown = false;
    for (file, path) in [target, previous, temporary].into_iter().flatten() {
        if process_atomic_payload_version(file, path, Some(target_path), kind)? == 1 {
            has_legacy = true;
        } else {
            has_current_or_unknown = true;
        }
    }
    if has_legacy && has_current_or_unknown {
        return Err("managed-process atomic transaction mixes schema versions".to_string());
    }
    Ok(has_legacy)
}

pub(crate) fn process_atomic_payload_version(
    file: &File,
    path: &Path,
    target: Option<&Path>,
    kind: ProcessAtomicKind,
) -> Result<u32, String> {
    match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, path)?;
            if record.version == 1 {
                validate_record_contents(&record)?;
                if let Some(target) = target {
                    validate_process_atomic_scope_key(target, &record.scope_id)?;
                }
            }
            Ok(record.version)
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, path)?;
            if record.version == 1 {
                validate_cleanup_lease_contents(&record)?;
                if let Some(target) = target {
                    validate_process_atomic_cleanup_lease_key(target, &record.scope_id)?;
                }
            }
            Ok(record.version)
        }
    }
}

pub(crate) fn validate_process_atomic_scope_key(
    target: &Path,
    scope_id: &str,
) -> Result<(), String> {
    let target_scope_id = target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
        .ok_or("managed-process atomic scope target has an invalid suffix")?;
    validate_scope_id(target_scope_id)?;
    if scope_id != target_scope_id {
        return Err("managed-process atomic scope target has a mismatched key".to_string());
    }
    Ok(())
}

pub(crate) fn validate_process_atomic_cleanup_lease_key(
    target: &Path,
    scope_id: &str,
) -> Result<(), String> {
    let target_scope_id = target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(CLEANUP_LEASE_SUFFIX))
        .ok_or("managed-process atomic cleanup-lease target has an invalid suffix")?;
    validate_scope_id(target_scope_id)?;
    if scope_id != target_scope_id {
        return Err("managed-process atomic cleanup-lease target has a mismatched key".to_string());
    }
    Ok(())
}

pub(crate) fn legacy_process_temporary_matches_payload(
    directory: &crate::daemons::state::StableDirectory,
    file: &File,
    temporary: &Path,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
) -> Result<bool, String> {
    let target = match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, temporary)?;
            directory.path().join(format!("{}.json", record.scope_id))
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, temporary)?;
            directory
                .path()
                .join(format!("{}{}", record.scope_id, CLEANUP_LEASE_SUFFIX))
        }
    };
    let expected = directory.deterministic_artifact_path(&target, temporary_prefix, ".tmp")?;
    Ok(expected == temporary)
}

pub(crate) fn validate_process_atomic_payload(
    _directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    file: &File,
    kind: ProcessAtomicKind,
) -> Result<(), String> {
    match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, target)?;
            validate_record(&record)?;
            validate_process_atomic_scope_key(target, &record.scope_id)?;
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, target)?;
            validate_cleanup_lease_record(&record)?;
            validate_process_atomic_cleanup_lease_key(target, &record.scope_id)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_process_atomic_pair(
    directory: &crate::daemons::state::StableDirectory,
    target_path: &Path,
    target_file: &File,
    previous_path: &Path,
    previous_file: &File,
    kind: ProcessAtomicKind,
) -> Result<(), String> {
    validate_process_atomic_payload(directory, target_path, target_file, kind)?;
    validate_process_atomic_payload(directory, target_path, previous_file, kind)?;
    match kind {
        ProcessAtomicKind::Scope => {
            let target: ProcessScopeRecord = read_bounded_json(target_file, target_path)?;
            let previous: ProcessScopeRecord = read_bounded_json(previous_file, previous_path)?;
            validate_process_scope_transition(&previous, &target)
        }
        ProcessAtomicKind::CleanupLease => {
            let target: CleanupLeaseRecord = read_bounded_json(target_file, target_path)?;
            let previous: CleanupLeaseRecord = read_bounded_json(previous_file, previous_path)?;
            if target == previous {
                Ok(())
            } else {
                Err(
                    "managed-process cleanup-lease target and prior belong to different generations"
                        .to_string(),
                )
            }
        }
    }
}

pub(crate) fn validate_process_scope_transition(
    previous: &ProcessScopeRecord,
    target: &ProcessScopeRecord,
) -> Result<(), String> {
    let immutable_matches = previous.version == target.version
        && previous.scope_id == target.scope_id
        && previous.workload_kind == target.workload_kind
        && previous.execution_generation == target.execution_generation
        && previous.cleanup_lease_id == target.cleanup_lease_id
        && previous.supervisor_registration_nonce == target.supervisor_registration_nonce
        && previous.owner == target.owner
        && previous.backend == target.backend
        && previous.created_at == target.created_at;
    let identities_are_monotonic = (previous.supervisor.is_none()
        || previous.supervisor == target.supervisor)
        && (previous.direct_child.is_none() || previous.direct_child == target.direct_child);
    let proof_is_monotonic = (previous.cleanup_proof.is_none()
        || previous.cleanup_proof == target.cleanup_proof)
        && (previous.launch_abort_proof.is_none()
            || previous.launch_abort_proof == target.launch_abort_proof);
    let launch_commit_is_monotonic = previous.launch_committed == target.launch_committed
        || (previous.launch_committed == Some(false) && target.launch_committed == Some(true))
        || (previous.launch_committed.is_none()
            && previous.status == ProcessScopeStatus::Prepared
            && target.status == ProcessScopeStatus::Running
            && target.launch_committed == Some(true));
    let launch_abort_is_valid = matches!(
        (previous.status, target.status),
        (ProcessScopeStatus::Prepared, ProcessScopeStatus::Complete)
            | (ProcessScopeStatus::Running, ProcessScopeStatus::Complete)
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::Complete
            )
            | (
                ProcessScopeStatus::RecoveryRequired,
                ProcessScopeStatus::Complete
            )
    ) && previous.launch_committed != Some(true)
        && previous.supervisor.is_some()
        && previous.supervisor == target.supervisor
        && previous.direct_child == target.direct_child
        && target.cleanup_reason.as_deref() == Some(LAUNCH_ABORT_OUTCOME)
        && target.cleanup_proof.is_none()
        && target
            .launch_abort_proof
            .as_ref()
            .is_some_and(|proof| proof.outcome == LAUNCH_ABORT_OUTCOME);
    let status_is_monotonic = matches!(
        (previous.status, target.status),
        (ProcessScopeStatus::Prepared, ProcessScopeStatus::Prepared)
            | (ProcessScopeStatus::Prepared, ProcessScopeStatus::Running)
            | (ProcessScopeStatus::Running, ProcessScopeStatus::Running)
            | (
                ProcessScopeStatus::Running,
                ProcessScopeStatus::CleanupInProgress
            )
            | (ProcessScopeStatus::Running, ProcessScopeStatus::Complete)
            | (
                ProcessScopeStatus::Running,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::CleanupInProgress
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::Complete
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::RecoveryRequired,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::RecoveryRequired,
                ProcessScopeStatus::CleanupInProgress
            )
            | (ProcessScopeStatus::Complete, ProcessScopeStatus::Complete)
    ) || launch_abort_is_valid;
    if immutable_matches
        && identities_are_monotonic
        && proof_is_monotonic
        && launch_commit_is_monotonic
        && status_is_monotonic
        && target.updated_at >= previous.updated_at
    {
        Ok(())
    } else {
        Err(
            "managed-process atomic target is not a legal monotonic successor of its prior"
                .to_string(),
        )
    }
}

pub(crate) fn is_process_deletion_quarantine(name: &str) -> bool {
    (name.starts_with(CLEANUP_LEASE_DELETE_PREFIX) || name.starts_with(SCOPE_DELETE_PREFIX))
        && name.ends_with(".quarantine")
}

pub(crate) fn validate_cleanup_lease_record(record: &CleanupLeaseRecord) -> Result<(), String> {
    if record.version != PROCESS_SCOPE_VERSION {
        return Err(format!(
            "unsupported managed-process cleanup lease version {}",
            record.version
        ));
    }
    validate_cleanup_lease_contents(record)
}

pub(crate) fn validate_cleanup_lease_contents(record: &CleanupLeaseRecord) -> Result<(), String> {
    validate_scope_id(&record.scope_id)?;
    if record.execution_generation == 0 {
        return Err("managed-process cleanup lease generation must be non-zero".to_string());
    }
    let lease_id = uuid::Uuid::parse_str(&record.cleanup_lease_id)
        .map_err(|_| "managed-process cleanup lease identifier is invalid".to_string())?;
    if lease_id.to_string() != record.cleanup_lease_id {
        return Err("managed-process cleanup lease identifier is not canonical".to_string());
    }
    Ok(())
}

pub(crate) fn recover_cleanup_lease_deletion_until(
    directory: &crate::daemons::state::StableDirectory,
    lease_path: &Path,
    scope: &ProcessScopeRecord,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    ensure_process_scope_deadline(deadline)?;
    let quarantine = directory.deterministic_artifact_path(
        lease_path,
        CLEANUP_LEASE_DELETE_PREFIX,
        ".quarantine",
    )?;
    if !directory.path_exists(&quarantine)? {
        return Ok(false);
    }
    if directory.path_exists(lease_path)? {
        ensure_process_scope_deadline(deadline)?;
        directory.recover_quarantined_file_guarded(
            lease_path,
            CLEANUP_LEASE_DELETE_PREFIX,
            &mut || ensure_process_scope_deadline(deadline),
        )?;
        return Ok(false);
    }
    if scope.status != ProcessScopeStatus::Complete
        || (scope.cleanup_proof.is_none() && scope.launch_abort_proof.is_none())
    {
        return Err(
            "managed-process cleanup lease has an unproven deletion quarantine".to_string(),
        );
    }
    let Some(file) = directory.open_read_write_if_exists(&quarantine)? else {
        ensure_process_scope_deadline(deadline)?;
        if directory.path_exists(&quarantine)? {
            return Err(
                "managed-process cleanup lease deletion quarantine changed while it was being opened; it was preserved"
                    .to_string(),
            );
        }
        return Ok(false);
    };
    let observed: CleanupLeaseRecord = read_bounded_json(&file, &quarantine)?;
    validate_cleanup_lease_record(&observed)?;
    let expected = CleanupLeaseRecord {
        version: PROCESS_SCOPE_VERSION,
        scope_id: scope.scope_id.clone(),
        execution_generation: scope.execution_generation,
        cleanup_lease_id: scope.cleanup_lease_id.clone(),
    };
    if observed != expected {
        return Err(
            "managed-process cleanup lease deletion quarantine belongs to another generation"
                .to_string(),
        );
    }
    match try_cleanup_lease_lock(&file) {
        Ok(()) => {
            ensure_process_scope_deadline(deadline)?;
            directory.remove_visible_file_if_matches_direct_with_guard(
                &quarantine,
                &file,
                || ensure_process_scope_deadline(deadline),
            )?;
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "failed to inspect quarantined managed-process cleanup lease {}: {error}",
            quarantine.display()
        )),
    }
}

pub(crate) fn recover_scope_deletion_for_expected_until(
    directory: &crate::daemons::state::StableDirectory,
    scope_path: &Path,
    expected: &ProcessScopeRecord,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    ensure_process_scope_deadline(deadline)?;
    let quarantine =
        directory.deterministic_artifact_path(scope_path, SCOPE_DELETE_PREFIX, ".quarantine")?;
    if !directory.path_exists(&quarantine)? {
        return Ok(false);
    }
    if directory.path_exists(scope_path)? {
        ensure_process_scope_deadline(deadline)?;
        directory.recover_quarantined_file_guarded(scope_path, SCOPE_DELETE_PREFIX, &mut || {
            ensure_process_scope_deadline(deadline)
        })?;
        return Ok(false);
    }
    let file = directory.open_read(&quarantine)?;
    let observed: ProcessScopeRecord = read_bounded_json(&file, &quarantine)?;
    validate_record(&observed)?;
    if observed != *expected {
        return Err(
            "managed-process scope deletion quarantine does not match the expected generation"
                .to_string(),
        );
    }
    ensure_process_scope_deadline(deadline)?;
    directory.remove_visible_file_if_matches_direct_with_guard(&quarantine, &file, || {
        ensure_process_scope_deadline(deadline)
    })?;
    Ok(true)
}

pub(crate) fn read_scope_record(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<ProcessScopeRecord, String> {
    let file = directory.open_read(path)?;
    let record = read_bounded_json(&file, path)?;
    validate_record(&record)?;
    validate_process_atomic_scope_key(path, &record.scope_id)?;
    Ok(record)
}

pub(crate) fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    file: &File,
    path: &Path,
) -> Result<T, String> {
    read_bounded_json_with_limit(file, path, MAX_SCOPE_RECORD_BYTES, "managed-process state")
}

pub(crate) fn read_bounded_json_with_limit<T: for<'de> Deserialize<'de>>(
    file: &File,
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<T, String> {
    let length = file
        .metadata()
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
        .len();
    if length > max_bytes {
        return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    let read_limit = max_bytes.saturating_add(1);
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    while offset < read_limit {
        let remaining = usize::try_from((read_limit - offset).min(buffer.len() as u64))
            .map_err(|_| format!("failed to size bounded read for {}", path.display()))?;
        let read = read_process_state_at(file, &mut buffer[..remaining], offset)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| format!("bounded read offset overflowed for {}", path.display()))?;
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid managed-process state {}: {error}", path.display()))
}

#[cfg(unix)]
pub(crate) fn read_process_state_at(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
pub(crate) fn read_process_state_at(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

pub(crate) fn acquire_file_lock(file: &File, path: &Path) -> Result<(), String> {
    match try_cleanup_lease_lock(file) {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "managed-process cleanup lease is already live: {}",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "failed to acquire managed-process cleanup lease {}: {error}",
            path.display()
        )),
    }
}

#[cfg(not(windows))]
pub(crate) fn try_cleanup_lease_lock(file: &File) -> Result<(), std::fs::TryLockError> {
    file.try_lock()
}

#[cfg(windows)]
pub(crate) fn try_cleanup_lease_lock(file: &File) -> Result<(), std::fs::TryLockError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = CLEANUP_LEASE_LOCK_OFFSET as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (CLEANUP_LEASE_LOCK_OFFSET >> 32) as u32;
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(std::fs::TryLockError::WouldBlock)
    } else {
        Err(std::fs::TryLockError::Error(error))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_identity_still_matches(identity: &ProcessIdentity) -> Result<bool, String> {
    match ProcessIdentity::capture(identity.pid) {
        Ok(current) => Ok(current == *identity),
        Err(capture_error) => match std::fs::metadata(format!("/proc/{}", identity.pid)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Ok(_) => Err(capture_error),
            Err(error) => Err(format!(
                "failed to determine whether process {} still exists: {error}; identity inspection failed: {capture_error}",
                identity.pid
            )),
        },
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("failed to inspect process {pid}: {error}"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| format!("process {pid} has an invalid procfs stat record"))?;
    let fields: Vec<_> = stat[close + 1..].split_whitespace().collect();
    let start_time = fields
        .get(19)
        .ok_or_else(|| format!("process {pid} procfs stat has no start time"))?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("failed to read Linux boot identity: {error}"))?;
    Ok(format!("{}:{}", boot_id.trim(), start_time))
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let expected = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).map_err(|_| "process identifier exceeds i32".to_string())?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected as libc::c_int,
        )
    };
    if read != expected as libc::c_int {
        return Err(format!(
            "failed to inspect process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(format!("process {pid} identity changed while inspected"));
    }
    Ok(format!(
        "{}:{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(windows)]
pub(crate) fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(format!(
            "failed to open process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut created = MaybeUninit::uninit();
    let mut exited = MaybeUninit::uninit();
    let mut kernel = MaybeUninit::uninit();
    let mut user = MaybeUninit::uninit();
    let result = unsafe {
        GetProcessTimes(
            handle,
            created.as_mut_ptr(),
            exited.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    if result == 0 {
        return Err(format!(
            "failed to query process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let created = unsafe { created.assume_init() };
    Ok(format!(
        "{:08x}{:08x}",
        created.dwHighDateTime, created.dwLowDateTime
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn platform_process_start_marker(_pid: u32) -> Result<String, String> {
    Err("process identity capture is unsupported on this platform".to_string())
}

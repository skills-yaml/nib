//! Split for T043 C02.

use super::*;

pub(crate) async fn git_optional_object_id(
    cwd: &Path,
    revision: &str,
) -> Result<Option<String>, String> {
    let output = git_output(cwd, ["rev-parse", "-q", "--verify", revision]).await?;
    match output.status.code() {
        Some(0) => {
            let object = String::from_utf8(output.stdout)
                .map_err(|error| format!("git {revision} output was not UTF-8: {error}"))?;
            let object = object.trim();
            if valid_git_object_id(object) {
                Ok(Some(object.to_string()))
            } else {
                Err(format!("git {revision} returned an invalid object ID"))
            }
        }
        Some(1) | Some(128) => Ok(None),
        _ => Err(git_failure(&output, &format!("inspect {revision}"))),
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn ensure_owned_merge_has_no_user_changes(
    project_root: &Path,
    active_merge_base: &str,
    branch_commit: &str,
) -> Result<(), String> {
    let common_base = git_stdout(
        project_root,
        ["merge-base", active_merge_base, branch_commit],
    )
    .await?;
    let expected = git_output(
        project_root,
        [
            "diff",
            "--name-only",
            "-z",
            common_base.as_str(),
            branch_commit,
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&expected, "inspect interrupted merge paths")?;
    let expected_paths = expected
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    let status = git_output(
        project_root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&status, "inspect interrupted merge state")?;

    let entries = status.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0;
    while index < entries.len() {
        let entry = entries[index];
        index += 1;
        if entry.is_empty() {
            continue;
        }
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(
                "git returned malformed status while recovering interrupted merge".to_string(),
            );
        }
        let x = entry[0];
        let y = entry[1];
        let path = &entry[3..];
        let second_path = if matches!(x, b'R' | b'C') {
            let original = entries
                .get(index)
                .copied()
                .filter(|path| !path.is_empty())
                .ok_or("git returned malformed rename status while recovering interrupted merge")?;
            index += 1;
            Some(original)
        } else {
            None
        };
        let expected_path = expected_paths
            .iter()
            .any(|candidate| candidate.as_slice() == path)
            || second_path.is_some_and(|original| {
                expected_paths
                    .iter()
                    .any(|candidate| candidate.as_slice() == original)
            });
        let unmerged = matches!(
            (x, y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'A', b'A')
                | (b'U', b'U')
        );
        let merge_owned = expected_path && (unmerged || (x != b' ' && y == b' '));
        if !merge_owned {
            return Err(format!(
                "parent contains changes not proven to belong to the interrupted subagent merge ({x_char}{y_char} {path}); refusing to abort",
                x_char = char::from(x),
                y_char = char::from(y),
                path = String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(())
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn reconcile_pending_merge(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    intent: &PendingMergeIntent,
    evidence: &VerificationEvidence,
) -> Result<Value, String> {
    let premerge_head = match ensure_parent_clean(project_root).await {
        Ok(head) => head,
        Err(error) => {
            return persist_pending_merge_failure(project_root, record, record_file, error);
        }
    };
    let parent_advanced_since_intent = premerge_head != intent.parent_head;
    let already_integrated =
        match git_is_ancestor(project_root, &intent.branch_commit, "HEAD").await {
            Ok(integrated) => integrated,
            Err(error) => {
                return persist_pending_merge_failure(project_root, record, record_file, error);
            }
        };

    let mut merge_stdout = if already_integrated {
        set_active_merge_base(record, None);
        format!(
            "subagent commit {} is already integrated",
            intent.branch_commit
        )
    } else {
        set_active_merge_base(record, Some(&premerge_head));
        record.error = None;
        persist_subagent_record_revision(project_root, record, record_file)?;
        let merge = merge_recorded_commit(project_root, &record.id, &intent.branch_commit).await;
        let merge = match merge {
            Ok(output) => output,
            Err(error) => {
                let restored = restore_after_failed_merge(
                    project_root,
                    &premerge_head,
                    &intent.branch_commit,
                    error,
                )
                .await;
                if restored.clear_active {
                    set_active_merge_base(record, None);
                }
                return persist_pending_merge_failure(
                    project_root,
                    record,
                    record_file,
                    restored.error,
                );
            }
        };
        if !merge.status.success() {
            let restored = restore_after_failed_merge(
                project_root,
                &premerge_head,
                &intent.branch_commit,
                git_failure(&merge, "merge"),
            )
            .await;
            if restored.clear_active {
                set_active_merge_base(record, None);
            }
            return persist_pending_merge_failure(
                project_root,
                record,
                record_file,
                restored.error,
            );
        }
        set_active_merge_base(record, None);
        match git_is_ancestor(project_root, &intent.branch_commit, "HEAD").await {
            Ok(true) => {}
            Ok(false) => {
                return persist_pending_merge_failure(
                    project_root,
                    record,
                    record_file,
                    "git merge returned success without integrating the recorded subagent commit"
                        .to_string(),
                );
            }
            Err(error) => {
                return persist_pending_merge_failure(project_root, record, record_file, error);
            }
        }
        String::from_utf8_lossy(&merge.stdout).trim().to_string()
    };
    if parent_advanced_since_intent {
        merge_stdout.push_str(&format!(
            "\nparent HEAD advanced from {} to {} after merge intent was persisted",
            intent.parent_head, premerge_head
        ));
    }

    set_merge_stdout(record, &merge_stdout);
    record.error = None;
    record.updated_at = Utc::now();
    if let Err(error) = require_record_branch_oid(record, &intent.branch_commit) {
        return persist_pending_merge_failure(project_root, record, record_file, error);
    }
    if let Err(error) = crate::sandbox::worktree::Worktree::remove_reconciled_async(
        project_root,
        &record.id,
        &intent.branch_commit,
    )
    .await
    {
        return persist_pending_merge_failure(
            project_root,
            record,
            record_file,
            format!("worktree cleanup failed after merge: {error}"),
        );
    }

    record.status = "merged".to_string();
    record.error = None;
    record.updated_at = Utc::now();
    persist_subagent_record_revision(project_root, record, record_file)?;
    Ok(json!({
        "success": true,
        "subagent_id": record.id,
        "status": record.status,
        "verification_command": intent.verification_command,
        "verification_provider": evidence
            .output
            .as_ref()
            .and_then(|output| output.get("provider"))
            .cloned()
            .unwrap_or_else(|| Value::String(evidence.configured_provider.clone())),
        "stdout": merge_stdout,
    }))
}

pub(crate) async fn merge_recorded_commit(
    project_root: &Path,
    _subagent_id: &str,
    branch_commit: &str,
) -> Result<Output, String> {
    #[cfg(debug_assertions)]
    interrupt_recorded_merge_for_test(project_root, _subagent_id, branch_commit).await?;
    git_output(
        project_root,
        ["merge", "--no-edit", "--no-verify", branch_commit],
    )
    .await
}

#[cfg(debug_assertions)]
pub(crate) async fn interrupt_recorded_merge_for_test(
    project_root: &Path,
    subagent_id: &str,
    branch_commit: &str,
) -> Result<(), String> {
    let key = (project_root.to_path_buf(), subagent_id.to_string());
    let reached = MERGE_INTERRUPTION_TEST_BARRIERS
        .lock()
        .map_err(|_| "merge interruption test barrier registry is poisoned".to_string())?
        .remove(&key);
    let Some(reached) = reached else {
        return Ok(());
    };

    let setup = async {
        let merge = git_output(
            project_root,
            [
                "merge",
                "--no-commit",
                "--no-edit",
                "--no-verify",
                branch_commit,
            ],
        )
        .await?;
        require_git_success(&merge, "establish interrupted merge test fixture")?;
        let merge_head = git_optional_object_id(project_root, "MERGE_HEAD").await?;
        if merge_head.as_deref() != Some(branch_commit) {
            return Err(
                "interrupted merge test fixture did not retain the expected MERGE_HEAD".to_string(),
            );
        }
        let git_directory = git_stdout(project_root, ["rev-parse", "--absolute-git-dir"]).await?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(PathBuf::from(git_directory).join("index.lock"))
            .map_err(|error| {
                format!("failed to create interrupted merge test index lock: {error}")
            })?;
        Ok(())
    }
    .await;

    match setup {
        Ok(()) => {
            let _ = reached.send(Ok(()));
            std::future::pending::<Result<(), String>>().await
        }
        Err(error) => {
            let _ = reached.send(Err(error.clone()));
            Err(error)
        }
    }
}

pub(crate) struct MergeRestoreOutcome {
    pub(crate) error: String,
    pub(crate) clear_active: bool,
}

pub(crate) async fn restore_after_failed_merge(
    project_root: &Path,
    premerge_head: &str,
    branch_commit: &str,
    merge_error: String,
) -> MergeRestoreOutcome {
    let (error, clear_active) = match recover_owned_merge_state(
        project_root,
        premerge_head,
        branch_commit,
    )
    .await
    {
        Ok(OwnedMergeRecovery::Aborted) => (
            format!("{merge_error}; owned parent merge was aborted and restored; retry is allowed"),
            true,
        ),
        Ok(OwnedMergeRecovery::NoMerge) => (
            format!(
                "{merge_error}; no merge state remained and the parent is clean at the pre-merge HEAD; retry is allowed"
            ),
            true,
        ),
        Ok(OwnedMergeRecovery::Integrated) => (
            format!(
                "{merge_error}; the recorded subagent commit is already integrated; retry will reconcile cleanup"
            ),
            true,
        ),
        Err(recovery_error) => (
            format!(
                "{merge_error}; parent recovery failed closed without aborting ambiguous state: {recovery_error}"
            ),
            false,
        ),
    };
    MergeRestoreOutcome {
        error,
        clear_active,
    }
}

pub(crate) fn set_merge_stdout(record: &mut SubagentRecord, merge_stdout: &str) {
    if let Some(result) = record.result.as_mut().and_then(Value::as_object_mut) {
        result.insert(
            "merge_stdout".to_string(),
            Value::String(merge_stdout.to_string()),
        );
    }
}

pub(crate) async fn git_is_ancestor(
    cwd: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, String> {
    let output = git_output(cwd, ["merge-base", "--is-ancestor", ancestor, descendant]).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&output, "merge-base --is-ancestor")),
    }
}

pub(crate) fn git_is_ancestor_sync(
    cwd: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, String> {
    let output = crate::sandbox::worktree::run_git_bounded_sync(
        cwd,
        ["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&output, "merge-base --is-ancestor")),
    }
}

pub(crate) fn persist_pending_merge_failure(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    error: String,
) -> Result<Value, String> {
    record.status = MERGE_PENDING_STATUS.to_string();
    record.error = Some(error.clone());
    record.updated_at = Utc::now();
    match persist_subagent_record_revision(project_root, record, record_file) {
        Ok(()) => Err(error),
        Err(persist_error) => Err(format!(
            "{error}; failed to persist pending merge evidence: {persist_error}"
        )),
    }
}

pub(crate) fn persist_merge_failure(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    error: String,
) -> Result<Value, String> {
    record.status = MERGE_FAILED_STATUS.to_string();
    record.error = Some(error.clone());
    record.updated_at = Utc::now();
    persist_subagent_record_revision(project_root, record, record_file)?;
    Err(error)
}

pub(crate) fn records_dir(project_root: &Path) -> PathBuf {
    project_root.join(".nib").join("subagents")
}

pub(crate) fn ensure_records_directory(project_root: &Path) -> Result<PathBuf, String> {
    ensure_records_directory_until(project_root, None)
}

pub(crate) fn ensure_records_directory_until(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<PathBuf, String> {
    ensure_records_directory_until_with_phase_hook(
        project_root,
        deadline,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |_| Ok(()),
    )
}

pub(crate) fn ensure_records_directory_capability_until(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let started = Instant::now();
    let effective_deadline = deadline.unwrap_or_else(|| {
        started
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or(started)
    });
    let directory = open_or_create_records_directory_capability_with_setup_hook(
        project_root,
        Some(effective_deadline),
        |_| Ok(()),
    )?;
    migrate_legacy_record_locks(project_root, directory.path(), Some(effective_deadline))?;
    directory.verify_visible()?;
    Ok(directory)
}

pub(crate) fn ensure_records_directory_until_with_phase_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    default_timeout: Duration,
    after_open: impl FnOnce(Instant) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    let effective_deadline =
        deadline.unwrap_or_else(|| started.checked_add(default_timeout).unwrap_or(started));
    ensure_subagent_reconciliation_deadline(Some(effective_deadline))?;
    let directory = open_or_create_records_directory(project_root, Some(effective_deadline))?;
    after_open(effective_deadline)?;
    ensure_subagent_reconciliation_deadline(Some(effective_deadline))?;
    migrate_legacy_record_locks(project_root, &directory, Some(effective_deadline))?;
    Ok(directory)
}

pub(crate) fn open_or_create_records_directory(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<PathBuf, String> {
    open_or_create_records_directory_with_setup_hook(project_root, deadline, |_| Ok(()))
}

pub(crate) fn open_or_create_records_directory_with_setup_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    before_setup_mutation: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    open_or_create_records_directory_capability_with_setup_hook(
        project_root,
        deadline,
        before_setup_mutation,
    )
    .map(|directory| directory.path().to_path_buf())
}

pub(crate) fn open_or_create_records_directory_capability_with_setup_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    mut before_setup_mutation: impl FnMut(&Path) -> Result<(), String>,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let started = Instant::now();
    let setup_deadline = deadline.unwrap_or_else(|| {
        started
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or(started)
    });
    ensure_subagent_reconciliation_deadline(Some(setup_deadline))?;
    let nib = project_root.join(".nib");
    let project_directory = crate::daemons::state::StableDirectory::open(project_root)?;
    let mut setup_guard = || ensure_subagent_reconciliation_deadline(Some(setup_deadline));
    let nib_directory = project_directory.open_or_create_descendant_directory_with_guard(
        &nib,
        &mut setup_guard,
        &mut before_setup_mutation,
    )?;
    setup_guard()?;
    drop(nib_directory);
    let directory = records_dir(project_root);
    let lock_path = nib.join(".subagent-legacy-lock-migration.lock");
    let initialize = |nib_directory: &crate::daemons::state::StableDirectory,
                      lock_deadline: Instant| {
        ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
        match nib_directory.entry_kind(&directory)? {
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                nib_directory.open_owned_child(&directory)
            }
            Some(crate::daemons::state::StableEntryKind::File) => Err(format!(
                "subagent records path must be a local project directory: {}",
                directory.display()
            )),
            None => {
                let staging = nib.join(NATIVE_RECORDS_STAGING_DIRECTORY);
                let records_directory = match nib_directory.entry_kind(&staging)? {
                    Some(crate::daemons::state::StableEntryKind::Directory) => {
                        open_valid_native_records_staging(nib_directory, &staging)?
                    }
                    Some(crate::daemons::state::StableEntryKind::File) => {
                        return Err(format!(
                            "native subagent namespace staging is unsafe and was preserved for inspection; verify it and remove only this exact entry before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}",
                            staging.display()
                        ));
                    }
                    None => create_native_records_staging(nib_directory, &staging, lock_deadline)?,
                };
                ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
                if nib_directory.entry_kind(&directory)?.is_some() {
                    return Err(format!(
                        "subagent records namespace appeared during native-origin publication and was preserved: {}",
                        directory.display()
                    ));
                }
                nib_directory.rename_child_directory_until(
                    &staging,
                    &records_directory,
                    &directory,
                    lock_deadline,
                )?;
                ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
                nib_directory.open_owned_child(&directory)
            }
        }
    };
    let records = with_delegation_lock_in_deadline_with_setup_hook(
        &lock_path,
        &nib,
        setup_deadline,
        deadline.is_none().then_some(SUBAGENT_RECORD_LOCK_TIMEOUT),
        initialize,
        &mut before_setup_mutation,
    )?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    validate_records_directory(project_root, &directory, &metadata)?;
    records.verify_visible()?;
    Ok(records)
}

pub(crate) fn create_native_records_staging(
    nib_directory: &crate::daemons::state::StableDirectory,
    staging: &Path,
    deadline: Instant,
) -> Result<crate::daemons::state::StableDirectory, String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let staged = nib_directory.create_owned_child_directory_until(staging, deadline)?;
    let now = Utc::now();
    let receipt = LegacyRecordLockMigrationReceipt {
        version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
        epoch_id: uuid::Uuid::new_v4().to_string(),
        records_identity: records_directory_identity(&staged)?,
        phase: LegacyRecordLockMigrationPhase::Completed,
        attested_at: now,
        completed_at: Some(now),
        artifacts: Vec::new(),
    };
    save_legacy_record_lock_migration_receipt(&staged, &receipt, Some(deadline))?;
    Ok(staged)
}

pub(crate) fn open_valid_native_records_staging(
    nib_directory: &crate::daemons::state::StableDirectory,
    staging: &Path,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let staged = nib_directory.open_owned_child(staging)?;
    let mut saw_receipt = false;
    staged
        .for_each_entry_bounded(2, 1024, |name| {
            if name != std::ffi::OsStr::new(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT) || saw_receipt {
                return Err(format!(
                    "native-origin staging contains an unowned or ambiguous entry: {}",
                    staging.join(name).display()
                ));
            }
            saw_receipt = true;
            Ok(())
        })
        .map_err(|error| {
            format!(
                "incomplete or ambiguous native subagent namespace staging was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}: {error}",
                staging.display()
            )
        })?;
    if !saw_receipt {
        return Err(format!(
            "incomplete native subagent namespace staging has no exact creation receipt and was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}",
            staging.display(),
        ));
    }
    let receipt = load_legacy_record_lock_migration_receipt(&staged)
        .and_then(|receipt| receipt.ok_or_else(|| "native-origin receipt is absent".to_string()))
        .and_then(|receipt| {
            validate_legacy_record_lock_migration_receipt(&staged, &receipt)?;
            if receipt.phase != LegacyRecordLockMigrationPhase::Completed
                || !receipt.artifacts.is_empty()
            {
                return Err("native-origin receipt is not complete".to_string());
            }
            Ok(receipt)
        });
    receipt.map(|_| staged).map_err(|error| {
        format!(
            "incomplete or ambiguous native subagent namespace staging was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}: {error}",
            staging.display()
        )
    })
}

pub(crate) fn validate_records_directory(
    project_root: &Path,
    directory: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    crate::fs_security::verify_directory_without_symlinks(directory)
        .map_err(|error| format!("subagent records path is unsafe: {error}"))?;
    let canonical = directory
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent records: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(metadata)
        || !metadata.is_dir()
        || !within_project
    {
        return Err(format!(
            "subagent records path must be a local project directory: {}",
            directory.display()
        ));
    }
    Ok(())
}

pub(crate) fn record_lock_path(project_root: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    let directory = project_root.join(".nib");
    crate::fs_security::ensure_directory_without_symlinks(&directory)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    let canonical = directory
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent record locks: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !within_project {
        return Err(format!(
            "subagent record lock path must be a local project directory: {}",
            directory.display()
        ));
    }
    Ok(directory.join(format!(
        ".subagent-record-stripe-{:02}.lock",
        subagent_record_lock_stripe(id)
    )))
}

pub(crate) fn record_lock_path_for_stripe(
    records: &crate::daemons::state::StableDirectory,
    stripe: usize,
) -> Result<PathBuf, String> {
    if stripe >= SUBAGENT_RECORD_LOCK_STRIPES {
        return Err("subagent record lock stripe is out of range".to_string());
    }
    let nib = records.path().parent().ok_or_else(|| {
        format!(
            "subagent records directory has no state parent: {}",
            records.path().display()
        )
    })?;
    Ok(nib.join(format!(".subagent-record-stripe-{stripe:02}.lock")))
}

pub(crate) fn acquire_spawn_preparation_authority_until(
    records: &crate::daemons::state::StableDirectory,
    id: &str,
    deadline: Instant,
) -> Result<std::sync::Arc<SpawnPreparationAuthority>, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    records.verify_visible()?;
    let migration_fence = acquire_spawn_preparation_fence_until(records, deadline)?;
    let lock_path = record_lock_path_for_stripe(records, subagent_record_lock_stripe(id))?;
    let record_stripe =
        crate::daemons::state::acquire_file_lock_in_until_bound(&lock_path, records, deadline)?;
    let authority = std::sync::Arc::new(SpawnPreparationAuthority {
        records: records.try_clone()?,
        migration_fence,
        record_stripe,
        operation_deadline: deadline,
    });
    authority.verify_until(deadline)?;
    Ok(authority)
}

pub(crate) fn acquire_spawn_preparation_fence_until(
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<crate::daemons::state::HeldFileLock, String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let nib = records.path().parent().ok_or_else(|| {
        format!(
            "subagent records directory has no state parent: {}",
            records.path().display()
        )
    })?;
    let path = nib.join(".subagent-legacy-lock-migration.lock");
    let lock = crate::daemons::state::acquire_file_lock_in_until_bound(&path, records, deadline)?;
    records.verify_visible()?;
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    Ok(lock)
}

#[cfg(test)]
pub(crate) fn legacy_record_lock_path(records: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    Ok(records.join(".locks").join(format!("{id}.lock")))
}

pub(crate) fn subagent_record_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash % SUBAGENT_RECORD_LOCK_STRIPES as u64) as usize
}

pub(crate) fn migrate_legacy_record_locks(
    project_root: &Path,
    records: &Path,
    deadline: Option<Instant>,
) -> Result<(), String> {
    migrate_legacy_record_locks_with_scan_hook(project_root, records, deadline, |_| Ok(()))
}

//! Split for T043 C02.

use super::*;

pub fn confirm_no_legacy_subagent_processes(project_root: &Path) -> Result<usize, String> {
    confirm_no_legacy_subagent_processes_with_scan_hook(project_root, |_| Ok(()))
}

pub(crate) fn confirm_no_legacy_subagent_processes_with_scan_hook(
    project_root: &Path,
    mut after_scan: impl FnMut(usize) -> Result<(), String>,
) -> Result<usize, String> {
    let project_root = canonical_project_root(project_root)?;
    let deadline = Instant::now() + SUBAGENT_RECORD_LOCK_TIMEOUT;
    let records = open_or_create_records_directory(&project_root, Some(deadline))?;
    let lock_path = project_root
        .join(".nib")
        .join(".subagent-legacy-lock-migration.lock");
    with_bounded_delegation_lock_in_until(
        &lock_path,
        &records,
        deadline,
        |records_directory, deadline| {
            let scan = scan_legacy_record_lock_namespaces(records_directory, Some(deadline))?;
            let existing = load_legacy_record_lock_migration_receipt(records_directory)?;
            let mut receipt = match existing {
                Some(receipt)
                    if receipt.phase == LegacyRecordLockMigrationPhase::Pending
                        && validate_legacy_record_lock_migration_receipt(
                            records_directory,
                            &receipt,
                        )
                        .is_ok()
                        && validate_legacy_record_lock_migration_artifacts(
                            records_directory,
                            &receipt,
                            &scan,
                        )
                        .is_ok() =>
                {
                    receipt
                }
                _ => LegacyRecordLockMigrationReceipt {
                    version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
                    epoch_id: uuid::Uuid::new_v4().to_string(),
                    records_identity: records_directory_identity(records_directory)?,
                    phase: LegacyRecordLockMigrationPhase::Pending,
                    attested_at: Utc::now(),
                    completed_at: None,
                    artifacts: scan.artifacts.clone(),
                },
            };
            save_legacy_record_lock_migration_receipt(records_directory, &receipt, Some(deadline))?;
            let artifact_count = receipt.artifacts.len();
            if let Err(error) = migrate_legacy_record_locks_locked_with_scan_hook(
                &project_root,
                records_directory,
                Some(deadline),
                &receipt,
                &mut after_scan,
            ) {
                receipt.phase = LegacyRecordLockMigrationPhase::Rejected;
                let rejection = save_legacy_record_lock_migration_receipt(
                    records_directory,
                    &receipt,
                    Some(deadline),
                )
                .err();
                return Err(match rejection {
                    Some(rejection) => format!(
                        "offline legacy-lock migration failed: {error}; its receipt could not be rejected safely: {rejection}"
                    ),
                    None => format!(
                        "offline legacy-lock migration failed and requires a fresh operator confirmation: {error}"
                    ),
                });
            }
            receipt.phase = LegacyRecordLockMigrationPhase::Completed;
            receipt.completed_at = Some(Utc::now());
            receipt.artifacts.clear();
            save_legacy_record_lock_migration_receipt(records_directory, &receipt, Some(deadline))?;
            Ok(artifact_count)
        },
    )
}

pub(crate) fn migrate_legacy_record_locks_with_scan_hook(
    project_root: &Path,
    records: &Path,
    deadline: Option<Instant>,
    mut after_scan: impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let lock_path = project_root
        .join(".nib")
        .join(".subagent-legacy-lock-migration.lock");
    match deadline {
        Some(deadline) => with_bounded_delegation_lock_in_until(
            &lock_path,
            records,
            deadline,
            |records_directory, deadline| {
                reconcile_legacy_record_lock_migration_locked(
                    project_root,
                    records_directory,
                    Some(deadline),
                    &mut after_scan,
                )
            },
        ),
        None => with_bounded_delegation_lock_in(
            &lock_path,
            records,
            SUBAGENT_RECORD_LOCK_TIMEOUT,
            |records_directory, deadline| {
                reconcile_legacy_record_lock_migration_locked(
                    project_root,
                    records_directory,
                    Some(deadline),
                    &mut after_scan,
                )
            },
        ),
    }
}

pub(crate) struct LegacyRecordLockScan {
    pub(crate) records_directory: crate::daemons::state::StableDirectory,
    pub(crate) legacy_directory: Option<crate::daemons::state::StableDirectory>,
    pub(crate) ids: Vec<String>,
    pub(crate) retained_quarantines: Vec<PathBuf>,
    pub(crate) artifacts: Vec<LegacyRecordLockMigrationArtifact>,
}

impl LegacyRecordLockScan {
    pub(crate) fn is_clean(&self) -> bool {
        self.ids.is_empty() && self.retained_quarantines.is_empty()
    }

    pub(crate) fn verify_namespace_identity(&self) -> Result<(), String> {
        self.records_directory.verify_visible()?;
        if let Some(legacy_directory) = &self.legacy_directory {
            legacy_directory.verify_visible()?;
        }
        Ok(())
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn scan_legacy_record_lock_namespaces(
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
) -> Result<LegacyRecordLockScan, String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    records_directory.verify_visible()?;
    let records = records_directory.path();
    let legacy_locks = records.join(".locks");
    let legacy_directory = match records_directory.entry_kind(&legacy_locks)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            Some(records_directory.open_child(&legacy_locks)?)
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err(format!(
                "legacy subagent lock path is unsafe: {}",
                legacy_locks.display()
            ));
        }
        None => None,
    };
    let mut ids = std::collections::HashSet::new();
    let mut retained_quarantines = Vec::new();
    let mut artifacts = Vec::new();
    if let Some(legacy_directory) = &legacy_directory {
        legacy_directory.for_each_entry_bounded(
            MAX_LEGACY_RECORD_LOCK_ENTRIES,
            MAX_LEGACY_RECORD_LOCK_NAME_BYTES,
            |name| {
                ensure_subagent_reconciliation_deadline(deadline)?;
                if exact_deletion_quarantine_name(&name, ".nib-legacy-lock-delete-") {
                    let path = legacy_locks.join(&name);
                    retained_quarantines.push(path.clone());
                    artifacts.push(snapshot_legacy_migration_artifact(
                        records_directory,
                        legacy_directory,
                        &path,
                        None,
                    )?);
                    return Ok(());
                }
                let Some(name) = name.to_str() else {
                    return Err("legacy lock namespace contains a non-UTF-8 filename".to_string());
                };
                if name.starts_with(".nib-legacy-lock-delete-") {
                    return Err(
                        "legacy lock namespace contains an invalid deletion quarantine".to_string(),
                    );
                }
                let Some(id) = name
                    .strip_suffix(".lock")
                    .filter(|id| is_valid_subagent_id(id))
                else {
                    return Ok(());
                };
                ids.insert(id.to_string());
                let path = legacy_locks.join(name);
                let quarantine_path = legacy_directory.deterministic_artifact_path(
                    &path,
                    ".nib-legacy-lock-delete-",
                    ".quarantine",
                )?;
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    legacy_directory,
                    &path,
                    Some(quarantine_path),
                )?);
                Ok(())
            },
        )?;
    }
    let anchor_prefix = ".nib-lock-6-.locks-";
    let anchor_suffix = ".lock.anchor";
    records_directory.for_each_entry_bounded(
        MAX_SUBAGENT_DIRECTORY_ENTRIES,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            if exact_deletion_quarantine_name(&name, ".nib-legacy-lock-delete-") {
                let path = records.join(name);
                retained_quarantines.push(path.clone());
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    records_directory,
                    &path,
                    None,
                )?);
                return Ok(());
            }
            let Some(name) = name.to_str() else {
                return Ok(());
            };
            if name.starts_with(".nib-legacy-lock-delete-") {
                return Err(
                    "subagent record namespace contains an invalid legacy-lock deletion quarantine"
                        .to_string(),
                );
            }
            if let Some(id) = name
                .strip_prefix(anchor_prefix)
                .and_then(|name| name.strip_suffix(anchor_suffix))
                .filter(|id| is_valid_subagent_id(id))
            {
                ids.insert(id.to_string());
                let path = records.join(name);
                let quarantine_path = records_directory.deterministic_artifact_path(
                    &path,
                    ".nib-legacy-lock-delete-",
                    ".quarantine",
                )?;
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    records_directory,
                    &path,
                    Some(quarantine_path),
                )?);
            }
            Ok(())
        },
    )?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort();
    retained_quarantines.sort();
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    artifacts.dedup_by(|left, right| left.path == right.path && left.identity == right.identity);
    Ok(LegacyRecordLockScan {
        records_directory: records_directory.try_clone()?,
        legacy_directory,
        ids,
        retained_quarantines,
        artifacts,
    })
}

pub(crate) fn snapshot_legacy_migration_artifact(
    records_directory: &crate::daemons::state::StableDirectory,
    artifact_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    quarantine_path: Option<PathBuf>,
) -> Result<LegacyRecordLockMigrationArtifact, String> {
    let relative = path
        .strip_prefix(records_directory.path())
        .map_err(|_| {
            format!(
                "legacy lock artifact escaped records state: {}",
                path.display()
            )
        })?
        .to_path_buf();
    let quarantine_path = quarantine_path
        .map(|path| {
            path.strip_prefix(records_directory.path())
                .map(Path::to_path_buf)
                .map_err(|_| {
                    format!(
                        "legacy lock quarantine escaped records state: {}",
                        path.display()
                    )
                })
        })
        .transpose()?;
    let file = artifact_directory.open_read_write(path)?;
    let identity = crate::fs_security::file_identity_snapshot(&file)
        .map_err(|error| format!("failed to identify legacy lock {}: {error}", path.display()))?;
    artifact_directory.verify_file_identity(path, &file)?;
    Ok(LegacyRecordLockMigrationArtifact {
        path: relative,
        quarantine_path,
        identity,
    })
}

pub(crate) fn reconcile_legacy_record_lock_migration_locked(
    _project_root: &Path,
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
    after_scan: &mut impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
    drop(scan);
    after_scan(0)?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    records_directory.verify_visible()?;
    let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
    let receipt = load_legacy_record_lock_migration_receipt(records_directory)?;
    let receipt = receipt.ok_or_else(legacy_record_lock_offline_migration_required)?;
    validate_legacy_record_lock_migration_receipt(records_directory, &receipt)?;
    if receipt.phase != LegacyRecordLockMigrationPhase::Completed || !scan.is_clean() {
        return Err(legacy_record_lock_offline_migration_required());
    }
    Ok(())
}

pub(crate) fn legacy_record_lock_offline_migration_required() -> String {
    "legacy per-ID subagent locks require an offline migration; stop and disable every prior nib binary, then run `nib doctor --fix --confirm-no-legacy-processes` from this project before retrying"
        .to_string()
}

pub(crate) fn legacy_record_lock_migration_receipt_path(
    records_directory: &crate::daemons::state::StableDirectory,
) -> PathBuf {
    records_directory
        .path()
        .join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT)
}

pub(crate) fn records_directory_identity(
    records_directory: &crate::daemons::state::StableDirectory,
) -> Result<crate::fs_security::DirectoryIdentity, String> {
    records_directory
        .directory_removal_receipt()
        .map(|receipt| receipt.identity())
}

pub(crate) fn load_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
) -> Result<Option<LegacyRecordLockMigrationReceipt>, String> {
    let path = legacy_record_lock_migration_receipt_path(records_directory);
    if !records_directory.path_exists(&path)? {
        return Ok(None);
    }
    let file = records_directory.open_read(&path)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err(format!(
            "legacy lock migration receipt is invalid or exceeds {} bytes: {}",
            MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES,
            path.display()
        ));
    }
    records_directory.verify_file_identity(&path, &file)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take(MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err(format!(
            "legacy lock migration receipt exceeds {} bytes: {}",
            MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES,
            path.display()
        ));
    }
    records_directory.verify_file_identity(&path, &file)?;
    let receipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid legacy lock migration receipt: {error}"))?;
    Ok(Some(receipt))
}

pub(crate) fn validate_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
) -> Result<(), String> {
    if receipt.version != LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION
        || uuid::Uuid::parse_str(&receipt.epoch_id).is_err()
        || receipt.records_identity != records_directory_identity(records_directory)?
        || receipt.artifacts.len() > MAX_LEGACY_RECORD_LOCK_ENTRIES * 2
        || (receipt.phase == LegacyRecordLockMigrationPhase::Completed
            && (!receipt.artifacts.is_empty() || receipt.completed_at.is_none()))
        || (receipt.phase != LegacyRecordLockMigrationPhase::Completed
            && receipt.completed_at.is_some())
    {
        return Err("legacy lock migration receipt is invalid or belongs to a replaced records directory; run the offline doctor migration again".to_string());
    }
    let mut paths = std::collections::HashSet::new();
    for artifact in &receipt.artifacts {
        validate_legacy_record_lock_receipt_relative_path(&artifact.path)?;
        if let Some(path) = &artifact.quarantine_path {
            validate_legacy_record_lock_receipt_relative_path(path)?;
        }
        if !paths.insert(artifact.path.clone()) {
            return Err("legacy lock migration receipt contains duplicate artifacts".to_string());
        }
    }
    Ok(())
}

pub(crate) fn validate_legacy_record_lock_receipt_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("legacy lock migration receipt contains an unsafe artifact path".to_string());
    }
    Ok(())
}

pub(crate) fn validate_legacy_record_lock_migration_artifacts(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
    scan: &LegacyRecordLockScan,
) -> Result<(), String> {
    validate_legacy_record_lock_migration_receipt(records_directory, receipt)?;
    for current in &scan.artifacts {
        let matches = receipt.artifacts.iter().any(|expected| {
            expected.identity == current.identity
                && (expected.path == current.path
                    || expected.quarantine_path.as_ref() == Some(&current.path))
        });
        if !matches {
            return Err(format!(
                "legacy lock state changed after offline quiescence attestation; artifacts were preserved and a fresh `nib doctor --fix --confirm-no-legacy-processes` run is required: {}",
                current.path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn save_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
    deadline: Option<Instant>,
) -> Result<(), String> {
    validate_legacy_record_lock_migration_receipt(records_directory, receipt)?;
    let path = legacy_record_lock_migration_receipt_path(records_directory);
    let encoded = serde_json::to_vec_pretty(receipt).map_err(|error| error.to_string())?;
    if encoded.len() as u64 > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err("legacy lock migration receipt exceeds its publication bound".to_string());
    }
    for attempt in 0..2 {
        ensure_subagent_reconciliation_deadline(deadline)?;
        let expected_file = if records_directory.path_exists(&path)? {
            Some(records_directory.open_read(&path)?)
        } else {
            None
        };
        let expected = expected_file
            .as_ref()
            .map_or(crate::daemons::state::FileExpectation::Missing, |file| {
                crate::daemons::state::FileExpectation::Present(file)
            });
        let result = records_directory.save_bytes_atomically_expected_with_guard_and_hook(
            &path,
            &encoded,
            ".nib-subagent-legacy-migration-",
            true,
            expected,
            || {
                ensure_subagent_reconciliation_deadline(deadline)?;
                records_directory.verify_visible()
            },
            || ensure_subagent_reconciliation_deadline(deadline),
        );
        match result {
            Ok(()) => return Ok(()),
            Err(_) if attempt == 0 => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded migration receipt save loop always returns")
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn migrate_legacy_record_locks_locked_with_scan_hook(
    project_root: &Path,
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
    receipt: &LegacyRecordLockMigrationReceipt,
    after_scan: &mut impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    let records = records_directory.path();
    let legacy_locks = records.join(".locks");
    for pass in 0..MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES {
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
        drop(scan);
        after_scan(pass)?;
        let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
        validate_legacy_record_lock_migration_artifacts(records_directory, receipt, &scan)?;
        let LegacyRecordLockScan {
            records_directory,
            legacy_directory,
            ids,
            retained_quarantines,
            artifacts: _,
        } = scan;
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        if let Some(legacy_directory) = &legacy_directory {
            legacy_directory.verify_visible()?;
        }
        let initial_legacy_present = legacy_directory.is_some();
        for id in ids {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let name = format!("{id}.lock");
            let legacy_path = legacy_locks.join(&name);
            let anchor_path = crate::daemons::state::daemon_lock_anchor_path(&legacy_path)?;
            with_modern_subagent_record_lock_in(
                &record_lock_path(project_root, &id)?,
                &records_directory,
                deadline,
                |protected_records, deadline| {
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    let protected_legacy = match protected_records.entry_kind(&legacy_locks)? {
                        Some(crate::daemons::state::StableEntryKind::Directory) => {
                            Some(protected_records.open_child(&legacy_locks)?)
                        }
                        Some(crate::daemons::state::StableEntryKind::File) => {
                            return Err(format!(
                                "legacy subagent lock path is unsafe: {}",
                                legacy_locks.display()
                            ));
                        }
                        None => None,
                    };
                    if let Some(protected_legacy) = protected_legacy.as_ref() {
                        crate::daemons::state::cleanup_legacy_lock_pair_with_guard(
                            protected_legacy,
                            &legacy_path,
                            protected_records,
                            &anchor_path,
                            || {
                                ensure_subagent_reconciliation_deadline(deadline)?;
                                protected_legacy.verify_visible()
                            },
                        )
                    } else {
                        crate::daemons::state::cleanup_legacy_lock_pair_optional_with_guard(
                            None,
                            &legacy_path,
                            protected_records,
                            &anchor_path,
                            || {
                                ensure_subagent_reconciliation_deadline(deadline)?;
                                if protected_records.entry_kind(&legacy_locks)?.is_some() {
                                    return Err(format!(
                                "legacy subagent lock namespace appeared during anchor cleanup; artifacts were preserved: {}",
                                legacy_locks.display()
                            ));
                                }
                                Ok(())
                            },
                        )
                    }
                },
            )?;
        }
        for quarantine in retained_quarantines {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let directory = if quarantine.parent() == Some(legacy_locks.as_path()) {
                legacy_directory
                    .as_ref()
                    .ok_or("legacy lock deletion quarantine lost its containing directory")?
            } else {
                &records_directory
            };
            if !directory.path_exists(&quarantine)? {
                continue;
            }
            let file = directory.open_read_write(&quarantine)?;
            match file.try_lock() {
                Ok(()) => directory.remove_visible_file_if_matches_direct_with_guard(
                    &quarantine,
                    &file,
                    || ensure_subagent_reconciliation_deadline(deadline),
                )?,
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "legacy lock deletion quarantine is still owned and was preserved: {}",
                        quarantine.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect legacy lock deletion quarantine {}: {error}",
                        quarantine.display()
                    ));
                }
            }
        }
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        if let Some(legacy_directory) = &legacy_directory {
            legacy_directory.verify_visible()?;
        }
        let final_scan = scan_legacy_record_lock_namespaces(&records_directory, deadline)?;
        validate_legacy_record_lock_migration_artifacts(&records_directory, receipt, &final_scan)?;
        if final_scan.is_clean() && final_scan.legacy_directory.is_some() == initial_legacy_present
        {
            final_scan.verify_namespace_identity()?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            return Ok(());
        }
    }
    Err(format!(
        "legacy subagent lock namespace did not stabilize within {MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES} passes"
    ))
}

pub(crate) fn is_valid_subagent_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

pub(crate) fn read_subagent_record(path: &Path) -> Result<SubagentRecord, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("subagent record has no parent: {}", path.display()))?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    read_subagent_record_in(&directory, path)
}

pub(crate) fn read_subagent_record_in(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<SubagentRecord, String> {
    read_opened_subagent_record_in(directory, path).map(|opened| opened.record)
}

pub(crate) fn read_opened_subagent_record_in(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<OpenedSubagentRecord, String> {
    if !directory.path_exists(path)? {
        return Err(format!("subagent record {} not found", path.display()));
    }
    let file = directory.open_read(path)?;
    let opened_metadata = file.metadata().map_err(|error| error.to_string())?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_SUBAGENT_RECORD_BYTES {
        return Err(format!(
            "subagent record {} exceeds the {MAX_SUBAGENT_RECORD_BYTES}-byte limit or is not a regular file",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
    (&file)
        .take(MAX_SUBAGENT_RECORD_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| error.to_string())?;
    if contents.len() as u64 > MAX_SUBAGENT_RECORD_BYTES {
        return Err(format!(
            "subagent record {} exceeds the {MAX_SUBAGENT_RECORD_BYTES}-byte limit",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    let record = serde_json::from_slice(&contents)
        .map_err(|error| format!("invalid subagent record {}: {error}", path.display()))?;
    Ok(OpenedSubagentRecord { record, file })
}

pub(crate) fn update_subagent_record<T>(
    project_root: &Path,
    id: &str,
    update: impl FnOnce(&mut SubagentRecord) -> Result<T, String>,
) -> Result<T, String> {
    update_subagent_record_until(project_root, id, None, update)
}

pub(crate) fn update_subagent_record_until<T>(
    project_root: &Path,
    id: &str,
    deadline: Option<Instant>,
    update: impl FnOnce(&mut SubagentRecord) -> Result<T, String>,
) -> Result<T, String> {
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, id)?;
    let records_directory = ensure_records_directory_until(&project_root, deadline)?;
    with_subagent_reconciliation_lock_in(
        &project_root,
        id,
        &records_directory,
        deadline,
        |directory, deadline| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let mut opened = read_opened_subagent_record_in(directory, &path)?;
            let result = update(&mut opened.record)?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            write_subagent_record_unlocked_until(
                &project_root,
                directory,
                &path,
                &opened.record,
                crate::daemons::state::FileExpectation::Present(&opened.file),
                deadline,
            )?;
            Ok(result)
        },
    )
}

pub(crate) fn record_path(project_root: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    Ok(records_dir(project_root).join(format!("{id}.json")))
}

pub(crate) fn canonical_project_root(project_root: &Path) -> Result<PathBuf, String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("invalid project root {}: {error}", project_root.display()))?;
    if !root.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

pub(crate) fn validate_record_worktree(
    project_root: &Path,
    record: &SubagentRecord,
) -> Result<(), String> {
    let worktree = record
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("subagent worktree is unavailable: {error}"))?;
    let allowed = project_root
        .join(".nib")
        .join("worktrees")
        .join("subagents")
        .canonicalize()
        .map_err(|error| format!("subagent worktree root is unavailable: {error}"))?;
    if !worktree.starts_with(&allowed) {
        return Err(format!(
            "subagent worktree {} is outside {}",
            worktree.display(),
            allowed.display()
        ));
    }
    Ok(())
}

pub(crate) async fn git_output<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    crate::sandbox::worktree::run_git_bounded(cwd, args).await
}

pub(crate) async fn git_checked<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_output(cwd, args).await?;
    if !output.status.success() {
        return Err(git_failure(&output, "command"));
    }
    Ok(output)
}

pub(crate) async fn git_stdout<I, S>(cwd: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_checked(cwd, args).await?;
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("git output was not UTF-8: {error}"))?;
    let value = value.trim();
    if value.is_empty() {
        Err("git command returned empty output".to_string())
    } else {
        Ok(value.to_string())
    }
}

pub(crate) fn require_git_success(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(output, operation))
    }
}

pub(crate) fn git_failure(output: &Output, operation: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!("git {operation} failed: {detail}")
}

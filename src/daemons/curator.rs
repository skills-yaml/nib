use crate::daemons::task::{DaemonAuditLog, DaemonAuditRecord};
use crate::session::memory::{MemoryStore, MemoryStoreData};
use crate::session::{Session, SessionDeleteOutcome, SessionError, SessionStore};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const MANAGED_SKILL_MARKER: &str = ".nib-managed.json";
const MAX_PINS_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PIN_ENTRIES: usize = 2048;
const MAX_MANAGED_SKILL_MARKER_BYTES: u64 = 64 * 1024;
const MAX_CURATOR_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_CURATOR_IDENTIFIER_BYTES: usize = 160;
const MAX_MEMORY_KEY_BYTES: usize = 256;
const MAX_AGGREGATED_SKILLS: usize = 4_096;
const MAX_AGGREGATED_SKILL_USAGE_RECORDS: usize = 100_000;
const MAX_AGGREGATED_SESSION_BYTES: u64 = 256 * 1024 * 1024;
const MANAGED_SKILL_LOCK: &str = ".managed-skills.lock";
const MANAGED_SKILL_QUARANTINE_PREFIX: &str = ".nib-skill-delete-";
const MANAGED_SKILL_QUARANTINE_SUFFIX: &str = ".quarantine";
const MAX_MANAGED_SKILL_TREE_DEPTH: usize = 32;
const MAX_MANAGED_SKILL_TREE_ENTRIES: usize = 20_000;
const MAX_MANAGED_SKILL_TREE_NAME_BYTES: usize = 2 * 1024 * 1024;

struct ManagedSkillQuarantine {
    id: String,
    path: PathBuf,
    parent: crate::daemons::state::StableDirectory,
    directory: crate::daemons::state::StableDirectory,
}

#[derive(Default)]
struct ManagedSkillTreeBudget {
    entries: usize,
    name_bytes: usize,
}

#[derive(Clone, Copy)]
struct ManagedSkillTreeLimits {
    max_depth: usize,
    max_entries: usize,
    max_name_bytes: usize,
}

const MANAGED_SKILL_TREE_LIMITS: ManagedSkillTreeLimits = ManagedSkillTreeLimits {
    max_depth: MAX_MANAGED_SKILL_TREE_DEPTH,
    max_entries: MAX_MANAGED_SKILL_TREE_ENTRIES,
    max_name_bytes: MAX_MANAGED_SKILL_TREE_NAME_BYTES,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CuratorPolicy {
    pub allow_destructive_cleanup: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CuratorReport {
    pub scanned: usize,
    pub deleted: usize,
    pub pinned: usize,
    pub policy_skipped: usize,
    pub retained: usize,
    pub sessions_deleted: usize,
    pub memory_deleted: usize,
    pub skills_deleted: usize,
    pub errors: Vec<String>,
}

impl CuratorReport {
    fn merge(&mut self, other: Self) {
        self.scanned += other.scanned;
        self.deleted += other.deleted;
        self.pinned += other.pinned;
        self.policy_skipped += other.policy_skipped;
        self.retained += other.retained;
        self.sessions_deleted += other.sessions_deleted;
        self.memory_deleted += other.memory_deleted;
        self.skills_deleted += other.skills_deleted;
        self.errors.extend(other.errors);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryNamespace {
    Environment,
    User,
}

impl MemoryNamespace {
    fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PinFile {
    #[serde(default)]
    sessions: BTreeSet<String>,
    #[serde(default)]
    memory_environment: BTreeSet<String>,
    #[serde(default)]
    memory_user: BTreeSet<String>,
    #[serde(default)]
    skills: BTreeSet<String>,
}

struct OpenedPinFile {
    pins: PinFile,
    file: Option<File>,
}

impl OpenedPinFile {
    fn expectation(&self) -> crate::daemons::state::FileExpectation<'_> {
        self.file.as_ref().map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        )
    }
}

impl PinFile {
    fn memory(&self, namespace: MemoryNamespace) -> &BTreeSet<String> {
        match namespace {
            MemoryNamespace::Environment => &self.memory_environment,
            MemoryNamespace::User => &self.memory_user,
        }
    }

    fn memory_mut(&mut self, namespace: MemoryNamespace) -> &mut BTreeSet<String> {
        match namespace {
            MemoryNamespace::Environment => &mut self.memory_environment,
            MemoryNamespace::User => &mut self.memory_user,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedSkillMetadata {
    pub id: String,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillUsageAggregate {
    pub skill_name: String,
    pub usage_count: usize,
    pub session_count: usize,
    pub latest_used_at: Option<DateTime<Utc>>,
    pub has_undated_usage: bool,
}

pub struct Curator {
    sessions_dir: PathBuf,
    memory_path: PathBuf,
    managed_skills_dir: PathBuf,
    retention_days: i64,
    policy: CuratorPolicy,
    pins_path: PathBuf,
    audit_log: DaemonAuditLog,
}

impl Curator {
    /// Creates a fail-closed curator for the legacy project state layout.
    pub fn new(project_root: &Path, retention_days: i64) -> Self {
        Self::new_with_policy(project_root, retention_days, CuratorPolicy::default())
    }

    pub fn new_with_policy(
        project_root: &Path,
        retention_days: i64,
        policy: CuratorPolicy,
    ) -> Self {
        let state_dir = project_root.join(".nib");
        Self::at_profile_paths(
            state_dir.join("sessions"),
            state_dir.join("memory.json"),
            state_dir.join("managed-skills"),
            state_dir.join("daemons"),
            retention_days,
            policy,
        )
    }

    /// Backward-compatible constructor. The other profile-owned paths are derived
    /// from the daemon directory's parent, never from configured/source skill roots.
    pub fn at_paths(
        sessions_dir: PathBuf,
        daemon_dir: PathBuf,
        retention_days: i64,
        policy: CuratorPolicy,
    ) -> Self {
        let state_dir = daemon_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| {
                sessions_dir
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf()
            });
        Self::at_profile_paths(
            sessions_dir,
            state_dir.join("memory.json"),
            state_dir.join("managed-skills"),
            daemon_dir,
            retention_days,
            policy,
        )
    }

    pub fn at_profile_paths(
        sessions_dir: PathBuf,
        memory_path: PathBuf,
        managed_skills_dir: PathBuf,
        daemon_dir: PathBuf,
        retention_days: i64,
        policy: CuratorPolicy,
    ) -> Self {
        Self {
            sessions_dir,
            memory_path,
            managed_skills_dir,
            retention_days,
            policy,
            pins_path: daemon_dir.join("pins.json"),
            audit_log: DaemonAuditLog::at_path(daemon_dir.join("audit.jsonl")),
        }
    }

    pub fn managed_skills_dir(&self) -> &Path {
        &self.managed_skills_dir
    }

    /// Rebuilds a bounded profile-wide view from authoritative session records.
    /// No secondary usage store is written, so a failed or partial session update
    /// cannot silently diverge from the curator's retention input.
    pub fn aggregate_skill_usage(&self) -> Result<Vec<SkillUsageAggregate>, String> {
        ensure_local_directory(&self.sessions_dir)?;
        let store = SessionStore::at_dir(self.sessions_dir.clone());
        store
            .with_skill_usage_lock(|| {
                self.aggregate_skill_usage_unlocked(&store)
                    .map(|usage| usage.into_values().collect())
                    .map_err(SessionError::InvalidMutation)
            })
            .map_err(|error| error.to_string())
    }

    pub fn cleanup(&self) -> Result<CuratorReport, String> {
        self.cleanup_at(Utc::now())
    }

    pub fn cleanup_at(&self, now: DateTime<Utc>) -> Result<CuratorReport, String> {
        self.validate_retention()?;
        let mut report = self.cleanup_old_sessions_at(now)?;
        report.merge(self.cleanup_old_memory_at(now)?);
        report.merge(self.cleanup_old_skills_at(now)?);
        Ok(report)
    }

    pub fn cleanup_old_sessions(&self) -> Result<usize, String> {
        Ok(self.cleanup_old_sessions_at(Utc::now())?.sessions_deleted)
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub fn cleanup_old_sessions_at(&self, now: DateTime<Utc>) -> Result<CuratorReport, String> {
        let cutoff = self.retention_cutoff(now)?;
        ensure_local_directory(&self.sessions_dir)?;
        let pins = self.load_pins()?;
        let store = SessionStore::at_dir(self.sessions_dir.clone());
        let mut report = CuratorReport::default();
        let session_ids = store.list_result().map_err(|error| error.to_string())?;

        for session_id in session_ids {
            report.scanned += 1;
            let path = self.sessions_dir.join(format!("{session_id}.json"));
            let (session, metadata) = match store.load_result_with_metadata(&session_id) {
                Ok(Some(opened)) => opened,
                Ok(None) => {
                    report.retained += 1;
                    continue;
                }
                Err(error) => {
                    let error = format!("failed to load {}: {error}", path.display());
                    report.errors.push(error.clone());
                    self.audit(
                        "inspect_session",
                        Some(&session_id),
                        "error",
                        false,
                        Some(error),
                    )?;
                    continue;
                }
            };
            if session.id != session_id {
                let error = format!(
                    "session id {} does not match file name {session_id}",
                    session.id
                );
                report.errors.push(error.clone());
                self.audit(
                    "inspect_session",
                    Some(&session_id),
                    "error",
                    false,
                    Some(error),
                )?;
                continue;
            }
            let file_activity = metadata.modified().ok().map(DateTime::<Utc>::from);
            if !is_session_old(&session, file_activity, cutoff) {
                report.retained += 1;
                continue;
            }
            if pins.sessions.contains(&session_id) {
                report.pinned += 1;
                self.audit(
                    "cleanup_session",
                    Some(&session_id),
                    "skipped_pinned",
                    false,
                    None,
                )?;
                continue;
            }
            if !self.authorize_cleanup(&mut report, "cleanup_session", Some(&session_id))? {
                continue;
            }

            match self.delete_session_if_old(&store, &session_id, cutoff) {
                Ok(CuratorSessionDelete::Deleted) => {
                    report.deleted += 1;
                    report.sessions_deleted += 1;
                    self.audit("cleanup_session", Some(&session_id), "deleted", true, None)?;
                }
                Ok(CuratorSessionDelete::Pinned) => {
                    report.pinned += 1;
                    self.audit(
                        "cleanup_session",
                        Some(&session_id),
                        "skipped_pinned_recheck",
                        false,
                        None,
                    )?;
                }
                Ok(CuratorSessionDelete::Retained) => {
                    report.retained += 1;
                    self.audit(
                        "cleanup_session",
                        Some(&session_id),
                        "skipped_active_recheck",
                        false,
                        None,
                    )?;
                }
                Ok(CuratorSessionDelete::Missing) => {
                    self.audit(
                        "cleanup_session",
                        Some(&session_id),
                        "skipped_missing_recheck",
                        false,
                        None,
                    )?;
                }
                Err(detail) => {
                    report.errors.push(format!("{session_id}: {detail}"));
                    self.audit(
                        "cleanup_session",
                        Some(&session_id),
                        "error",
                        true,
                        Some(detail),
                    )?;
                }
            }
        }

        Ok(report)
    }

    pub fn cleanup_old_memory_at(&self, now: DateTime<Utc>) -> Result<CuratorReport, String> {
        self.cleanup_old_memory_at_with_hook(now, || {})
    }

    fn cleanup_old_memory_at_with_hook(
        &self,
        now: DateTime<Utc>,
        before_pins_lock: impl FnOnce(),
    ) -> Result<CuratorReport, String> {
        let cutoff = self.retention_cutoff(now)?;
        reject_symlink_file(&self.memory_path)?;
        let store = MemoryStore::at_path(self.memory_path.clone());
        let mut report = CuratorReport::default();
        let mut removals = Vec::new();
        before_pins_lock();
        let update = self.with_pins_lock(|directory| {
            let pins = self.load_pins_opened_unlocked(directory)?;
            store.update_with_commit_check(
                |data| {
                    self.collect_memory_removals(
                        MemoryNamespace::Environment,
                        data,
                        cutoff,
                        &pins.pins,
                        &mut report,
                        &mut removals,
                    )?;
                    self.collect_memory_removals(
                        MemoryNamespace::User,
                        data,
                        cutoff,
                        &pins.pins,
                        &mut report,
                        &mut removals,
                    )?;

                    for (namespace, key) in &removals {
                        match namespace {
                            MemoryNamespace::Environment => {
                                data.environment.remove(key);
                                data.metadata.environment.remove(key);
                            }
                            MemoryNamespace::User => {
                                data.user.remove(key);
                                data.metadata.user.remove(key);
                            }
                        }
                    }
                    Ok(())
                },
                || directory.verify_file_expectation(&self.pins_path, pins.expectation()),
            )
        });
        match update {
            Ok(()) if removals.is_empty() => {}
            Ok(()) => {
                for (namespace, key) in removals {
                    let target = memory_target(namespace, &key);
                    report.deleted += 1;
                    report.memory_deleted += 1;
                    self.audit("cleanup_memory", Some(&target), "deleted", true, None)?;
                }
            }
            Err(error) => {
                report.errors.push(error.clone());
                if removals.is_empty() {
                    self.audit("inspect_memory", None, "error", false, Some(error))?;
                } else {
                    for (namespace, key) in removals {
                        let target = memory_target(namespace, &key);
                        self.audit(
                            "cleanup_memory",
                            Some(&target),
                            "error",
                            true,
                            Some(error.clone()),
                        )?;
                    }
                }
            }
        }
        Ok(report)
    }

    pub fn cleanup_old_skills_at(&self, now: DateTime<Utc>) -> Result<CuratorReport, String> {
        self.cleanup_old_skills_at_with_hooks(now, || {}, || {}, |_| {}, || {})
    }

    #[cfg(test)]
    fn cleanup_old_skills_at_with_hook(
        &self,
        now: DateTime<Utc>,
        before_pins_lock: impl FnOnce(),
    ) -> Result<CuratorReport, String> {
        self.cleanup_old_skills_at_with_hooks(now, before_pins_lock, || {}, |_| {}, || {})
    }

    fn cleanup_old_skills_at_with_hooks(
        &self,
        now: DateTime<Utc>,
        before_pins_lock: impl FnOnce(),
        before_skill_usage_lock: impl FnOnce(),
        mut before_quarantine: impl FnMut(&Path),
        after_quarantine: impl FnOnce(),
    ) -> Result<CuratorReport, String> {
        let cutoff = self.retention_cutoff(now)?;
        ensure_local_directory(&self.sessions_dir)?;
        ensure_local_directory(&self.managed_skills_dir)?;
        let store = SessionStore::at_dir(self.sessions_dir.clone());
        before_pins_lock();
        let (mut report, quarantines) = self.with_pins_lock(|directory| {
            let pins = self.load_pins_opened_unlocked(directory)?;
            // All curator paths that need both locks acquire pins before skill usage.
            before_skill_usage_lock();
            store
                .with_skill_usage_lock(|| {
                    (|| -> Result<(CuratorReport, Vec<ManagedSkillQuarantine>), String> {
                        let skill_usage = self.aggregate_skill_usage_unlocked(&store)?;
                        self.with_managed_skills_lock(|managed_directory| {
                            self.collect_managed_skill_quarantines(
                                managed_directory,
                                directory,
                                &pins,
                                &skill_usage,
                                cutoff,
                                &mut before_quarantine,
                            )
                        })
                    })()
                    .map_err(SessionError::InvalidMutation)
                })
                .map_err(|error| error.to_string())
        })?;

        after_quarantine();
        for quarantine in quarantines {
            let id = quarantine.id.clone();
            match delete_managed_skill_quarantine(quarantine) {
                Ok(()) => {
                    report.deleted += 1;
                    report.skills_deleted += 1;
                    self.audit("cleanup_skill", Some(&id), "deleted", true, None)?;
                }
                Err(detail) => {
                    report.errors.push(format!("managed skill {id}: {detail}"));
                    self.audit("cleanup_skill", Some(&id), "error", true, Some(detail))?;
                }
            }
        }
        Ok(report)
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    fn collect_managed_skill_quarantines(
        &self,
        managed_directory: &crate::daemons::state::StableDirectory,
        pins_directory: &crate::daemons::state::StableDirectory,
        pins: &OpenedPinFile,
        skill_usage: &BTreeMap<String, SkillUsageAggregate>,
        cutoff: DateTime<Utc>,
        before_quarantine: &mut impl FnMut(&Path),
    ) -> Result<(CuratorReport, Vec<ManagedSkillQuarantine>), String> {
        let mut report = CuratorReport::default();
        let mut names = Vec::new();
        managed_directory.for_each_entry_bounded(
            MAX_CURATOR_DIRECTORY_ENTRIES,
            MAX_CURATOR_DIRECTORY_ENTRIES * MAX_CURATOR_IDENTIFIER_BYTES,
            |name| {
                names.push(name);
                Ok(())
            },
        )?;
        names.sort();
        let mut quarantines = Vec::new();

        for name in names {
            let path = managed_directory.path().join(&name);
            if path
                .file_name()
                .is_some_and(|value| value == MANAGED_SKILL_LOCK)
            {
                continue;
            }
            let Some(directory_id) = name.to_str().map(ToOwned::to_owned) else {
                report.errors.push(format!(
                    "managed skill path is not valid UTF-8: {}",
                    path.display()
                ));
                continue;
            };
            if managed_skill_quarantine_id(&directory_id).is_some() {
                continue;
            }
            match managed_directory.entry_kind(&path) {
                Ok(Some(crate::daemons::state::StableEntryKind::Directory)) => {}
                Ok(_) => continue,
                Err(error) => {
                    report.errors.push(error);
                    continue;
                }
            }
            report.scanned += 1;
            if let Err(error) = validate_identifier(&directory_id, "managed skill id") {
                report.errors.push(error);
                continue;
            }
            let skill_directory = match managed_directory.open_owned_child(&path) {
                Ok(directory) => directory,
                Err(error) => {
                    report.errors.push(error);
                    continue;
                }
            };
            let marker_path = skill_directory.path().join(MANAGED_SKILL_MARKER);
            let (metadata, marker_file) =
                match load_managed_skill_metadata_opened(&skill_directory, &marker_path) {
                    Ok(opened) if opened.0.id == directory_id => opened,
                    Ok((metadata, _)) => {
                        let error = format!(
                            "managed skill marker id {} does not match directory {directory_id}",
                            metadata.id
                        );
                        report.errors.push(error.clone());
                        self.audit(
                            "inspect_skill",
                            Some(&directory_id),
                            "error",
                            false,
                            Some(error),
                        )?;
                        continue;
                    }
                    Err(error) if error.contains("failed to open") => {
                        report.retained += 1;
                        self.audit(
                            "cleanup_skill",
                            Some(&directory_id),
                            "skipped_unmanaged",
                            false,
                            Some(format!(
                                "missing or invalid {MANAGED_SKILL_MARKER}: {error}"
                            )),
                        )?;
                        continue;
                    }
                    Err(error) => {
                        report.errors.push(error.clone());
                        self.audit(
                            "inspect_skill",
                            Some(&directory_id),
                            "error",
                            false,
                            Some(error),
                        )?;
                        continue;
                    }
                };
            let directory_key = crate::context::skills::canonical_skill_id(&directory_id)
                .map_err(|error| format!("invalid managed skill id {directory_id}: {error}"))?;
            let aggregated = skill_usage.get(&directory_key);
            if metadata.last_used_at >= cutoff
                || aggregated.is_some_and(|usage| {
                    usage.has_undated_usage
                        || usage
                            .latest_used_at
                            .is_some_and(|used_at| used_at >= cutoff)
                })
            {
                report.retained += 1;
                if let Some(usage) = aggregated {
                    self.audit(
                        "cleanup_skill",
                        Some(&directory_id),
                        "skipped_recent_usage",
                        false,
                        Some(format!(
                            "uses={} sessions={} latest={}",
                            usage.usage_count,
                            usage.session_count,
                            usage
                                .latest_used_at
                                .map(|value| value.to_rfc3339())
                                .unwrap_or_else(|| "undated".to_string())
                        )),
                    )?;
                }
                continue;
            }
            if pins.pins.skills.contains(&directory_id) {
                report.pinned += 1;
                self.audit(
                    "cleanup_skill",
                    Some(&directory_id),
                    "skipped_pinned",
                    false,
                    None,
                )?;
                continue;
            }
            if !self.authorize_cleanup(&mut report, "cleanup_skill", Some(&directory_id))? {
                continue;
            }

            let marker_identity = crate::fs_security::file_identity_snapshot(&marker_file)
                .map_err(|error| {
                    format!(
                        "failed to retain managed skill marker identity {}: {error}",
                        marker_path.display()
                    )
                })?;
            let directory_identity = skill_directory
                .directory_removal_receipt()
                .map_err(|error| {
                    format!(
                        "failed to retain managed skill directory identity {}: {error}",
                        path.display()
                    )
                })?
                .identity();
            drop(marker_file);
            drop(skill_directory);
            before_quarantine(&path);
            let skill_directory = match pins_directory
                .verify_file_expectation(&self.pins_path, pins.expectation())
                .and_then(|()| {
                    let skill_directory = managed_directory.open_owned_child(&path)?;
                    let visible_identity = skill_directory
                        .directory_removal_receipt()
                        .map_err(|error| {
                            format!(
                                "failed to re-check managed skill directory identity {}: {error}",
                                path.display()
                            )
                        })?
                        .identity();
                    if visible_identity != directory_identity {
                        return Err(format!(
                            "managed skill directory identity changed while cleanup was authorized: {}",
                            path.display()
                        ));
                    }
                    verify_managed_skill_marker_identity(
                        &skill_directory,
                        &marker_path,
                        marker_identity,
                    )?;
                    Ok(skill_directory)
                })
            {
                Ok(skill_directory) => skill_directory,
                Err(error) => {
                    report.errors.push(error.clone());
                    self.audit(
                        "cleanup_skill",
                        Some(&directory_id),
                        "error",
                        true,
                        Some(error),
                    )?;
                    continue;
                }
            };
            let quarantine_path =
                managed_skill_quarantine_path(managed_directory.path(), &directory_id);
            if managed_directory.entry_kind(&quarantine_path)?.is_some() {
                let error = format!(
                    "managed skill quarantine already exists: {}",
                    quarantine_path.display()
                );
                report.errors.push(error);
                continue;
            }
            if let Err(error) =
                managed_directory.rename_child_directory(&path, &skill_directory, &quarantine_path)
            {
                report.errors.push(error.clone());
                self.audit(
                    "cleanup_skill",
                    Some(&directory_id),
                    "error",
                    true,
                    Some(error),
                )?;
                continue;
            }
            let quarantine_directory = skill_directory.try_clone_at(&quarantine_path)?;
            let quarantined_marker = quarantine_directory.path().join(MANAGED_SKILL_MARKER);
            verify_managed_skill_marker_identity(
                &quarantine_directory,
                &quarantined_marker,
                marker_identity,
            )?;
            quarantines.push(ManagedSkillQuarantine {
                id: directory_id,
                path: quarantine_path,
                parent: managed_directory.try_clone()?,
                directory: quarantine_directory,
            });
        }

        Ok((report, quarantines))
    }

    fn aggregate_skill_usage_unlocked(
        &self,
        store: &SessionStore,
    ) -> Result<BTreeMap<String, SkillUsageAggregate>, String> {
        let session_ids = store
            .list_for_skill_usage(MAX_CURATOR_DIRECTORY_ENTRIES, MAX_AGGREGATED_SESSION_BYTES)
            .map_err(|error| error.to_string())?;
        let mut aggregates = BTreeMap::<String, SkillUsageAggregate>::new();
        let mut total_records = 0usize;

        for session_id in session_ids {
            let session = store
                .load_result(&session_id)
                .map_err(|error| format!("failed to aggregate session {session_id}: {error}"))?
                .ok_or_else(|| {
                    format!("session {session_id} disappeared during skill usage aggregation")
                })?;
            let mut used_in_session = BTreeSet::new();
            for usage in &session.skill_usage {
                total_records = total_records.checked_add(1).ok_or_else(|| {
                    "skill usage record count overflowed during aggregation".to_string()
                })?;
                if total_records > MAX_AGGREGATED_SKILL_USAGE_RECORDS {
                    return Err(format!(
                        "profile skill usage exceeds the {MAX_AGGREGATED_SKILL_USAGE_RECORDS}-record aggregation limit"
                    ));
                }
                let skill_name = usage.skill_name.trim();
                let key =
                    crate::context::skills::canonical_skill_id(skill_name).map_err(|error| {
                        format!("invalid skill usage name in session {session_id}: {error}")
                    })?;
                if !aggregates.contains_key(&key) && aggregates.len() >= MAX_AGGREGATED_SKILLS {
                    return Err(format!(
                        "profile skill usage exceeds the {MAX_AGGREGATED_SKILLS}-skill aggregation limit"
                    ));
                }
                let aggregate =
                    aggregates
                        .entry(key.clone())
                        .or_insert_with(|| SkillUsageAggregate {
                            skill_name: skill_name.to_string(),
                            ..SkillUsageAggregate::default()
                        });
                aggregate.usage_count += 1;
                match usage.timestamp {
                    Some(timestamp) => {
                        aggregate.latest_used_at = Some(
                            aggregate
                                .latest_used_at
                                .map_or(timestamp, |current| current.max(timestamp)),
                        );
                    }
                    None => aggregate.has_undated_usage = true,
                }
                used_in_session.insert(key);
            }
            for skill_name in &session.active_skills {
                let skill_name = skill_name.trim();
                let key =
                    crate::context::skills::canonical_skill_id(skill_name).map_err(|error| {
                        format!("invalid active skill name in session {session_id}: {error}")
                    })?;
                if used_in_session.contains(&key) {
                    continue;
                }
                total_records = total_records.checked_add(1).ok_or_else(|| {
                    "skill usage record count overflowed during aggregation".to_string()
                })?;
                if total_records > MAX_AGGREGATED_SKILL_USAGE_RECORDS {
                    return Err(format!(
                        "profile skill usage exceeds the {MAX_AGGREGATED_SKILL_USAGE_RECORDS}-record aggregation limit"
                    ));
                }
                if !aggregates.contains_key(&key) && aggregates.len() >= MAX_AGGREGATED_SKILLS {
                    return Err(format!(
                        "profile skill usage exceeds the {MAX_AGGREGATED_SKILLS}-skill aggregation limit"
                    ));
                }
                let aggregate =
                    aggregates
                        .entry(key.clone())
                        .or_insert_with(|| SkillUsageAggregate {
                            skill_name: skill_name.to_string(),
                            ..SkillUsageAggregate::default()
                        });
                aggregate.usage_count += 1;
                aggregate.has_undated_usage = true;
                used_in_session.insert(key);
            }
            for key in used_in_session {
                if let Some(aggregate) = aggregates.get_mut(&key) {
                    aggregate.session_count += 1;
                }
            }
        }

        Ok(aggregates)
    }

    pub fn track_managed_skill_at(
        &self,
        skill_id: &str,
        last_used_at: DateTime<Utc>,
    ) -> Result<PathBuf, String> {
        validate_identifier(skill_id, "skill id")?;
        if skill_id == MANAGED_SKILL_LOCK || managed_skill_quarantine_id(skill_id).is_some() {
            return Err("skill id uses a reserved managed-skill state name".to_string());
        }
        ensure_local_directory(&self.managed_skills_dir)?;
        let skill_dir = self.managed_skills_dir.join(skill_id);
        self.with_managed_skills_lock(|managed_directory| {
            let skill_directory = match managed_directory.entry_kind(&skill_dir)? {
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    managed_directory.open_child(&skill_dir)?
                }
                None => managed_directory.create_child_directory(&skill_dir)?,
                Some(_) => {
                    return Err(format!(
                        "managed skill path is not a local directory: {}",
                        skill_dir.display()
                    ))
                }
            };
            skill_directory.recover_stale_temporary_files(
                ".nib-daemon-",
                MAX_CURATOR_DIRECTORY_ENTRIES,
                MAX_CURATOR_DIRECTORY_ENTRIES * MAX_CURATOR_IDENTIFIER_BYTES,
            )?;
            let marker_path = skill_directory.path().join(MANAGED_SKILL_MARKER);
            let marker_file = if skill_directory.path_exists(&marker_path)? {
                Some(skill_directory.open_read(&marker_path)?)
            } else {
                None
            };
            let expected = marker_file.as_ref().map_or(
                crate::daemons::state::FileExpectation::Missing,
                crate::daemons::state::FileExpectation::Present,
            );
            skill_directory.save_json_atomically_expected(
                &marker_path,
                &ManagedSkillMetadata {
                    id: skill_id.to_string(),
                    last_used_at,
                },
                expected,
            )
        })?;
        self.audit("track_skill", Some(skill_id), "tracked", false, None)?;
        Ok(skill_dir)
    }

    pub fn pin_session(&self, session_id: &str) -> Result<(), String> {
        validate_identifier(session_id, "session id")?;
        self.update_pin(PinKind::Session, session_id, true)
            .map(|_| ())
    }

    pub fn unpin_session(&self, session_id: &str) -> Result<bool, String> {
        validate_identifier(session_id, "session id")?;
        self.update_pin(PinKind::Session, session_id, false)
    }

    pub fn is_pinned(&self, session_id: &str) -> Result<bool, String> {
        validate_identifier(session_id, "session id")?;
        Ok(self.load_pins()?.sessions.contains(session_id))
    }

    pub fn pin_memory(&self, namespace: MemoryNamespace, key: &str) -> Result<(), String> {
        validate_memory_key(key)?;
        self.update_pin(PinKind::Memory(namespace), key, true)
            .map(|_| ())
    }

    pub fn unpin_memory(&self, namespace: MemoryNamespace, key: &str) -> Result<bool, String> {
        validate_memory_key(key)?;
        self.update_pin(PinKind::Memory(namespace), key, false)
    }

    pub fn pin_skill(&self, skill_id: &str) -> Result<(), String> {
        validate_identifier(skill_id, "skill id")?;
        self.update_pin(PinKind::Skill, skill_id, true).map(|_| ())
    }

    pub fn unpin_skill(&self, skill_id: &str) -> Result<bool, String> {
        validate_identifier(skill_id, "skill id")?;
        self.update_pin(PinKind::Skill, skill_id, false)
    }

    pub fn validate_state(&self) -> Result<(), String> {
        self.validate_retention()?;
        reject_symlink_directory(&self.sessions_dir)?;
        reject_symlink_directory(&self.managed_skills_dir)?;
        reject_symlink_file(&self.memory_path)?;
        self.load_pins()?;
        self.audit_log.read_all()?;
        MemoryStore::at_path(self.memory_path.clone()).load_result()?;
        Ok(())
    }

    pub fn audit_log(&self) -> &DaemonAuditLog {
        &self.audit_log
    }

    fn collect_memory_removals(
        &self,
        namespace: MemoryNamespace,
        data: &MemoryStoreData,
        cutoff: DateTime<Utc>,
        pins: &PinFile,
        report: &mut CuratorReport,
        removals: &mut Vec<(MemoryNamespace, String)>,
    ) -> Result<(), String> {
        let (values, metadata) = match namespace {
            MemoryNamespace::Environment => (&data.environment, &data.metadata.environment),
            MemoryNamespace::User => (&data.user, &data.metadata.user),
        };
        let mut keys: Vec<_> = values.keys().cloned().collect();
        keys.sort();
        for key in keys {
            report.scanned += 1;
            let Some(entry_metadata) = metadata.get(&key) else {
                // Legacy entries have no trustworthy age. Fail closed and retain them.
                report.retained += 1;
                continue;
            };
            if entry_metadata.updated_at >= cutoff {
                report.retained += 1;
                continue;
            }
            let target = memory_target(namespace, &key);
            if pins.memory(namespace).contains(&key) {
                report.pinned += 1;
                self.audit(
                    "cleanup_memory",
                    Some(&target),
                    "skipped_pinned",
                    false,
                    None,
                )?;
                continue;
            }
            if !self.authorize_cleanup(report, "cleanup_memory", Some(&target))? {
                continue;
            }
            removals.push((namespace, key));
        }
        Ok(())
    }

    fn authorize_cleanup(
        &self,
        report: &mut CuratorReport,
        action: &str,
        target: Option<&str>,
    ) -> Result<bool, String> {
        if !self.policy.allow_destructive_cleanup {
            report.policy_skipped += 1;
            self.audit(
                action,
                target,
                "skipped_policy",
                false,
                Some("destructive cleanup is not authorized".to_string()),
            )?;
            return Ok(false);
        }
        // Authorization is persisted before any destructive filesystem mutation.
        self.audit(action, target, "authorized", true, None)?;
        Ok(true)
    }

    fn validate_retention(&self) -> Result<(), String> {
        if self.retention_days < 0 {
            return Err("retention_days must not be negative".to_string());
        }
        Duration::try_days(self.retention_days)
            .ok_or_else(|| "retention_days is too large".to_string())?;
        Ok(())
    }

    fn retention_cutoff(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        self.validate_retention()?;
        let retention = Duration::try_days(self.retention_days)
            .ok_or_else(|| "retention_days is too large".to_string())?;
        now.checked_sub_signed(retention)
            .ok_or_else(|| "retention cutoff is outside the supported date range".to_string())
    }

    fn load_pins(&self) -> Result<PinFile, String> {
        self.with_pins_lock(|directory| self.load_pins_unlocked(directory))
    }

    fn load_pins_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
    ) -> Result<PinFile, String> {
        self.load_pins_opened_unlocked(directory)
            .map(|opened| opened.pins)
    }

    fn load_pins_opened_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
    ) -> Result<OpenedPinFile, String> {
        if !directory.path_exists(&self.pins_path)? {
            return Ok(OpenedPinFile {
                pins: PinFile::default(),
                file: None,
            });
        }
        let file = directory.open_read(&self.pins_path)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        validate_bounded_state_metadata(
            &self.pins_path,
            &metadata,
            MAX_PINS_FILE_BYTES,
            "curator pins",
        )?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(MAX_PINS_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_PINS_FILE_BYTES {
            return Err(format!(
                "curator pins {} exceeds the {MAX_PINS_FILE_BYTES}-byte limit",
                self.pins_path.display()
            ));
        }
        directory.verify_file_identity(&self.pins_path, &file)?;
        let pins: PinFile = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        validate_pins(&pins)?;
        Ok(OpenedPinFile {
            pins,
            file: Some(file),
        })
    }

    fn save_pins_unlocked_with_commit_hook(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        pins: &PinFile,
        expected: crate::daemons::state::FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        validate_pins(pins)?;
        let encoded = serde_json::to_vec_pretty(pins).map_err(|error| error.to_string())?;
        if encoded.len() as u64 > MAX_PINS_FILE_BYTES {
            return Err(format!(
                "curator pins exceed the {MAX_PINS_FILE_BYTES}-byte limit"
            ));
        }
        directory.save_bytes_atomically_expected_with_hook(
            &self.pins_path,
            &encoded,
            ".nib-daemon-",
            true,
            expected,
            before_commit,
        )
    }

    fn update_pin(&self, kind: PinKind, target: &str, insert: bool) -> Result<bool, String> {
        self.update_pin_with_commit_hook(kind, target, insert, || Ok(()))
    }

    fn update_pin_with_commit_hook(
        &self,
        kind: PinKind,
        target: &str,
        insert: bool,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        self.with_pins_lock(|directory| {
            let mut opened = self.load_pins_opened_unlocked(directory)?;
            let at_capacity = pin_count(&opened.pins) >= MAX_PIN_ENTRIES;
            let set = match kind {
                PinKind::Session => &mut opened.pins.sessions,
                PinKind::Memory(namespace) => opened.pins.memory_mut(namespace),
                PinKind::Skill => &mut opened.pins.skills,
            };
            if insert && !set.contains(target) && at_capacity {
                return Err(format!(
                    "curator pins exceed the {MAX_PIN_ENTRIES}-entry limit"
                ));
            }
            let changed = if insert {
                set.insert(target.to_string())
            } else {
                set.remove(target)
            };
            if changed {
                let action = if insert { "pin" } else { "unpin" };
                self.audit(action, Some(&kind.target(target)), "authorized", true, None)?;
                self.save_pins_unlocked_with_commit_hook(
                    directory,
                    &opened.pins,
                    opened.expectation(),
                    before_commit,
                )?;
                self.audit(action, Some(&kind.target(target)), "updated", true, None)?;
            }
            Ok(changed)
        })
    }

    fn with_pins_lock<T>(
        &self,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
    ) -> Result<T, String> {
        crate::daemons::state::with_file_lock(&pins_lock_path(&self.pins_path), operation)
    }

    fn with_managed_skills_lock<T>(
        &self,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
    ) -> Result<T, String> {
        crate::daemons::state::with_file_lock(
            &self.managed_skills_dir.join(MANAGED_SKILL_LOCK),
            operation,
        )
    }

    fn delete_session_if_old(
        &self,
        store: &SessionStore,
        session_id: &str,
        cutoff: DateTime<Utc>,
    ) -> Result<CuratorSessionDelete, String> {
        self.with_pins_lock(|directory| {
            let pins = self.load_pins_opened_unlocked(directory)?;
            if pins.pins.sessions.contains(session_id) {
                return Ok(CuratorSessionDelete::Pinned);
            }
            store
                .delete_if_with_commit_check(
                    session_id,
                    |session, metadata| {
                        let file_activity = metadata.modified().ok().map(DateTime::<Utc>::from);
                        is_session_old(session, file_activity, cutoff)
                    },
                    || {
                        directory
                            .verify_file_expectation(&self.pins_path, pins.expectation())
                            .map_err(SessionError::InvalidMutation)
                    },
                )
                .map(|outcome| match outcome {
                    SessionDeleteOutcome::Missing => CuratorSessionDelete::Missing,
                    SessionDeleteOutcome::Retained => CuratorSessionDelete::Retained,
                    SessionDeleteOutcome::Deleted => CuratorSessionDelete::Deleted,
                })
                .map_err(|error| error.to_string())
        })
    }

    fn audit(
        &self,
        action: &str,
        target: Option<&str>,
        outcome: &str,
        authorized: bool,
        detail: Option<String>,
    ) -> Result<(), String> {
        self.audit_log.append(&DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "curator".to_string(),
            action: action.to_string(),
            target: target.map(str::to_string),
            outcome: outcome.to_string(),
            authorized,
            detail,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum PinKind {
    Session,
    Memory(MemoryNamespace),
    Skill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CuratorSessionDelete {
    Missing,
    Retained,
    Pinned,
    Deleted,
}

impl PinKind {
    fn target(self, value: &str) -> String {
        match self {
            Self::Session => format!("session:{value}"),
            Self::Memory(namespace) => memory_target(namespace, value),
            Self::Skill => format!("skill:{value}"),
        }
    }
}

fn memory_target(namespace: MemoryNamespace, key: &str) -> String {
    format!("memory:{}:{key}", namespace.label())
}

fn pins_lock_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "pins.json".into());
    file_name.push(".lock");
    path.with_file_name(file_name)
}

fn ensure_local_directory(path: &Path) -> Result<(), String> {
    crate::fs_security::ensure_directory_without_symlinks(path)
        .map(|_| ())
        .map_err(|error| format!("daemon state path is unsafe: {error}"))
}

fn reject_symlink_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => crate::fs_security::verify_directory_without_symlinks(path)
            .map_err(|error| format!("daemon state directory is unsafe: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn reject_symlink_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if crate::fs_security::metadata_is_link_or_reparse(&metadata) => Err(format!(
            "daemon state file must not be a symlink or reparse point: {}",
            path.display()
        )),
        Ok(metadata) if !metadata.is_file() => Err(format!(
            "daemon state path is not a file: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
fn load_managed_skill_metadata(path: &Path) -> Result<ManagedSkillMetadata, String> {
    let contents =
        read_bounded_state_file(path, MAX_MANAGED_SKILL_MARKER_BYTES, "managed skill marker")?;
    let metadata: ManagedSkillMetadata = serde_json::from_slice(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    validate_identifier(&metadata.id, "managed skill marker id")?;
    Ok(metadata)
}

fn load_managed_skill_metadata_opened(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<(ManagedSkillMetadata, File), String> {
    let mut file = directory.open_read(path)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if metadata.len() > MAX_MANAGED_SKILL_MARKER_BYTES {
        return Err(format!(
            "managed skill marker {} exceeds the {MAX_MANAGED_SKILL_MARKER_BYTES}-byte limit",
            path.display()
        ));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_SKILL_MARKER_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| error.to_string())?;
    if contents.len() as u64 > MAX_MANAGED_SKILL_MARKER_BYTES {
        return Err(format!(
            "managed skill marker {} exceeds the {MAX_MANAGED_SKILL_MARKER_BYTES}-byte limit",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    let metadata: ManagedSkillMetadata = serde_json::from_slice(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    validate_identifier(&metadata.id, "managed skill marker id")?;
    Ok((metadata, file))
}

fn managed_skill_quarantine_path(parent: &Path, skill_id: &str) -> PathBuf {
    parent.join(format!(
        "{MANAGED_SKILL_QUARANTINE_PREFIX}{skill_id}{MANAGED_SKILL_QUARANTINE_SUFFIX}"
    ))
}

fn managed_skill_quarantine_id(name: &str) -> Option<String> {
    let id = name
        .strip_prefix(MANAGED_SKILL_QUARANTINE_PREFIX)?
        .strip_suffix(MANAGED_SKILL_QUARANTINE_SUFFIX)?;
    validate_identifier(id, "managed skill quarantine id")
        .ok()
        .map(|()| id.to_string())
}

fn verify_managed_skill_marker_identity(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    expected: crate::fs_security::FileIdentitySnapshot,
) -> Result<(), String> {
    let file = directory.open_read(path)?;
    let actual = crate::fs_security::file_identity_snapshot(&file)
        .map_err(|error| format!("failed to inspect managed skill marker identity: {error}"))?;
    if actual != expected {
        return Err(format!(
            "managed skill marker identity changed while cleanup was authorized: {}",
            path.display()
        ));
    }
    Ok(())
}

fn delete_managed_skill_quarantine(quarantine: ManagedSkillQuarantine) -> Result<(), String> {
    let ManagedSkillQuarantine {
        id: _,
        path,
        parent,
        directory,
    } = quarantine;
    directory.verify_visible()?;
    let mut budget = ManagedSkillTreeBudget::default();
    delete_managed_skill_tree_contents(&directory, 0, &mut budget, MANAGED_SKILL_TREE_LIMITS)?;
    parent.remove_empty_child_directory_if_matches(&path, directory)
}

fn delete_managed_skill_tree_contents(
    directory: &crate::daemons::state::StableDirectory,
    depth: usize,
    budget: &mut ManagedSkillTreeBudget,
    limits: ManagedSkillTreeLimits,
) -> Result<(), String> {
    if depth > limits.max_depth {
        return Err(format!(
            "managed skill tree exceeds the {}-level depth limit",
            limits.max_depth
        ));
    }
    let remaining_entries = limits.max_entries.saturating_sub(budget.entries);
    let remaining_name_bytes = limits.max_name_bytes.saturating_sub(budget.name_bytes);
    let mut names = Vec::new();
    directory.for_each_entry_bounded(remaining_entries, remaining_name_bytes, |name| {
        names.push(name);
        Ok(())
    })?;
    names.sort();

    for name in names {
        budget.entries = budget
            .entries
            .checked_add(1)
            .ok_or_else(|| "managed skill tree entry count overflowed".to_string())?;
        budget.name_bytes = budget
            .name_bytes
            .checked_add(name.as_encoded_bytes().len())
            .ok_or_else(|| "managed skill tree filename byte count overflowed".to_string())?;
        if budget.entries > limits.max_entries || budget.name_bytes > limits.max_name_bytes {
            return Err("managed skill tree exceeds its bounded deletion budget".to_string());
        }
        let path = directory.path().join(name);
        match directory.entry_kind(&path)? {
            Some(crate::daemons::state::StableEntryKind::File) => {
                let file = directory.open_read(&path)?;
                directory.remove_file_if_matches(&path, &file, ".nib-skill-tree-delete-")?;
            }
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                let child = directory.open_owned_child(&path)?;
                delete_managed_skill_tree_contents(&child, depth + 1, budget, limits)?;
                directory.remove_empty_child_directory_if_matches(&path, child)?;
            }
            None => {}
        }
    }
    Ok(())
}

#[cfg(test)]
fn session_file_activity(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

fn is_session_old(
    session: &Session,
    file_activity: Option<DateTime<Utc>>,
    cutoff: DateTime<Utc>,
) -> bool {
    if has_undated_skill_usage(session) {
        return false;
    }
    let last_activity = session
        .messages
        .iter()
        .filter_map(|message| message.timestamp)
        .chain(
            session
                .tool_calls
                .iter()
                .filter_map(|record| record.timestamp),
        )
        .chain(session.events.iter().filter_map(|event| event.timestamp))
        .chain(
            session
                .skill_usage
                .iter()
                .filter_map(|usage| usage.timestamp),
        )
        .chain(session.started_at)
        .chain(file_activity)
        .max();
    last_activity.is_some_and(|timestamp| timestamp < cutoff)
}

fn has_undated_skill_usage(session: &Session) -> bool {
    if session
        .skill_usage
        .iter()
        .any(|usage| usage.timestamp.is_none())
    {
        return true;
    }
    session.active_skills.iter().any(|active| {
        let Ok(active_id) = crate::context::skills::canonical_skill_id(active) else {
            return true;
        };
        !session.skill_usage.iter().any(|usage| {
            crate::context::skills::canonical_skill_id(&usage.skill_name)
                .is_ok_and(|usage_id| usage_id == active_id)
        })
    })
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > MAX_CURATOR_IDENTIFIER_BYTES
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!("{label} contains unsupported characters"))
    } else {
        Ok(())
    }
}

fn validate_memory_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty()
        || key.len() > MAX_MEMORY_KEY_BYTES
        || key.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0'))
    {
        Err(format!(
            "memory key must be non-empty, at most {MAX_MEMORY_KEY_BYTES} bytes, and contain no control characters"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn read_bounded_state_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>, String> {
    read_bounded_state_file_with_hook(path, max_bytes, label, || {})
}

#[cfg(test)]
fn read_bounded_state_file_with_hook(
    path: &Path,
    max_bytes: u64,
    label: &str,
    after_open: impl FnOnce(),
) -> Result<Vec<u8>, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} has no parent: {}", path.display()))?;
    crate::fs_security::verify_directory_without_symlinks(parent)
        .map_err(|error| format!("{label} parent is unsafe: {error}"))?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    if !directory.path_exists(path)? {
        return Err(format!("{label} does not exist: {}", path.display()));
    }
    let file = directory.open_read(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect {label} {}: {error}", path.display()))?;
    validate_bounded_state_metadata(path, &metadata, max_bytes, label)?;
    after_open();
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read {label} {}: {error}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    Ok(bytes)
}

fn validate_bounded_state_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    max_bytes: u64,
    label: &str,
) -> Result<(), String> {
    if crate::fs_security::metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(format!(
            "{label} must be a regular local file: {}",
            path.display()
        ));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        ));
    }
    Ok(())
}

fn pin_count(pins: &PinFile) -> usize {
    pins.sessions.len() + pins.memory_environment.len() + pins.memory_user.len() + pins.skills.len()
}

fn validate_pins(pins: &PinFile) -> Result<(), String> {
    if pin_count(pins) > MAX_PIN_ENTRIES {
        return Err(format!(
            "curator pins exceed the {MAX_PIN_ENTRIES}-entry limit"
        ));
    }
    for value in pins.sessions.iter().chain(pins.skills.iter()) {
        validate_identifier(value, "curator pin")?;
    }
    for key in pins
        .memory_environment
        .iter()
        .chain(pins.memory_user.iter())
    {
        validate_memory_key(key)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "curator_tests.rs"]
mod tests;

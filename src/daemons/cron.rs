use crate::daemons::curator::{Curator, CuratorPolicy, CuratorReport};
use crate::daemons::task::{DaemonAuditLog, DaemonAuditRecord};
use crate::profile::Profile;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_CRON_STATE_BYTES: u64 = 1024 * 1024;
const MAX_CRON_JOBS: usize = 1024;
const MAX_CRON_JOB_ID_BYTES: usize = 128;
const MAX_CRON_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_CRON_DIRECTORY_NAME_BYTES: usize = MAX_CRON_DIRECTORY_ENTRIES * 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronJob {
    pub id: String,
    pub interval_seconds: u64,
    pub next_run: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronRun {
    pub job_id: String,
    pub scheduled_for: DateTime<Utc>,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronOccurrenceOutcome {
    EffectUnknown,
    Completed,
    Error,
    SkippedAuditFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronOccurrence {
    pub job_id: String,
    pub scheduled_for: DateTime<Utc>,
    pub claimed_at: DateTime<Utc>,
    pub outcome: CronOccurrenceOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct CronState {
    #[serde(default)]
    jobs: BTreeMap<String, CronJob>,
    #[serde(default)]
    occurrences: BTreeMap<String, CronOccurrence>,
}

struct OpenedCronState {
    state: CronState,
    file: Option<File>,
}

#[derive(Debug, Default)]
pub struct Cron {
    jobs: BTreeMap<String, CronJob>,
    occurrences: BTreeMap<String, CronOccurrence>,
    state_path: Option<PathBuf>,
    audit_log: Option<DaemonAuditLog>,
}

impl Cron {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn at_dir(daemon_dir: &Path) -> Result<Self, String> {
        crate::fs_security::ensure_directory_without_symlinks(daemon_dir)
            .map_err(|error| format!("cron state path is unsafe: {error}"))?;
        let state_path = daemon_dir.join("cron.json");
        let state =
            crate::daemons::state::with_file_lock(&cron_lock_path(&state_path), |directory| {
                directory.recover_stale_temporary_files(
                    ".nib-daemon-",
                    MAX_CRON_DIRECTORY_ENTRIES,
                    MAX_CRON_DIRECTORY_NAME_BYTES,
                )?;
                load_cron_state_unlocked(directory, &state_path)
            })?;
        Ok(Self {
            jobs: state.state.jobs,
            occurrences: state.state.occurrences,
            state_path: Some(state_path),
            audit_log: Some(DaemonAuditLog::at_path(daemon_dir.join("audit.jsonl"))),
        })
    }

    pub fn schedule_every(
        &mut self,
        id: impl Into<String>,
        interval_seconds: u64,
        first_run: DateTime<Utc>,
    ) -> Result<(), String> {
        let id = id.into();
        validate_job(&id, interval_seconds)?;
        self.mutate_jobs(|jobs| {
            if jobs.len() >= MAX_CRON_JOBS {
                return Err(format!("cron jobs exceed the {MAX_CRON_JOBS}-job limit"));
            }
            if jobs.contains_key(&id) {
                return Err(format!("cron job already exists: {id}"));
            }
            jobs.insert(
                id.clone(),
                CronJob {
                    id,
                    interval_seconds,
                    next_run: first_run,
                },
            );
            Ok(())
        })
    }

    pub fn ensure_schedule(
        &mut self,
        id: impl Into<String>,
        interval_seconds: u64,
        first_run: DateTime<Utc>,
    ) -> Result<(), String> {
        let id = id.into();
        validate_job(&id, interval_seconds)?;
        self.mutate_jobs(|jobs| {
            match jobs.get_mut(&id) {
                Some(job) if job.interval_seconds == interval_seconds => {}
                Some(job) => {
                    job.interval_seconds = interval_seconds;
                    job.next_run = first_run;
                }
                None => {
                    if jobs.len() >= MAX_CRON_JOBS {
                        return Err(format!("cron jobs exceed the {MAX_CRON_JOBS}-job limit"));
                    }
                    jobs.insert(
                        id.clone(),
                        CronJob {
                            id,
                            interval_seconds,
                            next_run: first_run,
                        },
                    );
                }
            }
            Ok(())
        })
    }

    pub fn remove(&mut self, id: &str) -> Result<Option<CronJob>, String> {
        self.mutate_jobs(|jobs| Ok(jobs.remove(id)))
    }

    pub fn jobs(&self) -> impl Iterator<Item = &CronJob> {
        self.jobs.values()
    }

    pub fn due_jobs(&self, now: DateTime<Utc>) -> Vec<&CronJob> {
        self.jobs
            .values()
            .filter(|job| job.next_run <= now)
            .collect()
    }

    pub fn last_occurrence(&self, id: &str) -> Option<&CronOccurrence> {
        self.occurrences.get(id)
    }

    pub fn run_due<F>(&mut self, now: DateTime<Utc>, mut run: F) -> Result<Vec<CronRun>, String>
    where
        F: FnMut(&CronJob) -> Result<(), String>,
    {
        let audit_log = self.audit_log.clone();
        let Some(state_path) = self.state_path.clone() else {
            let mut state = CronState {
                jobs: std::mem::take(&mut self.jobs),
                occurrences: std::mem::take(&mut self.occurrences),
            };
            let outcome = run_due_jobs(&mut state, audit_log.as_ref(), now, &mut run, |_| Ok(()));
            self.jobs = state.jobs;
            self.occurrences = state.occurrences;
            return outcome;
        };
        let lock_path = cron_lock_path(&state_path);
        let (jobs, outcome) = crate::daemons::state::with_file_lock(&lock_path, |directory| {
            let opened = load_cron_state_unlocked(directory, &state_path)?;
            let mut state = opened.state;
            let mut file = opened.file;
            let outcome = run_due_jobs(&mut state, audit_log.as_ref(), now, &mut run, |state| {
                let expected = file.as_ref().map_or(
                    crate::daemons::state::FileExpectation::Missing,
                    crate::daemons::state::FileExpectation::Present,
                );
                save_cron_state_unlocked(directory, &state_path, state, expected)?;
                file = Some(directory.open_read(&state_path)?);
                Ok(())
            });
            Ok((state, outcome))
        })?;
        self.jobs = jobs.jobs;
        self.occurrences = jobs.occurrences;
        outcome
    }

    fn mutate_jobs<T>(
        &mut self,
        mutation: impl FnOnce(&mut BTreeMap<String, CronJob>) -> Result<T, String>,
    ) -> Result<T, String> {
        let Some(state_path) = self.state_path.clone() else {
            let outcome = mutation(&mut self.jobs);
            if outcome.is_ok() {
                self.occurrences.retain(|id, _| self.jobs.contains_key(id));
            }
            return outcome;
        };
        let lock_path = cron_lock_path(&state_path);
        let (jobs, outcome) = crate::daemons::state::with_file_lock(&lock_path, |directory| {
            let opened = load_cron_state_unlocked(directory, &state_path)?;
            let mut state = opened.state;
            let outcome = mutation(&mut state.jobs);
            if outcome.is_ok() {
                state
                    .occurrences
                    .retain(|id, _| state.jobs.contains_key(id));
                let expected = opened.file.as_ref().map_or(
                    crate::daemons::state::FileExpectation::Missing,
                    crate::daemons::state::FileExpectation::Present,
                );
                save_cron_state_unlocked(directory, &state_path, &state, expected)?;
            }
            Ok((state, outcome))
        })?;
        self.jobs = jobs.jobs;
        self.occurrences = jobs.occurrences;
        outcome
    }

    /// Backward-compatible legacy-layout maintenance entry point with strict errors.
    pub fn run_maintenance(
        project_root: &Path,
        retention_days: i64,
    ) -> Result<CuratorReport, String> {
        let report = Self::run_maintenance_with_policy(
            project_root,
            retention_days,
            CuratorPolicy::default(),
        )?;
        if report.deleted > 0 {
            println!("Curator: Cleaned up {} old records.", report.deleted);
        }
        Ok(report)
    }

    pub fn run_maintenance_with_policy(
        project_root: &Path,
        retention_days: i64,
        policy: CuratorPolicy,
    ) -> Result<CuratorReport, String> {
        Curator::new_with_policy(project_root, retention_days, policy).cleanup()
    }

    /// Runs the selected profile's curator only when its durable cron schedule is due.
    pub fn run_profile_maintenance_due(
        profile: &Profile,
        interval_seconds: u64,
        retention_days: i64,
        policy: CuratorPolicy,
        now: DateTime<Utc>,
    ) -> Result<Option<CuratorReport>, String> {
        let mut cron = Self::at_dir(profile.daemon_dir())?;
        cron.ensure_schedule("curator", interval_seconds, now)?;

        let mut report = None;
        let runs = cron.run_due(now, |_| {
            let curator = Curator::at_profile_paths(
                profile.sessions_dir().to_path_buf(),
                profile.memory_path().to_path_buf(),
                profile.managed_skills_dir().to_path_buf(),
                profile.daemon_dir().to_path_buf(),
                retention_days,
                policy,
            );
            match curator.cleanup_at(now) {
                Ok(curator_report) => {
                    let error = (!curator_report.errors.is_empty()).then(|| {
                        format!(
                            "curator completed with {} inspection error(s): {}",
                            curator_report.errors.len(),
                            curator_report.errors.join("; ")
                        )
                    });
                    report = Some(curator_report);
                    error.map_or(Ok(()), Err)
                }
                Err(error) => Err(error),
            }
        })?;
        if let Some(error) = runs.into_iter().find_map(|run| run.result.err()) {
            return Err(error);
        }
        Ok(report)
    }
}

fn run_due_jobs<F, P>(
    state: &mut CronState,
    audit_log: Option<&DaemonAuditLog>,
    now: DateTime<Utc>,
    run: &mut F,
    mut persist: P,
) -> Result<Vec<CronRun>, String>
where
    F: FnMut(&CronJob) -> Result<(), String>,
    P: FnMut(&CronState) -> Result<(), String>,
{
    let due: Vec<String> = state
        .jobs
        .values()
        .filter(|job| job.next_run <= now)
        .map(|job| job.id.clone())
        .collect();
    let mut runs = Vec::with_capacity(due.len());
    for id in due {
        let job = state.jobs.get(&id).expect("due job exists").clone();
        let previous_next_run = job.next_run;
        let previous_occurrence = state.occurrences.get(&id).cloned();
        if let Some(stored) = state.jobs.get_mut(&id) {
            stored.next_run = next_run_after(stored.next_run, stored.interval_seconds, now)?;
        }
        state.occurrences.insert(
            id.clone(),
            CronOccurrence {
                job_id: id.clone(),
                scheduled_for: job.next_run,
                claimed_at: now,
                outcome: CronOccurrenceOutcome::EffectUnknown,
                detail: None,
            },
        );
        if let Err(error) = persist(state) {
            if let Some(stored) = state.jobs.get_mut(&id) {
                stored.next_run = previous_next_run;
            }
            restore_occurrence(&mut state.occurrences, &id, previous_occurrence);
            return Err(error);
        }

        if let Err(audit_error) = append_cron_audit(
            audit_log,
            "run_job",
            Some(&id),
            "started",
            Some(format!("scheduled_for={}", job.next_run.to_rfc3339())),
        ) {
            let claim = state
                .occurrences
                .get(&id)
                .expect("persisted occurrence claim exists")
                .clone();
            let occurrence = state
                .occurrences
                .get_mut(&id)
                .expect("persisted occurrence claim exists");
            occurrence.outcome = CronOccurrenceOutcome::SkippedAuditFailure;
            occurrence.detail = Some(audit_error.clone());
            if let Err(persist_error) = persist(state) {
                state.occurrences.insert(id.clone(), claim);
                return Err(format!(
                    "cron start audit failed ({audit_error}); durable outcome update failed and the occurrence remains effect_unknown: {persist_error}"
                ));
            }
            return Err(audit_error);
        }

        let result = run(&job);
        let claim = state
            .occurrences
            .get(&id)
            .expect("persisted occurrence claim exists")
            .clone();
        let occurrence = state
            .occurrences
            .get_mut(&id)
            .expect("persisted occurrence claim exists");
        occurrence.outcome = if result.is_ok() {
            CronOccurrenceOutcome::Completed
        } else {
            CronOccurrenceOutcome::Error
        };
        occurrence.detail = result.as_ref().err().cloned();
        if let Err(error) = persist(state) {
            state.occurrences.insert(id.clone(), claim);
            return Err(format!(
                "cron effect outcome could not be persisted; the durable occurrence remains effect_unknown: {error}"
            ));
        }
        append_cron_audit(
            audit_log,
            "run_job",
            Some(&id),
            if result.is_ok() { "completed" } else { "error" },
            result.as_ref().err().cloned(),
        )?;
        runs.push(CronRun {
            job_id: id.clone(),
            scheduled_for: job.next_run,
            result,
        });
    }
    Ok(runs)
}

fn append_cron_audit(
    audit_log: Option<&DaemonAuditLog>,
    action: &str,
    target: Option<&str>,
    outcome: &str,
    detail: Option<String>,
) -> Result<(), String> {
    let Some(audit_log) = audit_log else {
        return Ok(());
    };
    audit_log.append(&DaemonAuditRecord {
        timestamp: Utc::now(),
        daemon: "cron".to_string(),
        action: action.to_string(),
        target: target.map(str::to_string),
        outcome: outcome.to_string(),
        authorized: false,
        detail,
    })
}

fn load_cron_state_unlocked(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<OpenedCronState, String> {
    let (state, file) = if directory.path_exists(path)? {
        let file = directory.open_read(path)?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        validate_cron_state_metadata(path, &metadata)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(MAX_CRON_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        validate_cron_state_size(path, bytes.len() as u64)?;
        directory.verify_file_identity(path, &file)?;
        let contents = String::from_utf8(bytes)
            .map_err(|error| format!("cron state {} is not UTF-8: {error}", path.display()))?;
        let state = serde_json::from_str::<CronState>(&contents)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
        (state, Some(file))
    } else {
        (CronState::default(), None)
    };
    if state.jobs.len() > MAX_CRON_JOBS {
        return Err(format!(
            "cron state contains more than {MAX_CRON_JOBS} jobs"
        ));
    }
    for (id, job) in &state.jobs {
        validate_job(id, job.interval_seconds)?;
        if job.id != *id {
            return Err(format!(
                "cron job map key {id} does not match stored id {}",
                job.id
            ));
        }
    }
    if state.occurrences.len() > MAX_CRON_JOBS {
        return Err(format!(
            "cron state contains more than {MAX_CRON_JOBS} occurrence records"
        ));
    }
    for (id, occurrence) in &state.occurrences {
        validate_job_id(id)?;
        if occurrence.job_id != *id {
            return Err(format!(
                "cron occurrence map key {id} does not match stored id {}",
                occurrence.job_id
            ));
        }
        if !state.jobs.contains_key(id) {
            return Err(format!(
                "cron occurrence {id} has no corresponding scheduled job"
            ));
        }
    }
    Ok(OpenedCronState { state, file })
}

fn save_cron_state_unlocked(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    state: &CronState,
    expected: crate::daemons::state::FileExpectation<'_>,
) -> Result<(), String> {
    directory.save_json_atomically_expected(path, state, expected)
}

fn cron_lock_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "cron.json".into());
    file_name.push(".lock");
    path.with_file_name(file_name)
}

fn validate_job(id: &str, interval_seconds: u64) -> Result<(), String> {
    if interval_seconds == 0 || interval_seconds > i64::MAX as u64 {
        return Err("cron interval must be between 1 and i64::MAX seconds".to_string());
    }
    validate_job_id(id)
}

fn validate_job_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() || id.len() > MAX_CRON_JOB_ID_BYTES {
        return Err(format!(
            "cron job id must be non-empty and at most {MAX_CRON_JOB_ID_BYTES} bytes"
        ));
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err("cron job id contains unsupported characters".to_string());
    }
    Ok(())
}

fn restore_occurrence(
    occurrences: &mut BTreeMap<String, CronOccurrence>,
    id: &str,
    previous: Option<CronOccurrence>,
) {
    match previous {
        Some(previous) => {
            occurrences.insert(id.to_string(), previous);
        }
        None => {
            occurrences.remove(id);
        }
    }
}

fn validate_cron_state_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "cron state must be a regular local file: {}",
            path.display()
        ));
    }
    validate_cron_state_size(path, metadata.len())
}

fn validate_cron_state_size(path: &Path, bytes: u64) -> Result<(), String> {
    if bytes > MAX_CRON_STATE_BYTES {
        return Err(format!(
            "cron state {} is {bytes} bytes; maximum is {MAX_CRON_STATE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(())
}

fn next_run_after(
    scheduled: DateTime<Utc>,
    interval_seconds: u64,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    if scheduled > now {
        return Ok(scheduled);
    }
    let elapsed = now.signed_duration_since(scheduled).num_seconds().max(0) as u64;
    let steps = elapsed
        .checked_div(interval_seconds)
        .and_then(|steps| steps.checked_add(1))
        .ok_or_else(|| "cron next-run calculation overflow".to_string())?;
    let advance = interval_seconds
        .checked_mul(steps)
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(Duration::try_seconds)
        .ok_or_else(|| "cron next-run calculation overflow".to_string())?;
    scheduled
        .checked_add_signed(advance)
        .ok_or_else(|| "cron next-run timestamp overflow".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProfileConfig, ProfilesConfig};
    use crate::profile::ProfileRegistry;
    use chrono::TimeZone;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    };
    use tempfile::tempdir;

    #[test]
    fn recurring_jobs_run_on_deterministic_cadence() {
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::new();
        cron.schedule_every("curator", 60, start)
            .expect("schedule job");

        assert!(cron.due_jobs(start - Duration::seconds(1)).is_empty());
        let mut calls = Vec::new();
        let runs = cron
            .run_due(start + Duration::seconds(125), |job| {
                calls.push(job.id.clone());
                Ok(())
            })
            .expect("run jobs");

        assert_eq!(calls, vec!["curator"]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].scheduled_for, start);
        assert_eq!(
            cron.jobs().next().unwrap().next_run,
            start + Duration::seconds(180)
        );
    }

    #[test]
    fn durable_schedule_and_error_history_survive_restart() {
        let dir = tempdir().expect("tempdir");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(dir.path()).expect("cron store");
        cron.schedule_every("review", 60, start)
            .expect("schedule job");
        let runs = cron
            .run_due(start, |_| Err("review failed".to_string()))
            .expect("run job");
        assert_eq!(runs[0].result, Err("review failed".to_string()));
        drop(cron);

        let restarted = Cron::at_dir(dir.path()).expect("restart cron");
        assert_eq!(
            restarted.jobs().next().unwrap().next_run,
            start + Duration::seconds(60)
        );
        assert!(restarted.due_jobs(start).is_empty());
        let audit = DaemonAuditLog::at_path(dir.path().join("audit.jsonl"))
            .read_all()
            .expect("audit");
        assert!(audit.iter().any(|record| {
            record.daemon == "cron"
                && record.target.as_deref() == Some("review")
                && record.outcome == "error"
                && record.detail.as_deref() == Some("review failed")
        }));
    }

    #[test]
    fn cadence_persistence_failure_prevents_external_effect() {
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut state = CronState {
            jobs: BTreeMap::from([(
                "review".to_string(),
                CronJob {
                    id: "review".to_string(),
                    interval_seconds: 60,
                    next_run: start,
                },
            )]),
            occurrences: BTreeMap::new(),
        };
        let mut effects = 0_usize;

        let error = run_due_jobs(
            &mut state,
            None,
            start,
            &mut |_| {
                effects += 1;
                Ok(())
            },
            |_| Err("injected cadence persistence failure".to_string()),
        )
        .expect_err("failed cadence claim must stop execution");

        assert!(error.contains("injected cadence"), "{error}");
        assert_eq!(effects, 0);
        assert_eq!(state.jobs["review"].next_run, start);
        assert!(state.occurrences.is_empty());
    }

    #[test]
    fn effect_outcome_persistence_failure_leaves_durable_unknown_claim() {
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut state = CronState {
            jobs: BTreeMap::from([(
                "review".to_string(),
                CronJob {
                    id: "review".to_string(),
                    interval_seconds: 60,
                    next_run: start,
                },
            )]),
            occurrences: BTreeMap::new(),
        };
        let mut persisted = CronState::default();
        let mut writes = 0_usize;
        let mut effects = 0_usize;

        let error = run_due_jobs(
            &mut state,
            None,
            start,
            &mut |_| {
                effects += 1;
                Ok(())
            },
            |candidate| {
                writes += 1;
                if writes == 2 {
                    return Err("injected outcome persistence failure".to_string());
                }
                persisted = candidate.clone();
                Ok(())
            },
        )
        .expect_err("effect outcome failure must be surfaced");

        assert!(error.contains("remains effect_unknown"), "{error}");
        assert_eq!(effects, 1);
        assert_eq!(
            persisted.occurrences["review"].outcome,
            CronOccurrenceOutcome::EffectUnknown
        );
        assert_eq!(
            persisted.jobs["review"].next_run,
            start + Duration::seconds(60)
        );
        assert_eq!(state, persisted);
    }

    #[test]
    fn legacy_state_without_occurrences_loads_and_migrates_on_next_run() {
        let dir = tempdir().expect("tempdir");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let legacy = serde_json::json!({
            "jobs": {
                "review": {
                    "id": "review",
                    "interval_seconds": 60,
                    "next_run": start,
                }
            }
        });
        std::fs::write(
            dir.path().join("cron.json"),
            serde_json::to_vec_pretty(&legacy).expect("legacy state"),
        )
        .expect("write legacy state");

        let mut cron = Cron::at_dir(dir.path()).expect("load legacy cron state");
        assert!(cron.last_occurrence("review").is_none());
        cron.run_due(start, |_| Ok(())).expect("run legacy job");
        assert_eq!(
            cron.last_occurrence("review").expect("occurrence").outcome,
            CronOccurrenceOutcome::Completed
        );
    }

    #[test]
    fn panic_after_claim_is_not_replayed_and_remains_effect_unknown() {
        let dir = tempdir().expect("tempdir");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(dir.path()).expect("cron store");
        cron.schedule_every("review", 60, start)
            .expect("schedule job");
        cron.audit_log = None;

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = cron.run_due(start, |_| -> Result<(), String> {
                panic!("simulated process failure after durable claim")
            });
        }));
        assert!(panic.is_err());
        drop(cron);

        let restarted = Cron::at_dir(dir.path()).expect("restart cron");
        assert!(restarted.due_jobs(start).is_empty());
        assert_eq!(
            restarted
                .last_occurrence("review")
                .expect("durable claim")
                .outcome,
            CronOccurrenceOutcome::EffectUnknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_audit_failure_persists_skipped_outcome_without_running_effect() {
        let dir = tempdir().expect("tempdir");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(dir.path()).expect("cron store");
        cron.schedule_every("review", 60, start)
            .expect("schedule job");
        std::fs::create_dir(dir.path().join("audit.jsonl")).expect("block audit file");
        let mut effects = 0_usize;

        cron.run_due(start, |_| {
            effects += 1;
            Ok(())
        })
        .expect_err("start audit failure must be reported");
        assert_eq!(effects, 0);
        drop(cron);

        let restarted = Cron::at_dir(dir.path()).expect("restart cron");
        assert!(restarted.due_jobs(start).is_empty());
        assert_eq!(
            restarted
                .last_occurrence("review")
                .expect("durable skipped outcome")
                .outcome,
            CronOccurrenceOutcome::SkippedAuditFailure
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_audit_failure_does_not_replay_successful_effect_after_restart() {
        let dir = tempdir().expect("tempdir");
        let audit_path = dir.path().join("audit.jsonl");
        let effect_path = dir.path().join("effect");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(dir.path()).expect("cron store");
        cron.schedule_every("review", 60, start)
            .expect("schedule job");

        let error = cron
            .run_due(start, |_| {
                std::fs::write(&effect_path, b"completed").map_err(|error| error.to_string())?;
                std::fs::remove_file(&audit_path).map_err(|error| error.to_string())?;
                std::fs::create_dir(&audit_path).map_err(|error| error.to_string())?;
                Ok(())
            })
            .expect_err("completion audit replacement must be reported");
        assert!(
            error.contains("audit") || error.contains("directory"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&effect_path).expect("external effect"),
            b"completed"
        );
        drop(cron);

        let restarted = Cron::at_dir(dir.path()).expect("restart cron");
        assert!(restarted.due_jobs(start).is_empty());
        assert_eq!(
            restarted.jobs().next().expect("job").next_run,
            start + Duration::seconds(60)
        );
        assert_eq!(
            restarted
                .last_occurrence("review")
                .expect("durable completed outcome")
                .outcome,
            CronOccurrenceOutcome::Completed
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_due_commits_advance_to_original_directory_after_job_effect_and_replacement() {
        let parent = tempdir().expect("parent");
        let daemon_dir = parent.path().join("daemon");
        std::fs::create_dir(&daemon_dir).expect("daemon directory");
        let displaced = parent.path().join("daemon.displaced");
        let effect = parent.path().join("job-effect");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(&daemon_dir).expect("cron store");
        cron.schedule_every("review", 60, start)
            .expect("schedule due job");
        cron.audit_log = None;
        let replacement_state =
            std::fs::read(daemon_dir.join("cron.json")).expect("replacement sentinel contents");

        let error = cron
            .run_due(start, |_| {
                std::fs::write(&effect, b"ran")
                    .map_err(|error| format!("failed to publish job effect: {error}"))?;
                std::fs::rename(&daemon_dir, &displaced)
                    .map_err(|error| format!("failed to detach cron directory: {error}"))?;
                std::fs::create_dir(&daemon_dir)
                    .map_err(|error| format!("failed to replace cron directory: {error}"))?;
                std::fs::write(daemon_dir.join("cron.json"), &replacement_state)
                    .map_err(|error| format!("failed to seed replacement cron state: {error}"))
            })
            .expect_err("detached cron state must be reported after the paired commit");
        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(std::fs::read(&effect).expect("job effect"), b"ran");
        let committed: CronState = serde_json::from_slice(
            &std::fs::read(displaced.join("cron.json")).expect("original cron state"),
        )
        .expect("decode original cron state");
        assert_eq!(
            committed.jobs["review"].next_run,
            start + Duration::seconds(60)
        );
        assert_eq!(
            std::fs::read(daemon_dir.join("cron.json")).expect("replacement cron sentinel"),
            replacement_state
        );
    }

    #[test]
    fn profile_maintenance_runs_once_per_persisted_interval() {
        let dir = tempdir().expect("tempdir");
        let config = ProfilesConfig {
            default: "test".to_string(),
            active: vec![ProfileConfig {
                id: "test".to_string(),
                root: dir.path().to_path_buf(),
                ..ProfileConfig::default()
            }],
        };
        let registry = ProfileRegistry::load(dir.path(), &config).expect("profiles");
        let profile = registry.default_profile();
        profile.ensure_state_dirs().expect("profile state");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

        let first =
            Cron::run_profile_maintenance_due(profile, 60, 30, CuratorPolicy::default(), start)
                .expect("first maintenance");
        assert!(first.is_some());

        let before_due = Cron::run_profile_maintenance_due(
            profile,
            60,
            30,
            CuratorPolicy::default(),
            start + Duration::seconds(30),
        )
        .expect("maintenance before due");
        assert!(before_due.is_none());

        let due = Cron::run_profile_maintenance_due(
            profile,
            60,
            30,
            CuratorPolicy::default(),
            start + Duration::seconds(60),
        )
        .expect("maintenance due after restart");
        assert!(due.is_some());
    }

    #[test]
    fn configured_interval_change_is_persisted_and_rescheduled() {
        let dir = tempdir().expect("tempdir");
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut cron = Cron::at_dir(dir.path()).expect("cron store");
        cron.schedule_every("curator", 60, start + Duration::hours(1))
            .expect("initial schedule");

        let mut restarted = Cron::at_dir(dir.path()).expect("restart cron");
        restarted
            .ensure_schedule("curator", 120, start)
            .expect("update schedule");
        drop(restarted);

        let persisted = Cron::at_dir(dir.path()).expect("reload updated schedule");
        let job = persisted.jobs().next().expect("curator job");
        assert_eq!(job.interval_seconds, 120);
        assert_eq!(job.next_run, start);
    }

    #[test]
    fn profile_maintenance_surfaces_partial_curator_errors() {
        let dir = tempdir().expect("tempdir");
        let config = ProfilesConfig {
            default: "test".to_string(),
            active: vec![ProfileConfig {
                id: "test".to_string(),
                root: dir.path().to_path_buf(),
                ..ProfileConfig::default()
            }],
        };
        let registry = ProfileRegistry::load(dir.path(), &config).expect("profiles");
        let profile = registry.default_profile();
        profile.ensure_state_dirs().expect("profile state");
        std::fs::write(profile.memory_path(), "not json").expect("corrupt memory");
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

        let error =
            Cron::run_profile_maintenance_due(profile, 60, 30, CuratorPolicy::default(), now)
                .expect_err("partial cleanup must fail the cron run");

        assert!(error.contains("inspection error"));
        let audit = DaemonAuditLog::at_path(profile.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("audit");
        assert!(audit
            .iter()
            .any(|record| record.daemon == "cron" && record.outcome == "error"));
    }

    #[test]
    fn concurrent_schedule_updates_do_not_lose_jobs() {
        const JOB_COUNT: usize = 12;
        let dir = tempdir().expect("tempdir");
        let root = Arc::new(dir.path().to_path_buf());
        Cron::at_dir(&root).expect("initialize cron store");
        let barrier = Arc::new(Barrier::new(JOB_COUNT));
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let handles: Vec<_> = (0..JOB_COUNT)
            .map(|index| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut cron = Cron::at_dir(&root).expect("open cron store");
                    cron.schedule_every(format!("job-{index}"), 60, start)
                        .expect("schedule job");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("schedule thread");
        }

        let restarted = Cron::at_dir(&root).expect("reload cron store");
        assert_eq!(restarted.jobs().count(), JOB_COUNT);
        assert!((0..JOB_COUNT)
            .all(|index| { restarted.jobs().any(|job| job.id == format!("job-{index}")) }));
    }

    #[test]
    fn concurrent_runners_claim_a_due_job_once() {
        const RUNNER_COUNT: usize = 2;
        let dir = tempdir().expect("tempdir");
        let root = Arc::new(dir.path().to_path_buf());
        let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut seed = Cron::at_dir(&root).expect("initialize cron store");
        seed.schedule_every("curator", 60, start)
            .expect("schedule due job");
        drop(seed);

        let barrier = Arc::new(Barrier::new(RUNNER_COUNT));
        let executions = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..RUNNER_COUNT)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                let executions = Arc::clone(&executions);
                std::thread::spawn(move || {
                    let mut cron = Cron::at_dir(&root).expect("open cron store");
                    barrier.wait();
                    cron.run_due(start, |_| {
                        executions.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .expect("run due jobs")
                    .len()
                })
            })
            .collect();
        let claimed: usize = handles
            .into_iter()
            .map(|handle| handle.join().expect("runner thread"))
            .sum();

        assert_eq!(claimed, 1);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let restarted = Cron::at_dir(&root).expect("reload cron store");
        assert_eq!(
            restarted.jobs().next().expect("job").next_run,
            start + Duration::seconds(60)
        );
    }

    #[test]
    fn corrupt_persisted_schedule_and_invalid_jobs_are_rejected() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cron.json"), "not json").expect("corrupt cron");
        assert!(Cron::at_dir(dir.path()).is_err());

        let now = Utc::now();
        let mut cron = Cron::new();
        assert!(cron.schedule_every("never", 0, now).is_err());
        assert!(cron.schedule_every("bad/id", 60, now).is_err());
        cron.schedule_every("curator", 60, now).unwrap();
        assert!(cron.schedule_every("curator", 60, now).is_err());
    }

    #[test]
    fn oversized_sparse_cron_state_is_rejected_before_reading() {
        let directory = tempdir().expect("tempdir");
        std::fs::File::create(directory.path().join("cron.json"))
            .and_then(|file| file.set_len(MAX_CRON_STATE_BYTES + 1))
            .expect("sparse cron state");

        let error = Cron::at_dir(directory.path()).expect_err("oversized cron state");

        assert!(error.contains("maximum is"));
    }

    #[test]
    fn stale_one_second_schedule_advances_without_iterating_each_occurrence() {
        let scheduled = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

        let next = next_run_after(scheduled, 1, now).expect("bounded catch-up");

        assert_eq!(next, now + Duration::seconds(1));
    }
}

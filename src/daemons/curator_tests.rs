use super::*;
use crate::session::{
    SessionEvent, SessionMessage, SessionStore, SkillUsageRecord, ToolCallRecord,
};
use chrono::TimeZone;
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

fn set_session_file_activity(store: &SessionStore, session_id: &str, timestamp: DateTime<Utc>) {
    let path = store.sessions_dir().join(format!("{session_id}.json"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open session for timestamp update");
    let timestamp = std::time::SystemTime::from(timestamp);
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(timestamp)
            .set_modified(timestamp),
    )
    .expect("set session timestamp");
}

fn old_session(store: &SessionStore, now: DateTime<Utc>) -> Session {
    let mut session = store.create_session();
    session.started_at = Some(now - Duration::days(60));
    store.save(&mut session).expect("save old session");
    set_session_file_activity(store, &session.id, now - Duration::days(60));
    session
}

#[test]
fn curator_identifiers_reject_dot_path_components() {
    let dir = tempdir().expect("tempdir");
    let curator = profile_curator(dir.path(), 30, true);
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

    for id in [".", ".."] {
        let error = curator
            .track_managed_skill_at(id, now)
            .expect_err("dot component must not be accepted as a skill id");
        assert!(error.contains("unsupported characters"), "{error}");
        assert!(curator.pin_session(id).is_err());
        assert!(curator.pin_skill(id).is_err());
    }
}

fn add_skill_usage_at(
    store: &SessionStore,
    session_id: &str,
    skill_name: &str,
    timestamp: Option<DateTime<Utc>>,
) {
    store
        .update_session(session_id, |session| {
            if !session.active_skills.iter().any(|name| name == skill_name) {
                session.active_skills.push(skill_name.to_string());
            }
            session.skill_usage.push(SkillUsageRecord {
                skill_name: skill_name.to_string(),
                reason: Some("test usage".to_string()),
                timestamp,
            });
            Ok(())
        })
        .expect("record dated skill usage");
}

fn profile_curator(root: &Path, retention_days: i64, allow_destructive_cleanup: bool) -> Curator {
    let state = root.join("state");
    Curator::at_profile_paths(
        state.join("sessions"),
        state.join("memory.json"),
        state.join("managed-skills"),
        state.join("daemons"),
        retention_days,
        CuratorPolicy {
            allow_destructive_cleanup,
        },
    )
}

#[cfg(windows)]
fn create_directory_junction(junction: &Path, target: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(junction)
        .arg(target)
        .output()
        .expect("create directory junction");
    assert!(
        output.status.success(),
        "mklink failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[test]
fn curator_memory_cleanup_rejects_junctioned_parent_without_outside_write() {
    let root = tempdir().expect("profile root");
    let outside = tempdir().expect("outside memory parent");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let outside_path = outside.path().join("memory.json");
    MemoryStore::at_path(outside_path.clone())
        .set_user_at("style", "preserve", now - Duration::days(60))
        .expect("seed outside memory");
    let before = std::fs::read(&outside_path).expect("outside memory before cleanup");
    let mut entries_before = std::fs::read_dir(outside.path())
        .expect("outside entries before cleanup")
        .map(|entry| entry.expect("outside entry").file_name())
        .collect::<Vec<_>>();
    entries_before.sort();
    let memory_parent = root.path().join("memory-parent");
    create_directory_junction(&memory_parent, outside.path());
    let state = root.path().join("state");
    let curator = Curator::at_profile_paths(
        state.join("sessions"),
        memory_parent.join("memory.json"),
        state.join("managed-skills"),
        state.join("daemons"),
        30,
        CuratorPolicy {
            allow_destructive_cleanup: true,
        },
    );

    let report = curator
        .cleanup_old_memory_at(now)
        .expect("unsafe memory path is reported per entry");

    assert_eq!(report.memory_deleted, 0);
    assert!(!report.errors.is_empty(), "junction must be reported");
    assert_eq!(
        std::fs::read(&outside_path).expect("outside memory after cleanup"),
        before
    );
    let mut entries_after = std::fs::read_dir(outside.path())
        .expect("outside entries after cleanup")
        .map(|entry| entry.expect("outside entry").file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(entries_after, entries_before);
}

#[cfg(windows)]
#[test]
fn curator_pin_update_rejects_junctioned_state_without_outside_write() {
    let root = tempdir().expect("profile root");
    let outside = tempdir().expect("outside daemon state");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state parent");
    let daemon_dir = state.join("daemons");
    create_directory_junction(&daemon_dir, outside.path());
    let curator = Curator::at_profile_paths(
        state.join("sessions"),
        state.join("memory.json"),
        state.join("managed-skills"),
        daemon_dir,
        30,
        CuratorPolicy {
            allow_destructive_cleanup: true,
        },
    );

    let error = curator
        .pin_session("blocked")
        .expect_err("junctioned pin state must fail closed");

    assert!(error.contains("reparse point"), "{error}");
    assert_eq!(
        std::fs::read_dir(outside.path())
            .expect("outside daemon state")
            .count(),
        0
    );
}

#[cfg(windows)]
#[test]
fn curator_skill_cleanup_preserves_junction_and_outside_tree() {
    let root = tempdir().expect("profile root");
    let outside = tempdir().expect("outside skill");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let state = root.path().join("state");
    let sessions = state.join("sessions");
    let managed = state.join("managed-skills");
    drop(SessionStore::at_dir(sessions.clone()));
    std::fs::create_dir(&managed).expect("managed skills root");
    std::fs::write(outside.path().join("sentinel"), b"preserve").expect("outside sentinel");
    std::fs::write(
        outside.path().join(MANAGED_SKILL_MARKER),
        serde_json::to_vec_pretty(&ManagedSkillMetadata {
            id: "junction-skill".to_string(),
            last_used_at: now - Duration::days(60),
        })
        .expect("encode managed skill marker"),
    )
    .expect("outside managed skill marker");
    let junction = managed.join("junction-skill");
    create_directory_junction(&junction, outside.path());
    let curator = Curator::at_profile_paths(
        sessions,
        state.join("memory.json"),
        managed,
        state.join("daemons"),
        30,
        CuratorPolicy {
            allow_destructive_cleanup: true,
        },
    );

    let report = curator
        .cleanup_old_skills_at(now)
        .expect("junctioned skill is reported per entry");

    assert_eq!(report.skills_deleted, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("reparse point")),
        "{:?}",
        report.errors
    );
    assert!(junction.exists(), "junction must remain visible");
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).expect("outside sentinel retained"),
        b"preserve"
    );
}

#[test]
fn curator_cleans_all_profile_owned_state_and_respects_every_pin() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let removable_session = old_session(&store, now);
    let pinned_session = old_session(&store, now);
    curator
        .pin_session(&pinned_session.id)
        .expect("pin session");

    let memory = MemoryStore::at_path(dir.path().join("state/memory.json"));
    memory
        .set_environment_at("old_gate", "task test", now - Duration::days(60))
        .expect("old environment memory");
    memory
        .set_user_at("pinned_style", "concise", now - Duration::days(60))
        .expect("old user memory");
    memory
        .set_user_at("fresh_style", "direct", now - Duration::days(1))
        .expect("fresh user memory");
    curator
        .pin_memory(MemoryNamespace::User, "pinned_style")
        .expect("pin memory");

    let removable_skill = curator
        .track_managed_skill_at("old-skill", now - Duration::days(60))
        .expect("track old skill");
    std::fs::write(removable_skill.join("SKILL.md"), "managed").expect("skill content");
    let pinned_skill = curator
        .track_managed_skill_at("pinned-skill", now - Duration::days(60))
        .expect("track pinned skill");
    curator.pin_skill("pinned-skill").expect("pin skill");
    let unmanaged = curator.managed_skills_dir().join("source-copy");
    std::fs::create_dir_all(&unmanaged).expect("unmanaged directory");
    std::fs::write(unmanaged.join("SKILL.md"), "must survive").expect("unmanaged skill");

    let report = curator.cleanup_at(now).expect("curator run");

    assert_eq!(report.sessions_deleted, 1);
    assert_eq!(report.memory_deleted, 1);
    assert_eq!(report.skills_deleted, 1);
    assert_eq!(report.deleted, 3);
    assert!(store.load(&removable_session.id).is_none());
    assert!(store.load(&pinned_session.id).is_some());
    assert_eq!(memory.environment("old_gate"), None);
    assert_eq!(memory.user("pinned_style").as_deref(), Some("concise"));
    assert_eq!(memory.user("fresh_style").as_deref(), Some("direct"));
    assert!(!removable_skill.exists());
    assert!(pinned_skill.exists());
    assert!(unmanaged.exists());

    let audit = curator.audit_log().read_all().expect("audit records");
    assert!(audit.iter().any(|entry| {
        entry.target.as_deref() == Some(removable_session.id.as_str())
            && entry.outcome == "deleted"
            && entry.authorized
    }));
    assert!(audit.iter().any(|entry| {
        entry.target.as_deref() == Some("memory:environment:old_gate")
            && entry.outcome == "deleted"
            && entry.authorized
    }));
    assert!(audit.iter().any(|entry| {
        entry.target.as_deref() == Some("old-skill")
            && entry.outcome == "deleted"
            && entry.authorized
    }));
    assert!(audit
        .iter()
        .any(|entry| entry.outcome == "skipped_unmanaged"));
}

#[test]
fn cross_session_skill_usage_survives_restart_and_drives_retention() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let old = now - Duration::days(60);
    let recent = now - Duration::days(2);
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("rust-safety", old)
        .expect("track old managed skill");
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let first = store.create_session();
    let second = store.create_session();
    add_skill_usage_at(&store, &first.id, "Rust-Safety", Some(old));
    add_skill_usage_at(&store, &first.id, "Rust-Safety", Some(recent));
    add_skill_usage_at(&store, &second.id, "rust-safety", Some(recent));

    let restarted = profile_curator(dir.path(), 30, true);
    let aggregates = restarted
        .aggregate_skill_usage()
        .expect("aggregate persisted session usage");
    let aggregate = aggregates
        .iter()
        .find(|usage| usage.skill_name.eq_ignore_ascii_case("rust-safety"))
        .expect("cross-session aggregate");
    assert_eq!(aggregate.usage_count, 3);
    assert_eq!(aggregate.session_count, 2);
    assert_eq!(aggregate.latest_used_at, Some(recent));

    let report = restarted
        .cleanup_old_skills_at(now)
        .expect("usage-aware skill cleanup");

    assert_eq!(report.skills_deleted, 0);
    assert_eq!(report.retained, 1);
    assert!(managed.exists());
    assert!(restarted
        .audit_log()
        .read_all()
        .unwrap()
        .iter()
        .any(|entry| {
            entry.target.as_deref() == Some("rust-safety")
                && entry.outcome == "skipped_recent_usage"
        }));
}

#[test]
fn recent_skill_usage_committed_before_cleanup_lock_is_retained() {
    let dir = tempdir().expect("tempdir");
    let root = Arc::new(dir.path().to_path_buf());
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let old = now - Duration::days(60);
    let recent = now - Duration::days(1);
    let skill = profile_curator(&root, 30, true)
        .track_managed_skill_at("recently-used", old)
        .expect("track old managed skill");
    let store = SessionStore::at_dir(root.join("state/sessions"));
    let session = store.create_session();
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let cleanup_root = Arc::clone(&root);
    let cleanup_ready = Arc::clone(&ready);
    let cleanup_release = Arc::clone(&release);
    let cleanup = std::thread::spawn(move || {
        profile_curator(&cleanup_root, 30, true).cleanup_old_skills_at_with_hooks(
            now,
            || {},
            || {
                cleanup_ready.wait();
                cleanup_release.wait();
            },
            |_| {},
            || {},
        )
    });

    ready.wait();
    add_skill_usage_at(&store, &session.id, "recently-used", Some(recent));
    release.wait();
    let report = cleanup.join().expect("cleanup thread").expect("cleanup");

    assert_eq!(report.skills_deleted, 0);
    assert_eq!(report.retained, 1);
    assert!(skill.exists());
    assert!(profile_curator(&root, 30, true)
        .aggregate_skill_usage()
        .expect("aggregate committed usage")
        .iter()
        .any(|usage| {
            usage.skill_name == "recently-used" && usage.latest_used_at == Some(recent)
        }));
}

#[test]
fn legacy_active_skill_without_usage_timestamp_is_retained_fail_closed() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("legacy-skill", now - Duration::days(60))
        .expect("track old managed skill");
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let session = store.create_session();
    store
        .update_session(&session.id, |session| {
            session.started_at = Some(now - Duration::days(60));
            session.active_skills.push("legacy-skill".to_string());
            Ok(())
        })
        .expect("write legacy active skill");
    set_session_file_activity(&store, &session.id, now - Duration::days(60));

    let aggregate = curator
        .aggregate_skill_usage()
        .expect("aggregate legacy active skill")
        .into_iter()
        .find(|usage| usage.skill_name == "legacy-skill")
        .expect("legacy skill aggregate");
    assert_eq!(aggregate.usage_count, 1);
    assert_eq!(aggregate.session_count, 1);
    assert_eq!(aggregate.latest_used_at, None);
    assert!(aggregate.has_undated_usage);

    let report = curator.cleanup_at(now).expect("legacy usage cleanup");
    assert_eq!(report.sessions_deleted, 0);
    assert_eq!(report.skills_deleted, 0);
    assert!(report.retained >= 2);
    assert!(store.load(&session.id).is_some());
    assert!(managed.exists());
}

#[test]
fn raw_skill_name_uses_the_installer_slug_for_managed_retention() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("rust-tool", now - Duration::days(60))
        .expect("track slugged managed skill");
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let session = store.create_session();
    add_skill_usage_at(
        &store,
        &session.id,
        "Rust Tool!",
        Some(now - Duration::days(1)),
    );

    let report = curator
        .cleanup_old_skills_at(now)
        .expect("canonical usage cleanup");

    assert_eq!(report.skills_deleted, 0);
    assert_eq!(report.retained, 1);
    assert!(managed.exists());
}

#[test]
fn concurrent_skill_usage_updates_are_not_lost_from_the_aggregate() {
    const WRITERS: usize = 12;
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let session = store.create_session();
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|index| {
            let store = store.clone();
            let session_id = session.id.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .record_skill_usage(
                        &session_id,
                        "concurrent-skill",
                        Some(format!("writer {index}")),
                    )
                    .expect("concurrent usage write");
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("usage writer");
    }

    let aggregate = profile_curator(dir.path(), 30, true)
        .aggregate_skill_usage()
        .expect("aggregate concurrent writes")
        .into_iter()
        .find(|usage| usage.skill_name == "concurrent-skill")
        .expect("concurrent skill aggregate");
    assert_eq!(aggregate.usage_count, WRITERS);
    assert_eq!(aggregate.session_count, 1);
    assert!(aggregate.latest_used_at.is_some());
}

#[test]
fn corrupt_session_usage_blocks_managed_skill_deletion() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("must-survive", now - Duration::days(60))
        .expect("track old managed skill");
    let sessions = dir.path().join("state/sessions");
    std::fs::create_dir_all(&sessions).expect("sessions directory");
    std::fs::write(sessions.join("corrupt.json"), "{").expect("corrupt session");

    let error = curator
        .cleanup_old_skills_at(now)
        .expect_err("corrupt usage source must block deletion");

    assert!(error.contains("failed to aggregate session corrupt"));
    assert!(managed.exists());
}

#[test]
fn aggregate_session_byte_budget_blocks_reads_and_managed_skill_deletion() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("must-survive", now - Duration::days(60))
        .expect("track old managed skill");
    let sessions = dir.path().join("state/sessions");
    std::fs::create_dir_all(&sessions).expect("sessions directory");
    std::fs::write(sessions.join("000-corrupt.json"), "{").expect("corrupt session");
    for id in ["001-sparse", "002-sparse", "003-sparse", "004-sparse"] {
        std::fs::File::create(sessions.join(format!("{id}.json")))
            .and_then(|file| file.set_len(MAX_AGGREGATED_SESSION_BYTES / 4))
            .expect("create regular sparse session");
    }

    let error = curator
        .cleanup_old_skills_at(now)
        .expect_err("aggregate byte budget must block cleanup before parsing");

    assert!(error.contains("skill usage aggregation limit"));
    assert!(!error.contains("failed to parse session JSON"));
    assert!(managed.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_session_usage_blocks_managed_skill_deletion() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("tempdir");
    let external = tempdir().expect("external");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let managed = curator
        .track_managed_skill_at("must-survive", now - Duration::days(60))
        .expect("track old managed skill");
    let sessions = dir.path().join("state/sessions");
    std::fs::create_dir_all(&sessions).expect("sessions directory");
    let outside = external.path().join("outside.json");
    std::fs::write(
        &outside,
        r#"{"id":"outside","messages":[],"tool_calls":[]}"#,
    )
    .expect("external session");
    symlink(&outside, sessions.join("linked.json")).expect("session symlink");

    let error = curator
        .cleanup_old_skills_at(now)
        .expect_err("symlinked usage source must block deletion");

    assert!(
        error.contains("linked.json")
            && (error.contains("regular local file") || error.contains("failed to open")),
        "{error}"
    );
    assert!(managed.exists());
    assert!(outside.exists());
}

#[test]
fn curator_is_fail_closed_for_sessions_memory_and_skills() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, false);
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let old = old_session(&store, now);
    let memory = MemoryStore::at_path(dir.path().join("state/memory.json"));
    memory
        .set_user_at("old", "retain", now - Duration::days(60))
        .expect("old memory");
    let skill = curator
        .track_managed_skill_at("old-skill", now - Duration::days(60))
        .expect("old skill");

    let report = curator.cleanup_at(now).expect("curator run");

    assert_eq!(report.deleted, 0);
    assert_eq!(report.policy_skipped, 3);
    assert!(store.load(&old.id).is_some());
    assert_eq!(memory.user("old").as_deref(), Some("retain"));
    assert!(skill.exists());
    assert_eq!(
        curator
            .audit_log()
            .read_all()
            .unwrap()
            .iter()
            .filter(|entry| entry.outcome == "skipped_policy" && !entry.authorized)
            .count(),
        3
    );
}

#[test]
fn legacy_memory_and_unmanaged_skills_are_never_age_pruned() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 0, true);
    let memory_path = dir.path().join("state/memory.json");
    std::fs::create_dir_all(memory_path.parent().unwrap()).expect("state");
    std::fs::write(
        &memory_path,
        r#"{"environment":{"legacy":"retain"},"user":{}}"#,
    )
    .expect("legacy memory");
    let unmanaged = curator.managed_skills_dir().join("unmanaged");
    std::fs::create_dir_all(&unmanaged).expect("unmanaged skill");

    let report = curator.cleanup_at(now).expect("curator run");

    assert_eq!(report.deleted, 0);
    assert_eq!(
        MemoryStore::at_path(memory_path)
            .environment("legacy")
            .as_deref(),
        Some("retain")
    );
    assert!(unmanaged.exists());
}

#[test]
fn corrupt_state_is_reported_without_deleting_other_data() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let memory_path = dir.path().join("state/memory.json");
    std::fs::create_dir_all(memory_path.parent().unwrap()).expect("state");
    std::fs::write(&memory_path, "not json").expect("corrupt memory");
    let skill = curator
        .track_managed_skill_at("fresh", now - Duration::days(1))
        .expect("fresh skill");

    let report = curator.cleanup_at(now).expect("curator run");

    assert_eq!(report.deleted, 0);
    assert_eq!(report.errors.len(), 1);
    assert!(memory_path.exists());
    assert!(skill.exists());
    assert!(curator
        .audit_log()
        .read_all()
        .unwrap()
        .iter()
        .any(|entry| entry.action == "inspect_memory" && entry.outcome == "error"));
}

#[test]
fn pins_survive_restart_and_old_pin_format_is_compatible() {
    let dir = tempdir().expect("tempdir");
    let curator = profile_curator(dir.path(), 30, true);
    curator.pin_session("session-1").expect("session pin");
    curator
        .pin_memory(MemoryNamespace::Environment, "gate")
        .expect("memory pin");
    curator.pin_skill("rust").expect("skill pin");

    let restarted = profile_curator(dir.path(), 30, true);
    assert!(restarted.is_pinned("session-1").unwrap());
    assert!(restarted
        .load_pins()
        .unwrap()
        .memory_environment
        .contains("gate"));
    assert!(restarted.load_pins().unwrap().skills.contains("rust"));

    let legacy = dir.path().join("legacy");
    let legacy_daemon = legacy.join("daemons");
    std::fs::create_dir_all(&legacy_daemon).expect("legacy daemon");
    std::fs::write(
        legacy_daemon.join("pins.json"),
        r#"{"sessions":["old-session"]}"#,
    )
    .expect("legacy pins");
    let legacy_curator = Curator::at_paths(
        legacy.join("sessions"),
        legacy_daemon,
        30,
        CuratorPolicy::default(),
    );
    assert!(legacy_curator.is_pinned("old-session").unwrap());
}

#[test]
fn oversized_sparse_pins_and_skill_markers_are_rejected_before_reading() {
    let directory = tempdir().expect("tempdir");
    let curator = profile_curator(directory.path(), 30, true);
    let pins_parent = curator.pins_path.parent().expect("pins parent");
    std::fs::create_dir_all(pins_parent).expect("pins parent");
    std::fs::File::create(&curator.pins_path)
        .and_then(|file| file.set_len(MAX_PINS_FILE_BYTES + 1))
        .expect("sparse pins");
    assert!(curator
        .load_pins()
        .expect_err("oversized pins")
        .contains("byte limit"));

    let marker = directory.path().join("marker.json");
    std::fs::File::create(&marker)
        .and_then(|file| file.set_len(MAX_MANAGED_SKILL_MARKER_BYTES + 1))
        .expect("sparse marker");
    assert!(load_managed_skill_metadata(&marker)
        .expect_err("oversized marker")
        .contains("byte limit"));
}

#[test]
fn managed_skill_marker_replacement_during_read_fails_closed() {
    let directory = tempdir().expect("tempdir");
    let marker = directory.path().join("marker.json");
    let replacement = directory.path().join("replacement.json");
    let displaced = directory.path().join("displaced.json");
    std::fs::write(
        &marker,
        r#"{"id":"original","last_used_at":"2026-01-01T00:00:00Z"}"#,
    )
    .expect("original marker");
    std::fs::write(
        &replacement,
        r#"{"id":"forged","last_used_at":"2020-01-01T00:00:00Z"}"#,
    )
    .expect("replacement marker");

    let error = read_bounded_state_file_with_hook(
        &marker,
        MAX_MANAGED_SKILL_MARKER_BYTES,
        "managed skill marker",
        || {
            std::fs::rename(&marker, &displaced).expect("displace marker");
            std::fs::rename(&replacement, &marker).expect("replace marker");
        },
    )
    .expect_err("replacement identity must fail closed");

    assert!(error.contains("identity changed"), "{error}");
    assert!(displaced.exists());
}

#[test]
fn managed_skill_substitution_before_quarantine_is_preserved() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let skill = curator
        .track_managed_skill_at("replace-before-quarantine", now - Duration::days(60))
        .expect("track old skill");
    std::fs::write(skill.join("SKILL.md"), b"original").expect("skill content");
    let displaced = curator.managed_skills_dir().join("displaced-skill");

    let report = curator
        .cleanup_old_skills_at_with_hooks(
            now,
            || {},
            || {},
            |path| {
                if path.ends_with("replace-before-quarantine") {
                    std::fs::rename(path, &displaced).expect("displace selected skill");
                    std::fs::create_dir(path).expect("replacement skill directory");
                    std::fs::write(path.join("replacement"), b"preserve")
                        .expect("replacement sentinel");
                }
            },
            || {},
        )
        .expect("cleanup reports substitution per entry");

    assert_eq!(report.skills_deleted, 0);
    assert!(!report.errors.is_empty());
    assert_eq!(
        std::fs::read(skill.join("replacement")).expect("replacement preserved"),
        b"preserve"
    );
    assert_eq!(
        std::fs::read(displaced.join("SKILL.md")).expect("original preserved"),
        b"original"
    );
}

#[test]
fn retracking_after_quarantine_preserves_new_skill_generation() {
    use std::cell::RefCell;

    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    curator
        .track_managed_skill_at("retracked", now - Duration::days(60))
        .expect("track old skill");
    let recreated = RefCell::new(None);

    let report = curator
        .cleanup_old_skills_at_with_hooks(
            now,
            || {},
            || {},
            |_| {},
            || {
                let path = curator
                    .track_managed_skill_at("retracked", now)
                    .expect("retrack after quarantine");
                *recreated.borrow_mut() = Some(path);
            },
        )
        .expect("cleanup old generation");

    assert_eq!(report.skills_deleted, 1);
    let recreated = recreated.into_inner().expect("recreated skill path");
    assert!(recreated.exists());
    let marker: ManagedSkillMetadata = serde_json::from_slice(
        &std::fs::read(recreated.join(MANAGED_SKILL_MARKER)).expect("new marker"),
    )
    .expect("decode new marker");
    assert_eq!(marker.last_used_at, now);
}

#[test]
fn oversized_managed_skill_depth_is_preserved_in_quarantine() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let skill = curator
        .track_managed_skill_at("deep-skill", now - Duration::days(60))
        .expect("track old skill");
    let mut nested = skill;
    for index in 0..=MAX_MANAGED_SKILL_TREE_DEPTH {
        nested = nested.join(format!("d{index}"));
        std::fs::create_dir(&nested).expect("nested skill directory");
    }
    std::fs::write(nested.join("sentinel"), b"preserve").expect("deep sentinel");

    let report = curator
        .cleanup_old_skills_at(now)
        .expect("bounded cleanup reports per-skill error");

    assert_eq!(report.skills_deleted, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("depth limit")),
        "{:?}",
        report.errors
    );
    let quarantine = managed_skill_quarantine_path(curator.managed_skills_dir(), "deep-skill");
    assert!(
        quarantine.exists(),
        "oversized tree must remain quarantined"
    );
}

#[test]
fn managed_skill_tree_entry_limit_fails_before_deletion() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("first"), b"retain first").expect("first file");
    std::fs::write(directory.path().join("second"), b"retain second").expect("second file");
    let stable = crate::daemons::state::StableDirectory::open(directory.path())
        .expect("stable managed skill directory");

    let error = delete_managed_skill_tree_contents(
        &stable,
        0,
        &mut ManagedSkillTreeBudget::default(),
        ManagedSkillTreeLimits {
            max_depth: 4,
            max_entries: 1,
            max_name_bytes: 1024,
        },
    )
    .expect_err("entry-count limit must stop traversal");

    assert!(error.contains("bounded scan limit"), "{error}");
    assert_eq!(
        std::fs::read(directory.path().join("first")).expect("first retained"),
        b"retain first"
    );
    assert_eq!(
        std::fs::read(directory.path().join("second")).expect("second retained"),
        b"retain second"
    );
}

#[test]
fn managed_skill_tree_aggregate_name_limit_spans_nested_directories() {
    let directory = tempdir().expect("tempdir");
    let child = directory.path().join("child");
    std::fs::create_dir(&child).expect("child directory");
    let leaf_name = "nested-name";
    std::fs::write(child.join(leaf_name), b"retain nested").expect("nested file");
    let stable = crate::daemons::state::StableDirectory::open(directory.path())
        .expect("stable managed skill directory");
    let aggregate_name_bytes = "child".len() + leaf_name.len();

    let error = delete_managed_skill_tree_contents(
        &stable,
        0,
        &mut ManagedSkillTreeBudget::default(),
        ManagedSkillTreeLimits {
            max_depth: 4,
            max_entries: 2,
            max_name_bytes: aggregate_name_bytes - 1,
        },
    )
    .expect_err("aggregate filename limit must stop nested traversal");

    assert!(error.contains("bounded scan limit"), "{error}");
    assert_eq!(
        std::fs::read(child.join(leaf_name)).expect("nested file retained"),
        b"retain nested"
    );
}

#[test]
fn latest_activity_across_every_session_source_prevents_cleanup() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let old = now - Duration::days(60);
    let fresh = now - Duration::days(1);
    let curator = profile_curator(dir.path(), 30, true);
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let mut sessions = Vec::new();

    for source in ["message", "tool", "event", "skill", "started", "file"] {
        let mut session = store.create_session();
        session.started_at = Some(old);
        match source {
            "message" => session.messages.push(SessionMessage {
                index: 0,
                role: "user".to_string(),
                content: "recent".to_string(),
                timestamp: Some(fresh),
                attachments: Vec::new(),
            }),
            "tool" => session.tool_calls.push(ToolCallRecord {
                timestamp: Some(fresh),
                ..ToolCallRecord::default()
            }),
            "event" => session.events.push(SessionEvent {
                index: 0,
                kind: "recent".to_string(),
                details: serde_json::Value::Null,
                timestamp: Some(fresh),
            }),
            "skill" => session.skill_usage.push(SkillUsageRecord {
                skill_name: "recent".to_string(),
                reason: None,
                timestamp: Some(fresh),
            }),
            "started" => session.started_at = Some(fresh),
            "file" => {}
            _ => unreachable!(),
        }
        store.save(&mut session).expect("save active session");
        set_session_file_activity(
            &store,
            &session.id,
            if source == "file" { fresh } else { old },
        );
        sessions.push(session.id);
    }

    let report = curator.cleanup_old_sessions_at(now).expect("cleanup");

    assert_eq!(report.sessions_deleted, 0);
    assert_eq!(report.retained, sessions.len());
    assert!(sessions.iter().all(|id| store.load(id).is_some()));
}

#[test]
fn deletion_recheck_retains_session_with_new_activity() {
    let dir = tempdir().expect("tempdir");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let cutoff = now - Duration::days(30);
    let curator = profile_curator(dir.path(), 30, true);
    let store = SessionStore::at_dir(dir.path().join("state/sessions"));
    let session = old_session(&store, now);
    let initially_loaded = store.load(&session.id).expect("old session");
    assert!(is_session_old(
        &initially_loaded,
        session_file_activity(&store.sessions_dir().join(format!("{}.json", session.id))),
        cutoff
    ));

    store
        .update_session(&session.id, |latest| {
            latest.events.push(SessionEvent {
                index: 0,
                kind: "recent".to_string(),
                details: serde_json::Value::Null,
                timestamp: Some(now - Duration::days(1)),
            });
            Ok(())
        })
        .expect("record new activity");
    set_session_file_activity(&store, &session.id, now - Duration::days(60));

    assert_eq!(
        curator
            .delete_session_if_old(&store, &session.id, cutoff)
            .expect("recheck session"),
        CuratorSessionDelete::Retained
    );
    assert!(store.load(&session.id).is_some());
}

#[test]
fn concurrent_pin_updates_do_not_lose_entries() {
    const PIN_COUNT: usize = 12;
    let dir = tempdir().expect("tempdir");
    let root = Arc::new(dir.path().to_path_buf());
    let barrier = Arc::new(Barrier::new(PIN_COUNT));
    let handles: Vec<_> = (0..PIN_COUNT)
        .map(|index| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let curator = profile_curator(&root, 30, true);
                barrier.wait();
                curator
                    .pin_session(&format!("session-{index}"))
                    .expect("pin session");
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("pin thread");
    }

    let pins = profile_curator(&root, 30, true)
        .load_pins()
        .expect("load pins");
    assert_eq!(pins.sessions.len(), PIN_COUNT);
    assert!((0..PIN_COUNT).all(|index| pins.sessions.contains(&format!("session-{index}"))));
}

#[test]
fn pin_commit_rejects_newer_valid_file_without_overwriting_it() {
    let dir = tempdir().expect("tempdir");
    let curator = profile_curator(dir.path(), 30, true);
    curator
        .pin_session("original")
        .expect("seed original pin file");
    let parent = curator.pins_path.parent().expect("pins parent");
    let replacement = parent.join("replacement-pins.json");
    let displaced = parent.join("displaced-pins.json");
    let mut newer = PinFile::default();
    newer.sessions.insert("newer".to_string());
    std::fs::write(
        &replacement,
        serde_json::to_vec_pretty(&newer).expect("encode newer pins"),
    )
    .expect("write newer pins");

    let error = curator
        .update_pin_with_commit_hook(PinKind::Session, "attempted", true, || {
            std::fs::rename(&curator.pins_path, &displaced).map_err(|error| error.to_string())?;
            std::fs::rename(&replacement, &curator.pins_path).map_err(|error| error.to_string())?;
            Ok(())
        })
        .expect_err("pin publication must reject a substituted prior file");

    assert!(error.contains("identity changed"), "{error}");
    let visible = curator.load_pins().expect("load newer visible pins");
    assert_eq!(visible.sessions, BTreeSet::from(["newer".to_string()]));
    let displaced: PinFile =
        serde_json::from_slice(&std::fs::read(displaced).expect("read displaced original pins"))
            .expect("parse displaced original pins");
    assert!(displaced.sessions.contains("original"));
    assert!(!visible.sessions.contains("attempted"));
}

#[test]
fn successful_memory_pin_before_cleanup_commit_preserves_entry() {
    let dir = tempdir().expect("tempdir");
    let root = Arc::new(dir.path().to_path_buf());
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let memory = MemoryStore::at_path(root.join("state/memory.json"));
    memory
        .set_user_at("style", "concise", now - Duration::days(60))
        .expect("old memory");
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let cleanup_root = Arc::clone(&root);
    let cleanup_ready = Arc::clone(&ready);
    let cleanup_release = Arc::clone(&release);
    let cleanup = std::thread::spawn(move || {
        profile_curator(&cleanup_root, 30, true).cleanup_old_memory_at_with_hook(now, || {
            cleanup_ready.wait();
            cleanup_release.wait();
        })
    });

    ready.wait();
    profile_curator(&root, 30, true)
        .pin_memory(MemoryNamespace::User, "style")
        .expect("pin memory before cleanup commit");
    release.wait();
    let report = cleanup.join().expect("cleanup thread").expect("cleanup");

    assert_eq!(report.memory_deleted, 0);
    assert_eq!(report.pinned, 1);
    assert_eq!(memory.user("style").as_deref(), Some("concise"));
}

#[test]
fn successful_skill_pin_before_cleanup_commit_preserves_directory() {
    let dir = tempdir().expect("tempdir");
    let root = Arc::new(dir.path().to_path_buf());
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let skill = profile_curator(&root, 30, true)
        .track_managed_skill_at("stable-skill", now - Duration::days(60))
        .expect("old managed skill");
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let cleanup_root = Arc::clone(&root);
    let cleanup_ready = Arc::clone(&ready);
    let cleanup_release = Arc::clone(&release);
    let cleanup = std::thread::spawn(move || {
        profile_curator(&cleanup_root, 30, true).cleanup_old_skills_at_with_hook(now, || {
            cleanup_ready.wait();
            cleanup_release.wait();
        })
    });

    ready.wait();
    profile_curator(&root, 30, true)
        .pin_skill("stable-skill")
        .expect("pin skill before cleanup commit");
    release.wait();
    let report = cleanup.join().expect("cleanup thread").expect("cleanup");

    assert_eq!(report.skills_deleted, 0);
    assert_eq!(report.pinned, 1);
    assert!(skill.exists());
}

#[cfg(unix)]
#[test]
fn memory_symlink_is_rejected_without_touching_external_data() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("tempdir");
    let external = tempdir().expect("external");
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let curator = profile_curator(dir.path(), 30, true);
    let external_memory = external.path().join("memory.json");
    std::fs::write(
        &external_memory,
        r#"{"environment":{"outside":"unchanged"},"user":{}}"#,
    )
    .expect("external memory");
    let profile_memory = dir.path().join("state/memory.json");
    std::fs::create_dir_all(profile_memory.parent().unwrap()).expect("profile state");
    symlink(&external_memory, &profile_memory).expect("memory symlink");

    let error = curator
        .cleanup_old_memory_at(now)
        .expect_err("memory symlink must be rejected");

    assert!(error.contains("must not be a symlink"));
    assert!(std::fs::read_to_string(external_memory)
        .unwrap()
        .contains("unchanged"));
}

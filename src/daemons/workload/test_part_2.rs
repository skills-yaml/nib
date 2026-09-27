use super::*;

#[cfg(any(unix, windows))]
#[test]
fn constructor_globally_migrates_untouched_legacy_locks_and_preserves_controls() {
    let root = tempdir().expect("tempdir");
    let daemon_dir = root.path().join("state/daemons");
    let tasks_dir = daemon_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).expect("task directory");

    let preserved_visible = tasks_dir.join(".task-stripe-00.lock");
    let preserved_anchor = daemon_dir.join(".task-stripe-00.lock.anchor");
    std::fs::write(&preserved_visible, b"current stripe").expect("current stripe");
    std::fs::hard_link(&preserved_visible, &preserved_anchor).expect("current stripe anchor");
    let preserved_identity = task_file_identity(
        &OpenOptions::new()
            .read(true)
            .write(true)
            .open(&preserved_anchor)
            .expect("current stripe anchor"),
        &preserved_anchor,
    )
    .expect("current stripe identity");

    let admission = tasks_dir.join(".admission.lock");
    let admission_anchor = daemon_dir.join(".admission.task.lock.anchor");
    std::fs::write(&admission, b"current admission").expect("current admission");
    std::fs::hard_link(&admission, &admission_anchor).expect("current admission anchor");
    let admission_identity = task_file_identity(
        &OpenOptions::new()
            .read(true)
            .write(true)
            .open(&admission_anchor)
            .expect("current admission anchor"),
        &admission_anchor,
    )
    .expect("current admission identity");

    for index in 0..128 {
        let id = format!("untouched-{index}");
        let visible = tasks_dir.join(format!("{id}.lock"));
        let anchor = daemon_dir.join(format!(".{id}.task.lock.anchor"));
        std::fs::write(&visible, b"legacy").expect("legacy lock");
        std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
    }

    let store = DurableTaskStore::at_daemon_dir(&daemon_dir)
        .expect("constructor performs global migration");
    for index in 0..128 {
        let id = format!("untouched-{index}");
        assert!(!tasks_dir.join(format!("{id}.lock")).exists());
        assert!(!daemon_dir.join(format!(".{id}.task.lock.anchor")).exists());
    }
    assert_eq!(
        task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&preserved_anchor)
                .expect("preserved stripe anchor"),
            &preserved_anchor,
        )
        .expect("preserved stripe identity"),
        preserved_identity
    );
    assert_eq!(
        task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&admission_anchor)
                .expect("preserved admission anchor"),
            &admission_anchor,
        )
        .expect("preserved admission identity"),
        admission_identity
    );
    assert_eq!(
        std::fs::read_dir(&store.tasks_dir)
            .expect("fixed task controls")
            .count(),
        TASK_LOCK_STRIPES + 1
    );
}

#[cfg(any(unix, windows))]
#[test]
fn global_legacy_lock_migration_fails_closed_for_a_live_owner() {
    let root = tempdir().expect("tempdir");
    let daemon_dir = root.path().join("state/daemons");
    let tasks_dir = daemon_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).expect("task directory");
    let visible = tasks_dir.join("untouched-live.lock");
    let anchor = daemon_dir.join(".untouched-live.task.lock.anchor");
    std::fs::write(&visible, b"live legacy").expect("legacy lock");
    std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&anchor)
        .expect("legacy owner");
    owner.try_lock().expect("hold legacy lock");

    let error = DurableTaskStore::at_daemon_dir(&daemon_dir)
        .expect_err("global migration must reject a live legacy owner");
    assert!(error.contains("still owned"), "{error}");
    assert!(visible.exists());
    assert!(anchor.exists());

    drop(owner);
    DurableTaskStore::at_daemon_dir(&daemon_dir)
        .expect("released legacy owner can be globally migrated");
    assert!(!visible.exists());
    assert!(!anchor.exists());
}

#[cfg(unix)]
#[test]
fn task_record_rejects_regular_file_replacement_during_open() {
    let (directory, store, session_store) = fixture();
    store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "replace-record",
        ))
        .expect("prepare record");
    let path = store.task_path("replace-record");
    let displaced = store.tasks_dir.join("displaced-record");
    let replacement = std::fs::read(&path).expect("replacement contents");
    let error = store
        .read_path_bounded_with_hook(&path, MAX_TASK_RECORD_BYTES, || {
            std::fs::rename(&path, &displaced)
                .map_err(|error| format!("failed to displace record: {error}"))?;
            std::fs::write(&path, replacement)
                .map_err(|error| format!("failed to replace record: {error}"))
        })
        .expect_err("replacement record inode must be rejected");
    assert!(
        error.contains("identity changed while it was in use"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn effectful_worker_commit_targets_original_directory_after_replacement() {
    let (directory, store, session_store) = fixture();
    let task_id = "paired-directory-replacement";
    let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    let task_path = store.task_path(task_id);
    let before = std::fs::read(&task_path).expect("record before replacement");
    let displaced_tasks = store.daemon_dir().join("tasks.displaced-after-effect");
    let replacement_task = store.tasks_dir.join(format!("{task_id}.json"));
    let effect_marker = directory.path().join("external-effect");

    let error = store
        .update_owned_with_hook(
            task_id,
            &owner,
            |task| {
                std::fs::write(&effect_marker, b"published")
                    .map_err(|error| format!("failed to publish test effect: {error}"))?;
                finish_task_file(task, "completed", Some(json!({"exit_code": 0})), None);
                Ok(())
            },
            || {
                std::fs::rename(&store.tasks_dir, &displaced_tasks)
                    .map_err(|error| format!("failed to detach task directory: {error}"))?;
                std::fs::create_dir(&store.tasks_dir)
                    .map_err(|error| format!("failed to replace task directory: {error}"))?;
                std::fs::write(&replacement_task, &before)
                    .map_err(|error| format!("failed to seed replacement task: {error}"))
            },
        )
        .expect_err("paired commit must report the detached task directory");
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        std::fs::read(&effect_marker).expect("external effect marker"),
        b"published"
    );
    let committed: DurableTaskFile = serde_json::from_slice(
        &std::fs::read(displaced_tasks.join(format!("{task_id}.json")))
            .expect("original capability record"),
    )
    .expect("decode committed original record");
    assert_eq!(committed.record.status, "completed");
    let replacement: DurableTaskFile = serde_json::from_slice(
        &std::fs::read(&replacement_task).expect("replacement sentinel record"),
    )
    .expect("decode replacement record");
    assert_eq!(replacement.record.status, "running");
}

#[test]
fn effectful_worker_commit_does_not_overwrite_replaced_task_record() {
    let (directory, store, session_store) = fixture();
    let task_id = "paired-file-replacement";
    let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    let task_path = store.task_path(task_id);
    let displaced = store.tasks_dir.join("displaced-paired-task.json");
    let mut replacement: DurableTaskFile =
        serde_json::from_slice(&std::fs::read(&task_path).expect("task before replacement"))
            .expect("decode replacement base");
    replacement.record.status = "failed".to_string();
    replacement.record.error = Some("newer reconciler decision".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement task");
    let effect_marker = directory.path().join("paired-file-effect");

    let error = store
        .update_owned_with_hook(
            task_id,
            &owner,
            |task| {
                std::fs::write(&effect_marker, b"published").map_err(|error| error.to_string())?;
                finish_task_file(task, "completed", Some(json!({"exit_code": 0})), None);
                Ok(())
            },
            || {
                std::fs::rename(&task_path, &displaced).map_err(|error| error.to_string())?;
                std::fs::write(&task_path, &replacement_bytes).map_err(|error| error.to_string())
            },
        )
        .expect_err("replaced task record must reject post-effect commit");

    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        std::fs::read(&effect_marker).expect("external effect"),
        b"published"
    );
    let visible: DurableTaskFile =
        serde_json::from_slice(&std::fs::read(&task_path).expect("newer visible record"))
            .expect("decode newer record");
    assert_eq!(visible.record.status, "failed");
    assert_eq!(
        visible.record.error.as_deref(),
        Some("newer reconciler decision")
    );
    let displaced_record: DurableTaskFile =
        serde_json::from_slice(&std::fs::read(displaced).expect("displaced prior record"))
            .expect("decode displaced record");
    assert_eq!(displaced_record.record.status, "running");
}

#[test]
fn worker_launch_compensation_failure_is_returned_and_audited() {
    let (directory, store, session_store) = fixture();
    let id = "worker-compensation";
    store
        .prepare_terminal(terminal_request(&directory, &session_store, id))
        .expect("prepare worker task");
    store
        .update(id, |task| {
            task.record.status = "starting".to_string();
            task.worker_lease = Some(WorkerLease {
                token: "lease".to_string(),
            });
            Ok(())
        })
        .expect("seed worker lease");
    let path = store.task_path(id);
    std::fs::remove_file(&path).expect("remove worker record");
    std::fs::create_dir(&path).expect("inject terminalization failure");

    let error = store.finish_worker_launch_failure(
        id,
        "lease",
        "injected worker launch failure".to_string(),
    );
    assert!(error.contains("durable compensation failed"), "{error}");
    assert!(error.contains("daemon audit"), "{error}");
    let records = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
        .read_all()
        .expect("compensation audit");
    assert!(records.iter().any(|record| {
        record.action == "worker_launch"
            && record.target.as_deref() == Some(id)
            && record.outcome == "compensation_failed"
    }));
}

#[cfg(unix)]
#[test]
fn task_record_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let (directory, store, _) = fixture();
    let outside = directory.path().join("outside.json");
    std::fs::write(&outside, "{}").unwrap();
    symlink(&outside, store.task_path("linked")).unwrap();
    assert!(store.get("linked").is_err());
}

#[test]
fn oversized_sparse_task_record_is_rejected_before_reading() {
    let (_directory, store, _) = fixture();
    let path = store.task_path("oversized");
    File::create(&path)
        .and_then(|file| file.set_len(MAX_TASK_RECORD_BYTES + 1))
        .expect("oversized sparse task record");

    let error = store
        .get("oversized")
        .expect_err("oversized task record must fail closed");

    assert!(error.contains("maximum is"));
}

#[cfg(unix)]
#[test]
fn task_store_rechecks_directory_after_constructor() {
    use std::os::unix::fs::symlink;

    let (directory, store, session_store) = fixture();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    let displaced = directory.path().join("tasks.displaced");
    std::fs::rename(&store.tasks_dir, &displaced).expect("displace task directory");
    symlink(&outside, &store.tasks_dir).expect("swap task directory");

    let error = store
        .prepare_terminal(DurableTerminalRequest {
            id: "blocked".to_string(),
            command: "printf blocked".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect_err("swapped task directory must fail closed");

    assert!(error.contains("unsafe") || error.contains("symlink"));
    assert!(!outside.join("blocked.json").exists());
    assert!(!outside.join("blocked.lock").exists());
    std::fs::remove_file(&store.tasks_dir).expect("remove replacement symlink");
    std::fs::rename(displaced, &store.tasks_dir).expect("restore task directory");
}

#[test]
fn direct_durable_requests_enforce_public_tool_bounds() {
    let (directory, store, session_store) = fixture();
    let terminal_error = store
        .prepare_terminal(DurableTerminalRequest {
            id: "oversized-command".to_string(),
            command: "x".repeat(MAX_TERMINAL_COMMAND_BYTES + 1),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect_err("oversized direct command must be rejected");
    assert!(terminal_error.contains("command exceeds"));

    let schedule_error = store
        .prepare_schedule(DurableScheduleRequest {
            id: "oversized-prompt".to_string(),
            prompt: "x".repeat(MAX_SCHEDULE_PROMPT_BYTES + 1),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(1),
            interval: Duration::from_secs(1),
            repeat_count: 1,
        })
        .expect_err("oversized direct prompt must be rejected");
    assert!(schedule_error.contains("prompt exceeds"));
}

use super::*;

#[cfg(windows)]
#[test]
fn durable_reservation_canonicalizes_a_dos_short_project_root() {
    let repository = repository();
    let canonical_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let short_root = windows_dos_short_path(&canonical_root);
    if short_root == windows_path_without_verbatim_prefix(&canonical_root) {
        return;
    }

    let id = "dos-short-reservation";
    let relative_path = Path::new(".nib/worktrees/subagents").join(id);
    let short_worktree_path = short_root.join(&relative_path);
    crate::fs_security::ensure_directory_without_symlinks(
        short_worktree_path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let reservation = reserve_managed_worktree_sync_controlled(
        &short_root,
        ManagedWorktreeKind::Subagent,
        id,
        &short_worktree_path,
        &branch_name(id),
        None,
    )
    .expect("reserve through DOS short project root");
    let record = reservation.intent.revision.record.clone();
    let canonical_worktree_path = canonical_root.join(relative_path);

    assert_eq!(record.project_root, canonical_root);
    assert_eq!(record.worktree_path, canonical_worktree_path);
    assert_eq!(
        record.worktree_staging_path.parent(),
        canonical_worktree_path.parent()
    );
    assert!(record.common_git_dir.starts_with(&record.project_root));
    assert!(
        reservation
            .intent
            .revision
            .path
            .starts_with(&record.project_root),
        "durable ownership path retained the DOS short root"
    );
    validate_durable_ownership_record(&record, &short_root, ManagedWorktreeKind::Subagent, id)
        .expect("validate canonical ownership through DOS short root");
    drop(reservation);

    let reloaded =
        load_durable_ownership_revision(&canonical_root, ManagedWorktreeKind::Subagent, id)
            .expect("reload reservation through canonical root")
            .expect("durable reservation");
    assert_eq!(reloaded.record.project_root, record.project_root);
    assert_eq!(reloaded.record.worktree_path, record.worktree_path);
    drop(reloaded);

    Worktree::remove(&canonical_root, id)
        .expect("clean short-root reservation through canonical root");
    let tombstone = load_durable_ownership_revision(&short_root, ManagedWorktreeKind::Subagent, id)
        .expect("reload cleanup through DOS short root")
        .expect("durable cleanup tombstone");
    assert_eq!(tombstone.record.phase, DurableOwnershipPhase::Complete);
    assert!(!canonical_worktree_path.exists());

    let created_id = "dos-short-create";
    let created = Worktree::create(&short_root, created_id)
        .expect("create registered worktree through DOS short project root");
    assert_eq!(
        created.path,
        canonical_root
            .join(".nib/worktrees/subagents")
            .join(created_id)
    );
    assert!(created.path.join(".git").is_file());
    Worktree::remove(&short_root, created_id)
        .expect("remove registered worktree through DOS short project root");
    let created_tombstone =
        load_durable_ownership_revision(&canonical_root, ManagedWorktreeKind::Subagent, created_id)
            .expect("reload created ownership through canonical root")
            .expect("created cleanup tombstone");
    assert_eq!(
        created_tombstone.record.phase,
        DurableOwnershipPhase::Complete
    );
    assert!(!created.path.exists());
}

#[cfg(windows)]
#[test]
fn managed_worktree_create_adapts_a_verbatim_destination_for_git() {
    let repository = repository();
    let canonical_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "windows-verbatim-git-argument";

    let worktree = Worktree::create(&canonical_root, id)
        .expect("Git accepts the adapted canonical worktree destination");

    assert_eq!(
        worktree.path,
        canonical_root.join(".nib/worktrees/subagents").join(id)
    );
    assert!(worktree.path.join(".git").is_file());
    Worktree::remove(&canonical_root, id).expect("remove adapted worktree");
    assert!(!worktree.path.exists());
}

#[test]
fn managed_git_commands_disable_external_configuration_sources() {
    let mut command = Command::new("git");
    configure_git_command_sync(&mut command, Path::new("."), &[OsString::from("status")]);
    let environment = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        environment.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        environment.get("GIT_CONFIG_SYSTEM").map(String::as_str),
        Some(git_null_device())
    );
    assert_eq!(
        environment.get("GIT_CONFIG_GLOBAL").map(String::as_str),
        Some(git_null_device())
    );
    assert_eq!(
        environment.get("GIT_ATTR_NOSYSTEM").map(String::as_str),
        Some("1")
    );
    for inherited in ["HOME", "RUSTUP_HOME", "CARGO_HOME"] {
        assert!(
            !environment.contains_key(inherited),
            "managed Git inherited {inherited}"
        );
    }
    assert!(!environment.contains_key("GIT_PAGER"));
    assert!(!environment.contains_key("PAGER"));

    let arguments = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(arguments.first().map(String::as_str), Some("--no-pager"));
    assert!(arguments.contains(&format!("core.hooksPath={}", git_null_device())));
    assert!(arguments.contains(&format!("core.attributesFile={}", git_null_device())));
    assert!(arguments.contains(&"core.fsmonitor=false".to_string()));
    assert!(arguments.contains(&"credential.helper=".to_string()));
    assert!(arguments.contains(&"protocol.ext.allow=never".to_string()));
}

#[cfg(unix)]
#[test]
fn managed_git_rejects_executable_repository_helpers_without_running_them() {
    use std::os::unix::fs::PermissionsExt;

    let repository = repository();
    let marker = repository.path().join("helper-ran");
    let helper = repository.path().join("hostile-helper.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
    )
    .expect("helper script");
    let mut permissions = std::fs::metadata(&helper)
        .expect("helper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).expect("helper permissions");
    let helper_command = helper.to_string_lossy().into_owned();
    for key in [
        "filter.hostile.smudge",
        "diff.hostile.textconv",
        "merge.hostile.driver",
        "credential.https://example.invalid.helper",
        "core.sshCommand",
    ] {
        let configured = Command::new("git")
            .current_dir(repository.path())
            .args(["config", key, helper_command.as_str()])
            .status()
            .expect("hostile repository config");
        assert!(configured.success());
    }
    for (key, value) in [
        (
            "include.path",
            repository.path().join("missing-include.config"),
        ),
        (
            "includeIf.gitdir:/tmp/.path",
            repository.path().join("missing-conditional-include.config"),
        ),
    ] {
        let configured = Command::new("git")
            .current_dir(repository.path())
            .args(["config", key, value.to_string_lossy().as_ref()])
            .status()
            .expect("hostile repository include config");
        assert!(configured.success());
    }
    std::fs::write(
        repository.path().join(".gitattributes"),
        "README.md filter=hostile diff=hostile merge=hostile\n",
    )
    .expect("attributes fixture");
    git_stdout(repository.path(), &["add", ".gitattributes"]);
    git_stdout(
        repository.path(),
        &["commit", "-m", "hostile attributes fixture"],
    );

    let error = Worktree::create(repository.path(), "hostile-config")
        .expect_err("executable repository config must fail closed");

    assert!(
        error.contains("executable repository configuration"),
        "{error}"
    );
    assert!(error.contains("filter.hostile.smudge"), "{error}");
    assert!(
        error.contains("credential.https://example.invalid.helper"),
        "{error}"
    );
    assert!(error.contains("core.sshcommand"), "{error}");
    assert!(error.contains("include.path"), "{error}");
    assert!(error.contains("includeif.gitdir:/tmp/.path"), "{error}");
    assert!(!marker.exists(), "repository helper executed");
    assert!(!repository
        .path()
        .join(".nib/worktrees/subagents/hostile-config")
        .exists());
    let reference = "refs/heads/nib/subagent/hostile-config";
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", reference])
        .status()
        .expect("hostile branch lookup");
    assert_eq!(branch.code(), Some(1), "hostile branch was created");
}

#[cfg(unix)]
#[test]
fn managed_git_rejects_executable_worktree_configuration_without_running_it() {
    use std::os::unix::fs::PermissionsExt;

    let repository = repository();
    std::fs::write(
        repository.path().join(".gitattributes"),
        "README.md filter=worktree-hostile\n",
    )
    .expect("attributes fixture");
    git_stdout(repository.path(), &["add", ".gitattributes"]);
    git_stdout(
        repository.path(),
        &["commit", "-m", "worktree config fixture"],
    );
    let marker = repository.path().join("worktree-helper-ran");
    let helper = repository.path().join("worktree-helper.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
    )
    .expect("worktree helper");
    let mut permissions = std::fs::metadata(&helper)
        .expect("worktree helper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).expect("worktree helper permissions");
    git_stdout(
        repository.path(),
        &["config", "extensions.worktreeConfig", "true"],
    );
    let helper_command = helper.to_string_lossy().into_owned();
    git_stdout(
        repository.path(),
        &[
            "config",
            "--worktree",
            "filter.worktree-hostile.smudge",
            &helper_command,
        ],
    );

    let error = Worktree::create(repository.path(), "hostile-worktree-config")
        .expect_err("worktree-scoped executable config must fail closed");

    assert!(
        error.contains("executable repository configuration"),
        "{error}"
    );
    assert!(error.contains("filter.worktree-hostile.smudge"), "{error}");
    assert!(!marker.exists(), "worktree-scoped filter executed");
    assert!(!repository
        .path()
        .join(".nib/worktrees/subagents/hostile-worktree-config")
        .exists());
}

#[test]
fn create_preserves_a_preexisting_branch_ref() {
    let repository = repository();
    let reference = "refs/heads/nib/subagent/preexisting";
    let original = git_stdout(repository.path(), &["rev-parse", "HEAD"]);
    git_stdout(repository.path(), &["update-ref", reference, &original]);

    let error = Worktree::create(repository.path(), "preexisting")
        .expect_err("preexisting branch claim must fail");

    assert!(error.contains("already has a loose ref"), "{error}");
    assert_eq!(
        git_stdout(
            repository.path(),
            &["show-ref", "--hash", "--verify", reference]
        ),
        original
    );
    assert!(!repository
        .path()
        .join(".nib/worktrees/subagents/preexisting")
        .exists());
}

#[test]
fn create_rejects_and_preserves_an_exact_packed_branch_ref() {
    let repository = repository();
    let id = "packed-exact";
    let reference = format!("refs/heads/{}", branch_name(id));

    assert_create_rejects_packed_ref_namespace_conflict(repository.path(), id, &reference);
}

#[test]
fn create_rejects_and_preserves_a_packed_branch_ancestor() {
    let repository = repository();

    assert_create_rejects_packed_ref_namespace_conflict(
        repository.path(),
        "packed-ancestor",
        "refs/heads/nib/subagent",
    );
}

#[test]
fn create_rejects_and_preserves_a_packed_branch_descendant() {
    let repository = repository();
    let id = "packed-descendant";
    let descendant = format!("refs/heads/{}/child", branch_name(id));

    assert_create_rejects_packed_ref_namespace_conflict(repository.path(), id, &descendant);
}

#[test]
fn create_rejects_a_preexisting_symref_without_creating_its_referent() {
    let repository = repository();
    let reference = "refs/heads/nib/subagent/preexisting-symref";
    let referent = "refs/heads/unowned-missing-referent";
    git_stdout(repository.path(), &["symbolic-ref", reference, referent]);

    let error = Worktree::create(repository.path(), "preexisting-symref")
        .expect_err("preexisting symbolic branch must fail closed");

    assert!(error.contains("is symbolic to"), "{error}");
    assert!(error.contains("preserving"), "{error}");
    assert_eq!(
        git_stdout(repository.path(), &["symbolic-ref", reference]),
        referent
    );
    let referent_status = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", referent])
        .status()
        .expect("referent lookup");
    assert_eq!(
        referent_status.code(),
        Some(1),
        "branch claim created a symbolic-ref referent"
    );
}

#[test]
fn create_preserves_a_preexisting_reciprocal_worktree_registration() {
    let repository = repository();
    let id = "preexisting-registration";
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    let registration = leave_stale_registration_for_path(repository.path(), &path);
    let gitdir_before =
        std::fs::read(registration.join("gitdir")).expect("stale registration backlink");

    let error = Worktree::create(repository.path(), id)
        .expect_err("pre-existing reciprocal registration must fail closed");

    assert!(
        error.contains("pre-existing Git worktree registration"),
        "{error}"
    );
    assert!(!path.exists(), "empty nib destination must be compensated");
    assert_eq!(
        std::fs::read(registration.join("gitdir")).expect("preserved registration backlink"),
        gitdir_before
    );
    assert_eq!(
        std::fs::read(registration.join("nib-preserve-sentinel"))
            .expect("preserved registration sentinel"),
        b"foreign"
    );
    let reference = format!("refs/heads/{}", branch_name(id));
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("compensated branch lookup");
    assert_eq!(branch.code(), Some(1));
}

#[tokio::test]
async fn cancellable_create_preserves_a_preexisting_reciprocal_registration() {
    let repository = repository();
    let id = "async-preexisting-registration";
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    let registration = leave_stale_registration_for_path(repository.path(), &path);

    let error = Worktree::create_cancellable(repository.path(), id, None)
        .await
        .expect_err("pre-existing reciprocal registration must fail closed");

    assert!(
        error.contains("pre-existing Git worktree registration"),
        "{error}"
    );
    assert!(!path.exists(), "empty nib destination must be compensated");
    assert_eq!(
        std::fs::read(registration.join("nib-preserve-sentinel"))
            .expect("preserved registration sentinel"),
        b"foreign"
    );
}

#[test]
fn failed_add_preserves_a_registration_forged_after_the_snapshot() {
    let repository = repository();
    let id = "post-snapshot-forged-registration";
    SYNC_AFTER_REGISTRATION_SNAPSHOT_FORGERIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("forged post-snapshot registration must fail closed");

    assert!(error.contains("registrations were preserved"), "{error}");
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(
        !path.exists(),
        "exact owned destination must be compensated"
    );
    let registration = repository
        .path()
        .join(".git/worktrees")
        .join(format!("forged-{id}"));
    assert_eq!(
        std::fs::read(registration.join("sentinel")).expect("foreign registration was preserved"),
        b"foreign"
    );
    let reference = format!("refs/heads/{}", branch_name(id));
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("compensated branch lookup");
    assert_eq!(branch.code(), Some(1));
}

#[test]
fn branch_claim_never_replaces_a_symref_installed_after_missing_inspection() {
    let repository = repository();
    let id = "claim-symref-race";
    let reference = format!("refs/heads/{}", branch_name(id));
    let referent = "refs/heads/unowned-race-referent";
    BEFORE_REF_PUBLICATION_SYMREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(reference.clone(), referent.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("concurrent symbolic ref must win no-replace publication");

    assert!(error.contains("is symbolic to"), "{error}");
    assert_eq!(
        git_stdout(repository.path(), &["symbolic-ref", &reference]),
        referent
    );
    let referent_status = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", referent])
        .status()
        .expect("race referent lookup");
    assert_eq!(referent_status.code(), Some(1));
}

#[test]
fn normal_remove_uses_exact_ownership_to_remove_path_registration_and_branch() {
    let repository = repository();
    let worktree = Worktree::create(repository.path(), "ambiguous-remove").expect("worktree");
    let reference = format!("refs/heads/{}", worktree.branch);
    Worktree::remove(repository.path(), &worktree.id).expect("remove worktree path");

    assert!(!worktree.path.exists());
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("removed branch lookup");
    assert_eq!(branch.code(), Some(1));
}

#[test]
fn cleanup_preserves_loose_ref_and_anchor_when_a_packed_copy_exists() {
    let repository = repository();
    let id = "packed-copy-cleanup";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    git_stdout(repository.path(), &["pack-refs", "--all", "--no-prune"]);
    let reference = format!("refs/heads/{}", worktree.branch);
    let loose = repository.path().join(".git").join(&reference);
    assert!(loose.exists(), "no-prune fixture lost its loose ref");
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("active ownership");
    let anchor = revision.record.branch_staging_path.clone();

    let error =
        Worktree::remove(repository.path(), id).expect_err("packed ref ambiguity must fail closed");

    assert!(error.contains("packed ref"), "{error}");
    assert!(loose.exists(), "owned loose ref evidence was removed");
    assert!(anchor.exists(), "owned generation anchor was removed");
    assert_eq!(
        git_stdout(
            repository.path(),
            &["show-ref", "--hash", "--verify", &reference]
        ),
        worktree.branch_oid
    );
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("incomplete cleanup");
    assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
    assert_ne!(
        revision.record.branch_cleanup,
        DurableArtifactPhase::Removed
    );
}

#[test]
fn removing_restart_preserves_anchor_before_reporting_a_pruned_packed_ref() {
    let repository = repository();
    let id = "packed-pruned-restart";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let key = (project_root.clone(), id.to_string());
    let ownership = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
        .expect("owned worktree receipt");
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    persist_cleanup_artifact_phase(
        &mut state,
        CleanupArtifact::Branch,
        DurableArtifactPhase::Removing,
    )
    .expect("persist removing branch phase");
    let anchor = state
        .owned_branch
        .as_ref()
        .and_then(|branch| branch.receipt.anchor_path.clone())
        .expect("generation anchor");
    drop(state);
    drop(ownership);
    git_stdout(repository.path(), &["pack-refs", "--all", "--prune"]);
    let reference = format!("refs/heads/{}", worktree.branch);
    assert!(!repository.path().join(".git").join(&reference).exists());
    forget_subagent_ownership(repository.path(), id);

    let error = Worktree::remove(repository.path(), id)
        .expect_err("packed ref ambiguity must preserve restart anchor");

    assert!(error.contains("packed ref"), "{error}");
    assert!(
        anchor.exists(),
        "generation anchor was removed before reporting"
    );
    assert!(
        worktree.path.exists(),
        "worktree changed before fail-closed result"
    );
    assert_eq!(
        git_stdout(
            repository.path(),
            &["show-ref", "--hash", "--verify", &reference]
        ),
        worktree.branch_oid
    );
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("incomplete cleanup");
    assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn ownership_compaction_lock_recovers_after_holder_exit() {
    if std::env::var(OWNERSHIP_LOCK_ROLE).as_deref() == Ok("holder") {
        let directory = crate::daemons::state::StableDirectory::open(Path::new(
            &std::env::var_os(OWNERSHIP_LOCK_DIRECTORY).expect("ownership lock directory fixture"),
        ))
        .expect("open ownership lock directory");
        let _lock = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(5))
            .expect("acquire child ownership lock");
        std::fs::write(
            std::env::var_os(OWNERSHIP_LOCK_READY).expect("ownership lock ready fixture"),
            b"ready",
        )
        .expect("publish ownership lock readiness");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    let directory = tempdir().expect("ownership lock fixture");
    let stable = crate::daemons::state::StableDirectory::open(directory.path())
        .expect("stable ownership lock directory");
    let ready = directory.path().join("ready");
    let mut child = Command::new(std::env::current_exe().expect("current test executable"));
    child
        .args(["--exact", OWNERSHIP_LOCK_TEST, "--nocapture"])
        .env(OWNERSHIP_LOCK_ROLE, "holder")
        .env(OWNERSHIP_LOCK_DIRECTORY, directory.path())
        .env(OWNERSHIP_LOCK_READY, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = child.spawn().expect("spawn ownership lock holder");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "ownership lock holder was not ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("kill ownership lock holder");
    child.wait().expect("reap ownership lock holder");

    OwnershipCompactionLock::acquire(&stable, Duration::from_secs(2))
        .expect("kernel lock is released after holder exit");
    let visible = directory.path().join(OWNERSHIP_COMPACTION_LOCK_NAME);
    let anchor = directory.path().join(OWNERSHIP_COMPACTION_ANCHOR_NAME);
    let anchor_file = stable.open_read(&anchor).expect("persistent lock anchor");
    stable
        .verify_file_identity(&visible, &anchor_file)
        .expect("stable lock identity");
}

#[test]
fn restart_recovers_receipt_lock_quarantine_and_pre_stage_scratch_after_holder_exit() {
    const TEST_NAME: &str = "sandbox::worktree::tests::part_a::restart_recovers_receipt_lock_quarantine_and_pre_stage_scratch_after_holder_exit";
    if run_restart_crash_fixture() {
        return;
    }
    let repository = repository();
    let id = "restart-pre-stage-lock";
    let branch = branch_name(id);
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    crate::fs_security::ensure_directory_without_symlinks(path.parent().expect("worktree parent"))
        .expect("worktree parent");
    let reservation = reserve_managed_worktree_sync_controlled(
        repository.path(),
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("reserve worktree");
    let record = reservation.intent.revision.record.clone();
    let (ref_path, anchor_path) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )
    .expect("managed branch paths");
    crate::fs_security::ensure_directory_without_symlinks(ref_path.parent().expect("ref parent"))
        .expect("ref parent");
    spawn_and_kill_restart_fixture(TEST_NAME, |child| {
        child
            .env(RESTART_CRASH_MODE, "packed-quarantine")
            .env(RESTART_CRASH_COMMON, &record.common_git_dir)
            .env(
                RESTART_CRASH_REF_DIRECTORY,
                ref_path.parent().expect("ref parent"),
            )
            .env(RESTART_CRASH_REF_PATH, &ref_path)
            .env(RESTART_CRASH_ANCHOR_PATH, &anchor_path)
            .env(RESTART_CRASH_RECEIPT, &record.receipt_id)
            .env(RESTART_CRASH_REFERENCE, &record.branch_reference)
            .env(RESTART_CRASH_OID, &record.initial_oid);
    });
    let common = crate::daemons::state::StableDirectory::open(&record.common_git_dir)
        .expect("common directory");
    let packed = common.path().join("packed-refs.lock");
    let quarantine = common
        .deterministic_artifact_path(&packed, MANAGED_REF_LOCK_DELETE_PREFIX, ".quarantine")
        .expect("packed quarantine");
    let ref_directory =
        crate::daemons::state::StableDirectory::open(ref_path.parent().expect("ref parent"))
            .expect("ref directory");
    let reserved_temporary = ref_directory
        .deterministic_artifact_path(&anchor_path, RESERVED_REF_TEMPORARY_PREFIX, ".tmp")
        .expect("reserved temporary");
    assert!(quarantine.exists());
    assert!(reserved_temporary.exists());
    drop(reservation);

    Worktree::remove(repository.path(), id).expect("recover pre-stage crash");

    assert!(!quarantine.exists());
    assert!(!reserved_temporary.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable tombstone")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn restart_recovers_dead_packed_and_target_ref_locks_after_holder_exit() {
    const TEST_NAME: &str = "sandbox::worktree::tests::part_a::restart_recovers_dead_packed_and_target_ref_locks_after_holder_exit";
    if run_restart_crash_fixture() {
        return;
    }
    let repository = repository();
    let id = "restart-target-lock";
    let branch = branch_name(id);
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    crate::fs_security::ensure_directory_without_symlinks(path.parent().expect("worktree parent"))
        .expect("worktree parent");
    let mut reservation = reserve_managed_worktree_sync_controlled(
        repository.path(),
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("reserve worktree");
    let staged = stage_reserved_branch_publication(&mut reservation).expect("stage branch");
    let record = reservation.intent.revision.record.clone();
    let (ref_path, _) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )
    .expect("managed branch paths");
    drop(staged);
    drop(reservation);
    spawn_and_kill_restart_fixture(TEST_NAME, |child| {
        child
            .env(RESTART_CRASH_MODE, "target-lock")
            .env(RESTART_CRASH_COMMON, &record.common_git_dir)
            .env(
                RESTART_CRASH_REF_DIRECTORY,
                ref_path.parent().expect("ref parent"),
            )
            .env(RESTART_CRASH_REF_PATH, &ref_path)
            .env(RESTART_CRASH_RECEIPT, &record.receipt_id)
            .env(RESTART_CRASH_REFERENCE, &record.branch_reference);
    });
    let packed = record.common_git_dir.join("packed-refs.lock");
    let mut target_name = ref_path.file_name().expect("ref leaf").to_os_string();
    target_name.push(".lock");
    let target = ref_path.parent().expect("ref parent").join(target_name);
    assert!(packed.exists());
    assert!(target.exists());

    Worktree::remove(repository.path(), id).expect("recover target-lock crash");

    assert!(!packed.exists());
    assert!(!target.exists());
    assert!(!record.branch_staging_path.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable tombstone")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn restart_rolls_back_ownership_evacuated_before_publication() {
    const TEST_NAME: &str =
        "sandbox::worktree::tests::part_a::restart_rolls_back_ownership_evacuated_before_publication";
    if run_restart_crash_fixture() {
        return;
    }
    assert_ownership_cas_crash_recovers("ownership-evacuated", TEST_NAME, "restart-cas-evacuated");
}

#[test]
fn restart_finalizes_committed_ownership_with_previous_scratch() {
    const TEST_NAME: &str =
        "sandbox::worktree::tests::part_a::restart_finalizes_committed_ownership_with_previous_scratch";
    if run_restart_crash_fixture() {
        return;
    }
    assert_ownership_cas_crash_recovers("ownership-committed", TEST_NAME, "restart-cas-committed");
}

#[test]
fn foreign_packed_lock_owner_is_deferred_until_its_receipt_is_recovered() {
    let repository = repository();
    let reserve = |id: &str| {
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        reserve_managed_worktree_sync_controlled(
            repository.path(),
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch_name(id),
            None,
        )
        .expect("reserve worktree")
    };
    let reservation_a = reserve("foreign-lock-owner-a");
    let reservation_b = reserve("foreign-lock-owner-b");
    let record_a = reservation_a.intent.revision.record.clone();
    let record_b = reservation_b.intent.revision.record.clone();
    let common = crate::daemons::state::StableDirectory::open(&record_a.common_git_dir)
        .expect("common directory");
    let packed = common.path().join("packed-refs.lock");
    let publication = common
        .save_bytes_atomically_expected_with_receipt(
            &packed,
            &managed_ref_lock_contents(&record_a.receipt_id, &record_a.branch_reference, "packed"),
            MANAGED_REF_LOCK_TEMPORARY_PREFIX,
            crate::daemons::state::FileExpectation::Missing,
        )
        .expect("dead packed lock fixture");
    drop(publication);

    recover_owned_ref_restart_artifacts(&record_b).expect("defer foreign owner");
    assert!(packed.exists());
    recover_owned_ref_restart_artifacts(&record_a).expect("recover matching owner");
    assert!(!packed.exists());
    drop(reservation_a);
    drop(reservation_b);
    Worktree::remove(repository.path(), "foreign-lock-owner-a").expect("remove owner A");
    Worktree::remove(repository.path(), "foreign-lock-owner-b").expect("remove owner B");
}

#[test]
fn collected_tombstone_fallback_respects_cleanup_deadline_while_lock_is_held() {
    let repository = repository();
    let project_root =
        repository_root_bounded_sync(repository.path()).expect("validated repository root");
    let directory =
        managed_worktree_ownership_directory(&project_root).expect("ownership directory");
    let _held = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(2))
        .expect("hold ownership lock");
    let timeout = Duration::from_millis(100);
    let started = Instant::now();
    let deadline = started + timeout;

    let error = remove_registered_worktree_until(
        &project_root,
        "deadline-with-collected-tombstone",
        deadline,
        timeout,
    )
    .expect_err("held ownership lock must exhaust cleanup deadline");

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        error.contains("timed out") || error.contains("deadline"),
        "{error}"
    );
}

#[test]
fn completed_tombstone_compaction_recovers_stale_cas_and_keeps_remove_idempotent() {
    let repository = repository();
    let ids = ["compact-a", "compact-b", "compact-c"];
    for id in ids {
        Worktree::create(repository.path(), id).expect("create compacted worktree");
        Worktree::remove(repository.path(), id).expect("complete compacted worktree");
    }
    let directory =
        managed_worktree_ownership_directory(repository.path()).expect("ownership directory");
    let _lock = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(2))
        .expect("ownership compaction lock");
    let retained =
        managed_worktree_ownership_path(&directory, ManagedWorktreeKind::Subagent, ids[0]);
    let retained_bytes = directory
        .open_read(&retained)
        .expect("retained tombstone")
        .metadata()
        .expect("retained tombstone metadata")
        .len();
    let stale_temporary = directory
        .deterministic_artifact_path(
            &retained,
            MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
            ".tmp",
        )
        .expect("stale ownership transaction path");
    std::fs::write(&stale_temporary, b"stale").expect("stale ownership transaction");

    compact_complete_ownership_records_with_limits(
        &directory,
        repository.path(),
        &retained,
        retained_bytes,
        1,
        MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES,
    )
    .expect("compact complete ownership tombstones");

    assert!(!stale_temporary.exists(), "stale CAS transaction remains");
    let mut records = 0;
    directory
        .for_each_entry_bounded(16, 4096, |name| {
            if Path::new(&name).extension() == Some(OsStr::new("json")) {
                records += 1;
            }
            Ok(())
        })
        .expect("bounded ownership listing");
    assert_eq!(records, 1);
    drop(_lock);
    Worktree::remove(repository.path(), ids[1])
        .expect("collected tombstone uses bounded absence proof");
}

#[test]
fn removing_branch_resumes_from_anchor_only_after_restart() {
    let repository = repository();
    let id = "restart-anchor-only";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    let key = (
        repository
            .path()
            .canonicalize()
            .expect("canonical repository"),
        id.to_string(),
    );
    let ownership = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
        .expect("owned worktree receipt");
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    persist_cleanup_artifact_phase(
        &mut state,
        CleanupArtifact::Branch,
        DurableArtifactPhase::Removing,
    )
    .expect("persist removing branch phase");
    let owned_branch = state.owned_branch.as_ref().expect("owned branch").clone();
    remove_owned_file_receipt(
        &owned_branch.receipt.directory,
        &owned_branch.receipt.path,
        &owned_branch.receipt.file,
        &owned_branch.receipt.contents,
        ".nib-owned-ref-delete-",
    )
    .expect("remove final ref only");
    drop(state);
    drop(ownership);
    forget_subagent_ownership(repository.path(), id);

    Worktree::remove(repository.path(), id).expect("resume anchor-only cleanup");

    assert!(!worktree.path.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn quarantine_only_branch_cleanup_remains_incomplete_and_reported() {
    let repository = repository();
    let id = "restart-ref-quarantine";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    let key = (
        repository
            .path()
            .canonicalize()
            .expect("canonical repository"),
        id.to_string(),
    );
    let ownership = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
        .expect("owned worktree receipt");
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    persist_cleanup_artifact_phase(
        &mut state,
        CleanupArtifact::Branch,
        DurableArtifactPhase::Removing,
    )
    .expect("persist removing branch phase");
    let owned_branch = state.owned_branch.as_ref().expect("owned branch").clone();
    let quarantine = owned_branch
        .receipt
        .directory
        .deterministic_artifact_path(
            &owned_branch.receipt.path,
            ".nib-owned-ref-delete-",
            ".quarantine",
        )
        .expect("branch quarantine path");
    std::fs::rename(&owned_branch.receipt.path, &quarantine)
        .expect("simulate ref deletion quarantine crash");
    drop(state);
    drop(ownership);
    forget_subagent_ownership(repository.path(), id);

    let error = Worktree::remove(repository.path(), id)
        .expect_err("quarantine-only cleanup requires physical recovery");

    assert!(error.contains("deletion quarantine"), "{error}");
    assert!(quarantine.exists());
    assert!(owned_branch
        .receipt
        .anchor_path
        .as_ref()
        .expect("branch anchor")
        .exists());
    assert!(worktree.path.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("incomplete cleanup");
    assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
}

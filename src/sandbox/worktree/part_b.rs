use super::*;

#[test]
fn durable_receipt_rehydrates_cleanup_after_process_state_loss() {
    let repository = repository();
    let id = "restart-cleanup";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    let reference = format!("refs/heads/{}", worktree.branch);
    forget_subagent_ownership(repository.path(), id);

    Worktree::remove(repository.path(), id).expect("restart cleanup");

    assert!(!worktree.path.exists());
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("removed branch lookup");
    assert_eq!(branch.code(), Some(1));
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("durable tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    assert_eq!(revision.record.path_cleanup, DurableArtifactPhase::Removed);
    assert_eq!(
        revision.record.registration_cleanup,
        DurableArtifactPhase::Removed
    );
    assert_eq!(
        revision.record.branch_cleanup,
        DurableArtifactPhase::Removed
    );
}

#[test]
fn durable_generational_receipt_adopts_branch_oid_after_restart() {
    let repository = repository();
    let id = "restart-adoption";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    let initial_revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("initial durable receipt")
            .expect("initial ownership");
    let initial_anchor = initial_revision.record.branch_staging_path.clone();
    std::fs::write(worktree.path.join("adopted.txt"), "adopted\n").expect("adopted fixture");
    git_stdout(&worktree.path, &["add", "adopted.txt"]);
    git_stdout(&worktree.path, &["commit", "-m", "adopted revision"]);
    let adopted_oid = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    forget_subagent_ownership(repository.path(), id);

    Worktree::adopt_branch_revision(repository.path(), id, &adopted_oid).expect("durable adoption");

    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("active ownership");
    assert_eq!(revision.record.current_oid, adopted_oid);
    assert_eq!(revision.record.branch_anchor_generation, 1);
    assert!(revision.record.previous_branch_anchor.is_none());
    assert!(!initial_anchor.exists(), "prior anchor was not retired");
    assert!(revision.record.branch_staging_path.exists());

    let anchor_directory = crate::daemons::state::StableDirectory::open(
        initial_anchor.parent().expect("initial anchor parent"),
    )
    .expect("branch anchor directory");
    let initial_contents = format!("{}\n", worktree.branch_oid).into_bytes();
    let previous_publication = anchor_directory
        .save_bytes_atomically_expected_with_receipt(
            &initial_anchor,
            &initial_contents,
            ".nib-test-previous-anchor-",
            crate::daemons::state::FileExpectation::Missing,
        )
        .expect("recreate previous anchor fixture");
    let previous_identity = crate::fs_security::file_identity_snapshot(&previous_publication.file)
        .expect("previous anchor identity");
    let key = (
        repository
            .path()
            .canonicalize()
            .expect("canonical repository"),
        id.to_string(),
    );
    let ownership =
        load_managed_worktree_ownership(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("rehydrate ownership")
            .expect("active ownership");
    WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, ownership.clone());
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let durable = state.durable.as_mut().expect("durable ownership state");
    let mut record = durable.record.clone();
    record.previous_branch_anchor = Some(DurablePreviousBranchAnchor {
        path: initial_anchor.clone(),
        identity: previous_identity,
        oid: worktree.branch_oid.clone(),
    });
    persist_durable_ownership_revision(durable, record)
        .expect("persist interrupted prior-anchor retirement");
    drop(state);
    drop(ownership);

    Worktree::adopt_branch_revision(repository.path(), id, &adopted_oid)
        .expect("same-OID retry retires the prior anchor");
    let reconciled =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("reconciled durable receipt")
            .expect("reconciled active ownership");
    assert!(reconciled.record.previous_branch_anchor.is_none());
    assert!(!initial_anchor.exists());
    forget_subagent_ownership(repository.path(), id);
    Worktree::remove(repository.path(), id).expect("cleanup adopted revision");
}

#[test]
fn durable_branch_identity_preserves_a_same_oid_replacement_after_restart() {
    let repository = repository();
    let id = "restart-branch-replacement";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    forget_subagent_ownership(repository.path(), id);
    let reference_path = repository
        .path()
        .join(".git/refs/heads")
        .join(&worktree.branch);
    let displaced = reference_path.with_extension("owned-away");
    std::fs::rename(&reference_path, &displaced).expect("displace owned ref");
    std::fs::write(&reference_path, format!("{}\n", worktree.branch_oid))
        .expect("same-OID replacement ref");

    let error = Worktree::remove(repository.path(), id)
        .expect_err("same contents must not replace durable ref identity");

    assert!(error.contains("durable ownership identity"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&reference_path).expect("preserved replacement ref"),
        format!("{}\n", worktree.branch_oid)
    );
    assert!(
        worktree.path.is_dir(),
        "worktree mutated before fail-closed result"
    );
}

#[test]
fn identical_oid_adoption_without_a_generational_receipt_fails_closed() {
    let repository = repository();
    let id = "missing-generational-receipt";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    forget_subagent_ownership(repository.path(), id);
    let ownership_directory =
        managed_worktree_ownership_directory(repository.path()).expect("ownership directory");
    let ownership_path =
        managed_worktree_ownership_path(&ownership_directory, ManagedWorktreeKind::Subagent, id);
    std::fs::remove_file(ownership_path).expect("remove durable receipt fixture");

    let error = Worktree::adopt_branch_revision(repository.path(), id, &worktree.branch_oid)
        .expect_err("OID equality alone must not prove ownership");

    assert!(error.contains("durable generational receipt"), "{error}");
    assert!(worktree.path.is_dir(), "owned worktree was mutated");
    assert_eq!(
        git_stdout(
            repository.path(),
            &[
                "show-ref",
                "--hash",
                "--verify",
                &format!("refs/heads/{}", worktree.branch)
            ]
        ),
        worktree.branch_oid
    );
}

#[test]
fn durable_path_identity_preserves_a_replacement_after_restart() {
    let repository = repository();
    let id = "restart-path-replacement";
    let worktree = Worktree::create(repository.path(), id).expect("worktree");
    forget_subagent_ownership(repository.path(), id);
    let displaced = worktree.path.with_extension("owned-away");
    std::fs::rename(&worktree.path, &displaced).expect("displace owned path");
    std::fs::create_dir(&worktree.path).expect("replacement path");
    std::fs::write(worktree.path.join("sentinel"), b"replacement").expect("replacement sentinel");

    let error = Worktree::remove(repository.path(), id)
        .expect_err("replacement must not match persisted identity");

    assert!(error.contains("durable ownership identity"), "{error}");
    assert_eq!(
        std::fs::read(worktree.path.join("sentinel")).expect("preserved replacement"),
        b"replacement"
    );
    assert!(displaced.is_dir(), "original owned path was mutated");
}

#[test]
fn removing_phases_reconcile_to_a_complete_tombstone_after_restart() {
    let repository = repository();
    let id = "restart-removing-phases";
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
        .expect("process ownership");
    let (path_receipt, registration_receipt, owned_branch) = {
        let mut state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for artifact in [
            CleanupArtifact::Path,
            CleanupArtifact::Registration,
            CleanupArtifact::Branch,
        ] {
            persist_cleanup_artifact_phase(&mut state, artifact, DurableArtifactPhase::Removing)
                .expect("write-ahead cleanup phase");
        }
        (
            ownership.path_receipt.clone().expect("path receipt"),
            ownership
                .registration_receipt
                .clone()
                .expect("registration receipt"),
            state.owned_branch.clone().expect("branch receipt"),
        )
    };
    crate::fs_security::remove_directory_tree_capability_bound_if_matches(
        ownership
            .registration_path
            .parent()
            .expect("registration parent"),
        &ownership.registration_path,
        registration_receipt,
        Instant::now() + GIT_COMMAND_TIMEOUT,
    )
    .expect("remove registration before simulated crash");
    crate::fs_security::remove_directory_tree_capability_bound_if_matches(
        ownership.path.parent().expect("worktree parent"),
        &ownership.path,
        path_receipt,
        Instant::now() + GIT_COMMAND_TIMEOUT,
    )
    .expect("remove path before simulated crash");
    delete_owned_branch_sync_with_timeout(repository.path(), &owned_branch, GIT_COMMAND_TIMEOUT)
        .expect("remove branch before simulated crash");
    forget_subagent_ownership(repository.path(), id);

    Worktree::remove(repository.path(), id).expect("reconcile write-ahead cleanup");

    assert!(!worktree.path.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("durable tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn unfinished_creation_intent_is_compensated_from_durable_provenance() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "restart-creation-intent";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let parent = crate::fs_security::ensure_directory_without_symlinks(
        path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let mut reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    let owned_branch = create_reserved_worktree_branch_sync_controlled(&mut reservation, None)
        .expect("owned branch");
    let path_receipt = publish_reserved_empty_worktree_destination(&mut reservation, &parent)
        .expect("owned empty destination");
    drop(reservation);
    drop(path_receipt);
    drop(owned_branch);

    Worktree::remove(&project_root, id).expect("recover incomplete creation intent");

    assert!(!path.exists());
    let reference = format!("refs/heads/{branch}");
    let branch_status = Command::new("git")
        .current_dir(&project_root)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("branch absence lookup");
    assert_eq!(branch_status.code(), Some(1));
    let revision =
        load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("durable tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    assert_eq!(
        revision.record.registration_cleanup,
        DurableArtifactPhase::Removed
    );
}

#[test]
fn partial_add_restart_preserves_and_reports_unattributed_registration() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "restart-partial-registration";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let parent = crate::fs_security::ensure_directory_without_symlinks(
        path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let mut reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    create_reserved_worktree_branch_sync_controlled(&mut reservation, None)
        .expect("reserved branch");
    publish_reserved_empty_worktree_destination(&mut reservation, &parent)
        .expect("reserved worktree path");
    let registration = reservation
        .intent
        .revision
        .record
        .common_git_dir
        .join("worktrees")
        .join("partial-restart-fixture");
    std::fs::create_dir_all(&registration).expect("partial Git registration");
    let gitdir = path.join(".git");
    std::fs::write(
        registration.join("gitdir"),
        gitdir.as_os_str().as_encoded_bytes(),
    )
    .expect("partial Git registration backlink");
    std::fs::write(registration.join("sentinel"), b"unattributed")
        .expect("partial Git registration sentinel");
    drop(reservation);

    let error = Worktree::remove(&project_root, id)
        .expect_err("unattributed partial registration must remain reported");

    assert!(
        error.contains("post-snapshot Git worktree registration"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(registration.join("sentinel")).expect("preserved registration"),
        b"unattributed"
    );
    assert!(!path.exists(), "exact reserved path was not compensated");
    let revision =
        load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("incomplete intent");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Intent);
    assert_eq!(revision.record.path_cleanup, DurableArtifactPhase::Removed);
    assert_eq!(
        revision.record.branch_cleanup,
        DurableArtifactPhase::Removed
    );
    assert_eq!(
        revision.record.registration_cleanup,
        DurableArtifactPhase::Unattributed
    );
    let retry = Worktree::remove(&project_root, id)
        .expect_err("unattributed registration remains nonterminal");
    assert!(
        retry.contains("post-snapshot Git worktree registration"),
        "{retry}"
    );
}

#[test]
fn reserved_branch_publisher_excludes_recovery_until_present_cas() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "live-reserved-branch-publisher";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let mut reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    let mut record = reservation.intent.revision.record.clone();
    record.path_cleanup = DurableArtifactPhase::Removed;
    record.registration_cleanup = DurableArtifactPhase::Removed;
    persist_durable_ownership_revision(&mut reservation.intent.revision, record.clone())
        .expect("isolate branch recovery");
    let recovery_attempted = std::cell::Cell::new(false);

    let staged = stage_reserved_branch_publication_with_hook(&mut reservation, || {
        let error = Worktree::remove(&project_root, id)
            .expect_err("recovery must preserve a live reserved branch publisher");
        assert!(error.contains("live publisher"), "{error}");
        assert!(
            record.branch_staging_path.is_file(),
            "recovery removed the live publisher's staging anchor"
        );
        let observed =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("read live reservation")
                .expect("durable reservation");
        assert_eq!(
            observed.record.branch_cleanup,
            DurableArtifactPhase::Reserved
        );
        recovery_attempted.set(true);
    })
    .expect("publish reserved branch identity");

    assert!(recovery_attempted.get());
    assert_eq!(
        reservation.intent.revision.record.branch_cleanup,
        DurableArtifactPhase::Present
    );
    let branch_directory = crate::daemons::state::StableDirectory::open(
        staged.path.parent().expect("branch staging parent"),
    )
    .expect("stable branch parent");
    let contender = branch_directory
        .open_read_write(&staged.path)
        .expect("open post-CAS lock contender");
    contender
        .try_lock()
        .expect("publisher must release the staging lock after the Present CAS");
    contender.unlock().expect("release lock contender");
    let staged_path = staged.path.clone();
    drop(contender);
    drop(branch_directory);
    drop(staged);
    drop(reservation);

    Worktree::remove(&project_root, id).expect("recover released staged branch");

    assert!(!staged_path.exists());
    let revision =
        load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn reserved_staging_names_recover_a_crash_before_identity_cas() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "restart-before-staging-cas";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let parent = crate::fs_security::ensure_directory_without_symlinks(
        path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    let record = reservation.intent.revision.record.clone();
    let parent_directory =
        crate::daemons::state::StableDirectory::open(&parent).expect("stable worktree parent");
    parent_directory
        .create_owned_child_directory(&record.worktree_staging_path)
        .expect("reserved worktree staging");
    let branch_parent = crate::fs_security::ensure_directory_without_symlinks(
        record
            .branch_staging_path
            .parent()
            .expect("branch staging parent"),
    )
    .expect("branch staging parent");
    let branch_directory =
        crate::daemons::state::StableDirectory::open(&branch_parent).expect("stable branch parent");
    let staged_receipt = branch_directory
        .save_bytes_atomically_expected_with_receipt(
            &record.branch_staging_path,
            format!("{}\n", record.current_oid).as_bytes(),
            ".nib-test-reserved-ref-",
            crate::daemons::state::FileExpectation::Missing,
        )
        .expect("reserved branch staging");
    drop(staged_receipt);
    drop(branch_directory);
    drop(parent_directory);
    drop(reservation);

    Worktree::remove(&project_root, id).expect("recover receipt-bound reserved staging");

    assert!(!record.worktree_staging_path.exists());
    assert!(!record.branch_staging_path.exists());
    let revision =
        load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn identity_cas_before_final_publication_recovers_staged_artifacts() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "restart-after-staging-cas";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let parent = crate::fs_security::ensure_directory_without_symlinks(
        path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let mut reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    let record = reservation.intent.revision.record.clone();
    let parent_directory =
        crate::daemons::state::StableDirectory::open(&parent).expect("stable worktree parent");
    let staged_directory = parent_directory
        .create_owned_child_directory(&record.worktree_staging_path)
        .expect("reserved worktree staging");
    let staged_receipt = staged_directory
        .directory_removal_receipt()
        .expect("reserved staging identity");
    let mut attached = reservation.intent.revision.record.clone();
    attached.worktree_identity = Some(staged_receipt.identity());
    attached.path_cleanup = DurableArtifactPhase::Present;
    persist_durable_ownership_revision(&mut reservation.intent.revision, attached)
        .expect("persist staged worktree identity");
    stage_reserved_branch_publication(&mut reservation).expect("persist staged branch identity");
    let staged_record = reservation.intent.revision.record.clone();
    drop(staged_receipt);
    drop(staged_directory);
    drop(parent_directory);
    drop(reservation);

    Worktree::remove(&project_root, id).expect("recover identity-bound staging");

    assert!(!staged_record.worktree_staging_path.exists());
    assert!(!staged_record.branch_staging_path.exists());
    assert!(!staged_record.worktree_path.exists());
    let revision =
        load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
            .expect("durable receipt")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[test]
fn reservation_preserves_artifacts_whose_exact_identity_was_not_attached() {
    let repository = repository();
    let project_root = repository
        .path()
        .canonicalize()
        .expect("canonical repository");
    let id = "restart-unattached-reservation";
    let branch = branch_name(id);
    let path = project_root.join(".nib/worktrees/subagents").join(id);
    let parent = crate::fs_security::ensure_directory_without_symlinks(
        path.parent().expect("worktree parent"),
    )
    .expect("worktree parent");
    let reservation = reserve_managed_worktree_sync_controlled(
        &project_root,
        ManagedWorktreeKind::Subagent,
        id,
        &path,
        &branch,
        None,
    )
    .expect("durable reservation");
    let owned_branch = create_owned_branch_sync(&project_root, &branch).expect("owned branch");
    let path_receipt =
        publish_owned_empty_worktree_destination(&parent, &path).expect("owned empty destination");
    drop(reservation);
    drop(path_receipt);
    drop(owned_branch);

    let error = Worktree::remove(&project_root, id)
        .expect_err("unattached identities must remain fail-closed");

    assert!(error.contains("before its reserved"), "{error}");
    assert!(path.is_dir(), "unattached path was removed");
    assert_eq!(
        git_stdout(
            &project_root,
            &[
                "show-ref",
                "--hash",
                "--verify",
                &format!("refs/heads/{branch}")
            ]
        ),
        git_stdout(&project_root, &["rev-parse", "HEAD"])
    );
}

#[test]
fn remove_without_a_retained_tombstone_uses_bounded_absence_proof() {
    let repository = repository();
    let id = "absent-without-receipt";
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(!path.exists());

    Worktree::remove(repository.path(), id).expect("bounded absence proof");
    assert!(!path.exists());
}

#[test]
fn failed_add_preserves_a_replacement_installed_after_owned_publication() {
    let repository = repository();
    let id = "failed-add-path-replacement";
    SYNC_AFTER_DESTINATION_PUBLICATION_REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("failed add must preserve the replacement destination");

    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert_eq!(
        std::fs::read(path.join("sentinel")).expect("replacement sentinel"),
        b"replacement"
    );
    assert!(
        error.contains("replacement") || error.contains("identity"),
        "{error}"
    );
    let reference = format!("refs/heads/{}", branch_name(id));
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("failed-add branch lookup");
    assert_eq!(branch.code(), Some(1));
}

#[test]
fn final_absence_proof_preserves_a_destination_that_appeared_before_publication() {
    let repository = repository();
    let id = "pre-publication-replacement";
    SYNC_BEFORE_ADD_DESTINATION_REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string(), PathBuf::from("replacement"));

    let error = Worktree::create(repository.path(), id)
        .expect_err("destination replacement must fail the final absence proof");

    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(path.join("sentinel").is_file());
    assert!(
        error.contains("appeared") || error.contains("preserved"),
        "{error}"
    );
}

#[test]
fn compensation_preserves_a_hostile_visible_path_replacement() {
    let repository = repository();
    let id = "post-add-path-replacement";
    SYNC_POST_CAPTURE_PATH_REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("post-add path replacement must fail closed");

    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert_eq!(
        std::fs::read(path.join("sentinel")).expect("replacement sentinel"),
        b"replacement"
    );
    assert!(
        error.contains("replaced") || error.contains("preserved"),
        "{error}"
    );
    let reference = format!("refs/heads/{}", branch_name(id));
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("replacement branch lookup");
    assert_eq!(branch.code(), Some(1));
}

#[test]
fn compensation_preserves_a_hostile_registration_replacement() {
    let repository = repository();
    let id = "post-add-registration-replacement";
    SYNC_POST_CAPTURE_REGISTRATION_REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("post-add registration replacement must fail closed");

    assert!(
        error.contains("registration") || error.contains("Git"),
        "{error}"
    );
    let registrations = repository.path().join(".git/worktrees");
    assert!(
        std::fs::read_dir(&registrations)
            .expect("registration directory")
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("sentinel").is_file()),
        "registration replacement was removed"
    );
    assert!(
        repository
            .path()
            .join(".nib/worktrees/subagents")
            .join(id)
            .exists(),
        "owned path must remain until registration cleanup is exact"
    );
}

#[test]
fn create_refuses_to_overwrite_a_stale_in_process_ownership_receipt() {
    let repository = repository();
    let id = "stale-ownership";
    let worktree = Worktree::create(repository.path(), id).expect("owned worktree");
    let displaced = repository.path().join("displaced-owned-worktree");
    std::fs::rename(&worktree.path, &displaced).expect("displace owned worktree");

    let error = Worktree::create(repository.path(), id)
        .expect_err("same-id create must retain the prior ownership receipt");

    assert!(error.contains("active ownership receipt"), "{error}");
    assert!(displaced.is_dir());
}

#[test]
fn compensation_preserves_a_branch_moved_after_creation() {
    let repository = repository();
    let id = "moved-branch-compensation";
    let replacement = replacement_commit(repository.path());
    SYNC_POST_ADD_BRANCH_MOVES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string(), replacement.clone());

    let error = Worktree::create(repository.path(), id)
        .expect_err("moved branch compensation must fail closed");

    assert!(error.contains("refusing publication"), "{error}");
    assert!(error.contains("changed from"), "{error}");
    assert!(error.contains("preserving it"), "{error}");
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(!path.exists(), "partial worktree path remains");
    let reference = format!("refs/heads/{}", branch_name(id));
    assert_eq!(
        git_stdout(
            repository.path(),
            &["show-ref", "--hash", "--verify", &reference]
        ),
        replacement
    );
}

#[test]
fn compensation_preserves_a_post_add_symref_and_its_referent() {
    let repository = repository();
    let id = "symref-branch-compensation";
    let referent = "refs/heads/unowned-protected-referent";
    let expected = git_stdout(repository.path(), &["rev-parse", "HEAD"]);
    git_stdout(repository.path(), &["update-ref", referent, &expected]);
    SYNC_POST_ADD_BRANCH_SYMREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string(), referent.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("post-add symbolic branch must fail closed");

    assert!(error.contains("is symbolic to"), "{error}");
    assert!(error.contains("preserving"), "{error}");
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(!path.exists(), "partial worktree path remains");
    let reference = format!("refs/heads/{}", branch_name(id));
    assert_eq!(
        git_stdout(repository.path(), &["symbolic-ref", &reference]),
        referent
    );
    assert_eq!(
        git_stdout(
            repository.path(),
            &["show-ref", "--hash", "--verify", referent]
        ),
        expected,
        "compensation deleted or moved the symbolic-ref referent"
    );
}

#[cfg(unix)]
#[test]
fn create_rejects_symlinked_worktree_ancestor_without_outside_mutation() {
    use std::os::unix::fs::symlink;

    let repository = repository();
    let outside = tempdir().expect("outside");
    std::fs::create_dir(repository.path().join(".nib")).expect("state");
    symlink(outside.path(), repository.path().join(".nib/worktrees")).expect("worktrees symlink");

    let error = Worktree::create(repository.path(), "hostile")
        .expect_err("symlinked worktree root must fail closed");

    assert!(error.contains("unsafe") || error.contains("symlink"));
    assert!(!outside.path().join("subagents").exists());
}

#[test]
fn sync_create_compensates_a_failure_after_worktree_add() {
    let repository = repository();
    let id = "post-add-compensation";
    SYNC_POST_ADD_VALIDATION_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.to_string());

    let error = Worktree::create(repository.path(), id)
        .expect_err("injected post-add validation must fail");

    assert!(error.contains("injected post-add"), "{error}");
    assert!(
        !error.contains("partial worktree cleanup failed"),
        "post-add compensation did not finish: {error}"
    );
    let path = repository.path().join(".nib/worktrees/subagents").join(id);
    assert!(!path.exists(), "partial worktree path remains: {error}");
    let registered = Command::new("git")
        .current_dir(repository.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("worktree list");
    assert!(
        !String::from_utf8_lossy(&registered.stdout).contains(path.to_string_lossy().as_ref()),
        "partial worktree registration remains: {error}"
    );
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/nib/subagent/post-add-compensation",
        ])
        .status()
        .expect("branch lookup");
    assert_eq!(
        branch.code(),
        Some(1),
        "partial worktree branch remains: {error}"
    );
}

#[test]
fn cleanup_retry_resumes_after_branch_deletion_and_lock_release_failure() {
    let repository = repository();
    let id = "retry-after-ref-lock-release";
    let worktree = Worktree::create(repository.path(), id).expect("managed worktree");
    let lock_path = repository
        .path()
        .join(".git")
        .join(format!("refs/heads/{}.lock", worktree.branch));
    OWNED_REF_LOCK_RELEASE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(lock_path);

    let error = Worktree::remove(repository.path(), id)
        .expect_err("injected ref lock release failure must be reported");
    assert!(error.contains("injected owned ref lock release"), "{error}");
    let reference = format!("refs/heads/{}", worktree.branch);
    let branch = Command::new("git")
        .current_dir(repository.path())
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .expect("branch absence lookup");
    assert_eq!(
        branch.code(),
        Some(1),
        "owned branch deletion did not commit"
    );

    Worktree::remove(repository.path(), id).expect("cleanup retry resumes from ref absence");
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_git_process_timeout_kills_its_descendants() {
    let directory = tempdir().expect("timeout fixture");
    let pid_file = directory.path().join("child.pid");
    let script = format!(
        "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
        pid_file.display()
    );
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let error = run_process_bounded(command, "timeout fixture", Duration::from_millis(100))
        .await
        .expect_err("command must time out");

    assert!(error.contains("timed out"), "{error}");
    let pid = wait_for_pid(&pid_file).await;
    assert_process_terminated(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_bounded_git_future_kills_its_descendants() {
    let directory = tempdir().expect("cancellation fixture");
    let pid_file = directory.path().join("child.pid");
    let script = format!(
        "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
        pid_file.display()
    );
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let task = tokio::spawn(run_process_bounded(
        command,
        "cancellation fixture",
        Duration::from_secs(30),
    ));
    let pid = wait_for_pid(&pid_file).await;

    task.abort();
    assert!(task.await.expect_err("task cancellation").is_cancelled());
    assert_process_terminated(pid).await;
}

#[cfg(any(unix, windows))]
#[test]
fn sync_managed_cancellation_reaps_descendant_tree() {
    if run_sync_control_fixture(SYNC_CANCEL_TEST) {
        return;
    }
    let directory = tempdir().expect("sync cancellation fixture");
    let pid_path = directory.path().join("descendant.pid");
    let command = sync_control_command(SYNC_CANCEL_TEST, &pid_path);
    let cancellation = BlockingGitCancellation::new(None);
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || {
        run_process_bounded_sync_controlled(
            command,
            "sync cancellation fixture",
            Duration::from_secs(30),
            Some(&worker_cancellation),
        )
    });
    let descendant = wait_for_pid_sync(&pid_path);

    cancellation.cancel();
    let error = worker
        .join()
        .expect("sync cancellation worker")
        .expect_err("sync managed command must be cancelled");

    assert!(error.contains("cancelled"), "{error}");
    assert_process_terminated_sync(descendant);
}

#[cfg(any(unix, windows))]
#[test]
fn dropping_sync_managed_child_reaps_descendant_tree() {
    if run_sync_control_fixture(SYNC_DROP_TEST) {
        return;
    }
    let directory = tempdir().expect("sync drop fixture");
    let pid_path = directory.path().join("descendant.pid");
    let mut command = sync_control_command(SYNC_DROP_TEST, &pid_path);
    let managed = SyncManagedChild::spawn(&mut command).expect("spawn sync managed child");
    let descendant = wait_for_pid_sync(&pid_path);

    drop(managed);

    assert_process_terminated_sync(descendant);
}

#[test]
fn managed_scope_process_group_selection_preserves_macos_outer_scope() {
    assert!(!should_create_inner_process_group(true, true));
    assert!(should_create_inner_process_group(true, false));
    assert!(should_create_inner_process_group(false, true));
    assert!(should_create_inner_process_group(false, false));
}

#[cfg(unix)]
#[test]
fn sync_managed_wait_discards_stale_group_authority_after_direct_reap() {
    use std::os::unix::process::CommandExt;

    let mut victim_command = Command::new("sh");
    victim_command
        .args(["-c", "sleep 60"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let victim_child = victim_command.spawn().expect("spawn victim group");
    let victim_group = victim_child.id();
    let mut victim = SyncManagedChild {
        child: victim_child,
        process_group: Some(victim_group),
        reaped: false,
    };

    let mut completed_command = Command::new("sh");
    completed_command
        .args(["-c", "exit 0"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let completed_child = completed_command.spawn().expect("spawn completed group");
    let completed_group = completed_child.id();
    let mut completed = SyncManagedChild {
        child: completed_child,
        process_group: Some(completed_group),
        reaped: false,
    };
    assert!(completed.child.wait().expect("direct reap").success());

    // Simulate reuse of the stored numeric PGID after another wait consumed
    // the child identity that established signalling authority.
    completed.process_group = Some(victim_group);
    assert!(completed
        .poll_exit()
        .expect("cached wait")
        .expect("completed status")
        .success());
    assert!(
        victim.child.try_wait().expect("inspect victim").is_none(),
        "sync Git wait signalled a process group after losing the leader identity"
    );

    victim.terminate_and_reap();
}

#[cfg(unix)]
#[tokio::test]
async fn successful_sync_leader_does_not_wait_for_descendant_owned_pipes() {
    let directory = tempdir().expect("successful leader fixture");
    let pid_file = directory.path().join("child.pid");
    let script = format!(
        "(trap '' HUP; sleep 30) & child=$!; printf '%s' \"$child\" > '{}'; exit 0",
        pid_file.display()
    );
    let mut command = Command::new("sh");
    command
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();

    let output =
        run_process_bounded_sync(command, "successful leader fixture", Duration::from_secs(5))
            .expect("successful leader result");

    assert!(output.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid = wait_for_pid(&pid_file).await;
    assert_process_terminated(pid).await;
}

#[cfg(windows)]
#[test]
fn sync_bounded_timeout_kills_windows_descendant_tree() {
    match std::env::var(SYNC_JOB_ROLE).as_deref() {
        Ok("leader") => {
            let mut descendant =
                Command::new(std::env::current_exe().expect("current worktree test executable"));
            descendant
                .args(["--exact", SYNC_JOB_TEST, "--nocapture"])
                .env(SYNC_JOB_ROLE, "descendant")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut descendant = descendant.spawn().expect("spawn sync descendant");
            std::fs::write(
                std::env::var_os(SYNC_JOB_PID_PATH).expect("sync descendant pid path"),
                descendant.id().to_string(),
            )
            .expect("write sync descendant pid");
            let _ = descendant.wait();
            return;
        }
        Ok("descendant") => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
        _ => {}
    }

    let directory = tempdir().expect("sync Windows Job fixture");
    let pid_path = directory.path().join("descendant.pid");
    let mut command =
        Command::new(std::env::current_exe().expect("current worktree test executable"));
    command
        .args(["--exact", SYNC_JOB_TEST, "--nocapture"])
        .env(SYNC_JOB_ROLE, "leader")
        .env(SYNC_JOB_PID_PATH, &pid_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let error = run_process_bounded_sync(
        command,
        "Windows sync Job Object fixture",
        Duration::from_secs(5),
    )
    .expect_err("sync fixture must time out");
    assert!(error.contains("timed out"), "{error}");
    let descendant_id = std::fs::read_to_string(&pid_path)
        .expect("sync descendant pid")
        .parse::<u32>()
        .expect("numeric sync descendant pid");

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_id) };
    if !descendant.is_null() {
        let wait = unsafe { WaitForSingleObject(descendant, 5_000) };
        unsafe {
            let _ = CloseHandle(descendant);
        }
        assert_eq!(wait, WAIT_OBJECT_0, "sync descendant survived timeout");
    }
}

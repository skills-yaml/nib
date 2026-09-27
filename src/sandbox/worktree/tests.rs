use super::*;
use tempfile::tempdir;

#[cfg(windows)]
pub(crate) const SYNC_JOB_ROLE: &str = "NIB_SYNC_WORKTREE_JOB_ROLE";
#[cfg(windows)]
pub(crate) const SYNC_JOB_PID_PATH: &str = "NIB_SYNC_WORKTREE_JOB_PID_PATH";
#[cfg(windows)]
pub(crate) const SYNC_JOB_TEST: &str =
    "sandbox::worktree::tests::part_b::sync_bounded_timeout_kills_windows_descendant_tree";

#[cfg(any(unix, windows))]
pub(crate) const SYNC_CONTROL_ROLE: &str = "NIB_SYNC_MANAGED_CONTROL_ROLE";
#[cfg(any(unix, windows))]
pub(crate) const SYNC_CONTROL_PID_PATH: &str = "NIB_SYNC_MANAGED_CONTROL_PID_PATH";
#[cfg(any(unix, windows))]
pub(crate) const SYNC_CANCEL_TEST: &str =
    "sandbox::worktree::tests::part_b::sync_managed_cancellation_reaps_descendant_tree";
#[cfg(any(unix, windows))]
pub(crate) const SYNC_DROP_TEST: &str =
    "sandbox::worktree::tests::part_b::dropping_sync_managed_child_reaps_descendant_tree";
pub(crate) const OWNERSHIP_LOCK_ROLE: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_ROLE";
pub(crate) const OWNERSHIP_LOCK_DIRECTORY: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_DIRECTORY";
pub(crate) const OWNERSHIP_LOCK_READY: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_READY";
pub(crate) const OWNERSHIP_LOCK_TEST: &str =
    "sandbox::worktree::tests::part_a::ownership_compaction_lock_recovers_after_holder_exit";
pub(crate) const RESTART_CRASH_ROLE: &str = "NIB_WORKTREE_RESTART_CRASH_ROLE";
pub(crate) const RESTART_CRASH_MODE: &str = "NIB_WORKTREE_RESTART_CRASH_MODE";
pub(crate) const RESTART_CRASH_COMMON: &str = "NIB_WORKTREE_RESTART_CRASH_COMMON";
pub(crate) const RESTART_CRASH_REF_DIRECTORY: &str = "NIB_WORKTREE_RESTART_CRASH_REF_DIRECTORY";
pub(crate) const RESTART_CRASH_REF_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_REF_PATH";
pub(crate) const RESTART_CRASH_ANCHOR_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_ANCHOR_PATH";
pub(crate) const RESTART_CRASH_RECEIPT: &str = "NIB_WORKTREE_RESTART_CRASH_RECEIPT";
pub(crate) const RESTART_CRASH_REFERENCE: &str = "NIB_WORKTREE_RESTART_CRASH_REFERENCE";
pub(crate) const RESTART_CRASH_OID: &str = "NIB_WORKTREE_RESTART_CRASH_OID";
pub(crate) const RESTART_CRASH_OWNERSHIP_DIRECTORY: &str =
    "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_DIRECTORY";
pub(crate) const RESTART_CRASH_OWNERSHIP_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_PATH";
pub(crate) const RESTART_CRASH_OWNERSHIP_NEW_BYTES: &str =
    "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_NEW_BYTES";
pub(crate) const RESTART_CRASH_READY: &str = "NIB_WORKTREE_RESTART_CRASH_READY";

#[cfg(unix)]
pub(crate) async fn wait_for_pid(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(path) {
                if let Ok(pid) = value.parse::<u32>() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant pid is recorded")
}

#[cfg(unix)]
pub(crate) fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("kill probe")
        .success()
}

#[cfg(unix)]
pub(crate) async fn assert_process_terminated(pid: u32) {
    let terminated = tokio::time::timeout(Duration::from_secs(2), async {
        while process_is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(terminated.is_ok(), "descendant {pid} remained alive");
}

#[cfg(any(unix, windows))]
pub(crate) fn wait_for_pid_sync(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            if let Ok(pid) = value.parse::<u32>() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "descendant pid was not recorded");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
pub(crate) fn assert_process_terminated_sync(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_is_alive(pid) {
        assert!(Instant::now() < deadline, "descendant {pid} remained alive");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
pub(crate) fn assert_process_terminated_sync(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return;
    }
    let wait = unsafe { WaitForSingleObject(process, 5_000) };
    unsafe {
        let _ = CloseHandle(process);
    }
    assert_eq!(wait, WAIT_OBJECT_0, "descendant {pid} remained alive");
}

#[cfg(any(unix, windows))]
pub(crate) fn run_sync_control_fixture(test_name: &str) -> bool {
    match std::env::var(SYNC_CONTROL_ROLE).as_deref() {
        Ok("leader") => {
            let mut descendant =
                Command::new(std::env::current_exe().expect("current worktree test executable"));
            descendant
                .args(["--exact", test_name, "--nocapture"])
                .env(SYNC_CONTROL_ROLE, "descendant")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut descendant = descendant.spawn().expect("spawn managed descendant");
            std::fs::write(
                std::env::var_os(SYNC_CONTROL_PID_PATH).expect("managed descendant pid path"),
                descendant.id().to_string(),
            )
            .expect("write managed descendant pid");
            let _ = descendant.wait();
            true
        }
        Ok("descendant") => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
        _ => false,
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn sync_control_command(test_name: &str, pid_path: &Path) -> Command {
    let mut command =
        Command::new(std::env::current_exe().expect("current worktree test executable"));
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(SYNC_CONTROL_ROLE, "leader")
        .env(SYNC_CONTROL_PID_PATH, pid_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn run_restart_crash_fixture() -> bool {
    if std::env::var(RESTART_CRASH_ROLE).as_deref() != Ok("holder") {
        return false;
    }
    let mode = std::env::var(RESTART_CRASH_MODE).expect("restart crash mode");
    let ready =
        PathBuf::from(std::env::var_os(RESTART_CRASH_READY).expect("restart crash ready path"));
    match mode.as_str() {
        "packed-quarantine" | "target-lock" => {
            let common_path = PathBuf::from(
                std::env::var_os(RESTART_CRASH_COMMON).expect("restart common directory"),
            );
            let ref_directory_path = PathBuf::from(
                std::env::var_os(RESTART_CRASH_REF_DIRECTORY).expect("restart ref directory"),
            );
            let ref_path =
                PathBuf::from(std::env::var_os(RESTART_CRASH_REF_PATH).expect("restart ref path"));
            let receipt = std::env::var(RESTART_CRASH_RECEIPT).expect("restart receipt");
            let reference = std::env::var(RESTART_CRASH_REFERENCE).expect("restart reference");
            let common = crate::daemons::state::StableDirectory::open(&common_path)
                .expect("restart common directory");
            let ref_directory = crate::daemons::state::StableDirectory::open(&ref_directory_path)
                .expect("restart ref directory");
            let packed_path = common.path().join("packed-refs.lock");
            let _packed = OwnedRefLock::acquire_with_contents(
                &common,
                packed_path.clone(),
                managed_ref_lock_contents(&receipt, &reference, "packed"),
            )
            .expect("restart packed lock");
            let mut target_lock_name = ref_path.file_name().expect("ref leaf").to_os_string();
            target_lock_name.push(".lock");
            let target_lock_path = ref_directory.path().join(target_lock_name);
            let _target = (mode == "target-lock")
                .then(|| {
                    OwnedRefLock::acquire_with_contents(
                        &ref_directory,
                        target_lock_path,
                        managed_ref_lock_contents(&receipt, &reference, "target"),
                    )
                })
                .transpose()
                .expect("restart target lock");
            if mode == "packed-quarantine" {
                let quarantine = common
                    .deterministic_artifact_path(
                        &packed_path,
                        MANAGED_REF_LOCK_DELETE_PREFIX,
                        ".quarantine",
                    )
                    .expect("packed lock quarantine");
                std::fs::rename(&packed_path, &quarantine).expect("quarantine packed lock fixture");
                let anchor_path = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_ANCHOR_PATH).expect("restart anchor path"),
                );
                let temporary = ref_directory
                    .deterministic_artifact_path(
                        &anchor_path,
                        RESERVED_REF_TEMPORARY_PREFIX,
                        ".tmp",
                    )
                    .expect("reserved ref temporary path");
                let mut temporary_file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .expect("reserved ref temporary file");
                use std::io::Write as _;
                temporary_file
                    .write_all(
                        format!(
                            "{}\n",
                            std::env::var(RESTART_CRASH_OID).expect("restart oid")
                        )
                        .as_bytes(),
                    )
                    .expect("reserved ref temporary contents");
                temporary_file.sync_all().expect("sync ref temporary");
                temporary_file.lock().expect("lock ref temporary");
                std::mem::forget(temporary_file);
            }
            std::mem::forget(_packed);
            if let Some(target) = _target {
                std::mem::forget(target);
            }
        }
        "ownership-evacuated" | "ownership-committed" => {
            let directory_path = PathBuf::from(
                std::env::var_os(RESTART_CRASH_OWNERSHIP_DIRECTORY).expect("ownership directory"),
            );
            let target = PathBuf::from(
                std::env::var_os(RESTART_CRASH_OWNERSHIP_PATH).expect("ownership target"),
            );
            let directory = crate::daemons::state::StableDirectory::open(&directory_path)
                .expect("stable ownership directory");
            let temporary = directory
                .deterministic_artifact_path(
                    &target,
                    MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                    ".tmp",
                )
                .expect("ownership temporary path");
            let previous = directory
                .deterministic_previous_artifact_path(
                    &target,
                    MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                )
                .expect("ownership previous path");
            let contents = std::fs::read(
                std::env::var_os(RESTART_CRASH_OWNERSHIP_NEW_BYTES)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| target.clone()),
            )
            .expect("ownership contents");
            let mut temporary_file = directory
                .open_read_write_create(&temporary)
                .expect("ownership temporary file");
            use std::io::Write as _;
            temporary_file
                .write_all(&contents)
                .expect("ownership temporary contents");
            temporary_file.sync_all().expect("sync ownership temporary");
            temporary_file.lock().expect("lock ownership temporary");
            let previous_file = directory
                .open_read_write(&target)
                .expect("ownership target file");
            previous_file.lock().expect("lock ownership previous");
            std::fs::rename(&target, &previous).expect("evacuate ownership target");
            if mode == "ownership-committed" {
                std::fs::rename(&temporary, &target).expect("publish ownership target");
            }
            directory.sync_directory().expect("sync crash fixture");
            std::mem::forget(temporary_file);
            std::mem::forget(previous_file);
        }
        other => panic!("unknown restart crash mode {other}"),
    }
    std::fs::write(ready, b"ready").expect("publish restart crash readiness");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

pub(crate) fn spawn_and_kill_restart_fixture(
    test_name: &str,
    configure: impl FnOnce(&mut Command),
) {
    let ready = tempfile::NamedTempFile::new()
        .expect("restart ready fixture")
        .into_temp_path();
    std::fs::remove_file(&ready).expect("remove initial ready fixture");
    let mut child = Command::new(std::env::current_exe().expect("current test executable"));
    child
        .args(["--exact", test_name, "--nocapture"])
        .env(RESTART_CRASH_ROLE, "holder")
        .env(RESTART_CRASH_READY, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut child);
    let mut child = child.spawn().expect("spawn restart crash holder");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "restart crash holder was not ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("kill restart crash holder");
    child.wait().expect("reap restart crash holder");
}

pub(crate) fn repository() -> tempfile::TempDir {
    let directory = tempdir().expect("repository");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "nib@example.invalid"],
        vec!["config", "user.name", "nib"],
    ] {
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(args)
            .status()
            .expect("git fixture")
            .success());
    }
    std::fs::write(directory.path().join("README.md"), "fixture\n").expect("fixture file");
    assert!(Command::new("git")
        .current_dir(directory.path())
        .args(["add", "."])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .current_dir(directory.path())
        .args(["commit", "-qm", "fixture"])
        .status()
        .unwrap()
        .success());
    directory
}

#[cfg(windows)]
pub(crate) fn windows_path_without_verbatim_prefix(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    const COLON: u16 = b':' as u16;
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if !encoded.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
        return path.to_path_buf();
    }
    let remainder = &encoded[4..];
    let ascii_eq = |value: u16, expected: u8| {
        value == u16::from(expected) || value == u16::from(expected.to_ascii_lowercase())
    };
    let normalized = if remainder.len() >= 4
        && ascii_eq(remainder[0], b'U')
        && ascii_eq(remainder[1], b'N')
        && ascii_eq(remainder[2], b'C')
        && remainder[3] == BACKSLASH
    {
        let mut normalized = vec![BACKSLASH, BACKSLASH];
        normalized.extend_from_slice(&remainder[4..]);
        normalized
    } else if remainder.get(1) == Some(&COLON) {
        remainder.to_vec()
    } else {
        return path.to_path_buf();
    };
    PathBuf::from(OsString::from_wide(&normalized))
}

#[cfg(windows)]
pub(crate) fn windows_dos_short_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let path = windows_path_without_verbatim_prefix(path);
    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    assert_ne!(
        required,
        0,
        "failed to size the DOS short-path buffer: {}",
        std::io::Error::last_os_error()
    );
    let mut output = vec![0_u16; required as usize];
    let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
    assert_ne!(
        written,
        0,
        "failed to resolve the DOS short path: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        (written as usize) < output.len(),
        "DOS short-path output exceeded its sized buffer"
    );
    output.truncate(written as usize);
    PathBuf::from(OsString::from_wide(&output))
}

pub(crate) fn git_stdout(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .expect("git fixture command");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn forget_subagent_ownership(repository: &Path, id: &str) {
    let key = (
        repository.canonicalize().expect("canonical repository"),
        sanitize_component(id),
    );
    WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
}

pub(crate) fn leave_stale_registration_for_path(repository: &Path, path: &Path) -> PathBuf {
    let parent = path.parent().expect("stale worktree parent");
    std::fs::create_dir_all(parent).expect("stale worktree parent fixture");
    let output = Command::new("git")
        .current_dir(repository)
        .args(["worktree", "add", "--detach"])
        .arg(path)
        .arg("HEAD")
        .output()
        .expect("create stale worktree fixture");
    assert!(
        output.status.success(),
        "stale worktree fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registration = parse_gitdir_pointer(
        &std::fs::read(path.join(".git")).expect("stale worktree pointer"),
        "stale worktree pointer",
    )
    .expect("stale registration path");
    std::fs::write(registration.join("nib-preserve-sentinel"), b"foreign")
        .expect("stale registration sentinel");
    std::fs::remove_dir_all(path).expect("remove stale worktree path only");
    assert!(registration.is_dir(), "stale registration remains");
    registration
}

pub(crate) fn replacement_commit(repository: &Path) -> String {
    let initial = git_stdout(repository, &["rev-parse", "HEAD"]);
    std::fs::write(repository.join("replacement.txt"), "replacement\n").expect("replacement file");
    git_stdout(repository, &["add", "replacement.txt"]);
    git_stdout(repository, &["commit", "-m", "replacement fixture"]);
    let replacement = git_stdout(repository, &["rev-parse", "HEAD"]);
    git_stdout(repository, &["reset", "--hard", &initial]);
    replacement
}

pub(crate) fn assert_create_rejects_packed_ref_namespace_conflict(
    repository: &Path,
    id: &str,
    packed_reference: &str,
) {
    let requested = format!("refs/heads/{}", branch_name(id));
    let original = git_stdout(repository, &["rev-parse", "HEAD"]);
    git_stdout(repository, &["update-ref", packed_reference, &original]);
    git_stdout(repository, &["pack-refs", "--all", "--prune"]);

    let git_directory = repository.join(".git");
    assert!(
        !git_directory.join(packed_reference).exists(),
        "packed fixture retained a loose ref"
    );
    let packed_path = git_directory.join("packed-refs");
    let packed_before = std::fs::read(&packed_path).expect("packed-refs fixture");

    let error = Worktree::create(repository, id)
        .expect_err("packed ref namespace conflict must fail closed");

    assert!(
            error.contains(&format!(
                "managed worktree branch {requested} conflicts with packed ref {packed_reference}; preserving it"
            )),
            "{error}"
        );
    assert_eq!(
        git_stdout(
            repository,
            &["show-ref", "--hash", "--verify", packed_reference]
        ),
        original
    );
    assert_eq!(
        std::fs::read(&packed_path).expect("preserved packed-refs"),
        packed_before
    );
    assert!(
        !git_directory.join(&requested).exists(),
        "managed branch published a loose ref despite the packed conflict"
    );
    assert!(
        !git_directory.join("packed-refs.lock").exists(),
        "packed-refs lock was not released"
    );
    assert!(
        !git_directory.join(format!("{requested}.lock")).exists(),
        "managed branch lock was not released"
    );
    assert!(!repository
        .join(".nib/worktrees/subagents")
        .join(id)
        .exists());
}

pub(crate) fn assert_ownership_cas_crash_recovers(mode: &str, test_name: &str, id: &str) {
    let repository = repository();
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
    let generation_marker = encoded_name_hash(OsStr::new("restart-cas-new-generation"));
    assert!(!record
        .preexisting_registration_name_hashes
        .contains(&generation_marker));
    let mut next_record = record.clone();
    next_record
        .preexisting_registration_name_hashes
        .push(generation_marker.clone());
    next_record.preexisting_registration_name_hashes.sort();
    next_record.preexisting_registration_name_hashes.dedup();
    validate_durable_ownership_record(
        &next_record,
        repository.path(),
        ManagedWorktreeKind::Subagent,
        id,
    )
    .expect("next ownership generation");
    let next_generation = tempfile::NamedTempFile::new().expect("next ownership fixture");
    std::fs::write(
        next_generation.path(),
        encode_durable_ownership(&next_record).expect("next ownership bytes"),
    )
    .expect("write next ownership fixture");
    let directory = reservation
        .intent
        .revision
        .directory
        .try_clone()
        .expect("ownership dir");
    let ownership_path = reservation.intent.revision.path.clone();
    drop(staged);
    drop(reservation);
    spawn_and_kill_restart_fixture(test_name, |child| {
        child
            .env(RESTART_CRASH_MODE, mode)
            .env(RESTART_CRASH_OWNERSHIP_DIRECTORY, directory.path())
            .env(RESTART_CRASH_OWNERSHIP_PATH, &ownership_path)
            .env(RESTART_CRASH_OWNERSHIP_NEW_BYTES, next_generation.path());
    });
    let previous = directory
        .deterministic_previous_artifact_path(
            &ownership_path,
            MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
        )
        .expect("ownership previous");
    assert!(previous.exists());
    if mode == "ownership-evacuated" {
        assert!(!ownership_path.exists());
    } else {
        assert!(ownership_path.exists());
    }

    let recovered =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("recover interrupted ownership CAS")
            .expect("recovered ownership generation");
    assert_eq!(
        recovered
            .record
            .preexisting_registration_name_hashes
            .contains(&generation_marker),
        mode == "ownership-committed",
        "restart selected the wrong side of the ownership CAS commit point"
    );
    assert!(!previous.exists());
    drop(recovered);

    Worktree::remove(repository.path(), id).expect("recover ownership CAS crash");

    assert!(!previous.exists());
    assert!(!record.branch_staging_path.exists());
    let revision =
        load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
            .expect("durable tombstone")
            .expect("complete tombstone");
    assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
}

#[path = "part_a.rs"]
mod part_a;
#[path = "part_b.rs"]
mod part_b;

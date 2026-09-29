use super::*;
use serial_test::serial;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use tempfile::tempdir;

#[cfg(windows)]
const WINDOWS_JOB_ROLE_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_ROLE";
#[cfg(windows)]
const WINDOWS_JOB_PID_PATH_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_PID_PATH";
#[cfg(windows)]
const WINDOWS_JOB_ACK_PATH_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_ACK_PATH";
#[cfg(windows)]
const WINDOWS_JOB_FIXTURE_TEST: &str =
    "skill_cmd::tests::bounded_command_windows_job_terminates_descendant_before_return";

fn create_skill(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).expect("create source");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test skill\n---\nBody\n"),
    )
    .expect("write skill");
    dir
}

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

#[cfg(windows)]
fn windows_dos_short_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let canonical = path.canonicalize().expect("canonical Windows source path");
    let mut input = canonical.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    assert_ne!(
        required,
        0,
        "failed to size DOS short-path buffer: {}",
        std::io::Error::last_os_error()
    );
    let mut output = vec![0_u16; required as usize];
    let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
    assert_ne!(
        written,
        0,
        "failed to resolve DOS short path: {}",
        std::io::Error::last_os_error()
    );
    assert!((written as usize) < output.len());
    output.truncate(written as usize);
    PathBuf::from(OsString::from_wide(&output))
}

#[test]
fn cleanup_failures_remain_visible_with_primary_and_secondary_errors() {
    assert_eq!(
        error_after_cleanup(
            "fixture timed out".to_string(),
            Err("job did not empty".to_string())
        ),
        "fixture timed out; process cleanup failed: job did not empty"
    );
    assert_eq!(
        combine_cleanup_results(
            Err("job termination failed".to_string()),
            Err("direct reap failed".to_string())
        ),
        Err("job termination failed; direct reap failed".to_string())
    );
}

#[test]
fn installs_directory_and_removes_skill() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = create_skill(source.path(), "safe-rust");

    let installed = install_skill_to(skill.to_str().expect("source path"), global.path())
        .expect("install skill");
    assert!(installed.join("SKILL.md").is_file());
    assert!(install_skill_to(skill.to_str().expect("source path"), global.path()).is_err());

    remove_skill_from("safe-rust", global.path()).expect("remove skill");
    assert!(!installed.exists());
    assert!(remove_skill_from("safe-rust", global.path()).is_err());
}

#[cfg(windows)]
#[test]
fn installs_a_real_directory_through_its_dos_short_alias() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let long_root = source.path().join("Long Skill Source Directory");
    let skill = create_skill(&long_root, "windows-short-skill");
    let canonical = skill.canonicalize().expect("canonical skill source");
    let short = windows_dos_short_path(&skill);
    assert_ne!(
        short, canonical,
        "fixture requires a distinct DOS short alias for the real source directory"
    );

    let installed = install_skill_to(short.to_str().expect("UTF-8 short path"), global.path())
        .expect("install through DOS short alias");
    assert_eq!(
        fs::read_to_string(installed.join("SKILL.md")).expect("installed manifest"),
        fs::read_to_string(canonical.join("SKILL.md")).expect("source manifest")
    );
}

#[test]
fn installs_direct_manifest_and_rejects_invalid_sources() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = create_skill(source.path(), "direct-skill");
    let manifest = skill.join("SKILL.md");

    let installed = install_skill_to(manifest.to_str().expect("manifest"), global.path())
        .expect("install direct manifest");
    assert!(installed.join("SKILL.md").is_file());
    assert!(install_skill_to("missing-skill", global.path()).is_err());
}

#[test]
fn direct_manifest_install_preserves_declared_references_and_assets() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = source.path().join("resource-skill");
    fs::create_dir_all(skill.join("references")).expect("references");
    fs::create_dir_all(skill.join("templates")).expect("templates");
    fs::write(skill.join("references/guide.md"), "guide").expect("reference");
    fs::write(skill.join("templates/report.md"), "report").expect("asset");
    fs::write(
            skill.join("SKILL.md"),
            "---\nname: resource-skill\nreferences: [references/guide.md]\nassets: [templates/report.md]\n---\nBody\n",
        )
        .expect("manifest");

    let installed = install_skill_to(
        skill.join("SKILL.md").to_str().expect("manifest path"),
        global.path(),
    )
    .expect("install direct resource skill");

    assert_eq!(
        fs::read_to_string(installed.join("references/guide.md")).unwrap(),
        "guide"
    );
    assert_eq!(
        fs::read_to_string(installed.join("templates/report.md")).unwrap(),
        "report"
    );
    nib::context::skills::parse_skill_file(&installed.join("SKILL.md"))
        .expect("installed skill remains valid");
}

#[test]
fn directory_install_copies_only_bounded_declared_resources() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = source.path().join("selected-resources");
    fs::create_dir_all(skill.join("assets")).expect("asset directory");
    fs::write(skill.join("assets/selected.txt"), "selected").expect("selected asset");
    fs::File::create(skill.join("undeclared-large.bin"))
        .and_then(|file| file.set_len(MAX_INSTALLED_TOTAL_BYTES * 2))
        .expect("sparse undeclared file");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: selected-resources\nassets: [assets/selected.txt]\n---\nBody\n",
    )
    .expect("manifest");

    let installed = install_skill_to(skill.to_str().expect("source path"), global.path())
        .expect("bounded install");
    assert_eq!(
        fs::read_to_string(installed.join("assets/selected.txt")).unwrap(),
        "selected"
    );
    assert!(!installed.join("undeclared-large.bin").exists());
    assert_eq!(
        fs::read_dir(&installed)
            .expect("installed directory")
            .count(),
        2
    );
}

#[test]
fn missing_directory_validation_does_not_create_source_components() {
    let source = tempdir().expect("source tempdir");
    let missing = source.path().join("missing/nested");

    verify_existing_directory_without_symlinks(&missing, "skill resource")
        .expect_err("missing resource parent must fail validation");

    assert!(!source.path().join("missing").exists());
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_declared_resource_ancestor_without_publication() {
    use std::os::unix::fs::symlink;

    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = source.path().join("ancestor-link");
    let outside = source.path().join("outside");
    fs::create_dir_all(&skill).expect("skill directory");
    fs::create_dir_all(&outside).expect("outside directory");
    fs::write(outside.join("secret.md"), "secret").expect("outside resource");
    symlink(&outside, skill.join("references")).expect("ancestor symlink");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: ancestor-link\nreferences: [references/secret.md]\n---\nBody\n",
    )
    .expect("manifest");

    let error = install_skill_to(skill.to_str().expect("source path"), global.path())
        .expect_err("ancestor symlink must be rejected");
    assert!(error.contains("invalid SKILL.md"), "{error}");
    assert!(fs::read_dir(global.path()).unwrap().next().is_none());
}

#[test]
fn oversized_declared_asset_fails_without_partial_install() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    let skill = source.path().join("oversized-asset");
    fs::create_dir(&skill).expect("skill directory");
    fs::File::create(skill.join("asset.bin"))
        .and_then(|file| file.set_len(MAX_INSTALLED_RESOURCE_BYTES + 1))
        .expect("sparse oversized asset");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: oversized-asset\nassets: [asset.bin]\n---\nBody\n",
    )
    .expect("manifest");

    let error = install_skill_to(skill.to_str().expect("source path"), global.path())
        .expect_err("oversized asset must fail");
    assert!(error.contains("2097152-byte limit"));
    assert!(fs::read_dir(global.path()).unwrap().next().is_none());
}

#[test]
fn installs_skill_from_bounded_http_manifest() {
    let global = tempdir().expect("global tempdir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let body = "---\nname: http-skill\ndescription: remote fixture\n---\nHTTP body\n";
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request).expect("read request");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    });

    let installed = install_skill_to(&format!("http://{address}/SKILL.md"), global.path())
        .expect("HTTP install");
    server.join().expect("server thread");
    assert!(installed.ends_with("http-skill"));
    assert!(installed.join("SKILL.md").is_file());
}

#[test]
fn installs_skill_from_git_repository_url() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    fs::write(
        source.path().join("SKILL.md"),
        "---\nname: git-skill\ndescription: git fixture\n---\nGit body\n",
    )
    .expect("write Git skill");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "SKILL.md"],
        vec!["commit", "--quiet", "-m", "skill"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(source.path())
            .status()
            .expect("git command");
        assert!(status.success());
    }

    let installed = install_skill_to(
        &format!("file://{}", source.path().display()),
        global.path(),
    )
    .expect("Git install");
    assert!(installed.ends_with("git-skill"));
    assert!(installed.join("SKILL.md").is_file());
    assert!(!installed.join(".git").exists());
}

#[test]
fn oversized_git_resource_fails_without_partial_install() {
    let source = tempdir().expect("source tempdir");
    let global = tempdir().expect("global tempdir");
    fs::File::create(source.path().join("asset.bin"))
        .and_then(|file| file.set_len(MAX_INSTALLED_RESOURCE_BYTES + 1))
        .expect("sparse oversized asset");
    fs::write(
        source.path().join("SKILL.md"),
        "---\nname: oversized-git\nassets: [asset.bin]\n---\nBody\n",
    )
    .expect("manifest");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "SKILL.md", "asset.bin"],
        vec!["commit", "--quiet", "-m", "skill"],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(source.path())
            .status()
            .expect("git command")
            .success());
    }

    let error = install_skill_to(
        &format!("file://{}", source.path().display()),
        global.path(),
    )
    .expect_err("oversized cloned resource must fail");
    assert!(error.contains("2097152-byte limit"));
    assert!(fs::read_dir(global.path()).unwrap().next().is_none());
}

#[test]
fn git_staging_limit_failure_removes_the_checkout() {
    let source = tempdir().expect("source tempdir");
    let staging = tempdir().expect("staging tempdir");
    fs::write(
        source.path().join("SKILL.md"),
        "---\nname: bounded-git\n---\nBody\n",
    )
    .expect("manifest");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "SKILL.md"],
        vec!["commit", "--quiet", "-m", "skill"],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(source.path())
            .status()
            .expect("git command")
            .success());
    }
    let limits = StagingLimits {
        max_file_bytes: 1,
        max_bytes: 1,
        max_entries: 100,
        max_depth: 16,
    };

    let error = match prepare_git_source_with_limits(
        &format!("file://{}", source.path().display()),
        staging.path(),
        limits,
    ) {
        Ok(_) => panic!("staging limits must reject the clone"),
        Err(error) => error,
    };

    assert!(error.contains("staging limits"));
    assert!(!staging.path().join("checkout").exists());
}

#[test]
fn staging_tree_enforces_entry_depth_per_file_and_aggregate_budgets() {
    let entries = tempdir().expect("entry tempdir");
    fs::write(entries.path().join("one"), "1").expect("entry one");
    fs::write(entries.path().join("two"), "2").expect("entry two");
    let entry_error = validate_staging_tree(
        entries.path(),
        StagingLimits {
            max_file_bytes: 8,
            max_bytes: 16,
            max_entries: 1,
            max_depth: 4,
        },
    )
    .expect_err("entry limit");
    assert!(entry_error.contains("entry limit"));

    let depth = tempdir().expect("depth tempdir");
    fs::create_dir_all(depth.path().join("one/two")).expect("deep staging tree");
    let depth_error = validate_staging_tree(
        depth.path(),
        StagingLimits {
            max_file_bytes: 8,
            max_bytes: 16,
            max_entries: 4,
            max_depth: 1,
        },
    )
    .expect_err("depth limit");
    assert!(depth_error.contains("depth limit"));

    let per_file = tempdir().expect("per-file tempdir");
    fs::write(per_file.path().join("large"), "123456789").expect("large file");
    let per_file_error = validate_staging_tree(
        per_file.path(),
        StagingLimits {
            max_file_bytes: 8,
            max_bytes: 16,
            max_entries: 2,
            max_depth: 1,
        },
    )
    .expect_err("per-file limit");
    assert!(per_file_error.contains("per-file limit"));

    let aggregate = tempdir().expect("aggregate tempdir");
    fs::write(aggregate.path().join("one"), "123456").expect("aggregate one");
    fs::write(aggregate.path().join("two"), "123456").expect("aggregate two");
    let aggregate_error = validate_staging_tree(
        aggregate.path(),
        StagingLimits {
            max_file_bytes: 8,
            max_bytes: 10,
            max_entries: 2,
            max_depth: 1,
        },
    )
    .expect_err("aggregate limit");
    assert!(aggregate_error.contains("aggregate limit"));
}

#[cfg(unix)]
#[test]
fn bounded_command_kills_a_process_that_exceeds_live_staging_limits() {
    let staging = tempdir().expect("staging tempdir");
    let mut command = Command::new("sh");
    command
            .args([
                "-c",
                "mkdir -p \"$NIB_TEST_STAGING\"; printf 123456789 > \"$NIB_TEST_STAGING/large\"; sleep 10",
            ])
            .env("NIB_TEST_STAGING", staging.path());
    let started = Instant::now();
    let error = run_bounded_command_with_staging(
        &mut command,
        "live staging fixture",
        Duration::from_secs(5),
        Some((
            staging.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 16,
                max_entries: 2,
                max_depth: 1,
            },
        )),
    )
    .expect_err("live staging limit must stop the command");

    assert!(error.contains("per-file limit"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn bounded_command_watchdog_stops_aggregate_staging_overage() {
    let staging = tempdir().expect("staging tempdir");
    let mut command = Command::new("sh");
    command
            .args([
                "-c",
                "printf 123456 > \"$NIB_TEST_STAGING/one\"; printf 123456 > \"$NIB_TEST_STAGING/two\"; sleep 10",
            ])
            .env("NIB_TEST_STAGING", staging.path());
    let started = Instant::now();
    let error = run_bounded_command_with_staging(
        &mut command,
        "aggregate staging fixture",
        Duration::from_secs(5),
        Some((
            staging.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 10,
                max_entries: 2,
                max_depth: 1,
            },
        )),
    )
    .expect_err("aggregate staging limit must stop the command");

    assert!(error.contains("aggregate limit"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn bounded_command_applies_a_child_file_size_limit() {
    let staging = tempdir().expect("staging tempdir");
    let output_path = staging.path().join("large");
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "dd if=/dev/zero of=\"$NIB_TEST_OUTPUT\" bs=1024 count=1024 2>/dev/null",
        ])
        .env("NIB_TEST_OUTPUT", &output_path);
    let limit = 4 * 1024;
    let error = run_bounded_command_with_staging(
        &mut command,
        "file size limit fixture",
        Duration::from_secs(2),
        Some((
            staging.path(),
            StagingLimits {
                max_file_bytes: limit,
                max_bytes: limit * 2,
                max_entries: 2,
                max_depth: 1,
            },
        )),
    )
    .expect_err("kernel file limit terminates the writer");

    assert!(error.contains("per-file limit"), "{error}");
    assert!(fs::metadata(output_path).unwrap().len() <= limit);
}

#[cfg(unix)]
#[test]
fn managed_macos_scope_never_claims_an_inner_process_group() {
    assert!(!should_create_unix_process_group(true, true));
    assert!(should_create_unix_process_group(true, false));
    assert!(should_create_unix_process_group(false, true));
}

#[cfg(unix)]
#[test]
fn command_wait_discards_stale_group_authority_after_direct_reap() {
    use std::os::unix::process::CommandExt;

    let mut victim_command = Command::new("sh");
    victim_command.args(["-c", "sleep 60"]).process_group(0);
    let mut victim = victim_command.spawn().expect("spawn victim group");
    let mut victim_ownership =
        CommandProcessOwnership::from_spawned_child(&victim, true).expect("victim ownership");
    let victim_group = victim_ownership
        .process_group
        .expect("victim process group");

    let mut completed_command = Command::new("sh");
    completed_command.args(["-c", "exit 0"]).process_group(0);
    let mut completed = completed_command.spawn().expect("spawn completed child");
    let mut completed_ownership =
        CommandProcessOwnership::from_spawned_child(&completed, true).expect("completed ownership");
    assert!(completed.wait().expect("direct reap").success());

    // Simulate reuse of the stored numeric PGID after an external wait
    // consumed the process identity that established ownership.
    completed_ownership.process_group = Some(victim_group);
    assert!(completed_ownership
        .poll_exit(&mut completed)
        .expect("cached wait")
        .expect("completed status")
        .success());
    assert!(
        victim.try_wait().expect("inspect victim").is_none(),
        "wait signalled a process group after losing the leader identity"
    );

    let _ = victim_ownership.terminate_and_reap(&mut victim);
}

#[cfg(windows)]
#[test]
fn bounded_command_windows_job_terminates_descendant_before_return() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    match std::env::var(WINDOWS_JOB_ROLE_ENV).as_deref() {
        Ok("leader") => {
            let pid_path =
                PathBuf::from(std::env::var_os(WINDOWS_JOB_PID_PATH_ENV).expect("pid path"));
            let ack_path =
                PathBuf::from(std::env::var_os(WINDOWS_JOB_ACK_PATH_ENV).expect("ack path"));
            let mut descendant =
                Command::new(std::env::current_exe().expect("current test executable"));
            descendant
                .args(["--exact", WINDOWS_JOB_FIXTURE_TEST, "--nocapture"])
                .env(WINDOWS_JOB_ROLE_ENV, "descendant")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let descendant = descendant.spawn().expect("spawn descendant fixture");
            fs::write(&pid_path, descendant.id().to_string()).expect("write descendant pid");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ack_path.is_file() {
                assert!(
                    Instant::now() < deadline,
                    "descendant observer did not acknowledge its process handle"
                );
                thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        Ok("descendant") => loop {
            thread::sleep(Duration::from_secs(60));
        },
        _ => {}
    }

    let root = tempdir().expect("Windows Job fixture root");
    let pid_path = root.path().join("descendant.pid");
    let ack_path = root.path().join("descendant.ack");
    let observed_pid_path = pid_path.clone();
    let observed_ack_path = ack_path.clone();
    let observer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !observed_pid_path.is_file() {
            assert!(
                Instant::now() < deadline,
                "leader did not publish its descendant pid"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let process_id: u32 = fs::read_to_string(&observed_pid_path)
            .expect("read descendant pid")
            .parse()
            .expect("numeric descendant pid");
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
        assert!(!process.is_null(), "open descendant process handle");
        let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
        fs::write(observed_ack_path, b"observed").expect("acknowledge descendant handle");
        process
    });

    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", WINDOWS_JOB_FIXTURE_TEST, "--nocapture"])
        .env(WINDOWS_JOB_ROLE_ENV, "leader")
        .env(WINDOWS_JOB_PID_PATH_ENV, &pid_path)
        .env(WINDOWS_JOB_ACK_PATH_ENV, &ack_path);
    let output = run_bounded_command(
        &mut command,
        "Windows Job descendant fixture",
        Duration::from_secs(15),
    )
    .expect("bounded leader command");
    let descendant = observer.join().expect("descendant observer");

    assert!(output.status.success());
    let wait_result = unsafe { WaitForSingleObject(descendant.as_raw_handle() as HANDLE, 0) };
    assert_eq!(
        wait_result, WAIT_OBJECT_0,
        "bounded command returned before its Windows Job became empty"
    );
}

#[cfg(unix)]
#[test]
fn bounded_command_terminates_a_hung_process_group() {
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 10"]);
    let started = Instant::now();
    let error = run_bounded_command(&mut command, "hung fixture", Duration::from_millis(50))
        .expect_err("command must time out");

    assert!(error.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn bounded_command_deadline_is_not_starved_by_stderr_flooding() {
    let mut command = Command::new("sh");
    command.args(["-c", "while :; do printf 12345678901234567890 >&2; done"]);
    let started = Instant::now();
    let error = run_bounded_command(
        &mut command,
        "stderr flood fixture",
        Duration::from_millis(50),
    )
    .expect_err("command must time out");

    assert!(error.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn bounded_command_cleans_up_a_lingering_process_group_after_success() {
    let root = tempdir().expect("pid marker root");
    let marker = root.path().join("descendant.pid");
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "sleep 10 & printf '%s' \"$!\" > \"$NIB_TEST_DESCENDANT_PID\"",
        ])
        .env("NIB_TEST_DESCENDANT_PID", &marker);
    let started = Instant::now();
    let output = run_bounded_command(
        &mut command,
        "successful leader fixture",
        Duration::from_secs(1),
    )
    .expect("leader succeeds");

    assert!(output.status.success());
    assert!(started.elapsed() < Duration::from_secs(1));
    let descendant_pid: i32 = fs::read_to_string(&marker)
        .expect("read descendant pid")
        .parse()
        .expect("numeric descendant pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = unsafe { libc::kill(descendant_pid, 0) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bounded command returned while its lingering process-group member was still live"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_command_does_not_wait_for_a_detached_stderr_holder() {
    let mut command = Command::new("sh");
    command.args(["-c", "setsid sh -c 'sleep 3' &"]);
    let deadline = Duration::from_millis(250);
    let started = Instant::now();
    let output = run_bounded_command(&mut command, "detached holder fixture", deadline)
        .expect("leader succeeds while detached holder remains");

    assert!(output.status.success());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
#[serial]
fn installed_skills_classifies_global_and_project_local_roots() {
    let home = tempdir().expect("home");
    let project = tempdir().expect("project");
    let previous_home = std::env::var_os("HOME");
    let previous_skills = std::env::var_os("NIB_SKILLS_DIR");
    std::env::remove_var("NIB_SKILLS_DIR");
    std::env::set_var("HOME", home.path());
    create_skill(&home.path().join(".config/nib/skills"), "global-skill");
    create_skill(&project.path().join(".nib/skills"), "local-skill");

    let installed = installed_skills(project.path());

    restore_env("HOME", previous_home);
    restore_env("NIB_SKILLS_DIR", previous_skills);
    let installed = installed.expect("list installed skills");
    assert!(installed
        .iter()
        .any(|skill| skill.name == "global-skill" && skill.location == "global"));
    assert!(installed
        .iter()
        .any(|skill| skill.name == "local-skill" && skill.location == "local"));
}

#[test]
#[serial]
fn installed_skills_are_sorted_by_location_name_and_path() {
    let global = tempdir().expect("global");
    let project = tempdir().expect("project");
    let previous = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global.path());
    create_skill(global.path(), "zeta-global");
    create_skill(global.path(), "alpha-global");
    create_skill(&project.path().join(".nib/skills"), "zeta-local");
    create_skill(&project.path().join(".skills"), "alpha-local");

    let installed = installed_skills(project.path());

    restore_env("NIB_SKILLS_DIR", previous);
    let installed = installed.expect("list installed skills");
    let order = installed
        .iter()
        .map(|skill| (skill.location, skill.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        order,
        vec![
            ("global", "alpha-global"),
            ("global", "zeta-global"),
            ("local", "alpha-local"),
            ("local", "zeta-local"),
        ]
    );
}

#[test]
#[serial]
fn skill_list_reports_malformed_discovered_manifest() {
    let global = tempdir().expect("global");
    let project = tempdir().expect("project");
    let previous = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global.path());
    let broken = project.path().join(".nib/skills/broken");
    fs::create_dir_all(&broken).expect("broken skill directory");
    fs::write(broken.join("SKILL.md"), "name: broken\n").expect("broken manifest");

    let result = list_skills(project.path());

    restore_env("NIB_SKILLS_DIR", previous);
    let error = result.expect_err("malformed installed skill must fail listing");
    let malformed_path = PathBuf::from("broken").join("SKILL.md");
    assert!(error.contains(malformed_path.to_string_lossy().as_ref()));
    assert!(error.contains("must start with YAML frontmatter"));
}

#[test]
#[serial]
fn skill_list_reports_truncated_discovery() {
    let global = tempdir().expect("global");
    let project = tempdir().expect("project");
    let previous = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global.path());
    let local = project.path().join(".nib/skills");
    for index in 0..257 {
        create_skill(&local, &format!("skill-{index:03}"));
    }

    let result = list_skills(project.path());

    restore_env("NIB_SKILLS_DIR", previous);
    let error = result.expect_err("truncated installed skill inventory must fail");
    assert!(error.contains("skill discovery was truncated"));
    assert!(error.contains("skill count"));
    assert!(error.contains("256"));
}

#[test]
#[serial]
fn command_dispatch_installs_lists_and_removes_from_configured_global_root() {
    let source = tempdir().expect("source");
    let global = tempdir().expect("global");
    let project = tempdir().expect("project");
    let skill = create_skill(source.path(), "dispatch-skill");
    let previous = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global.path());

    run_skill_cmd(
        &SkillArgs {
            command: SkillCommands::List,
        },
        project.path(),
    )
    .expect("list empty skills");
    run_skill_cmd(
        &SkillArgs {
            command: SkillCommands::Install {
                source: skill.to_string_lossy().into_owned(),
            },
        },
        project.path(),
    )
    .expect("install through dispatcher");
    assert!(global.path().join("dispatch-skill/SKILL.md").is_file());
    let installed = installed_skills(project.path()).expect("list installed skill");
    assert!(installed.iter().any(|skill| {
        skill.name == "dispatch-skill"
            && skill.location == "global"
            && skill.path.starts_with(global.path())
    }));
    run_skill_cmd(
        &SkillArgs {
            command: SkillCommands::List,
        },
        project.path(),
    )
    .expect("list installed skill");
    run_skill_cmd(
        &SkillArgs {
            command: SkillCommands::Remove {
                name: "dispatch-skill".to_string(),
            },
        },
        project.path(),
    )
    .expect("remove through dispatcher");
    assert!(!global.path().join("dispatch-skill").exists());

    restore_env("NIB_SKILLS_DIR", previous);
}

#[test]
#[serial]
fn global_directory_resolution_uses_home_and_reports_missing_environment() {
    let home = tempdir().expect("home");
    let previous_skills = std::env::var_os("NIB_SKILLS_DIR");
    let previous_home = std::env::var_os("HOME");
    std::env::remove_var("NIB_SKILLS_DIR");
    std::env::set_var("HOME", home.path());
    assert_eq!(
        global_skills_dir().expect("HOME fallback"),
        home.path().join(".config/nib/skills")
    );

    std::env::remove_var("HOME");
    assert!(global_skills_dir()
        .expect_err("missing global root")
        .contains("could not determine"));

    restore_env("HOME", previous_home);
    restore_env("NIB_SKILLS_DIR", previous_skills);
}

#[test]
fn skill_name_and_manifest_guards_reject_unsafe_sources() {
    assert_eq!(safe_skill_name("  Rust Tool!  ").unwrap(), "rust-tool");
    assert!(safe_skill_name("---").is_err());
    assert!(safe_skill_name(&"x".repeat(129)).is_err());

    let source = tempdir().expect("source");
    let global = tempdir().expect("global");
    let wrong_name = source.path().join("manifest.md");
    fs::write(
        &wrong_name,
        "---\nname: wrong-file\ndescription: test\n---\nBody\n",
    )
    .expect("manifest");
    assert!(
        install_skill_to(wrong_name.to_str().unwrap(), global.path())
            .expect_err("manifest filename")
            .contains("named SKILL.md")
    );

    let missing_manifest = source.path().join("missing-manifest");
    fs::create_dir(&missing_manifest).expect("directory");
    assert!(
        install_skill_to(missing_manifest.to_str().unwrap(), global.path())
            .expect_err("missing manifest")
            .contains("invalid SKILL.md")
    );

    fs::write(global.path().join("not-a-directory"), "file").expect("guard file");
    assert!(remove_skill_from("not-a-directory", global.path())
        .expect_err("file removal guard")
        .contains("not a local directory"));
}

#[test]
fn http_manifest_failure_is_reported_without_installing() {
    let global = tempdir().expect("global");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).expect("read request");
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing"
        )
        .expect("write response");
    });

    let error = install_skill_to(&format!("http://{address}/SKILL.md"), global.path())
        .expect_err("HTTP failure");
    server.join().expect("server thread");
    assert!(error.contains("failed to download skill"));
    assert!(fs::read_dir(global.path()).unwrap().next().is_none());
}

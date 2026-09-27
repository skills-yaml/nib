use super::*;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::thread;

fn manifest(channel: ReleaseChannel, commit: &str) -> Vec<u8> {
    let assets = RELEASE_ARCHIVES
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                ReleaseAsset {
                    sha256: "a".repeat(64),
                    size: 10,
                },
            )
        })
        .collect();
    serde_json::to_vec(&ReleaseManifest {
        schema_version: 1,
        repository: OFFICIAL_REPOSITORY.to_string(),
        channel: channel.as_str().to_string(),
        tag: channel.tag().to_string(),
        version: "0.1.0".to_string(),
        commit: commit.to_string(),
        assets,
    })
    .expect("manifest JSON")
}

#[test]
fn strict_manifest_distinguishes_current_and_available_builds() {
    let current_commit = "1".repeat(40);
    let latest_commit = "2".repeat(40);
    let current = BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.1.0".to_string(),
        commit: current_commit.clone(),
    };
    let current_manifest = parse_manifest(
        &manifest(ReleaseChannel::Prod, &current_commit),
        ReleaseChannel::Prod,
    )
    .expect("current manifest");
    assert!(matches!(
        classify(current.clone(), ReleaseChannel::Prod, &current_manifest),
        Ok(Availability::Current(_))
    ));
    let next_manifest = parse_manifest(
        &manifest(ReleaseChannel::Prod, &latest_commit),
        ReleaseChannel::Prod,
    )
    .expect("next manifest");
    assert!(matches!(
        classify(current, ReleaseChannel::Prod, &next_manifest),
        Ok(Availability::Available { .. })
    ));
    let available = classify(
        BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: current_commit,
        },
        ReleaseChannel::Prod,
        &next_manifest,
    )
    .expect("available update");
    assert_eq!(
        startup_notice(&available).as_deref(),
        Some("[nib] Channel update available: 0.1.0 (prod, 2222222). Run `nib update`.")
    );
    assert!(startup_notice(&Availability::Current(BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.1.0".to_string(),
        commit: latest_commit,
    }))
    .is_none());
}

#[test]
fn channel_switch_is_available_even_when_commit_is_unchanged() {
    let commit = "1".repeat(40);
    let current = BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.1.0".to_string(),
        commit: commit.clone(),
    };
    let development_manifest = parse_manifest(
        &manifest(ReleaseChannel::Development, &commit),
        ReleaseChannel::Development,
    )
    .expect("development manifest");

    let availability = classify(
        current.clone(),
        ReleaseChannel::Development,
        &development_manifest,
    )
    .expect("channel switch");
    assert_eq!(
        availability,
        Availability::Available {
            current,
            latest: BuildIdentity {
                channel: ReleaseChannel::Development,
                version: "0.1.0".to_string(),
                commit,
            },
        }
    );
    let Availability::Available { current, latest } = availability else {
        panic!("expected channel switch");
    };
    assert_eq!(
        completed_update_message(&current, &latest),
        "Switched nib channel: 0.1.0 (prod, 1111111) -> 0.1.0 (development, 1111111)"
    );
}

#[test]
fn within_channel_update_retains_update_output() {
    let current = BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.1.0".to_string(),
        commit: "1".repeat(40),
    };
    let latest = BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.2.0".to_string(),
        commit: "2".repeat(40),
    };

    assert_eq!(
        completed_update_message(&current, &latest),
        "Updated nib: 0.1.0 (prod, 1111111) -> 0.2.0 (prod, 2222222)"
    );
}

#[test]
fn same_commit_with_conflicting_version_fails_for_channel_switch() {
    let commit = "1".repeat(40);
    let current = BuildIdentity {
        channel: ReleaseChannel::Prod,
        version: "0.1.0".to_string(),
        commit: commit.clone(),
    };
    let mut manifest = parse_manifest(
        &manifest(ReleaseChannel::Development, &commit),
        ReleaseChannel::Development,
    )
    .expect("development manifest");
    manifest.version = "0.2.0".to_string();

    assert!(matches!(
        classify(current, ReleaseChannel::Development, &manifest),
        Err(UpdateError::InvalidRelease(_))
    ));
}

#[test]
fn strict_manifest_rejects_unknown_fields_and_wrong_channel() {
    let mut value: serde_json::Value =
        serde_json::from_slice(&manifest(ReleaseChannel::Prod, &"1".repeat(40)))
            .expect("manifest value");
    value["unexpected"] = serde_json::json!(true);
    assert!(parse_manifest(&serde_json::to_vec(&value).unwrap(), ReleaseChannel::Prod).is_err());
    assert!(parse_manifest(
        &manifest(ReleaseChannel::Development, &"1".repeat(40)),
        ReleaseChannel::Prod
    )
    .is_err());
}

#[test]
fn checksum_requires_exact_digest_and_archive_name() {
    let digest = "a".repeat(64);
    let valid = format!("{digest}  nib-linux-x86_64.tar.gz\n");
    assert_eq!(
        parse_checksum(valid.as_bytes(), "nib-linux-x86_64.tar.gz").unwrap(),
        digest
    );
    assert!(parse_checksum(valid.as_bytes(), "nib-macos-x86_64.tar.gz").is_err());
    assert!(parse_checksum(
        format!("{} *nib-linux-x86_64.tar.gz", "a".repeat(64)).as_bytes(),
        "nib-linux-x86_64.tar.gz"
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn tar_extraction_accepts_only_one_regular_nib_binary() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let mut encoded = Vec::new();
    {
        let encoder = GzEncoder::new(&mut encoded, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let bytes = b"binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "nib", &bytes[..])
            .expect("append binary");
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }
    assert_eq!(
        extract_binary("nib-linux-x86_64.tar.gz", &encoded).unwrap(),
        b"binary"
    );
}

#[test]
fn windows_cleanup_state_requires_digest_proven_terminal_shapes() {
    let old = "a".repeat(64);
    let candidate = "b".repeat(64);
    let unknown = "c".repeat(64);

    assert_eq!(
        classify_windows_cleanup_state(Some(&candidate), Some(&old), &old, &candidate),
        WindowsCleanupState::PublishedWithBackup
    );
    assert_eq!(
        classify_windows_cleanup_state(Some(&candidate), None, &old, &candidate),
        WindowsCleanupState::PublishedClean
    );
    assert_eq!(
        classify_windows_cleanup_state(None, Some(&old), &old, &candidate),
        WindowsCleanupState::RestoreBackup
    );
    assert_eq!(
        classify_windows_cleanup_state(Some(&old), None, &old, &candidate),
        WindowsCleanupState::RolledBack
    );

    for (target, backup) in [
        (None, None),
        (Some(old.as_str()), Some(old.as_str())),
        (Some(candidate.as_str()), Some(candidate.as_str())),
        (Some(unknown.as_str()), Some(old.as_str())),
        (Some(candidate.as_str()), Some(unknown.as_str())),
    ] {
        assert_eq!(
            classify_windows_cleanup_state(target, backup, &old, &candidate),
            WindowsCleanupState::Ambiguous
        );
    }
}

#[test]
fn windows_candidate_publish_rolls_back_every_failure_boundary() {
    use std::cell::RefCell;

    let staged = Path::new("staged");
    let target = Path::new("target");
    let backup = Path::new("backup");

    let publish_calls = RefCell::new(Vec::new());
    let publish_error = commit_windows_candidate(
        staged,
        target,
        backup,
        |source, destination| {
            publish_calls
                .borrow_mut()
                .push((source.to_path_buf(), destination.to_path_buf()));
            if source == staged && destination == target {
                Err(UpdateError::Filesystem(
                    "injected publish failure".to_string(),
                ))
            } else {
                Ok(())
            }
        },
        || panic!("failed publication must not be verified"),
    )
    .expect_err("candidate publication failure");
    assert!(publish_error
        .to_string()
        .contains("injected publish failure"));
    assert_eq!(
        publish_calls.into_inner(),
        vec![
            (target.to_path_buf(), backup.to_path_buf()),
            (staged.to_path_buf(), target.to_path_buf()),
            (backup.to_path_buf(), target.to_path_buf()),
        ]
    );

    let verify_calls = RefCell::new(Vec::new());
    let verification_error = commit_windows_candidate(
        staged,
        target,
        backup,
        |source, destination| {
            verify_calls
                .borrow_mut()
                .push((source.to_path_buf(), destination.to_path_buf()));
            Ok(())
        },
        || {
            Err(UpdateError::Filesystem(
                "injected verification failure".to_string(),
            ))
        },
    )
    .expect_err("published verification failure");
    assert!(verification_error
        .to_string()
        .contains("injected verification failure"));
    assert_eq!(
        verify_calls.into_inner(),
        vec![
            (target.to_path_buf(), backup.to_path_buf()),
            (staged.to_path_buf(), target.to_path_buf()),
            (target.to_path_buf(), staged.to_path_buf()),
            (backup.to_path_buf(), target.to_path_buf()),
        ]
    );

    let rollback_error = commit_windows_candidate(
        staged,
        target,
        backup,
        |source, destination| {
            if (source == backup || source == staged) && destination == target {
                Err(UpdateError::Filesystem("injected move failure".to_string()))
            } else {
                Ok(())
            }
        },
        || panic!("failed publication must not be verified"),
    )
    .expect_err("rollback failure must remain visible");
    assert!(rollback_error
        .to_string()
        .contains("backup rollback failed"));
}

#[cfg(windows)]
fn windows_cleanup_request_for_test(
    nonce: &str,
    target_name: &OsStr,
    old: &[u8],
    candidate: &[u8],
) -> WindowsCleanupRequest {
    use std::os::windows::ffi::OsStrExt;

    WindowsCleanupRequest {
        schema_version: 1,
        parent_pid: std::process::id(),
        nonce: nonce.to_string(),
        target_name: target_name.encode_wide().collect(),
        staged_name: target_name.encode_wide().collect(),
        backup_name: OsStr::new(&format!(".nib-update-previous-{nonce}.exe"))
            .encode_wide()
            .collect(),
        old_sha256: hex_sha256(old),
        candidate_sha256: hex_sha256(candidate),
    }
}

#[cfg(windows)]
#[test]
fn windows_cleanup_reconciles_published_and_interrupted_states() {
    let old: &[u8] = b"old executable";
    let candidate: &[u8] = b"candidate executable";

    for restore_interrupted in [false, true] {
        let parent = tempfile::tempdir().expect("cleanup parent");
        let staging = tempfile::Builder::new()
            .prefix(".nib-update-")
            .tempdir_in(parent.path())
            .expect("cleanup staging");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let request =
            windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
        let request_path = staging.path().join(WINDOWS_CLEANUP_REQUEST);
        write_windows_cleanup_request(&request_path, &request).expect("cleanup request");
        let (request, paths) =
            load_windows_cleanup_request(&request_path).expect("load cleanup request");
        fs::write(&paths.backup, old).expect("old image backup");
        if !restore_interrupted {
            fs::write(&paths.target, candidate).expect("published candidate");
        }

        reconcile_windows_cleanup(&request, &paths).expect("reconcile cleanup state");
        assert!(!paths.backup.exists());
        let expected = if restore_interrupted { old } else { candidate };
        assert_eq!(
            fs::read(&paths.target).expect("reconciled target"),
            expected
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_cleanup_replay_after_committed_state_is_idempotent() {
    let old: &[u8] = b"old executable";
    let candidate: &[u8] = b"candidate executable";
    let parent = tempfile::tempdir().expect("cleanup replay parent");
    let staging = tempfile::Builder::new()
        .prefix(".nib-update-")
        .tempdir_in(parent.path())
        .expect("cleanup replay staging");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let request = windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
    let request_path = staging.path().join(WINDOWS_CLEANUP_REQUEST);
    write_windows_cleanup_request(&request_path, &request).expect("cleanup replay request");
    let (request, paths) =
        load_windows_cleanup_request(&request_path).expect("load cleanup replay request");
    fs::write(&paths.target, candidate).expect("published candidate");

    reconcile_windows_cleanup(&request, &paths).expect("first cleanup reconciliation");
    assert_eq!(
        fs::read(&paths.target).expect("first reconciled target"),
        candidate
    );
    assert!(!paths.backup.exists());

    reconcile_windows_cleanup(&request, &paths).expect("replayed cleanup reconciliation");
    assert_eq!(fs::read(&paths.target).expect("replayed target"), candidate);
    assert!(!paths.backup.exists());
}

#[cfg(windows)]
#[test]
fn windows_new_update_is_fenced_until_prior_cleanup_converges() {
    let parent = tempfile::tempdir().expect("cleanup fence parent");
    reject_pending_windows_cleanup(parent.path()).expect("clean install directory");
    fs::write(parent.path().join(".nib-update-stale"), b"evidence")
        .expect("stale cleanup evidence");
    assert!(matches!(
        reject_pending_windows_cleanup(parent.path()),
        Err(UpdateError::PendingWindowsCleanup)
    ));
}

#[cfg(windows)]
#[test]
fn windows_worker_readiness_is_bounded_and_token_bound() {
    let directory = tempfile::tempdir().expect("readiness fixture");
    let ready = directory.path().join("ready");
    let started = Instant::now();
    let timeout =
        wait_for_windows_ready(&ready, "expected", Duration::from_millis(50), || Ok(None))
            .expect_err("missing readiness must time out");
    assert!(timeout.to_string().contains("readiness timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));

    fs::write(&ready, b"wrong").expect("wrong readiness token");
    assert!(
        wait_for_windows_ready(&ready, "expected", Duration::from_secs(1), || Ok(None))
            .expect_err("wrong readiness token")
            .to_string()
            .contains("invalid readiness token")
    );
    fs::write(&ready, b"expected").expect("correct readiness token");
    wait_for_windows_ready(&ready, "expected", Duration::from_secs(1), || Ok(None))
        .expect("correct readiness token");
}

#[cfg(windows)]
#[test]
fn windows_readiness_is_invisible_until_the_complete_nonce_is_synced() {
    use std::sync::{Arc, Barrier};

    let directory = tempfile::tempdir().expect("readiness publication fixture");
    let ready = directory.path().join(WINDOWS_CLEANUP_READY);
    let nonce = "a".repeat(32);
    let staged = Arc::new(Barrier::new(2));
    let publish = Arc::new(Barrier::new(2));
    let child_ready = ready.clone();
    let child_nonce = nonce.clone();
    let child_staged = Arc::clone(&staged);
    let child_publish = Arc::clone(&publish);
    let writer = thread::spawn(move || {
        write_windows_ready_file_with_hook(&child_ready, &child_nonce, |publishing| {
            assert_eq!(
                fs::read_to_string(publishing).expect("synced staged readiness"),
                child_nonce
            );
            child_staged.wait();
            child_publish.wait();
        })
    });

    staged.wait();
    assert!(
        !ready.exists(),
        "final readiness must remain absent while publication is paused"
    );
    publish.wait();
    writer
        .join()
        .expect("readiness writer")
        .expect("publish readiness");
    assert_eq!(fs::read_to_string(&ready).expect("final readiness"), nonce);
}

#[cfg(windows)]
#[test]
fn windows_cleanup_request_rejects_traversal_unknown_fields_and_digest_replay() {
    use std::os::windows::ffi::OsStrExt;

    let parent = tempfile::tempdir().expect("cleanup parent");
    let old: &[u8] = b"old executable";
    let candidate: &[u8] = b"candidate executable";

    let traversal_staging = tempfile::Builder::new()
        .prefix(".nib-update-")
        .tempdir_in(parent.path())
        .expect("traversal staging");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut traversal =
        windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
    traversal.target_name = OsStr::new("..\\escape.exe").encode_wide().collect();
    traversal.staged_name = traversal.target_name.clone();
    let traversal_path = traversal_staging.path().join(WINDOWS_CLEANUP_REQUEST);
    write_windows_cleanup_request(&traversal_path, &traversal).expect("traversal request");
    assert!(load_windows_cleanup_request(&traversal_path).is_err());

    let unknown_staging = tempfile::Builder::new()
        .prefix(".nib-update-")
        .tempdir_in(parent.path())
        .expect("unknown-field staging");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let valid = windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
    let mut unknown = serde_json::to_value(valid).expect("request value");
    unknown
        .as_object_mut()
        .expect("request object")
        .insert("unexpected".to_string(), serde_json::json!(true));
    let unknown_path = unknown_staging.path().join(WINDOWS_CLEANUP_REQUEST);
    fs::write(
        &unknown_path,
        serde_json::to_vec(&unknown).expect("unknown-field JSON"),
    )
    .expect("unknown-field request");
    assert!(load_windows_cleanup_request(&unknown_path).is_err());

    let replay_staging = tempfile::Builder::new()
        .prefix(".nib-update-")
        .tempdir_in(parent.path())
        .expect("digest-replay staging");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut replay =
        windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
    replay.candidate_sha256 = replay.old_sha256.clone();
    let replay_path = replay_staging.path().join(WINDOWS_CLEANUP_REQUEST);
    write_windows_cleanup_request(&replay_path, &replay).expect("digest-replay request");
    assert!(load_windows_cleanup_request(&replay_path).is_err());
}

#[cfg(windows)]
#[test]
fn windows_running_image_can_be_renamed_before_candidate_publication() {
    const CHILD_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_CHILD";
    const READY_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_READY";
    const STOP_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_STOP";

    if std::env::var_os(CHILD_ENV).as_deref() == Some(OsStr::new("1")) {
        let ready = PathBuf::from(std::env::var_os(READY_ENV).expect("child ready path"));
        let stop = PathBuf::from(std::env::var_os(STOP_ENV).expect("child stop path"));
        fs::write(&ready, b"ready").expect("publish child readiness");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !stop.exists() {
            assert!(
                Instant::now() < deadline,
                "parent did not release test child"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        return;
    }

    let directory = tempfile::tempdir().expect("Windows replacement fixture");
    let source = std::env::current_exe().expect("test executable");
    let target = directory.path().join("nib-running-test.exe");
    let backup = directory.path().join("nib-running-test.previous.exe");
    let ready = directory.path().join("child.ready");
    let stop = directory.path().join("child.stop");
    fs::copy(&source, &target).expect("copy running-image fixture");
    let mut child = Command::new(&target)
        .arg("windows_running_image_can_be_renamed_before_candidate_publication")
        .env(CHILD_ENV, "1")
        .env(READY_ENV, &ready)
        .env(STOP_ENV, &stop)
        .spawn()
        .expect("start running-image fixture");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect fixture child") {
            panic!("fixture child exited before readiness with {status}");
        }
        assert!(
            Instant::now() < deadline,
            "fixture child readiness timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    move_file_windows(&target, &backup).expect("rename running executable to backup");
    fs::copy(&source, &target).expect("publish candidate at original path");
    assert!(target.is_file());
    assert!(backup.is_file());
    fs::write(&stop, b"stop").expect("release fixture child");
    assert!(child.wait().expect("wait fixture child").success());
    fs::remove_file(&backup).expect("remove exited old image");
    assert!(target.is_file());
    assert!(!backup.exists());
}

#[cfg(unix)]
#[test]
fn executable_replacement_commits_one_complete_file() {
    let directory = tempfile::tempdir().expect("replacement directory");
    let target = directory.path().join("nib");
    let staged = directory.path().join("nib.staged");
    fs::write(&target, b"old").expect("old binary");
    fs::write(&staged, b"new").expect("staged binary");

    replace_executable(&staged, &target).expect("replace binary");
    sync_parent(directory.path()).expect("sync replacement");

    assert_eq!(fs::read(&target).expect("new target"), b"new");
    assert!(!staged.exists());
}

#[test]
fn bounded_transport_fetches_the_requested_channel_manifest() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local server");
    let address = listener.local_addr().expect("server address");
    let body = manifest(ReleaseChannel::Development, &"1".repeat(40));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request");
        let path = read_request_path(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("response headers");
        stream.write_all(&body).expect("response body");
        path
    });
    let base = Url::parse(&format!("http://{address}/")).expect("base URL");
    let transport = Transport::for_test(base, Duration::from_secs(2));
    let fetched = fetch_manifest(&transport, ReleaseChannel::Development)
        .expect("development manifest fetch");
    assert_eq!(fetched.commit, "1".repeat(40));
    assert_eq!(
        server.join().expect("server"),
        "/development-latest/nib-release.json"
    );
}

#[test]
fn verified_archive_and_checksum_follow_the_target_channel() {
    let asset_name = current_asset_name().expect("supported test platform");
    let archive = b"target-channel-archive".to_vec();
    let digest = hex_sha256(&archive);
    let checksum = format!("{digest}  {asset_name}\n").into_bytes();
    let mut parsed_manifest = parse_manifest(
        &manifest(ReleaseChannel::Development, &"2".repeat(40)),
        ReleaseChannel::Development,
    )
    .expect("development manifest");
    parsed_manifest.assets.insert(
        asset_name.to_string(),
        ReleaseAsset {
            sha256: digest,
            size: archive.len() as u64,
        },
    );
    let latest = BuildIdentity {
        channel: ReleaseChannel::Development,
        version: parsed_manifest.version.clone(),
        commit: parsed_manifest.commit.clone(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").expect("local server");
    let address = listener.local_addr().expect("server address");
    let archive_response = archive.clone();
    let server = thread::spawn(move || {
        let mut paths = Vec::new();
        for response in [archive_response, checksum] {
            let (mut stream, _) = listener.accept().expect("request");
            paths.push(read_request_path(&mut stream));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .expect("response headers");
            stream.write_all(&response).expect("response body");
        }
        paths
    });
    let base = Url::parse(&format!("http://{address}/")).expect("base URL");
    let transport = Transport::for_test(base, Duration::from_secs(2));

    assert_eq!(
        fetch_verified_archive(&transport, &parsed_manifest, &latest, asset_name)
            .expect("verified target-channel archive"),
        archive
    );
    assert_eq!(
        server.join().expect("server"),
        [
            format!("/development-latest/{asset_name}"),
            format!("/development-latest/{asset_name}.sha256"),
        ]
    );
}

fn read_request_path(stream: &mut TcpStream) -> String {
    let mut bytes = [0u8; 2048];
    let count = stream.read(&mut bytes).expect("read request");
    let request = std::str::from_utf8(&bytes[..count]).expect("HTTP request UTF-8");
    request
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .expect("HTTP request path")
        .to_string()
}

#[test]
fn local_build_metadata_is_unmanaged_and_safe() {
    assert!(!crate::version::build_commit().is_empty());
    assert!(!crate::version::build_channel().is_empty());
    if ReleaseChannel::from_embedded(crate::version::build_channel()).is_none() {
        assert!(matches!(
            managed_current_identity(),
            Err(UpdateError::Unmanaged { .. })
        ));
    }
}

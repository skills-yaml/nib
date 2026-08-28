use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::process::{Command, Output};

#[cfg(unix)]
const RELEASE_ARCHIVE_NAMES: [&str; 4] = [
    "nib-linux-x86_64.tar.gz",
    "nib-macos-aarch64.tar.gz",
    "nib-macos-x86_64.tar.gz",
    "nib-windows-x86_64.zip",
];

#[cfg(unix)]
const RELEASE_ASSET_NAMES: [&str; 9] = [
    "nib-linux-x86_64.tar.gz",
    "nib-linux-x86_64.tar.gz.sha256",
    "nib-macos-aarch64.tar.gz",
    "nib-macos-aarch64.tar.gz.sha256",
    "nib-macos-x86_64.tar.gz",
    "nib-macos-x86_64.tar.gz.sha256",
    "nib-release.json",
    "nib-windows-x86_64.zip",
    "nib-windows-x86_64.zip.sha256",
];

#[cfg(unix)]
const LEGACY_RELEASE_ASSET_NAMES: [&str; 8] = [
    "nib-linux-x86_64.tar.gz",
    "nib-linux-x86_64.tar.gz.sha256",
    "nib-macos-aarch64.tar.gz",
    "nib-macos-aarch64.tar.gz.sha256",
    "nib-macos-x86_64.tar.gz",
    "nib-macos-x86_64.tar.gz.sha256",
    "nib-windows-x86_64.zip",
    "nib-windows-x86_64.zip.sha256",
];

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn normalize_repository_text(text: String) -> String {
    text.replace("\r\n", "\n")
}

fn read_repository_text(relative_path: &str) -> String {
    let path = repository_root().join(relative_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read repository file {}: {error}", path.display()));
    normalize_repository_text(text)
}

#[test]
fn repository_text_normalization_accepts_windows_line_endings() {
    assert_eq!(
        normalize_repository_text("first\r\nsecond\r\n".to_string()),
        "first\nsecond\n"
    );
}

#[test]
fn interactive_release_smoke_is_offline_bounded_and_restoration_aware() {
    let script = read_repository_text("scripts/check-interactive-release.sh");
    let windows_script = read_repository_text("scripts/check-interactive-release.ps1");
    let taskfile = read_repository_text("Taskfile.yml");
    let workflow = read_repository_text(".github/workflows/ci.yml");

    for contract in [
        "timeout -k 2",
        "NIB_NO_UPDATE_CHECK=1",
        "NIB_ENABLE_INTERACTIVE_SMOKE=1",
        "interactive provider failure smoke",
        "LLM request failed [LLM-AUTH]",
        "OPENAI_API_KEY",
        "TERM=dumb",
        "NO_COLOR=1",
        "__NIB_TERMINAL_RESTORED__",
        "__NIB_INTERACTIVE_CHILD_STATUS__",
        "bracketed_paste_disable",
        "alternate_screen_exit",
        "private_run_ids",
        "private_sentinel",
    ] {
        assert!(
            script.contains(contract),
            "missing smoke contract: {contract}"
        );
    }
    assert!(script.contains("mktemp -d"));
    assert!(script.contains("--provider mock --model mock-model"));
    assert!(script.contains("Linux | Darwin"));
    assert!(script.contains("script -q -e -c"));
    assert!(script.contains("script -q /dev/null /bin/sh -c"));
    assert!(script.contains("terminate_process_tree"));
    assert!(script.contains("printf '/status\\n/quit\\n'"));
    assert!(script.contains("printf 'y\\n\\n'"));
    assert!(script.contains("for session_file in \"$session_directory\"/*.json; do"));
    assert!(script.contains("grep -Fq \"$private_sentinel\" \"$session_file\""));
    assert!(!script.contains("mapfile"));
    assert!(!script.contains("-maxdepth"));
    assert!(!script.contains("realpath"));
    assert!(!script.contains("curl "));
    assert!(!script.contains("wget "));

    for contract in [
        "Invoke-WindowsPseudoTerminal",
        "NIB_NO_UPDATE_CHECK",
        "NIB_ENABLE_INTERACTIVE_SMOKE",
        "OPENAI_API_KEY",
        "active_provider = \"mock\"",
        "interactive-private-sentinel-windows",
        "[string][char]17",
        "/status`r`n/quit`r`n",
        "TERM = \"dumb\"",
        "NO_COLOR = \"1\"",
        "ConsoleModesRestored",
        "ChildConsoleModesRestored",
        "[?1049l",
        "[?2004l",
    ] {
        assert!(
            windows_script.contains(contract),
            "missing Windows interactive smoke contract: {contract}"
        );
    }
    assert!(windows_script.contains("if (Test-Path -LiteralPath $fixture) {"));
    assert!(windows_script.contains("could not remove its isolated fixture"));
    assert!(!windows_script.contains("Invoke-WebRequest"));
    assert!(!windows_script.contains("curl"));

    assert!(taskfile.contains("  test:interactive:\n"));
    assert!(taskfile.contains("      - task: test:interactive\n"));
    assert!(taskfile.contains("  smoke:interactive:binary:\n"));
    assert!(taskfile.contains("      - task: smoke:interactive:binary\n"));
    assert!(taskfile.contains("    platforms: [linux, darwin]\n"));
    assert!(taskfile.contains("  smoke:interactive:windows:binary:\n"));
    assert!(taskfile.contains("scripts/check-interactive-release.ps1"));

    let windows_job = workflow
        .split_once("  windows-tests:\n")
        .and_then(|(_, remainder)| remainder.split_once("  macos-tests:\n"))
        .map(|(job, _)| job)
        .expect("Windows CI job");
    let windows_build = windows_job
        .find("run: task build")
        .expect("Windows release build");
    let windows_native_smoke = windows_job
        .find("run: task smoke:interactive:windows:binary")
        .expect("Windows native interactive smoke");
    assert!(windows_build < windows_native_smoke);

    let macos_job = workflow
        .split_once("  macos-tests:\n")
        .map(|(_, job)| job)
        .expect("macOS CI job");
    let macos_build = macos_job
        .find("run: task build")
        .expect("macOS release build");
    let macos_native_smoke = macos_job
        .find("run: task smoke:interactive:binary")
        .expect("macOS native interactive smoke");
    assert!(macos_build < macos_native_smoke);
}

#[test]
fn release_workflow_emits_portable_checksum_manifests() {
    let workflow = read_repository_text(".github/workflows/release.yml");
    let enter_dist = workflow
        .find("          cd dist\n")
        .expect("Unix packaging must enter the artifact directory");
    let unix_checksum = workflow
        .find(r#"          shasum -a 256 "${{ matrix.asset }}" > "${{ matrix.asset }}.sha256""#)
        .expect("Unix checksum manifest must contain a portable asset basename");
    let windows_checksum = workflow
        .find(
            r#"          "$($hash.Hash.ToLower())  ${{ matrix.asset }}" | Out-File -Encoding ascii "dist/${{ matrix.asset }}.sha256""#,
        )
        .expect("Windows checksum manifest must contain a portable asset basename");
    let upload = workflow
        .find("      - name: Upload workflow artifact")
        .expect("packaged artifacts must be uploaded");

    assert!(enter_dist < unix_checksum);
    assert!(unix_checksum < upload);
    assert!(windows_checksum < upload);
    assert!(!workflow.contains(
        r#"shasum -a 256 "dist/${{ matrix.asset }}" > "dist/${{ matrix.asset }}.sha256""#
    ));
}

#[test]
fn release_update_qualification_is_read_only_and_native() {
    let workflow = read_repository_text(".github/workflows/release-update-qualification.yml");
    let release_workflow = read_repository_text(".github/workflows/release.yml");
    let taskfile = read_repository_text("Taskfile.yml");
    let unix = read_repository_text("scripts/qualify-release-update.sh");
    let windows = read_repository_text("scripts/qualify-release-update.ps1");
    let windows_pty_host = read_repository_text("scripts/host-windows-pseudoterminal.ps1");
    let windows_pty_child = read_repository_text("scripts/start-windows-pseudoterminal-child.ps1");
    let windows_pty_invoke = read_repository_text("scripts/invoke-windows-pseudoterminal.ps1");
    let windows_pty_test = read_repository_text("scripts/test-windows-pseudoterminal.ps1");
    let resistant_child =
        read_repository_text("scripts/test-windows-pseudoterminal-resistant-child.ps1");

    assert!(workflow.contains("  workflow_dispatch:\n"));
    assert!(!workflow.contains("\n  push:\n"));
    assert!(!workflow.contains("\n  pull_request:\n"));
    assert!(workflow.contains("permissions:\n  actions: read\n  contents: read\n"));
    assert!(!workflow.contains("contents: write"));
    assert!(workflow.contains("test \"$GITHUB_REF\" = \"refs/heads/development\""));
    assert!(workflow
        .contains("test \"$(jq -r .path <<<\"$run\")\" = \".github/workflows/release.yml\""));
    assert!(workflow.contains("test \"$(jq -r .workflow_id <<<\"$run\")\" = \"$workflow_id\""));
    assert!(workflow
        .contains("test \"$(jq -r .head_sha <<<\"$bootstrap_run\")\" = \"$BOOTSTRAP_COMMIT\""));
    assert!(workflow.contains("test \"$(jq -r .conclusion <<<\"$candidate_run\")\" = \"success\""));
    assert!(workflow.contains("test \"$(jq -r .status <<<\"$production_run\")\" != \"completed\""));
    assert!(workflow.contains("select(.environment.name == \"release-prod\")"));
    assert!(workflow.contains(
        "test \"$(jq -r .merge_base_commit.sha <<<\"$comparison\")\" = \"$BOOTSTRAP_COMMIT\""
    ));
    assert!(workflow.contains("intervening=$(jq"));
    assert!(workflow.contains("run-id: ${{ inputs.bootstrap_run_id }}"));
    assert!(workflow.contains("candidate_run_id:"));
    assert!(workflow.contains("production_run_id:"));
    assert!(
        workflow.contains("CANDIDATE_VERSION=\"${{ needs.prepare.outputs.candidate_version }}\"")
    );
    assert!(workflow.contains(
        "      - name: Exercise notification and replacement (Unix)\n        if: runner.os != 'Windows'\n        shell: bash\n"
    ));
    assert!(workflow.contains(
        "      - name: Exercise notification and replacement (Windows)\n        if: runner.os == 'Windows'\n        shell: pwsh\n"
    ));
    assert!(!workflow.contains("shell: ${{ runner.os"));
    assert!(workflow.contains("  verify:\n    name: Confirm production remains held\n"));
    assert!(release_workflow
        .contains("    paths-ignore:\n      - 'docs/**'\n      - 'agents/memory/**'\n"));

    for runner in [
        "ubuntu-latest",
        "macos-15-intel",
        "macos-15",
        "windows-2025",
    ] {
        assert!(workflow.contains(&format!("          - os: {runner}\n")));
    }
    for asset in [
        "nib-linux-x86_64.tar.gz",
        "nib-macos-aarch64.tar.gz",
        "nib-macos-x86_64.tar.gz",
        "nib-windows-x86_64.zip",
    ] {
        assert!(workflow.contains(&format!("            asset: {asset}\n")));
    }

    let action_refs: Vec<_> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("uses: "))
        .map(|reference| reference.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        action_refs,
        vec![
            "actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5",
            "arduino/setup-task@b91d5d2c96a56797b48ac1e0e89220bf64044611",
            "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
        ]
    );

    assert!(taskfile.contains("  qualify:release-update:unix:\n"));
    assert!(taskfile.contains("  qualify:release-update:windows:\n"));
    assert!(unix.contains("script -q"));
    assert!(unix.contains("\"$nib_path\" update"));
    assert!(unix.contains("expected_candidate_version=$4"));
    assert!(unix.contains("candidate_identity=$(NIB_NO_UPDATE_CHECK=1"));
    assert!(unix.contains("already-current update changed the executable"));
    assert!(!windows.contains("winpty.exe"));
    let windows_pty_invoke_binding =
        "Join-Path $PSScriptRoot \"invoke-windows-pseudoterminal.ps1\"";
    let windows_pty_child_binding =
        "Join-Path $PSScriptRoot \"start-windows-pseudoterminal-child.ps1\"";
    assert!(windows.contains(windows_pty_invoke_binding));
    assert!(windows.contains("Invoke-WindowsPseudoTerminal"));
    assert!(windows.contains("ExpectedCandidateVersion"));
    assert!(windows.contains("$candidateIdentity = (& $nibPath version"));
    assert!(windows.contains("& $nibPath update"));
    assert!(windows.contains("already-current update changed the executable"));
    assert!(windows.contains("Windows self-update cleanup did not converge"));
    assert!(windows.contains(".Name.StartsWith('.nib-update-'"));
    assert!(taskfile.contains(
        "  test:windows-pseudoterminal:\n    desc: Prove the inbox headless Windows console qualification host\n    platforms: [windows]\n    cmds:\n      - pwsh -NoLogo -NoProfile -File scripts/test-windows-pseudoterminal.ps1\n"
    ));
    assert!(windows_pty_host.contains("System32\\conhost.exe"));
    assert!(windows_pty_host.contains(windows_pty_child_binding));
    assert!(windows_pty_host.contains("$startInfo.ArgumentList.Add(\"--headless\")"));
    assert!(windows_pty_host.contains("$startInfo.ArgumentList.Add(\"--\")"));
    assert!(windows_pty_host.contains("$startInfo.RedirectStandardInput = $true"));
    assert!(windows_pty_host.contains("$startInfo.RedirectStandardOutput = $true"));
    assert!(windows_pty_host.contains("$startInfo.RedirectStandardError = $true"));
    assert!(windows_pty_host.contains("NIB_WINDOWS_PTY_CHILD_REQUEST"));
    assert!(windows_pty_host.contains("NIB_WINDOWS_PTY_EXIT_MARKER"));
    assert!(windows_pty_host.contains("NIB_WINDOWS_PTY_MODE_MARKER"));
    assert!(windows_pty_host.contains("$inputChunks = @($request.input_chunks)"));
    assert!(windows_pty_host.contains("$process.StandardInput.WriteAsync"));
    assert!(windows_pty_host.contains("$process.StandardInput.Close()"));
    assert!(windows_pty_host.contains("console_modes_restored"));
    assert!(windows_pty_host.contains("$process.Kill($true)"));
    assert!(windows_pty_host.contains("$process.WaitForExit(5000)"));
    assert!(windows_pty_host.contains("while ($true)"));
    assert!(windows_pty_child.contains("NIB_WINDOWS_PTY_CHILD_REQUEST"));
    assert!(windows_pty_child.contains("NIB_WINDOWS_PTY_EXIT_MARKER"));
    assert!(windows_pty_child.contains("NIB_WINDOWS_PTY_MODE_MARKER"));
    assert!(windows_pty_child.contains("GetConsoleMode"));
    assert!(windows_pty_child.contains("$consoleModesBefore"));
    assert!(windows_pty_child.contains("$consoleModesAfter"));
    assert!(windows_pty_child.contains("& ([string]$request.executable) @arguments"));
    assert!(windows_pty_child.contains("[Console]::Out.WriteLine"));
    assert!(windows_pty_invoke.contains("host-windows-pseudoterminal.ps1"));
    assert!(windows_pty_invoke.contains("[object[]]$InputChunks"));
    assert!(windows_pty_invoke.contains("64 chunk limit"));
    assert!(windows_pty_invoke.contains("4096 bytes"));
    assert!(windows_pty_invoke.contains("32768 bytes"));
    assert!(windows_pty_invoke.contains("Get-NibWindowsConsoleModeSnapshot"));
    assert!(windows_pty_invoke.contains("NibConsoleModeEvidence"));
    assert!(windows_pty_invoke.contains("$process.Kill($true)"));
    assert!(windows_pty_invoke.contains("$process.WaitForExit(5000)"));
    assert!(windows_pty_test.contains(windows_pty_invoke_binding));
    assert!(windows_pty_test.contains("Invoke-WindowsPseudoTerminal"));
    assert!(windows_pty_test.contains("-InputChunks"));
    assert!(windows_pty_test.contains("NIB_PSEUDOTERMINAL_INPUT:bounded-input"));
    assert!(windows_pty_test.contains("ChildConsoleModesRestored"));
    assert!(windows_pty_test.contains("NibConsoleModeEvidence"));
    assert!(windows_pty_test.contains("[Console]::IsErrorRedirected"));
    assert!(windows_pty_test.contains("$result.ExitCode -ne 0"));
    assert!(windows_pty_test.contains("$exitResult.ExitCode -ne 23"));
    assert!(windows_pty_test.contains("NIB_PTY_DESCENDANT_READY_FILE"));
    assert!(windows_pty_test.contains("NIB_PTY_PROBE_ARMED_FILE"));
    assert!(windows_pty_test.contains("-TimeoutMilliseconds 20000"));
    assert!(windows_pty_test.contains("$hostElapsedMilliseconds -lt 18000"));
    assert!(windows_pty_test.contains("$hostElapsedMilliseconds -ge 35000"));
    assert!(windows_pty_test.contains("$stopwatch.ElapsedMilliseconds -ge 40000"));
    assert!(windows_pty_test.contains("Windows pseudoterminal host exceeded its bounded timeout"));
    assert!(windows_pty_test.contains("timeout probe exited early with status"));
    assert!(windows_pty_test.contains("ready resistant descendant"));
    assert!(windows_pty_test.contains("if ($descendant.HasExited) { exit 44 }"));
    assert!(windows_pty_test.contains("probe did not arm after descendant readiness"));
    assert!(windows_pty_test.contains("timeout left its descendant running"));
    assert!(windows_pty_test.contains("$null -eq $descendantPid -and"));
    assert!(windows_pty_test.contains("test-windows-pseudoterminal-resistant-child.ps1"));
    assert!(resistant_child.contains("SetConsoleCtrlHandler"));
    assert!(resistant_child.contains("return controlType == 2"));
    let descendant_pid = resistant_child
        .find("NIB_PTY_DESCENDANT_PID_FILE")
        .expect("resistant descendant PID signal");
    let install_handler = resistant_child
        .find("[NibResistantConsoleChild]::Install()")
        .expect("resistant descendant handler installation");
    let descendant_ready = resistant_child
        .find("NIB_PTY_DESCENDANT_READY_FILE")
        .expect("resistant descendant readiness signal");
    assert!(descendant_pid < install_handler);
    assert!(install_handler < descendant_ready);
    let ci = read_repository_text(".github/workflows/ci.yml");
    let windows_smoke = ci
        .find("run: task test:windows-pseudoterminal")
        .expect("Windows pseudoterminal smoke step");
    let windows_checks = ci
        .find("run: task check:all-targets")
        .expect("Windows check step");
    assert!(windows_smoke < windows_checks);
}

#[test]
fn release_workflow_serializes_channels_and_rejects_stale_publication() {
    let workflow = read_repository_text(".github/workflows/release.yml");
    let transaction = read_repository_text("scripts/publish-release.sh");

    assert!(workflow.contains("concurrency:\n"));
    assert!(workflow.contains(
        "  group: release-${{ inputs.channel || (github.ref_name == 'main' && 'prod' || 'development') }}"
    ));
    assert!(workflow.contains("  cancel-in-progress: false"));
    assert!(workflow.contains("permissions:\n  contents: read\n"));
    let release_job = workflow
        .split_once("  release:\n")
        .map(|(_, release)| release)
        .expect("release job");
    assert!(release_job.contains("    permissions:\n      contents: write\n"));
    assert!(release_job
        .contains("    environment:\n      name: release-${{ needs.prepare.outputs.channel }}\n"));
    assert_eq!(workflow.matches("contents: write").count(), 1);

    let action_refs: Vec<_> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("uses: "))
        .map(|reference| reference.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        action_refs,
        vec![
            "actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5",
            "dtolnay/rust-toolchain@4be7066ada62dd38de10e7b70166bc74ed198c30",
            "arduino/setup-task@b91d5d2c96a56797b48ac1e0e89220bf64044611",
            "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
            "actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5",
            "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
        ]
    );

    assert!(workflow.contains("      - name: Publish staged release transaction"));
    assert!(workflow.contains("        run: scripts/publish-release.sh"));

    let draft_upload = transaction
        .rfind("release create \"$stage_tag\"")
        .expect("all assets must upload to a draft staging release");
    let stage_ref = transaction
        .rfind(r#"create_stage_ref_via_api "$GITHUB_SHA""#)
        .expect("the staging ref must be created explicitly before the draft release");
    let stage_target = transaction
        .rfind(r#"--target "$GITHUB_SHA""#)
        .expect("the draft release must bind to the fixed staging tag at the candidate SHA");
    let backup_ref = transaction
        .find(r#"--force-with-lease="refs/tags/$backup_tag:""#)
        .expect("the prior release SHA must have an owned backup tag");
    let asset_validation = transaction
        .rfind(r#"wait_for_staged_transaction "$GITHUB_SHA""#)
        .expect("staged release metadata and exact asset names must become visible boundedly");
    let backup_release = transaction
        .find(r#"backup_stable_release "$old_release_id""#)
        .expect("old release must remain available for rollback");
    let atomic_push = transaction
        .find(r#""$git_bin" push --atomic"#)
        .expect("source lease and rolling tag update must be atomic");
    let source_lease = transaction
        .find(r#"--force-with-lease="refs/heads/$GITHUB_REF_NAME:$GITHUB_SHA""#)
        .expect("tag update must lease the exact source branch SHA");
    let tag_lease = transaction
        .find(r#"--force-with-lease="refs/tags/$RELEASE_TAG:$old_tag_sha""#)
        .expect("rolling tag update must reject a concurrent replacement");
    let promote_release = transaction
        .rfind(r#"promote_stage_release "$stage_release_id""#)
        .expect("staged release must be promoted by stable release ID");
    let commit_marker = transaction
        .rfind("committed=1")
        .expect("promotion must have a transaction commit marker");

    assert!(transaction.contains("trap on_exit EXIT"));
    assert!(transaction.contains("recover_rollback"));
    assert!(transaction.contains("forward-only"));
    assert!(transaction.contains("move_rolling_ref_via_api"));
    assert!(transaction.contains("--verify-tag"));
    assert!(transaction.contains("stage_visibility_attempts=12"));
    assert!(transaction.contains(
        "release_is_owned_staged_transaction \"$release_id\" \"$candidate_sha\" || return 1"
    ));
    assert!(transaction
        .contains("Staged release identity changed from $observed_release_id to $release_id."));
    assert!(transaction.contains(
        "Staged release $release_id transaction marker changed during visibility checks."
    ));
    assert!(transaction.contains("Failed to read staged release $release_id assets."));
    assert!(transaction.contains(r#"stage_tag="nib-release-stage-$RELEASE_CHANNEL""#));
    assert!(transaction.contains(r#"backup_tag="nib-release-backup-$RELEASE_CHANNEL""#));
    assert!(transaction.contains(r#"--jq '.assets[].name'"#));
    assert!(transaction.contains(r#"prerelease=$(release_field "$release_id" prerelease)"#));
    assert!(draft_upload < stage_target);
    assert!(backup_ref < draft_upload);
    assert!(backup_ref < stage_ref);
    assert!(stage_ref < draft_upload);
    assert!(draft_upload < asset_validation);
    assert!(backup_ref < backup_release);
    assert!(asset_validation < backup_release);
    assert!(backup_release < atomic_push);
    assert!(atomic_push < source_lease);
    assert!(source_lease < tag_lease);
    assert!(tag_lease < promote_release);
    assert!(promote_release < commit_marker);
    assert!(!transaction.contains("release upload \"$RELEASE_TAG\""));
}

#[cfg(unix)]
#[test]
fn atomic_release_ref_lease_rejects_a_stale_source_branch() {
    let fixture = tempfile::tempdir().expect("release lease fixture");
    let remote = fixture.path().join("remote.git");
    let work = fixture.path().join("work");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git command")
    };

    assert!(git(
        fixture.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()]
    )
    .status
    .success());
    assert!(git(fixture.path(), &["init", "-q", work.to_str().unwrap()])
        .status
        .success());
    for args in [
        ["config", "user.email", "release@example.invalid"],
        ["config", "user.name", "release fixture"],
    ] {
        assert!(git(&work, &args).status.success());
    }
    fs::write(work.join("artifact.txt"), "one\n").expect("first commit fixture");
    assert!(git(&work, &["add", "artifact.txt"]).status.success());
    assert!(git(&work, &["commit", "-qm", "one"]).status.success());
    assert!(git(&work, &["branch", "-M", "main"]).status.success());
    assert!(git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()]
    )
    .status
    .success());
    assert!(git(&work, &["push", "-q", "-u", "origin", "main"])
        .status
        .success());
    let stale_sha = String::from_utf8(git(&work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert!(git(&work, &["tag", "prod-latest", &stale_sha])
        .status
        .success());
    assert!(git(
        &work,
        &[
            "push",
            "--atomic",
            &format!("--force-with-lease=refs/heads/main:{stale_sha}"),
            "--force-with-lease=refs/tags/prod-latest:",
            "origin",
            &format!("{stale_sha}:refs/heads/main"),
            "refs/tags/prod-latest",
        ],
    )
    .status
    .success());

    fs::write(work.join("artifact.txt"), "two\n").expect("second commit fixture");
    assert!(git(&work, &["commit", "-qam", "two"]).status.success());
    assert!(git(&work, &["push", "-q", "origin", "main"])
        .status
        .success());

    let stale_push = git(
        &work,
        &[
            "push",
            "--atomic",
            &format!("--force-with-lease=refs/heads/main:{stale_sha}"),
            &format!("--force-with-lease=refs/tags/prod-latest:{stale_sha}"),
            "origin",
            &format!("{stale_sha}:refs/heads/main"),
            "refs/tags/prod-latest",
        ],
    );
    assert!(!stale_push.status.success());
    assert!(String::from_utf8_lossy(&stale_push.stderr).contains("stale"));
}

#[cfg(unix)]
#[derive(Clone, Copy, Default)]
struct ReleaseFaults {
    partial_create: bool,
    advance_after_atomic: bool,
    ambiguous_promote: bool,
    advance_after_promote: bool,
    fail_first_atomic: bool,
    fail_old_restore: bool,
    kill_after_backup_ref: bool,
    kill_after_stage_ref: bool,
    kill_before_backup_ref_delete: bool,
    kill_after_old_backup: bool,
    move_rolling_on_second_stage_lookup: bool,
    fail_rolling_rollback: bool,
    fail_promote_before_mutation: bool,
    empty_stage_asset: bool,
    fail_release_list_read: bool,
    fail_tagged_release_list_read: bool,
    fail_untagged_release_list_read: bool,
    fail_release_get_read: bool,
    fail_stage_delete_before_mutation: bool,
    fail_release_id_list_after_delete: bool,
    fail_ls_remote_read: bool,
    kill_after_old_delete: bool,
    rewrite_stage_to_untagged: bool,
    rewrite_stage_to_untagged_on_create: bool,
    hide_stage_list_initially: bool,
    hide_stage_list_after_observation_once: bool,
    delay_stage_assets_once: bool,
    delay_stage_asset_state_once: bool,
    kill_after_untagged_rewrite: bool,
}

#[cfg(unix)]
struct ReleaseTransactionHarness {
    _temp: tempfile::TempDir,
    work: PathBuf,
    remote: PathBuf,
    fake_git: PathBuf,
    fake_gh: PathBuf,
    gh_state: PathBuf,
    dist: PathBuf,
    old_sha: String,
    release_sha: String,
    advance_sha: String,
}

#[cfg(unix)]
impl ReleaseTransactionHarness {
    fn new() -> Self {
        Self::with_workflow_change(false)
    }

    fn with_workflow_change(workflow_change: bool) -> Self {
        let temp = tempfile::tempdir().expect("release transaction fixture");
        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");
        let fake_bin = temp.path().join("bin");
        let gh_state = temp.path().join("gh-state");
        let dist = temp.path().join("dist");
        fs::create_dir_all(&fake_bin).expect("fake release bin");
        fs::create_dir_all(gh_state.join("releases")).expect("fake release state");
        fs::create_dir_all(&dist).expect("release assets");

        let git = |cwd: &std::path::Path, args: &[&str]| {
            Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .expect("git command")
        };
        assert!(git(
            temp.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()]
        )
        .status
        .success());
        assert!(git(temp.path(), &["init", "-q", work.to_str().unwrap()])
            .status
            .success());
        for args in [
            ["config", "user.email", "release@example.invalid"],
            ["config", "user.name", "release fixture"],
        ] {
            assert!(git(&work, &args).status.success());
        }
        fs::write(work.join("artifact.txt"), "old\n").expect("old release fixture");
        fs::create_dir_all(work.join(".github/workflows")).expect("workflow fixture directory");
        fs::write(work.join(".github/workflows/release.yml"), "name: old\n")
            .expect("old workflow fixture");
        assert!(git(&work, &["add", "."]).status.success());
        assert!(git(&work, &["commit", "-qm", "old"]).status.success());
        assert!(git(&work, &["branch", "-M", "main"]).status.success());
        assert!(git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()]
        )
        .status
        .success());
        assert!(git(&work, &["push", "-q", "-u", "origin", "main"])
            .status
            .success());
        let old_sha = Self::git_stdout(&work, &["rev-parse", "HEAD"]);
        assert!(git(&work, &["tag", "prod-latest", &old_sha])
            .status
            .success());
        assert!(git(&work, &["push", "-q", "origin", "prod-latest"])
            .status
            .success());

        fs::write(work.join("artifact.txt"), "release\n").expect("release fixture");
        if workflow_change {
            fs::write(
                work.join(".github/workflows/release.yml"),
                "name: release\n",
            )
            .expect("release workflow fixture");
        }
        assert!(git(&work, &["commit", "-qam", "release"]).status.success());
        let release_sha = Self::git_stdout(&work, &["rev-parse", "HEAD"]);
        assert!(git(&work, &["push", "-q", "origin", "main"])
            .status
            .success());

        fs::write(work.join("artifact.txt"), "advance\n").expect("advance fixture");
        assert!(git(&work, &["commit", "-qam", "advance"]).status.success());
        let advance_sha = Self::git_stdout(&work, &["rev-parse", "HEAD"]);
        assert!(git(
            &work,
            &["push", "-q", "origin", "HEAD:refs/nib-tests/advance"]
        )
        .status
        .success());
        assert!(git(&work, &["reset", "--hard", &release_sha])
            .status
            .success());

        fs::write(gh_state.join("releases/old.tag"), "prod-latest\n").expect("old release tag");
        fs::write(gh_state.join("releases/old.draft"), "false\n").expect("old release draft state");
        fs::write(gh_state.join("releases/old.prerelease"), "false\n")
            .expect("old release prerelease state");
        fs::write(
            gh_state.join("releases/old.body"),
            "Prior production release.\n",
        )
        .expect("old release body");
        fs::write(gh_state.join("releases/old.target"), format!("{old_sha}\n"))
            .expect("old release target");
        fs::write(
            gh_state.join("releases/old.assets"),
            format!("{}\n", LEGACY_RELEASE_ASSET_NAMES.join("\n")),
        )
        .expect("old release assets");
        fs::write(
            gh_state.join("releases/old.asset-metadata"),
            format!(
                "{}\n",
                LEGACY_RELEASE_ASSET_NAMES
                    .map(|name| format!("{name}|uploaded|1"))
                    .join("\n")
            ),
        )
        .expect("old release asset metadata");

        for name in RELEASE_ARCHIVE_NAMES {
            let archive = dist.join(name);
            fs::write(&archive, format!("archive fixture for {name}\n")).expect("archive fixture");
            let checksum = Command::new("sha256sum")
                .arg(&archive)
                .output()
                .expect("calculate fixture checksum");
            assert!(
                checksum.status.success(),
                "{}",
                String::from_utf8_lossy(&checksum.stderr)
            );
            let digest = String::from_utf8(checksum.stdout)
                .expect("UTF-8 fixture checksum")
                .split_whitespace()
                .next()
                .expect("fixture checksum digest")
                .to_string();
            fs::write(
                dist.join(format!("{name}.sha256")),
                format!("{digest}  {name}\n"),
            )
            .expect("checksum fixture");
        }

        let fake_gh = fake_bin.join("gh");
        write_executable(
            &fake_gh,
            r#"#!/usr/bin/env bash
set -euo pipefail
releases="$FAKE_GH_STATE/releases"
events="$FAKE_GH_STATE/events.log"

if [ "${1:-}" = release ] && [ "${2:-}" = create ]; then
  tag=$3
  id=stage
  shift 3
  assets=()
  draft=false
  prerelease=false
  body=
  target=
  verify_tag=false
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --draft) draft=true ;;
      --prerelease) prerelease=true ;;
      --notes) shift; body=$1 ;;
      --target) shift; target=$1 ;;
      --verify-tag) verify_tag=true ;;
      --title) shift ;;
  *.tar.gz|*.zip|*.sha256|*.json) assets+=("$(basename "$1")") ;;
    esac
    shift
  done
  if [ "$verify_tag" = true ]; then
    tag_sha=$("$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" rev-parse --verify "refs/tags/$tag")
    [ -z "$target" ] || [ "$tag_sha" = "$target" ]
  elif [ -n "$target" ]; then
    "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref "refs/tags/$tag" "$target"
  fi
  printf '%s\n' "$tag" > "$releases/$id.tag"
  printf '%s\n' "$draft" > "$releases/$id.draft"
  printf '%s\n' "$prerelease" > "$releases/$id.prerelease"
  printf '%s\n' "$body" > "$releases/$id.body"
  printf '%s\n' "$target" > "$releases/$id.target"
  printf 'CREATE %s %s\n' "$id" "$tag" >> "$events"
  if [ "${FAKE_GH_FAIL_CREATE:-0}" = 1 ]; then
    printf '%s\n' "${assets[0]}" > "$releases/$id.assets"
    printf '%s|uploaded|1\n' "${assets[0]}" > "$releases/$id.asset-metadata"
    exit 1
  fi
  printf '%s\n' "${assets[@]}" | LC_ALL=C sort > "$releases/$id.assets"
  : > "$releases/$id.asset-metadata"
  empty_written=0
  while IFS= read -r asset; do
    size=1
    if [ "${FAKE_GH_EMPTY_STAGE_ASSET:-0}" = 1 ] && [ "$empty_written" -eq 0 ]; then
      size=0
      empty_written=1
    fi
    printf '%s|uploaded|%s\n' "$asset" "$size" >> "$releases/$id.asset-metadata"
  done < "$releases/$id.assets"
  if [ "${FAKE_REWRITE_STAGE_TO_UNTAGGED_ON_CREATE:-0}" = 1 ]; then
    printf '%s\n' 'untagged-708ba2fca6bbe012874c' > "$releases/$id.tag"
    printf '%s\n' 'RETAG CREATE stage untagged-708ba2fca6bbe012874c' >> "$events"
  fi
  exit 0
fi

if [ "${1:-}" != api ]; then
  echo "unsupported fake gh command: $*" >&2
  exit 2
fi
shift
method=GET
endpoint=
query=
fields=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --paginate) shift ;;
    --method) method=$2; shift 2 ;;
    --jq) query=$2; shift 2 ;;
    -f|-F) fields+=("$2"); shift 2 ;;
    repos/*) endpoint=$1; shift ;;
    *) shift ;;
  esac
done

if [[ "$endpoint" == *"releases?"* ]]; then
  if [ "${FAKE_GH_FAIL_LIST_READ:-0}" = 1 ]; then
    exit 1
  fi
  if [[ "$query" == *'startswith("untagged-")'* ]]; then
    if [ "${FAKE_GH_FAIL_UNTAGGED_LIST_READ:-0}" = 1 ]; then
      exit 1
    fi
    for path in "$releases"/*.tag; do
      [ -e "$path" ] || continue
      id=$(basename "$path" .tag)
      tag=$(tr -d '\n' < "$path")
      draft=$(tr -d '\n' < "$releases/$id.draft")
      body=$(cat "$releases/$id.body")
      if [[ "$tag" == untagged-* ]] && [ "$draft" = true ] &&
        grep -Fq 'channel=prod' <<<"$body"; then
        if [ "${FAKE_HIDE_STAGE_LIST_INITIALLY:-0}" = 1 ] &&
          [ ! -e "$FAKE_GH_STATE/hidden-stage-list-initially" ]; then
          touch "$FAKE_GH_STATE/hidden-stage-list-initially"
          touch "$FAKE_GH_STATE/last-stage-list-empty"
        elif [ "${FAKE_HIDE_STAGE_LIST_AFTER_OBSERVATION_ONCE:-0}" = 1 ] &&
          [ -e "$FAKE_GH_STATE/delayed-stage-assets" ] &&
          [ ! -e "$FAKE_GH_STATE/hidden-stage-list-after-observation" ]; then
          touch "$FAKE_GH_STATE/hidden-stage-list-after-observation"
          touch "$FAKE_GH_STATE/last-stage-list-empty"
        else
          rm -f "$FAKE_GH_STATE/last-stage-list-empty"
          printf '%s\n' "$id"
        fi
      fi
    done
  elif [[ "$query" == *".id | tostring"* ]]; then
    if [ "${FAKE_GH_FAIL_ID_LIST_AFTER_DELETE:-0}" = 1 ] &&
      [ -e "$FAKE_GH_STATE/stage-delete-attempted" ]; then
      exit 1
    fi
    id=$(printf '%s\n' "$query" | sed -n 's/.*tostring) == "\([^"]*\)".*/\1/p')
    if [ "$id" = stage ] && [ "${FAKE_MOVE_ROLLING_ON_SECOND_STAGE_LOOKUP:-0}" = 1 ]; then
      lookup_file="$FAKE_GH_STATE/stage-id-lookups"
      lookup_count=0
      [ ! -f "$lookup_file" ] || lookup_count=$(cat "$lookup_file")
      lookup_count=$((lookup_count + 1))
      printf '%s\n' "$lookup_count" > "$lookup_file"
      if [ "$lookup_count" -eq 2 ]; then
        "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref refs/tags/prod-latest "$GITHUB_SHA"
      fi
    fi
    if [ -f "$releases/$id.tag" ]; then
      cat "$releases/$id.tag"
    fi
  else
    if [ "${FAKE_GH_FAIL_TAGGED_LIST_READ:-0}" = 1 ]; then
      exit 1
    fi
    tag=$(printf '%s\n' "$query" | sed -n 's/.*tag_name == "\([^"]*\)".*/\1/p')
    for path in "$releases"/*.tag; do
      [ -e "$path" ] || continue
      if [ "$(tr -d '\n' < "$path")" = "$tag" ]; then
        basename "$path" .tag
      fi
    done
  fi
  exit 0
fi

if [[ "$endpoint" == */git/refs || "$endpoint" == */git/refs/tags/* ]]; then
  ref=
  sha=
  if [ "$method" != DELETE ]; then
    for field in "${fields[@]}"; do
      key=${field%%=*}
      value=${field#*=}
      case "$key" in
        ref) ref=$value ;;
        sha) sha=$value ;;
      esac
    done
  fi
  case "$method" in
    POST)
      printf 'REF CREATE %s %s\n' "$ref" "$sha" >> "$events"
      "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref "$ref" "$sha"
      if [ "${FAKE_KILL_AFTER_STAGE_REF:-0}" = 1 ] && [ "$ref" = refs/tags/nib-release-stage-prod ]; then
        kill -KILL "$PPID"
        exit 137
      fi
      ;;
    PATCH)
      ref="refs/${endpoint#*/git/refs/}"
      printf 'REF PATCH %s %s\n' "$ref" "$sha" >> "$events"
      "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref "$ref" "$sha"
      ;;
    DELETE)
      ref="refs/${endpoint#*/git/refs/}"
      printf 'REF DELETE %s\n' "$ref" >> "$events"
      "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref -d "$ref"
      ;;
    *) echo "unsupported fake ref method: $method" >&2; exit 2 ;;
  esac
  exit 0
fi

id=${endpoint##*/}
case "$method" in
  GET)
    if [ "${FAKE_GH_FAIL_GET_READ:-0}" = 1 ]; then
      exit 1
    fi
    case "$query" in
      .tag_name)
        if [ "$id" = stage ] && [ -e "$FAKE_GH_STATE/last-stage-list-empty" ]; then
          touch "$FAKE_GH_STATE/pinned-stage-revalidated"
        fi
        cat "$releases/$id.tag"
        ;;
      .draft) cat "$releases/$id.draft" ;;
      .prerelease) cat "$releases/$id.prerelease" ;;
      .body) cat "$releases/$id.body" ;;
      .target_commitish) cat "$releases/$id.target" ;;
      '.assets[].name')
        if [ "$id" = stage ] && [ "${FAKE_DELAY_STAGE_ASSETS_ONCE:-0}" = 1 ] &&
          [ ! -e "$FAKE_GH_STATE/delayed-stage-assets" ]; then
          head -n 1 "$releases/$id.assets"
          touch "$FAKE_GH_STATE/delayed-stage-assets"
        else
          cat "$releases/$id.assets"
        fi
        ;;
      '.assets[] | select(.state != "uploaded" or .size <= 0) | .name')
        if [ "$id" = stage ] && [ "${FAKE_DELAY_STAGE_ASSET_STATE_ONCE:-0}" = 1 ] &&
          [ ! -e "$FAKE_GH_STATE/delayed-stage-asset-state" ]; then
          head -n 1 "$releases/$id.assets"
          touch "$FAKE_GH_STATE/delayed-stage-asset-state"
        else
          awk -F '|' '$2 != "uploaded" || $3 <= 0 { print $1 }' "$releases/$id.asset-metadata"
        fi
        ;;
      '.assets | length') awk 'NF { count += 1 } END { print count + 0 }' "$releases/$id.assets" ;;
      *) echo "unsupported fake gh query: $query" >&2; exit 2 ;;
    esac
    ;;
  PATCH)
    promoted=0
    backed_up=0
    event="PATCH $id"
    for field in "${fields[@]}"; do
      key=${field%%=*}
      case "$key" in
        tag_name|draft|prerelease) event="$event $field" ;;
      esac
    done
    printf '%s\n' "$event" >> "$events"
    if [ "$id" = stage ] && [ "${FAKE_GH_FAIL_PROMOTE_BEFORE_MUTATION:-0}" = 1 ]; then
      for field in "${fields[@]}"; do
        if [ "$field" = tag_name=prod-latest ]; then
          exit 1
        fi
      done
    fi
    for field in "${fields[@]}"; do
      key=${field%%=*}
      value=${field#*=}
      if [ "$id" = old ] && [ "$key" = tag_name ] && [ "$value" = prod-latest ] && [ "${FAKE_GH_FAIL_OLD_RESTORE:-0}" = 1 ]; then
        exit 1
      fi
      case "$key" in
        tag_name)
          printf '%s\n' "$value" > "$releases/$id.tag"
          [ "$id:$value" = stage:prod-latest ] && promoted=1
          [ "$id:$value" = old:nib-release-backup-prod ] && backed_up=1
          ;;
        draft) printf '%s\n' "$value" > "$releases/$id.draft" ;;
        prerelease) printf '%s\n' "$value" > "$releases/$id.prerelease" ;;
        body) printf '%s\n' "$value" > "$releases/$id.body" ;;
        target_commitish) printf '%s\n' "$value" > "$releases/$id.target" ;;
      esac
    done
    if [ "$id" = stage ] && [ "${FAKE_REWRITE_STAGE_TO_UNTAGGED:-0}" = 1 ] &&
      [ ! -e "$FAKE_GH_STATE/retagged-stage" ] &&
      grep -Fq 'staged_release_id=stage' "$releases/$id.body"; then
      printf '%s\n' 'untagged-708ba2fca6bbe012874c' > "$releases/$id.tag"
      printf '%s\n' 'RETAG stage untagged-708ba2fca6bbe012874c' >> "$events"
      touch "$FAKE_GH_STATE/retagged-stage"
      if [ "${FAKE_KILL_AFTER_UNTAGGED_REWRITE:-0}" = 1 ]; then
        kill -KILL "$PPID"
        exit 137
      fi
    fi
    if [ "$backed_up" -eq 1 ] && [ "${FAKE_KILL_AFTER_OLD_BACKUP:-0}" = 1 ]; then
      kill -KILL "$PPID"
      exit 137
    fi
    if [ "$promoted" -eq 1 ] && [ "${FAKE_ADVANCE_AFTER_PROMOTE:-0}" = 1 ] && [ ! -e "$FAKE_GIT_MARKER" ]; then
      "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref refs/heads/main "$FAKE_ADVANCE_SHA"
      touch "$FAKE_GIT_MARKER"
    fi
    if [ "$promoted" -eq 1 ] && [ "${FAKE_GH_FAIL_PROMOTE:-0}" = 1 ]; then
      exit 1
    fi
    ;;
  DELETE)
    printf 'DELETE %s\n' "$id" >> "$events"
    if [ "$id" = stage ] && [ "${FAKE_GH_FAIL_STAGE_DELETE_BEFORE_MUTATION:-0}" = 1 ]; then
      touch "$FAKE_GH_STATE/stage-delete-attempted"
      exit 1
    fi
    rm -f "$releases/$id.tag" "$releases/$id.draft" "$releases/$id.prerelease" "$releases/$id.body" "$releases/$id.target" "$releases/$id.assets" "$releases/$id.asset-metadata"
    if [ "$id" = old ] && [ "${FAKE_KILL_AFTER_OLD_DELETE:-0}" = 1 ]; then
      kill -KILL "$PPID"
      exit 137
    fi
    ;;
  *) echo "unsupported fake gh method: $method" >&2; exit 2 ;;
esac
"#,
        );

        let fake_git = fake_bin.join("git");
        write_executable(
            &fake_git,
            r#"#!/usr/bin/env bash
set +e
if [ "${1:-}" = ls-remote ] && [ "${FAKE_FAIL_LS_REMOTE_READ:-0}" = 1 ]; then
  exit 1
fi
if [ "${1:-}" = push ]; then
  printf 'GIT %s\n' "$*" >> "$FAKE_GH_STATE/events.log"
fi
if [ "${1:-}" = push ] && [ "${FAKE_KILL_BEFORE_BACKUP_REF_DELETE:-0}" = 1 ]; then
  for argument in "$@"; do
    if [ "$argument" = ":refs/tags/nib-release-backup-prod" ]; then
      kill -KILL "$PPID"
      exit 137
    fi
  done
fi
if [ "${1:-}" = push ] && [ "${FAKE_FAIL_ROLLING_ROLLBACK:-0}" = 1 ]; then
  rollback_lease=0
  rollback_target=0
  for argument in "$@"; do
    [ "$argument" != "--force-with-lease=refs/tags/prod-latest:$GITHUB_SHA" ] || rollback_lease=1
    [ "$argument" != "$FAKE_OLD_SHA:refs/tags/prod-latest" ] || rollback_target=1
  done
  if [ "$rollback_lease" -eq 1 ] && [ "$rollback_target" -eq 1 ]; then
    exit 1
  fi
fi
if [ "${1:-}" = push ] && [ "${FAKE_FAIL_FIRST_ATOMIC:-0}" = 1 ] && [ ! -e "$FAKE_GIT_MARKER" ]; then
  for argument in "$@"; do
    if [ "$argument" = --atomic ]; then
      touch "$FAKE_GIT_MARKER"
      exit 1
    fi
  done
fi
"$REAL_GIT_BIN" "$@"
status=$?
if [ "$status" -eq 0 ] && [ "${1:-}" = push ] && [ "${FAKE_KILL_AFTER_BACKUP_REF:-0}" = 1 ]; then
  for argument in "$@"; do
    if [ "$argument" = "$FAKE_OLD_SHA:refs/tags/nib-release-backup-prod" ]; then
      kill -KILL "$PPID"
      exit 137
    fi
  done
fi
if [ "$status" -eq 0 ] && [ "${1:-}" = push ] && [ "${FAKE_ADVANCE_AFTER_ATOMIC:-0}" = 1 ]; then
  for argument in "$@"; do
    if [ "$argument" = --atomic ] && [ ! -e "$FAKE_GIT_MARKER" ]; then
      "$REAL_GIT_BIN" --git-dir="$FAKE_REMOTE" update-ref refs/heads/main "$FAKE_ADVANCE_SHA"
      touch "$FAKE_GIT_MARKER"
      break
    fi
  done
fi
exit "$status"
"#,
        );

        Self {
            _temp: temp,
            work,
            remote,
            fake_git,
            fake_gh,
            gh_state,
            dist,
            old_sha,
            release_sha,
            advance_sha,
        }
    }

    fn git_stdout(cwd: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git output");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_string()
    }

    fn run(&self, faults: ReleaseFaults) -> Output {
        self.run_from_ref("main", faults)
    }

    fn run_from_ref(&self, source_ref: &str, faults: ReleaseFaults) -> Output {
        self.run_from_ref_with_stage_visibility_delay(source_ref, faults, "0")
    }

    fn run_with_stage_visibility_delay(&self, delay: &str) -> Output {
        self.run_from_ref_with_stage_visibility_delay("main", ReleaseFaults::default(), delay)
    }

    fn run_from_ref_with_stage_visibility_delay(
        &self,
        source_ref: &str,
        faults: ReleaseFaults,
        delay: &str,
    ) -> Output {
        let real_git = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .expect("locate git")
                .stdout,
        )
        .expect("git path")
        .trim()
        .to_string();
        Command::new("bash")
            .arg(repository_root().join("scripts/publish-release.sh"))
            .current_dir(&self.work)
            .env("GITHUB_REF_NAME", source_ref)
            .env("GITHUB_REPOSITORY", "example/nib")
            .env("GITHUB_SHA", &self.release_sha)
            .env("RELEASE_CHANNEL", "prod")
            .env("RELEASE_PRERELEASE", "false")
            .env("RELEASE_TAG", "prod-latest")
            .env("RELEASE_TITLE", "nib production")
            .env("NIB_RELEASE_VERSION", env!("CARGO_PKG_VERSION"))
            .env("NIB_RELEASE_GIT_BIN", &self.fake_git)
            .env("NIB_RELEASE_GH_BIN", &self.fake_gh)
            .env("NIB_RELEASE_DIST_DIR", &self.dist)
            .env("NIB_RELEASE_STAGE_VISIBILITY_DELAY_SECONDS", delay)
            .env("FAKE_GH_STATE", &self.gh_state)
            .env(
                "FAKE_GH_FAIL_CREATE",
                if faults.partial_create { "1" } else { "0" },
            )
            .env(
                "FAKE_GH_FAIL_PROMOTE",
                if faults.ambiguous_promote { "1" } else { "0" },
            )
            .env(
                "FAKE_ADVANCE_AFTER_PROMOTE",
                if faults.advance_after_promote {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_OLD_RESTORE",
                if faults.fail_old_restore { "1" } else { "0" },
            )
            .env(
                "FAKE_KILL_AFTER_OLD_BACKUP",
                if faults.kill_after_old_backup {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_KILL_AFTER_BACKUP_REF",
                if faults.kill_after_backup_ref {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_KILL_AFTER_STAGE_REF",
                if faults.kill_after_stage_ref {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_KILL_BEFORE_BACKUP_REF_DELETE",
                if faults.kill_before_backup_ref_delete {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_MOVE_ROLLING_ON_SECOND_STAGE_LOOKUP",
                if faults.move_rolling_on_second_stage_lookup {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_PROMOTE_BEFORE_MUTATION",
                if faults.fail_promote_before_mutation {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_EMPTY_STAGE_ASSET",
                if faults.empty_stage_asset { "1" } else { "0" },
            )
            .env(
                "FAKE_GH_FAIL_LIST_READ",
                if faults.fail_release_list_read {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_TAGGED_LIST_READ",
                if faults.fail_tagged_release_list_read {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_UNTAGGED_LIST_READ",
                if faults.fail_untagged_release_list_read {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_GET_READ",
                if faults.fail_release_get_read {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_STAGE_DELETE_BEFORE_MUTATION",
                if faults.fail_stage_delete_before_mutation {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_GH_FAIL_ID_LIST_AFTER_DELETE",
                if faults.fail_release_id_list_after_delete {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_ADVANCE_AFTER_ATOMIC",
                if faults.advance_after_atomic {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_FAIL_FIRST_ATOMIC",
                if faults.fail_first_atomic { "1" } else { "0" },
            )
            .env(
                "FAKE_FAIL_ROLLING_ROLLBACK",
                if faults.fail_rolling_rollback {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_FAIL_LS_REMOTE_READ",
                if faults.fail_ls_remote_read { "1" } else { "0" },
            )
            .env(
                "FAKE_KILL_AFTER_OLD_DELETE",
                if faults.kill_after_old_delete {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_REWRITE_STAGE_TO_UNTAGGED",
                if faults.rewrite_stage_to_untagged {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_REWRITE_STAGE_TO_UNTAGGED_ON_CREATE",
                if faults.rewrite_stage_to_untagged_on_create {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_DELAY_STAGE_ASSETS_ONCE",
                if faults.delay_stage_assets_once {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_HIDE_STAGE_LIST_INITIALLY",
                if faults.hide_stage_list_initially {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_HIDE_STAGE_LIST_AFTER_OBSERVATION_ONCE",
                if faults.hide_stage_list_after_observation_once {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_DELAY_STAGE_ASSET_STATE_ONCE",
                if faults.delay_stage_asset_state_once {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "FAKE_KILL_AFTER_UNTAGGED_REWRITE",
                if faults.kill_after_untagged_rewrite {
                    "1"
                } else {
                    "0"
                },
            )
            .env("FAKE_ADVANCE_SHA", &self.advance_sha)
            .env("FAKE_OLD_SHA", &self.old_sha)
            .env("FAKE_REMOTE", &self.remote)
            .env("FAKE_GIT_MARKER", self.gh_state.join("advanced"))
            .env("REAL_GIT_BIN", real_git)
            .output()
            .expect("release transaction")
    }

    fn remote_ref(&self, reference: &str) -> String {
        let output = Command::new("git")
            .args(["ls-remote", self.remote.to_str().unwrap(), reference])
            .output()
            .expect("remote ref");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("remote ref output")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    }

    fn delete_remote_ref(&self, reference: &str) {
        let output = Command::new("git")
            .args([
                "--git-dir",
                self.remote.to_str().unwrap(),
                "update-ref",
                "-d",
                reference,
            ])
            .output()
            .expect("delete fixture remote ref");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_remote_ref(&self, reference: &str, sha: &str) {
        let output = Command::new("git")
            .args([
                "--git-dir",
                self.remote.to_str().unwrap(),
                "update-ref",
                reference,
                sha,
            ])
            .output()
            .expect("write fixture remote ref");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn release_id(&self, tag: &str) -> Option<String> {
        fs::read_dir(self.gh_state.join("releases"))
            .expect("release state")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("tag")
            })
            .find_map(|entry| {
                (fs::read_to_string(entry.path()).ok()?.trim() == tag).then(|| {
                    entry
                        .path()
                        .file_stem()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
            })
    }

    fn release_field(&self, release_id: &str, field: &str) -> String {
        fs::read_to_string(
            self.gh_state
                .join("releases")
                .join(format!("{release_id}.{field}")),
        )
        .unwrap_or_else(|error| panic!("read {release_id} release {field}: {error}"))
        .trim()
        .to_string()
    }

    fn release_assets(&self, release_id: &str) -> Vec<String> {
        self.release_field(release_id, "assets")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn assert_complete_release(&self, release_id: &str, expected_tag: &str, expected_draft: &str) {
        assert_eq!(self.release_field(release_id, "tag"), expected_tag);
        assert_eq!(self.release_field(release_id, "draft"), expected_draft);
        assert_eq!(self.release_field(release_id, "prerelease"), "false");
        assert_eq!(
            self.release_assets(release_id),
            RELEASE_ASSET_NAMES.map(str::to_string)
        );
        assert_eq!(
            self.release_field(release_id, "asset-metadata")
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            RELEASE_ASSET_NAMES
                .map(|name| format!("{name}|uploaded|1"))
                .to_vec()
        );
    }

    fn assert_complete_candidate_release(&self, expected_tag: &str, expected_draft: &str) {
        self.assert_complete_candidate_release_with_prior(expected_tag, expected_draft, "old");
    }

    fn assert_complete_prior_release(
        &self,
        release_id: &str,
        expected_tag: &str,
        expected_draft: &str,
    ) {
        assert_eq!(self.release_field(release_id, "tag"), expected_tag);
        assert_eq!(self.release_field(release_id, "draft"), expected_draft);
        assert_eq!(self.release_field(release_id, "prerelease"), "false");
        assert_eq!(
            self.release_assets(release_id),
            LEGACY_RELEASE_ASSET_NAMES.map(str::to_string)
        );
        assert_eq!(
            self.release_field(release_id, "asset-metadata")
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            LEGACY_RELEASE_ASSET_NAMES
                .map(|name| format!("{name}|uploaded|1"))
                .to_vec()
        );
    }

    fn assert_complete_candidate_release_with_prior(
        &self,
        expected_tag: &str,
        expected_draft: &str,
        prior_release_id: &str,
    ) {
        self.assert_complete_release("stage", expected_tag, expected_draft);
        let marker = self.release_field("stage", "body");
        let expected_fields = [
            "<!-- nib-release-transaction-v1".to_string(),
            "channel=prod".to_string(),
            format!("candidate_sha={}", self.release_sha),
            format!("prior_sha={}", self.old_sha),
            "staged_release_id=stage".to_string(),
            format!("prior_release_id={prior_release_id}"),
            "prior_release_draft=false".to_string(),
            "-->".to_string(),
        ];
        for field in expected_fields {
            assert!(
                marker.lines().any(|line| line == field.as_str()),
                "candidate marker is missing {field:?}: {marker}"
            );
        }
    }

    fn assert_prior_release_restored(&self) {
        assert_eq!(self.remote_ref("refs/tags/prod-latest"), self.old_sha);
        assert_eq!(self.release_id("prod-latest").as_deref(), Some("old"));
        self.assert_complete_prior_release("old", "prod-latest", "false");
        assert!(self.release_id("nib-release-stage-prod").is_none());
        assert!(self
            .remote_ref("refs/tags/nib-release-stage-prod")
            .is_empty());
        assert!(self
            .remote_ref("refs/tags/nib-release-backup-prod")
            .is_empty());
    }

    fn assert_backed_up_transaction_retained(&self) {
        assert_eq!(self.remote_ref("refs/heads/main"), self.release_sha);
        assert_eq!(self.remote_ref("refs/tags/prod-latest"), self.old_sha);
        assert_eq!(
            self.remote_ref("refs/tags/nib-release-stage-prod"),
            self.release_sha
        );
        assert_eq!(
            self.remote_ref("refs/tags/nib-release-backup-prod"),
            self.old_sha
        );
        assert_eq!(
            self.release_id("nib-release-stage-prod").as_deref(),
            Some("stage")
        );
        assert_eq!(
            self.release_id("nib-release-backup-prod").as_deref(),
            Some("old")
        );
        self.assert_complete_candidate_release("nib-release-stage-prod", "true");
        self.assert_complete_prior_release("old", "nib-release-backup-prod", "true");
    }

    fn clear_fault_observations(&self) {
        fs::write(self.gh_state.join("events.log"), "").expect("clear fake GH events");
        for name in [
            "advanced",
            "stage-id-lookups",
            "retagged-stage",
            "delayed-stage-assets",
            "delayed-stage-asset-state",
            "hidden-stage-list-initially",
            "hidden-stage-list-after-observation",
            "last-stage-list-empty",
            "pinned-stage-revalidated",
            "stage-delete-attempted",
        ] {
            let path = self.gh_state.join(name);
            if path.exists() {
                fs::remove_file(path).expect("clear release fault observation");
            }
        }
    }

    fn events(&self) -> Vec<String> {
        fs::read_to_string(self.gh_state.join("events.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn write_release_field(&self, release_id: &str, field: &str, value: &str) {
        fs::write(
            self.gh_state
                .join("releases")
                .join(format!("{release_id}.{field}")),
            format!("{value}\n"),
        )
        .unwrap_or_else(|error| panic!("write {release_id} release {field}: {error}"));
    }

    fn mark_old_release_as_completed_transaction(&self) {
        self.write_release_field(
            "old",
            "body",
            &format!(
                "Prior production release.\n\n<!-- nib-release-transaction-v1\nchannel=prod\ncandidate_sha={}\nprior_sha=none\nstaged_release_id=old\nprior_release_id=none\nprior_release_draft=false\n-->",
                self.old_sha
            ),
        );
    }

    fn remove_release(&self, release_id: &str) {
        for field in [
            "tag",
            "draft",
            "prerelease",
            "body",
            "target",
            "assets",
            "asset-metadata",
        ] {
            fs::remove_file(
                self.gh_state
                    .join("releases")
                    .join(format!("{release_id}.{field}")),
            )
            .unwrap_or_else(|error| panic!("remove {release_id} release {field}: {error}"));
        }
    }
}

#[cfg(unix)]
#[test]
fn staged_release_transaction_publishes_complete_assets_coherently() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults::default());

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha,
        "stdout: {}\nstderr: {}\nevents: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        harness.events()
    );
    let release_id = harness.release_id("prod-latest").expect("promoted release");
    assert_eq!(release_id, "stage");
    harness.assert_complete_candidate_release("prod-latest", "false");
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(harness.dist.join("nib-release.json"))
            .expect("generated release manifest"),
    )
    .expect("valid release manifest JSON");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["repository"], "example/nib");
    assert_eq!(manifest["channel"], "prod");
    assert_eq!(manifest["tag"], "prod-latest");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["commit"], harness.release_sha);
    assert_eq!(
        manifest["assets"].as_object().map(|assets| assets.len()),
        Some(4)
    );
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn stage_visibility_delay_accepts_only_the_bounded_lexical_range() {
    let boundary = ReleaseTransactionHarness::new();
    let boundary_output = boundary.run_with_stage_visibility_delay("10");
    assert!(
        boundary_output.status.success(),
        "{}",
        String::from_utf8_lossy(&boundary_output.stderr)
    );
    assert_eq!(
        boundary.remote_ref("refs/tags/prod-latest"),
        boundary.release_sha
    );

    for invalid in ["01", "11", "9999999999999999999"] {
        let harness = ReleaseTransactionHarness::new();
        let output = harness.run_with_stage_visibility_delay(invalid);
        assert_eq!(output.status.code(), Some(2), "invalid delay {invalid}");
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("Invalid NIB_RELEASE_STAGE_VISIBILITY_DELAY_SECONDS value."));
        harness.assert_prior_release_restored();
        assert!(harness.events().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn workflow_change_uses_forward_only_release_transaction_without_backup_ref() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let output = harness.run(ReleaseFaults::default());

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .release_field("stage", "body")
        .contains("transaction_mode=forward-only\ntransaction_phase=forward"));
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
    assert!(!harness
        .events()
        .iter()
        .any(|event| event.contains("nib-release-backup-prod")));
}

#[cfg(unix)]
#[test]
fn workflow_change_tolerates_github_rewriting_the_private_draft_tag() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let output = harness.run(ReleaseFaults {
        rewrite_stage_to_untagged: true,
        ..ReleaseFaults::default()
    });

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .events()
        .iter()
        .any(|event| event == "RETAG stage untagged-708ba2fca6bbe012874c"));
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn workflow_change_waits_for_rewritten_draft_assets_to_become_visible() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let output = harness.run(ReleaseFaults {
        rewrite_stage_to_untagged_on_create: true,
        hide_stage_list_initially: true,
        hide_stage_list_after_observation_once: true,
        delay_stage_assets_once: true,
        delay_stage_asset_state_once: true,
        ..ReleaseFaults::default()
    });

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .events()
        .iter()
        .any(|event| event == "RETAG CREATE stage untagged-708ba2fca6bbe012874c"));
    assert!(harness.gh_state.join("delayed-stage-assets").exists());
    assert!(harness.gh_state.join("delayed-stage-asset-state").exists());
    assert!(harness
        .gh_state
        .join("hidden-stage-list-initially")
        .exists());
    assert!(harness
        .gh_state
        .join("hidden-stage-list-after-observation")
        .exists());
    assert!(harness.gh_state.join("pinned-stage-revalidated").exists());
}

#[cfg(unix)]
#[test]
fn rerun_recovers_an_exact_marked_untagged_draft_without_its_stage_ref() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let interrupted = harness.run(ReleaseFaults {
        rewrite_stage_to_untagged: true,
        kill_after_untagged_rewrite: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    assert_eq!(
        harness
            .release_id("untagged-708ba2fca6bbe012874c")
            .as_deref(),
        Some("stage")
    );
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-stage-prod"),
        harness.release_sha
    );
    harness.delete_remote_ref("refs/tags/nib-release-stage-prod");

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults::default());
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    let events = harness.events();
    let cleanup = events
        .iter()
        .position(|event| event == "DELETE stage")
        .expect("rerun must delete the exact marked untagged draft");
    let recreate = events
        .iter()
        .position(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must create a fresh transaction after cleanup");
    assert!(cleanup < recreate, "{events:?}");
}

#[cfg(unix)]
#[test]
fn multiple_untagged_channel_drafts_fail_closed_without_mutation() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let interrupted = harness.run(ReleaseFaults {
        rewrite_stage_to_untagged: true,
        kill_after_untagged_rewrite: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    harness.delete_remote_ref("refs/tags/nib-release-stage-prod");

    for field in [
        "draft",
        "prerelease",
        "body",
        "target",
        "assets",
        "asset-metadata",
    ] {
        fs::copy(
            harness
                .gh_state
                .join("releases")
                .join(format!("stage.{field}")),
            harness
                .gh_state
                .join("releases")
                .join(format!("duplicate.{field}")),
        )
        .unwrap_or_else(|error| panic!("copy duplicate release {field}: {error}"));
    }
    harness.write_release_field("duplicate", "tag", "untagged-ffffffffffffffffffff");

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults::default());
    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr)
        .contains("Multiple untagged draft transactions exist for prod."));
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert!(harness.events().is_empty());
    assert_eq!(
        harness
            .release_id("untagged-708ba2fca6bbe012874c")
            .as_deref(),
        Some("stage")
    );
    assert_eq!(
        harness
            .release_id("untagged-ffffffffffffffffffff")
            .as_deref(),
        Some("duplicate")
    );
}

#[cfg(unix)]
#[test]
fn workflow_change_rerun_recovers_forward_after_prior_release_deletion() {
    let harness = ReleaseTransactionHarness::with_workflow_change(true);
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_delete: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert!(harness.release_id("prod-latest").is_none());
    assert_eq!(
        harness.release_id("nib-release-stage-prod").as_deref(),
        Some("stage")
    );

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults::default());
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .events()
        .iter()
        .any(|event| event.starts_with("REF PATCH refs/tags/prod-latest ")));
}

#[cfg(unix)]
#[test]
fn production_release_rejects_a_manual_run_from_a_non_main_branch() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run_from_ref("development", ReleaseFaults::default());

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Invalid release channel, tag, prerelease, or source branch combination."));
    harness.assert_prior_release_restored();
}

#[cfg(unix)]
#[test]
fn failed_draft_asset_upload_preserves_the_prior_release() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        partial_create: true,
        ..ReleaseFaults::default()
    });

    assert!(!output.status.success());
    harness.assert_prior_release_restored();
}

#[cfg(unix)]
#[test]
fn staged_release_with_an_empty_asset_is_rejected_and_rolled_back() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        empty_stage_asset: true,
        ..ReleaseFaults::default()
    });

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Release stage contains incomplete or empty assets: nib-linux-x86_64.tar.gz"));
    harness.assert_prior_release_restored();
}

#[cfg(unix)]
#[test]
fn source_advance_after_atomic_tag_move_repairs_forward_coherently() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        advance_after_atomic: true,
        ..ReleaseFaults::default()
    });

    assert!(!output.status.success());
    assert_eq!(harness.remote_ref("refs/heads/main"), harness.advance_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn ambiguous_release_promotion_failure_is_reconciled_by_release_id() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        ambiguous_promote: true,
        ..ReleaseFaults::default()
    });

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
}

#[cfg(unix)]
#[test]
fn source_advance_after_promotion_keeps_the_public_candidate_coherent() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        advance_after_promote: true,
        ..ReleaseFaults::default()
    });

    assert!(!output.status.success());
    assert_eq!(harness.remote_ref("refs/heads/main"), harness.advance_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn failed_old_release_restoration_repairs_forward_with_the_complete_stage() {
    let harness = ReleaseTransactionHarness::new();
    let output = harness.run(ReleaseFaults {
        fail_first_atomic: true,
        fail_old_restore: true,
        ..ReleaseFaults::default()
    });

    assert!(!output.status.success());
    assert_eq!(harness.remote_ref("refs/heads/main"), harness.release_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
    assert!(harness.release_id("nib-release-backup-prod").is_none());
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Prior release restoration failed; attempting coherent forward repair."));
}

#[cfg(unix)]
#[test]
fn rerun_recovers_a_process_killed_after_the_old_release_backup_before_new_work() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    harness.assert_backed_up_transaction_retained();

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );

    let events = harness.events();
    let restore = events
        .iter()
        .position(|event| event == "PATCH old tag_name=prod-latest draft=false")
        .expect("rerun must restore the prior release during recovery");
    let cleanup = events
        .iter()
        .position(|event| event == "DELETE stage")
        .expect("rerun must clean the interrupted staged release");
    let new_create = events
        .iter()
        .position(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must begin a new transaction after recovery");
    assert!(restore < cleanup && cleanup < new_create, "{events:?}");

    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn rerun_recovers_an_untagged_rollback_after_prior_backup_without_the_stage_ref() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        rewrite_stage_to_untagged: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    assert_eq!(
        harness
            .release_id("untagged-708ba2fca6bbe012874c")
            .as_deref(),
        Some("stage")
    );
    assert_eq!(
        harness.release_id("nib-release-backup-prod").as_deref(),
        Some("old")
    );
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    harness.delete_remote_ref("refs/tags/nib-release-stage-prod");

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn rerun_finishes_an_untagged_rollback_after_public_ref_move_without_the_stage_ref() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        rewrite_stage_to_untagged: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    harness.delete_remote_ref("refs/tags/nib-release-stage-prod");
    harness.write_remote_ref("refs/tags/prod-latest", &harness.release_sha);

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn rerun_finalizes_and_removes_a_pending_untagged_legacy_orphan() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        rewrite_stage_to_untagged: true,
        kill_after_untagged_rewrite: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    let pending_body = harness
        .release_field("stage", "body")
        .replace("staged_release_id=stage", "staged_release_id=pending");
    harness.write_release_field("stage", "body", &pending_body);
    harness.delete_remote_ref("refs/tags/nib-release-stage-prod");
    harness.delete_remote_ref("refs/tags/nib-release-backup-prod");
    harness.write_remote_ref("refs/heads/main", &harness.advance_sha);

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults::default());

    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("Refusing stale prod publication"));
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("old"));
    harness.assert_complete_prior_release("old", "prod-latest", "false");
    assert!(harness
        .release_id("untagged-708ba2fca6bbe012874c")
        .is_none());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
    let events = harness.events();
    let finalize = events
        .iter()
        .position(|event| event == "PATCH stage")
        .expect("rerun must finalize the pending immutable release ID");
    let cleanup = events
        .iter()
        .position(|event| event == "DELETE stage")
        .expect("rerun must delete the exact pending orphan");
    assert!(finalize < cleanup, "{events:?}");
    assert!(!events.iter().any(|event| event.starts_with("CREATE stage")));
}

#[cfg(unix)]
#[test]
fn rerun_exact_lease_cleans_a_backup_only_crash_before_publishing() {
    let harness = ReleaseTransactionHarness::new();
    harness.remove_release("old");

    let interrupted = harness.run(ReleaseFaults {
        kill_after_backup_ref: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness.release_id("prod-latest").is_none());
    assert!(harness.release_id("nib-release-backup-prod").is_none());
    assert!(harness.release_id("nib-release-stage-prod").is_none());
    assert!(!harness
        .events()
        .iter()
        .any(|event| event.starts_with("CREATE ")));

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );

    let events = harness.events();
    let exact_cleanup = format!(
        "GIT push --force-with-lease=refs/tags/nib-release-backup-prod:{} origin :refs/tags/nib-release-backup-prod",
        harness.old_sha
    );
    let cleanup = events
        .iter()
        .position(|event| event == &exact_cleanup)
        .expect("rerun must exact-lease delete the orphan backup ref");
    let new_create = events
        .iter()
        .position(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must create a staged release after backup cleanup");
    assert!(cleanup < new_create, "{events:?}");

    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release_with_prior("prod-latest", "false", "none");
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn rerun_exact_lease_cleans_a_stage_only_crash_before_publishing() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_stage_ref: true,
        ..ReleaseFaults::default()
    });

    assert!(!interrupted.status.success());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-stage-prod"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("old"));
    assert!(harness.release_id("nib-release-stage-prod").is_none());

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );

    let events = harness.events();
    let cleanup = events
        .iter()
        .position(|event| event == "REF DELETE refs/tags/nib-release-stage-prod")
        .expect("rerun must delete the exact unreleased staging ref");
    let new_create = events
        .iter()
        .rposition(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must create a staged release after recovery");
    assert!(cleanup < new_create, "{events:?}");

    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn stage_only_recovery_ignores_an_older_stable_transaction_marker_in_both_modes() {
    for workflow_change in [false, true] {
        let harness = ReleaseTransactionHarness::with_workflow_change(workflow_change);
        harness.mark_old_release_as_completed_transaction();
        let interrupted = harness.run(ReleaseFaults {
            kill_after_stage_ref: true,
            ..ReleaseFaults::default()
        });

        assert!(!interrupted.status.success());
        assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-stage-prod"),
            harness.release_sha
        );
        assert!(harness.release_id("nib-release-stage-prod").is_none());

        harness.clear_fault_observations();
        let rerun = harness.run(ReleaseFaults::default());
        assert!(
            rerun.status.success(),
            "{}",
            String::from_utf8_lossy(&rerun.stderr)
        );
        assert!(harness
            .events()
            .iter()
            .any(|event| event == "REF DELETE refs/tags/nib-release-stage-prod"));
        assert_eq!(
            harness.remote_ref("refs/tags/prod-latest"),
            harness.release_sha
        );
        assert_eq!(harness.release_id("prod-latest").as_deref(), Some("stage"));
        harness.assert_complete_candidate_release("prod-latest", "false");
    }
}

#[cfg(unix)]
#[test]
fn backup_only_recovery_precedes_an_older_stable_transaction_marker() {
    let harness = ReleaseTransactionHarness::new();
    harness.mark_old_release_as_completed_transaction();

    let interrupted = harness.run(ReleaseFaults {
        kill_after_backup_ref: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("old"));
    assert!(harness.release_id("nib-release-stage-prod").is_none());
    assert!(harness.release_id("nib-release-backup-prod").is_none());

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );

    let events = harness.events();
    let exact_cleanup = format!(
        "GIT push --force-with-lease=refs/tags/nib-release-backup-prod:{} origin :refs/tags/nib-release-backup-prod",
        harness.old_sha
    );
    let cleanup = events
        .iter()
        .position(|event| event == &exact_cleanup)
        .expect("rerun must exact-lease delete the rollback-terminal backup ref");
    let new_create = events
        .iter()
        .position(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must publish only after classifying the backup-only state");
    assert!(cleanup < new_create, "{events:?}");

    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn rollback_process_loss_retains_the_current_marker_until_backup_cleanup() {
    let harness = ReleaseTransactionHarness::new();
    harness.mark_old_release_as_completed_transaction();

    let interrupted = harness.run(ReleaseFaults {
        fail_first_atomic: true,
        kill_before_backup_ref_delete: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("old"));
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    assert!(harness.release_id("nib-release-backup-prod").is_none());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert_eq!(
        harness.release_id("nib-release-stage-prod").as_deref(),
        Some("stage")
    );
    harness.assert_complete_candidate_release("nib-release-stage-prod", "true");

    harness.clear_fault_observations();
    let rerun = harness.run(ReleaseFaults::default());
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );

    let events = harness.events();
    let exact_cleanup = format!(
        "GIT push --force-with-lease=refs/tags/nib-release-backup-prod:{} origin :refs/tags/nib-release-backup-prod",
        harness.old_sha
    );
    let backup_cleanup = events
        .iter()
        .position(|event| event == &exact_cleanup)
        .expect("rerun must exact-lease delete the recorded backup ref");
    let marker_cleanup = events
        .iter()
        .position(|event| event == "DELETE stage")
        .expect("rerun must delete the staged marker after backup cleanup");
    let new_create = events
        .iter()
        .position(|event| event == "CREATE stage nib-release-stage-prod")
        .expect("rerun must start new publication after rollback cleanup");
    assert!(backup_cleanup < marker_cleanup, "{events:?}");
    assert!(marker_cleanup < new_create, "{events:?}");

    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    harness.assert_complete_candidate_release("prod-latest", "false");
    assert!(!harness.gh_state.join("releases/old.tag").exists());
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
}

#[cfg(unix)]
#[test]
fn failed_rollback_and_failed_forward_repair_retain_all_durable_state() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    harness.assert_backed_up_transaction_retained();

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults {
        move_rolling_on_second_stage_lookup: true,
        fail_rolling_rollback: true,
        fail_promote_before_mutation: true,
        ..ReleaseFaults::default()
    });

    assert!(!recovery.status.success());
    let stderr = String::from_utf8_lossy(&recovery.stderr);
    assert!(stderr.contains("Rolling-tag rollback failed; attempting coherent forward repair."));
    assert!(harness
        .events()
        .iter()
        .any(|event| { event == "PATCH stage tag_name=prod-latest draft=false prerelease=false" }));
    assert_eq!(
        harness.remote_ref("refs/tags/prod-latest"),
        harness.release_sha
    );
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-stage-prod"),
        harness.release_sha
    );
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    harness.assert_complete_candidate_release("nib-release-stage-prod", "true");
    harness.assert_complete_prior_release("old", "nib-release-backup-prod", "true");
    assert_eq!(
        harness.release_id("nib-release-stage-prod").as_deref(),
        Some("stage")
    );
    assert_eq!(
        harness.release_id("nib-release-backup-prod").as_deref(),
        Some("old")
    );
}

#[cfg(unix)]
#[test]
fn externally_retagged_prior_release_is_never_mutated_or_deleted() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());
    harness.assert_backed_up_transaction_retained();

    harness.write_release_field("old", "tag", "external-release-tag");
    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults::default());

    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr)
        .contains("Recorded prior release changed to unowned tag external-release-tag."));
    assert!(harness.events().is_empty(), "{:?}", harness.events());
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-stage-prod"),
        harness.release_sha
    );
    assert_eq!(
        harness.remote_ref("refs/tags/nib-release-backup-prod"),
        harness.old_sha
    );
    harness.assert_complete_candidate_release("nib-release-stage-prod", "true");
    harness.assert_complete_prior_release("old", "external-release-tag", "true");
}

#[cfg(unix)]
#[test]
fn invalid_candidate_markers_cause_zero_cleanup_mutations() {
    for marker_case in ["malformed", "mismatched"] {
        let harness = ReleaseTransactionHarness::new();
        let interrupted = harness.run(ReleaseFaults {
            kill_after_old_backup: true,
            ..ReleaseFaults::default()
        });
        assert!(!interrupted.status.success(), "{marker_case}");
        harness.assert_backed_up_transaction_retained();

        let marker = match marker_case {
            "malformed" => "not a transaction marker".to_string(),
            "mismatched" => harness.release_field("stage", "body").replace(
                &format!("candidate_sha={}", harness.release_sha),
                &format!("candidate_sha={}", harness.advance_sha),
            ),
            _ => unreachable!(),
        };
        harness.write_release_field("stage", "body", &marker);
        harness.clear_fault_observations();
        let recovery = harness.run(ReleaseFaults::default());

        assert!(!recovery.status.success(), "{marker_case}");
        let stderr = String::from_utf8_lossy(&recovery.stderr);
        match marker_case {
            "malformed" => assert!(
                stderr.contains("Release stage has a missing or ambiguous transaction marker.")
            ),
            "mismatched" => assert!(stderr.contains(
                "Release stage transaction marker does not match this channel and candidate."
            )),
            _ => unreachable!(),
        }
        assert!(
            harness.events().is_empty(),
            "{marker_case}: {:?}",
            harness.events()
        );
        assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-stage-prod"),
            harness.release_sha
        );
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-backup-prod"),
            harness.old_sha
        );
        harness.assert_complete_release("stage", "nib-release-stage-prod", "true");
        harness.assert_complete_prior_release("old", "nib-release-backup-prod", "true");
    }
}

#[cfg(unix)]
#[test]
fn recovery_read_errors_fail_closed_without_release_or_ref_mutations() {
    let fault_cases = [
        (
            "release-list",
            ReleaseFaults {
                fail_release_list_read: true,
                ..ReleaseFaults::default()
            },
        ),
        (
            "release-get",
            ReleaseFaults {
                fail_release_get_read: true,
                ..ReleaseFaults::default()
            },
        ),
        (
            "git-ls-remote",
            ReleaseFaults {
                fail_ls_remote_read: true,
                ..ReleaseFaults::default()
            },
        ),
    ];

    for (fault_name, faults) in fault_cases {
        let harness = ReleaseTransactionHarness::new();
        let interrupted = harness.run(ReleaseFaults {
            kill_after_old_backup: true,
            ..ReleaseFaults::default()
        });
        assert!(!interrupted.status.success(), "{fault_name}");
        harness.assert_backed_up_transaction_retained();

        harness.clear_fault_observations();
        let recovery = harness.run(faults);

        assert!(!recovery.status.success(), "{fault_name}");
        assert!(
            harness.events().is_empty(),
            "{fault_name}: {:?}",
            harness.events()
        );
        harness.assert_backed_up_transaction_retained();
    }
}

#[cfg(unix)]
#[test]
fn each_staged_draft_discovery_read_must_succeed_before_recovery_mutates() {
    let fault_cases = [
        (
            "tagged-list",
            true,
            "untagged-708ba2fca6bbe012874c",
            ReleaseFaults {
                fail_tagged_release_list_read: true,
                ..ReleaseFaults::default()
            },
        ),
        (
            "untagged-list",
            false,
            "nib-release-stage-prod",
            ReleaseFaults {
                fail_untagged_release_list_read: true,
                ..ReleaseFaults::default()
            },
        ),
    ];

    for (fault_name, rewrite_stage, candidate_tag, faults) in fault_cases {
        let harness = ReleaseTransactionHarness::new();
        let interrupted = harness.run(ReleaseFaults {
            kill_after_old_backup: true,
            rewrite_stage_to_untagged: rewrite_stage,
            ..ReleaseFaults::default()
        });
        assert!(!interrupted.status.success(), "{fault_name}");
        assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-stage-prod"),
            harness.release_sha
        );
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-backup-prod"),
            harness.old_sha
        );
        assert_eq!(harness.release_id(candidate_tag).as_deref(), Some("stage"));
        assert_eq!(
            harness.release_id("nib-release-backup-prod").as_deref(),
            Some("old")
        );

        harness.clear_fault_observations();
        let recovery = harness.run(faults);

        assert!(!recovery.status.success(), "{fault_name}");
        assert!(
            harness.events().is_empty(),
            "{fault_name}: {:?}",
            harness.events()
        );
        assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-stage-prod"),
            harness.release_sha
        );
        assert_eq!(
            harness.remote_ref("refs/tags/nib-release-backup-prod"),
            harness.old_sha
        );
        assert_eq!(harness.release_id(candidate_tag).as_deref(), Some("stage"));
        assert_eq!(
            harness.release_id("nib-release-backup-prod").as_deref(),
            Some("old")
        );
    }
}

#[cfg(unix)]
#[test]
fn ambiguous_staged_delete_with_failed_id_read_never_starts_a_new_transaction() {
    let harness = ReleaseTransactionHarness::new();
    let interrupted = harness.run(ReleaseFaults {
        kill_after_old_backup: true,
        rewrite_stage_to_untagged: true,
        ..ReleaseFaults::default()
    });
    assert!(!interrupted.status.success());

    harness.clear_fault_observations();
    let recovery = harness.run(ReleaseFaults {
        fail_stage_delete_before_mutation: true,
        fail_release_id_list_after_delete: true,
        ..ReleaseFaults::default()
    });

    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("Failed to find release ID stage."));
    assert_eq!(harness.remote_ref("refs/tags/prod-latest"), harness.old_sha);
    assert_eq!(harness.release_id("prod-latest").as_deref(), Some("old"));
    harness.assert_complete_prior_release("old", "prod-latest", "false");
    assert_eq!(
        harness
            .release_id("untagged-708ba2fca6bbe012874c")
            .as_deref(),
        Some("stage")
    );
    assert!(harness
        .remote_ref("refs/tags/nib-release-stage-prod")
        .is_empty());
    assert!(harness
        .remote_ref("refs/tags/nib-release-backup-prod")
        .is_empty());
    let events = harness.events();
    assert!(events.iter().any(|event| event == "DELETE stage"));
    assert!(!events.iter().any(|event| event.starts_with("CREATE stage")));
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, source: &str) {
    fs::write(path, source).expect("write fake installer command");
    let mut permissions = fs::metadata(path)
        .expect("read fake command metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fake command executable");
}

#[cfg(unix)]
struct UnixInstallerHarness {
    _temp: tempfile::TempDir,
    fake_bin: PathBuf,
    install_dir: PathBuf,
    log: PathBuf,
}

#[cfg(unix)]
impl UnixInstallerHarness {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create installer harness");
        let fake_bin = temp.path().join("bin");
        let install_dir = temp.path().join("install");
        let log = temp.path().join("events.log");
        fs::create_dir_all(&fake_bin).expect("create fake command directory");

        write_executable(
            &fake_bin.join("uname"),
            r#"#!/bin/sh
case "$1" in
  -s) printf '%s\n' "${FAKE_OS:-Linux}" ;;
  -m) printf '%s\n' "${FAKE_ARCH:-x86_64}" ;;
  *) exit 2 ;;
esac
"#,
        );
        write_executable(
            &fake_bin.join("curl"),
            r#"#!/bin/sh
url=
out=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      shift
      out="$1"
      ;;
    https://*)
      url="$1"
      ;;
  esac
  shift
done

printf 'curl %s\n' "$url" >> "$INSTALLER_LOG"
case "$url" in
  *.sha256)
    printf '%s  archive\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' > "$out"
    ;;
  *)
    printf 'fake release archive\n' > "$out"
    ;;
esac
"#,
        );
        write_executable(
            &fake_bin.join("sha256sum"),
            r#"#!/bin/sh
printf 'sha256sum\n' >> "$INSTALLER_LOG"
case "${CHECKSUM_MODE:-match}" in
  match) hash='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
  mismatch) hash='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' ;;
  *) exit 2 ;;
esac
printf '%s  %s\n' "$hash" "$1"
"#,
        );
        write_executable(
            &fake_bin.join("tar"),
            r#"#!/bin/sh
printf 'tar\n' >> "$INSTALLER_LOG"
destination=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-C" ]; then
    shift
    destination="$1"
  fi
  shift
done
[ -n "$destination" ]
: > "$destination/nib"
"#,
        );
        write_executable(
            &fake_bin.join("install"),
            r#"#!/bin/sh
printf 'install\n' >> "$INSTALLER_LOG"
while [ "$#" -gt 1 ]; do
  shift
done
: > "$1"
"#,
        );

        Self {
            _temp: temp,
            fake_bin,
            install_dir,
            log,
        }
    }

    fn run(&self, checksum_mode: &str) -> Output {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            inherited_path.to_string_lossy()
        );

        Command::new("sh")
            .arg(repository_root().join("scripts/install.sh"))
            .env("PATH", path)
            .env("NIB_INSTALL_DIR", &self.install_dir)
            .env("INSTALLER_LOG", &self.log)
            .env("CHECKSUM_MODE", checksum_mode)
            .env_remove("NIB_REPO")
            .env_remove("NIB_CHANNEL")
            .output()
            .expect("run Unix installer")
    }

    fn events(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(unix)]
#[test]
fn unix_installer_uses_defaults_and_verifies_before_installing() {
    let harness = UnixInstallerHarness::new();
    let output = harness.run("match");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "installer failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let asset_url =
        "https://github.com/skills-yaml/nib/releases/download/prod-latest/nib-linux-x86_64.tar.gz";
    assert!(stdout.contains(&format!("Downloading {asset_url}")));
    assert!(stdout.contains("Verified SHA-256 for nib-linux-x86_64.tar.gz"));
    assert!(harness.install_dir.join("nib").is_file());

    assert_eq!(
        harness.events(),
        vec![
            format!("curl {asset_url}"),
            format!("curl {asset_url}.sha256"),
            "sha256sum".to_string(),
            "tar".to_string(),
            "install".to_string(),
        ]
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_fails_closed_on_checksum_mismatch() {
    let harness = UnixInstallerHarness::new();
    let output = harness.run("mismatch");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "checksum mismatch was accepted");
    assert!(
        stderr.contains("Checksum verification failed for nib-linux-x86_64.tar.gz"),
        "unexpected stderr: {stderr}"
    );
    assert_eq!(
        harness.events(),
        vec![
            "curl https://github.com/skills-yaml/nib/releases/download/prod-latest/nib-linux-x86_64.tar.gz".to_string(),
            "curl https://github.com/skills-yaml/nib/releases/download/prod-latest/nib-linux-x86_64.tar.gz.sha256".to_string(),
            "sha256sum".to_string(),
        ]
    );
    assert!(!harness.install_dir.join("nib").exists());
}

#[test]
fn powershell_installer_defaults_and_verifies_before_expanding() {
    let script = fs::read_to_string(repository_root().join("scripts/install.ps1"))
        .expect("read PowerShell installer");

    assert!(script.contains("[string] $Channel = \"prod\""));
    assert!(script.contains("[string] $Repo = \"skills-yaml/nib\""));
    assert!(script.contains("$Asset = \"nib-windows-x86_64.zip\""));

    let checksum_download = script
        .find("Invoke-WebRequest -Uri \"$Url.sha256\" -OutFile $Checksum")
        .expect("checksum must be downloaded");
    let checksum_shape_validation = script
        .find("$ExpectedHash -notmatch \"^[0-9a-f]{64}$\"")
        .expect("downloaded checksum must be validated");
    let archive_hash = script
        .find("Get-FileHash -Path $Archive -Algorithm SHA256")
        .expect("archive hash must be computed");
    let mismatch_check = script
        .find("$ActualHash -ne $ExpectedHash")
        .expect("hash mismatch must be checked");
    let mismatch_failure = script
        .find("throw \"Checksum verification failed for $Asset\"")
        .expect("hash mismatch must fail closed");
    let expand_archive = script
        .find("Expand-Archive -Path $Archive")
        .expect("verified archive must be expanded");

    assert!(checksum_download < checksum_shape_validation);
    assert!(checksum_shape_validation < archive_hash);
    assert!(archive_hash < mismatch_check);
    assert!(mismatch_check < mismatch_failure);
    assert!(mismatch_failure < expand_archive);
}

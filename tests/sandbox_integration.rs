use nib::config::BoundaryConfig;
use nib::sandbox::{detect_capabilities, run_sandboxed};
use tempfile::tempdir;

#[tokio::test]
async fn test_sandbox_write_restrictions() {
    let caps = detect_capabilities();
    let project_dir = tempdir().expect("Failed to create temp project dir");
    let external_dir = tempdir().expect("Failed to create temp external dir");

    let cwd = project_dir.path();
    let external_path = external_dir.path();

    let mut boundaries = BoundaryConfig::default();
    boundaries
        .allow_write
        .push(external_path.to_string_lossy().to_string());

    // 1. Should be able to write to cwd
    let res1 = run_sandboxed("touch test_cwd.txt", cwd, "restricted", &boundaries)
        .await
        .expect("Failed to run sandbox");
    assert!(res1.0.status.success());
    assert!(cwd.join("test_cwd.txt").exists());

    if !caps.bwrap_available {
        assert!(res1.1.is_none(), "unusable bwrap must use direct fallback");
        return;
    }
    assert!(res1.1.is_some(), "executable bwrap must be selected");

    // 2. Should be able to write to external_path (explicitly allowed)
    let res2 = run_sandboxed(
        &format!("touch {}/test_ext.txt", external_path.display()),
        cwd,
        "restricted",
        &boundaries,
    )
    .await
    .expect("Failed to run sandbox");
    assert!(res2.0.status.success());
    assert!(external_path.join("test_ext.txt").exists());

    // 3. Should fail to write to arbitrary unauthorized external path (e.g., /tmp directly if we use a different tmp folder)
    let unauth_dir = tempdir().expect("Failed to create unauth dir");
    let res3 = run_sandboxed(
        &format!("touch {}/test_unauth.txt", unauth_dir.path().display()),
        cwd,
        "restricted",
        &boundaries,
    )
    .await
    .expect("Failed to run sandbox");

    // Should fail
    assert!(!res3.0.status.success());
    assert!(!unauth_dir.path().join("test_unauth.txt").exists());
}

#[tokio::test]
async fn sandbox_rejects_an_unavailable_working_directory() {
    let directory = tempdir().expect("tempdir");
    let missing = directory.path().join("missing");
    let error = run_sandboxed("true", &missing, "restricted", &BoundaryConfig::default())
        .await
        .expect_err("missing cwd must fail closed");
    assert!(error.contains("invalid sandbox working directory"));
}

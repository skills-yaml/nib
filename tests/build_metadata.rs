use std::path::Path;
use std::process::Command;

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

fn git(project: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(project)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn identity_watches_existing_metadata_in_ordinary_linked_packed_and_detached_checkouts() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "-b", "main"]);
    git(&project, &["config", "user.name", "Build fixture"]);
    git(&project, &["config", "user.email", "build@example.invalid"]);
    git(&project, &["commit", "--allow-empty", "-m", "initial"]);
    let linked = directory.path().join("linked");
    git(
        &project,
        &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );

    for checkout in [&project, &linked] {
        let paths = build_script::git_metadata_paths(checkout);
        assert!(!paths.is_empty());
        assert!(paths.iter().all(|path| path.exists()));
        assert!(paths.iter().any(|path| path.file_name().unwrap() == "HEAD"));
        assert!(paths
            .iter()
            .any(|path| path.ends_with(if checkout == &project {
                "refs/heads/main"
            } else {
                "refs/heads/feature"
            })));
    }
    assert!(linked.join(".git").is_file());
    git(&project, &["pack-refs", "--all", "--prune"]);
    let paths = build_script::git_metadata_paths(&linked);
    assert!(paths.iter().all(|path| path.exists()));
    assert!(paths.iter().any(|path| path.ends_with("packed-refs")));
    assert!(paths
        .iter()
        .any(|path| path.ends_with("refs/heads") && path.is_dir()));
    git(&linked, &["commit", "--allow-empty", "-m", "new loose ref"]);
    assert!(build_script::git_metadata_paths(&linked)
        .iter()
        .any(|path| path.ends_with("refs/heads/feature") && path.is_file()));

    git(&linked, &["checkout", "--detach"]);
    let paths = build_script::git_metadata_paths(&linked);
    assert!(paths.iter().all(|path| path.exists()));
    assert!(paths.iter().any(|path| path.file_name().unwrap() == "HEAD"));
    assert!(!paths
        .iter()
        .any(|path| path.ends_with("refs/heads/feature")));
}

#[test]
fn source_archive_does_not_watch_nonexistent_git_paths() {
    let directory = tempfile::tempdir().unwrap();
    assert!(build_script::git_metadata_paths(directory.path()).is_empty());
}

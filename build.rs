use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let build_commit = std::env::var("NIB_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(git_head)
        .unwrap_or_else(|| "unknown".to_string());
    let build_channel = std::env::var("NIB_BUILD_CHANNEL").unwrap_or_else(|_| "local".to_string());

    println!("cargo:rustc-env=NIB_BUILD_COMMIT={build_commit}");
    println!("cargo:rustc-env=NIB_BUILD_CHANNEL={build_channel}");
    println!("cargo:rerun-if-env-changed=NIB_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=NIB_BUILD_CHANNEL");
    println!("cargo:rerun-if-changed=Cargo.toml");
    for path in git_metadata_paths(Path::new(".")) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_head() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();

    if commit.is_empty() {
        None
    } else {
        Some(commit.to_string())
    }
}

fn git_output(project: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(project)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(crate) fn git_metadata_paths(project: &Path) -> Vec<PathBuf> {
    let git_path = |name: &str| {
        git_output(project, &["rev-parse", "--git-path", name]).map(|value| {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                project.join(path)
            }
        })
    };
    let mut paths = Vec::new();
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git_path(name).filter(|path| path.is_file()) {
            paths.push(path);
        }
    }
    if let Some(reference) = git_output(project, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(mut path) = git_path(&reference) {
            // A packed branch has no loose ref yet. Watch its existing parent so
            // creating the loose ref on the next commit still invalidates identity.
            while !path.exists() && path.pop() {}
            if path.exists() {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

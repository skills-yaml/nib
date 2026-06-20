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
    println!("cargo:rerun-if-changed=.git/HEAD");

    if let Some(head_ref) = git_head_ref() {
        println!("cargo:rerun-if-changed=.git/{head_ref}");
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

fn git_head_ref() -> Option<String> {
    let head = std::fs::read_to_string(".git/HEAD").ok()?;
    head.strip_prefix("ref: ")
        .map(|value| value.trim().to_string())
}

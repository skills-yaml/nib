//! Branch first before a subagent merge (T080 phase 5c, user decision D6).
//!
//! A merge commits on the parent's current branch. Like `git_commit`, it
//! never commits directly to the default branch, a protected branch or a
//! detached HEAD: nib first creates and switches to `nib/<subagent>`. The
//! parent is clean at this point, so switching changes no files. A retried
//! pending merge branches first again, because a failed merge returns to the
//! original branch.
//!
//! Detection repeats `git_tools`' branch queries on purpose: subagent merges
//! use nib's hardened Git (no hooks or ambient configuration), while
//! `git_commit` runs the user's own Git.

use super::*;

/// A branch created for a merge, and the branch (or commit) to return to.
pub(crate) struct MergeBranch {
    pub(crate) created: String,
    original: Option<String>,
    original_commit: String,
}

async fn optional_stdout(cwd: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let output = git_output(cwd, args).await?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((output.status.success() && !value.is_empty()).then_some(value))
}

/// Creates and switches to `nib/<subagent>` when the parent is on a default
/// or protected branch or a detached HEAD. Returns `None` otherwise.
pub(crate) async fn branch_before_merge(
    project_root: &Path,
    subagent_id: &str,
) -> Result<Option<MergeBranch>, String> {
    let current = optional_stdout(
        project_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await?;
    let default = optional_stdout(
        project_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?
    .and_then(|name| name.strip_prefix("origin/").map(str::to_string));
    let protected = current.as_deref().is_none_or(|branch| {
        crate::tools::git_tools::PROTECTED_BRANCHES.contains(&branch)
            || default.as_deref() == Some(branch)
    });
    if !protected {
        return Ok(None);
    }
    let original_commit = git_stdout(project_root, ["rev-parse", "HEAD"]).await?;
    let base = format!(
        "nib/{}",
        crate::tools::git_tools::branch_slug(&format!("subagent {subagent_id}"))
    );
    let mut name = base.clone();
    let mut suffix = 2;
    loop {
        let exists = git_output(
            project_root,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .await?
        .status
        .success();
        if !exists {
            break;
        }
        if suffix > 50 {
            return Err("could not choose a free branch name for the merge".to_string());
        }
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    let switched = git_output(project_root, ["switch", "--quiet", "-c", &name]).await?;
    require_git_success(&switched, "create the merge branch")?;
    Ok(Some(MergeBranch {
        created: name,
        original: current,
        original_commit,
    }))
}

/// Runs `merge` on a branch nib may commit to: unless the subagent commit is
/// already integrated, it branches first when needed, reports the created
/// branch, and undoes it when the merge fails.
pub(crate) async fn merge_on_safe_branch(
    project_root: &Path,
    subagent_id: &str,
    branch_commit: &str,
    merge: impl Future<Output = Result<Value, String>>,
) -> Result<Value, String> {
    let branch = if git_is_ancestor(project_root, branch_commit, "HEAD").await? {
        None
    } else {
        branch_before_merge(project_root, subagent_id).await?
    };
    match (merge.await, branch) {
        (Ok(mut merged), Some(branch)) => {
            merged["created_branch"] = json!(branch.created);
            Ok(merged)
        }
        (Err(error), Some(branch)) => {
            undo_merge_branch(project_root, &branch).await;
            Err(error)
        }
        (merged, None) => merged,
    }
}

/// After a failed merge that left HEAD where it was, returns to the
/// original branch and removes the created one. Best effort.
pub(crate) async fn undo_merge_branch(project_root: &Path, branch: &MergeBranch) {
    let unchanged = git_stdout(project_root, ["rev-parse", "HEAD"])
        .await
        .is_ok_and(|head| head == branch.original_commit);
    if !unchanged {
        return;
    }
    let switched = match &branch.original {
        Some(original) => git_output(project_root, ["switch", "--quiet", original]).await,
        None => {
            git_output(
                project_root,
                ["switch", "--quiet", "--detach", &branch.original_commit],
            )
            .await
        }
    };
    if switched.is_ok_and(|output| output.status.success()) {
        let _ = git_output(project_root, ["branch", "--quiet", "-D", &branch.created]).await;
    }
}

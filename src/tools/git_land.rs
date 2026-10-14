//! Git for `/land` (T080 phase 5d): preview a session worktree against the
//! user's folder, and merge its branch into the folder's branch.
//!
//! Landing is the user's own action, so like `git_commit` it runs the
//! user's Git on the host: their hooks, signing and configuration apply. A
//! merge never commits directly to the default or a protected branch, or to
//! a detached HEAD: nib branches first (user decision D6).

use super::*;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Commits listed in a preview before the rest are counted.
const MAX_LISTED_COMMITS: usize = 20;
/// Output kept for listings `/land` checks or hashes. A listing that
/// reaches it is refused rather than checked in part.
const MAX_LISTING_BYTES: usize = 16 * 1024 * 1024;
const TRUNCATED: &str = "[output truncated]";

/// What `/land` shows, and the code that confirms it.
#[derive(Debug, Clone)]
pub(crate) struct LandPreview {
    pub(crate) worktree: PathBuf,
    pub(crate) branch: String,
    pub(crate) tip: String,
    /// The folder's branch, or `None` for a detached HEAD.
    pub(crate) folder_branch: Option<String>,
    pub(crate) folder_head: String,
    /// Whether the folder's branch is the default or a protected branch
    /// (or a detached HEAD), so a merge branches first.
    pub(crate) folder_protected: bool,
    /// Session commits the folder does not have, newest first, as
    /// `<short id> <subject>`.
    pub(crate) commits: Vec<String>,
    pub(crate) commit_count: usize,
    /// `git status --short` lines for the worktree, without nib state.
    pub(crate) uncommitted: Vec<String>,
    /// Ignored files and directories in the worktree, which are removed
    /// with it.
    pub(crate) ignored: Vec<String>,
    /// `git diff --stat` of the session commits against the folder.
    pub(crate) stat: String,
    pub(crate) code: String,
}

/// The result of a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    /// The folder already contained the session branch.
    AlreadyMerged,
    Merged {
        commit: String,
        branch: Option<String>,
        created_branch: Option<String>,
    },
}

/// The whole output of a listing, or an error when it is too large to
/// check completely.
async fn listing(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let command = args.first().copied().unwrap_or("");
    let output = git_capped(cwd, args, QUERY_TIMEOUT, MAX_LISTING_BYTES).await?;
    if !output.success {
        return Err(format!("git {command} failed: {}", output.stderr));
    }
    if output.stdout.ends_with(TRUNCATED) {
        return Err(format!("git {command} listed too much to check"));
    }
    Ok(output.stdout)
}

async fn lines(cwd: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    Ok(listing(cwd, args)
        .await?
        .split(['\0', '\n'])
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

async fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = git(
        cwd,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        QUERY_TIMEOUT,
    )
    .await?;
    if output.success {
        return Ok(true);
    }
    if output.stderr.is_empty() {
        return Ok(false);
    }
    Err(format!("git merge-base failed: {}", output.stderr))
}

/// Previews the session worktree on `branch` against the folder.
pub(crate) async fn preview(
    folder: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<LandPreview, String> {
    let worktree_branch = current_branch(worktree).await;
    if worktree_branch.as_deref() != Some(branch) {
        return Err(format!(
            "the session worktree is no longer on its branch {branch}; land it by hand with Git"
        ));
    }
    let tip = require(worktree, &["rev-parse", "HEAD"], QUERY_TIMEOUT).await?;
    let folder_branch = current_branch(folder).await;
    let folder_head = require(folder, &["rev-parse", "HEAD"], QUERY_TIMEOUT).await?;
    let folder_protected = is_protected(folder, folder_branch.as_deref()).await;
    let range = format!("{folder_head}..{tip}");
    let commit_count = require(folder, &["rev-list", "--count", &range], QUERY_TIMEOUT)
        .await?
        .parse::<usize>()
        .map_err(|error| format!("unexpected commit count: {error}"))?;
    let listed = MAX_LISTED_COMMITS.to_string();
    let commits = require(
        folder,
        &["log", "--format=%h %s", "--no-color", "-n", &listed, &range],
        QUERY_TIMEOUT,
    )
    .await?
    .lines()
    .map(str::to_string)
    .collect();
    let uncommitted = lines(
        worktree,
        &[
            "status",
            "--short",
            "--untracked-files=all",
            "--",
            ".",
            EXCLUDE_NIB_STATE,
        ],
    )
    .await?;
    let ignored = lines(
        worktree,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--",
            ".",
            EXCLUDE_NIB_STATE,
        ],
    )
    .await?;
    let stat = require(
        folder,
        &[
            "diff",
            "--stat",
            "--no-color",
            &format!("{folder_head}...{tip}"),
        ],
        QUERY_TIMEOUT,
    )
    .await?;
    let code = confirmation_code(
        &tip,
        &[&uncommitted, &ignored],
        folder_branch.as_deref(),
        &folder_head,
    );
    Ok(LandPreview {
        worktree: worktree.to_path_buf(),
        branch: branch.to_string(),
        tip,
        folder_branch,
        folder_head,
        folder_protected,
        commits,
        commit_count,
        uncommitted,
        ignored,
        stat,
        code,
    })
}

/// A short hash of everything a preview shows, so an action confirms
/// exactly the state the user saw.
fn confirmation_code(
    tip: &str,
    listings: &[&[String]],
    folder_branch: Option<&str>,
    folder_head: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [tip, folder_branch.unwrap_or(""), folder_head] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    for listing in listings {
        for line in *listing {
            hasher.update(line.as_bytes());
            hasher.update([0]);
        }
        hasher.update([1]);
    }
    let digest = hasher.finalize();
    digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..7]
        .to_string()
}

/// Files with uncommitted changes in the folder (tracked or untracked),
/// without nib state. Fails when anything is staged.
async fn folder_changes(folder: &Path) -> Result<Vec<String>, String> {
    let staged = git(folder, &["diff", "--cached", "--quiet"], QUERY_TIMEOUT).await?;
    if !staged.success {
        return Err(
            "the folder has staged changes; commit or unstage them, then run /land merge again"
                .to_string(),
        );
    }
    let mut changed = lines(
        folder,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD",
            "--",
            ".",
            EXCLUDE_NIB_STATE,
        ],
    )
    .await?;
    changed.extend(
        lines(
            folder,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
                EXCLUDE_NIB_STATE,
            ],
        )
        .await?,
    );
    Ok(changed)
}

/// Merges the previewed session commit into the folder's branch, after
/// checking that the merge cannot touch the folder's uncommitted changes.
/// A failed merge is aborted and any branch created for it is removed.
pub(crate) async fn merge(
    folder: &Path,
    preview: &LandPreview,
    topic: &str,
) -> Result<MergeOutcome, String> {
    if !preview.uncommitted.is_empty() {
        return Err(
            "the worktree has uncommitted changes; commit them with /land commit <message>, then merge"
                .to_string(),
        );
    }
    if is_ancestor(folder, &preview.tip, "HEAD").await? {
        return Ok(MergeOutcome::AlreadyMerged);
    }
    refuse_overlap(folder, preview).await?;

    let original_branch = current_branch(folder).await;
    let created_branch = if preview.folder_protected {
        Some(branch_first(folder, topic).await?)
    } else {
        None
    };
    // The previewed commit, not whatever the branch points at now. Ignored
    // files in the folder (a local `.env`) are never overwritten.
    let message = format!("Merge branch '{}'", preview.branch);
    let merge = git(
        folder,
        &[
            "merge",
            "--no-edit",
            "--quiet",
            "--no-overwrite-ignore",
            "--no-autostash",
            "-m",
            &message,
            &preview.tip,
        ],
        COMMIT_TIMEOUT,
    )
    .await;
    let failure = match merge {
        Ok(output) if output.success => None,
        Ok(output) => Some(if output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        }),
        Err(error) => Some(error),
    };
    if let Some(failure) = failure {
        return Err(undo_failed_merge(
            folder,
            preview,
            original_branch.as_deref(),
            created_branch.as_deref(),
            &failure,
        )
        .await);
    }
    Ok(MergeOutcome::Merged {
        commit: require(folder, &["rev-parse", "HEAD"], QUERY_TIMEOUT).await?,
        branch: current_branch(folder).await,
        created_branch,
    })
}

/// Refuses a merge that would change a file with uncommitted changes in
/// the folder.
async fn refuse_overlap(folder: &Path, preview: &LandPreview) -> Result<(), String> {
    let merged_paths = lines(
        folder,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &format!("{}...{}", preview.folder_head, preview.tip),
        ],
    )
    .await?;
    let mut overlap: Vec<String> = folder_changes(folder)
        .await?
        .into_iter()
        .filter(|path| merged_paths.contains(path))
        .collect();
    overlap.sort();
    overlap.dedup();
    if overlap.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the merge would change files you have uncommitted changes in: {}. Commit or set them aside, then run /land again",
        bounded_lines(overlap).join(", ")
    ))
}

/// Creates and switches to a free `nib/<topic>` branch.
async fn branch_first(folder: &Path, topic: &str) -> Result<String, String> {
    let base = format!("{BRANCH_PREFIX}{}", branch_slug(topic));
    let mut name = base.clone();
    let mut suffix = 2;
    while branch_exists(folder, &name).await {
        if suffix > 50 {
            return Err("could not choose a free branch name for the merge".to_string());
        }
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    require(folder, &["switch", "--quiet", "-c", &name], QUERY_TIMEOUT).await?;
    Ok(name)
}

/// Aborts a failed merge and returns to the original branch, removing the
/// branch created for it. Returns the message to show.
async fn undo_failed_merge(
    folder: &Path,
    preview: &LandPreview,
    original_branch: Option<&str>,
    created_branch: Option<&str>,
    failure: &str,
) -> String {
    let conflicts = lines(folder, &["diff", "--name-only", "--diff-filter=U", "-z"])
        .await
        .unwrap_or_default();
    let merging = git(
        folder,
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
        QUERY_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.success);
    if merging {
        let _ = require(folder, &["merge", "--abort"], QUERY_TIMEOUT).await;
    }
    let mut message = if conflicts.is_empty() {
        format!("git merge failed: {failure}")
    } else {
        format!(
            "the merge conflicts in {}; it was undone",
            bounded_lines(conflicts).join(", ")
        )
    };
    if let Some(created) = created_branch {
        let unchanged = require(folder, &["rev-parse", "HEAD"], QUERY_TIMEOUT)
            .await
            .is_ok_and(|head| head == preview.folder_head);
        let mut switch = vec!["switch", "--quiet"];
        match original_branch {
            Some(branch) => switch.push(branch),
            None => switch.extend(["--detach", preview.folder_head.as_str()]),
        }
        let returned = unchanged && require(folder, &switch, QUERY_TIMEOUT).await.is_ok();
        if returned {
            let _ = require(folder, &["branch", "--quiet", "-D", created], QUERY_TIMEOUT).await;
        } else {
            message.push_str(&format!(
                "; the folder was left on the new branch {created}"
            ));
        }
    }
    message.push_str(". The session worktree and its branch were kept.");
    message
}

#[cfg(test)]
#[path = "git_land_tests.rs"]
mod tests;

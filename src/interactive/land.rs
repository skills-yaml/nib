//! `/land`: preview a session worktree, then commit to its branch, merge it
//! into the folder, or discard it (T080 phase 5d).
//!
//! A merge or discard names the confirmation code of the preview it
//! confirms. When the worktree or the folder changed since, the code no
//! longer matches and nib shows the new preview instead of acting.

use crate::session::{SessionStore, SessionWorkspace};
use crate::tools::git_tools::land::{self, LandPreview, MergeOutcome};
use crate::tools::workspace;
use std::future::Future;
use std::path::Path;

/// What `/land` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandAction {
    Preview,
    Commit { message: String },
    Merge { code: String },
    Discard { code: Option<String> },
}

pub(crate) const LAND_USAGE: &str = "/land [commit <message> | merge <code> | discard <code>]";

pub(crate) fn parse_land_action(arguments: &[String]) -> Result<LandAction, String> {
    let usage = || format!("usage: {LAND_USAGE}");
    let Some((action, rest)) = arguments.split_first() else {
        return Ok(LandAction::Preview);
    };
    match (action.to_ascii_lowercase().as_str(), rest) {
        ("commit", message) if !message.is_empty() => Ok(LandAction::Commit {
            message: message.join(" "),
        }),
        ("merge", [code]) => Ok(LandAction::Merge { code: code.clone() }),
        ("discard", []) => Ok(LandAction::Discard { code: None }),
        ("discard", [code]) => Ok(LandAction::Discard {
            code: Some(code.clone()),
        }),
        _ => Err(usage()),
    }
}

/// Runs Git work for a slash command on its own thread and runtime, so it
/// works whether or not the caller is inside a runtime.
fn run<T: Send>(work: impl Future<Output = Result<T, String>> + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("failed to start /land: {error}"))?
                    .block_on(work)
            })
            .join()
            .map_err(|_| "/land stopped unexpectedly".to_string())?
    })
}

pub(crate) fn land(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    action: &LandAction,
) -> Result<String, String> {
    // A load error is reported below, with the way out.
    if crate::integrations::worktree::session_worktree_removal_started(project_root, session_id)
        .unwrap_or(false)
    {
        return match action {
            LandAction::Discard { .. } => finish(
                project_root,
                store,
                session_id,
                &Removal {
                    reason: workspace::DISCARDED_REASON,
                    expected_tip: None,
                    done: None,
                    removed: "Finished removing the session worktree and its branch, which an earlier /land could not remove completely.".to_string(),
                },
            ),
            _ => Ok("An earlier /land could not finish removing this session's worktree. Run /land discard to finish removing it.".to_string()),
        };
    }
    let found =
        match crate::integrations::worktree::session_worktree_branch(project_root, session_id) {
            Ok(found) => found,
            Err(error) => {
                return unusable_worktree(project_root, store, session_id, action, &error)
            }
        };
    let Some((worktree, branch)) = found else {
        return without_worktree(project_root, store, session_id, action);
    };
    let preview = run(land::preview(project_root, &worktree, &branch))?;
    match action {
        LandAction::Preview => Ok(format_preview(&preview)),
        LandAction::Commit { message } => {
            let committed = run(crate::tools::git_tools::git_commit(
                &serde_json::json!({ "message": message }),
                &worktree,
            ))?;
            let commit = committed["commit"].as_str().unwrap_or_default();
            let after = run(land::preview(project_root, &worktree, &branch))?;
            Ok(format!(
                "Committed {} to {branch}.\n\n{}",
                commit.get(..7).unwrap_or(commit),
                format_preview(&after)
            ))
        }
        LandAction::Merge { code } => {
            merge_and_finish(project_root, store, session_id, &preview, code)
        }
        LandAction::Discard { code } => {
            confirm(&preview, code.as_deref())?;
            finish(
                project_root,
                store,
                session_id,
                &Removal {
                    reason: workspace::DISCARDED_REASON,
                    expected_tip: Some(&preview.tip),
                    done: None,
                    removed: discarded(&preview),
                },
            )
        }
    }
}

/// `/land merge`: merges the previewed commit, then removes the worktree.
fn merge_and_finish(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    preview: &LandPreview,
    code: &str,
) -> Result<String, String> {
    let branch = &preview.branch;
    confirm(preview, Some(code))?;
    let topic = landing_topic(store, session_id);
    let done = match run(land::merge(project_root, preview, &topic))? {
        MergeOutcome::AlreadyMerged => {
            format!("Your folder already has everything on {branch}, so nothing was merged.")
        }
        MergeOutcome::Merged {
            commit,
            branch: into,
            created_branch,
        } => {
            let into = into.unwrap_or_else(|| "a detached HEAD".to_string());
            let mut text = format!(
                "Merged {branch} into {into} ({}).",
                commit.get(..7).unwrap_or(&commit)
            );
            if let Some(created) = created_branch {
                text.push_str(&match preview.folder_branch.as_deref() {
                    Some(protected) => format!(
                        " {protected} is protected, so nib first created and switched to {created}."
                    ),
                    None => format!(
                        " Your folder was on a detached HEAD, so nib first created and switched to {created}."
                    ),
                });
            }
            text
        }
    };
    finish(
        project_root,
        store,
        session_id,
        &Removal {
            reason: workspace::LANDED_REASON,
            expected_tip: Some(&preview.tip),
            done: Some(done),
            removed: "The worktree and its branch were removed.".to_string(),
        },
    )
}

/// A worktree nib cannot load (an interrupted removal, or a worktree off its
/// branch) is never deleted blindly. `/land discard` stops the session from
/// using it and leaves what remains for the user to inspect.
fn unusable_worktree(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    action: &LandAction,
    error: &str,
) -> Result<String, String> {
    if !matches!(action, LandAction::Discard { .. }) {
        return Err(format!(
            "nib cannot use this session's worktree: {error}. Fix that, or run /land discard to stop using the worktree without deleting anything."
        ));
    }
    store
        .update_session(session_id, |session| {
            session.workspace = Some(SessionWorkspace::in_place(workspace::DISCARDED_REASON));
            Ok(())
        })
        .map_err(|update| {
            format!("Recording that the session works in your folder failed: {update}")
        })?;
    let next = super::workspace::stored_session_workspace(project_root, store, session_id);
    let next = if next.is_worktree() {
        format!(
            "Its next request needs a new worktree, because {}, which may fail until the old one is removed.",
            next.reason
        )
    } else {
        "Its next request works in your folder.".to_string()
    };
    Ok(format!(
        "This session no longer uses its worktree, which nib could not load: {error}. Nothing was deleted: remove what remains under .nib/worktrees/sessions/ (and its nib/session/ branch, if it is still there) by hand when you no longer need them. {next}"
    ))
}

/// A session without a worktree has nothing to land. One that asked for a
/// worktree but never created it can return to the folder.
fn without_worktree(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    action: &LandAction,
) -> Result<String, String> {
    let current = super::workspace::stored_session_workspace(project_root, store, session_id);
    if !current.is_worktree() {
        return Ok(
            "This session works in your folder, so there is nothing to land: its edits are already there."
                .to_string(),
        );
    }
    if matches!(action, LandAction::Discard { .. }) {
        return finish(
            project_root,
            store,
            session_id,
            &Removal {
                reason: workspace::DISCARDED_REASON,
                expected_tip: None,
                done: None,
                removed: "This session had not created its worktree yet.".to_string(),
            },
        );
    }
    Ok(format!(
        "This session works in its own worktree ({}), but has not created it yet, so there is nothing to land. To work in your folder instead, run /land discard.",
        current.reason
    ))
}

fn confirm(preview: &LandPreview, code: Option<&str>) -> Result<(), String> {
    match code {
        Some(code) if code.eq_ignore_ascii_case(&preview.code) => Ok(()),
        Some(code) => Err(format!(
            "The session worktree or your folder's branch changed since the preview with code {code}, so nothing was done. Check what it shows now:\n\n{}",
            format_preview(preview)
        )),
        None => Err(format!(
            "Confirm with the code from the preview, so nothing is dropped that you did not see:\n\n{}",
            format_preview(preview)
        )),
    }
}

/// How a landing ends: what already happened, and what removing the
/// worktree means.
struct Removal<'a> {
    reason: &'a str,
    /// The previewed branch commit; nothing is removed when it moved.
    expected_tip: Option<&'a str>,
    /// What already happened (a merge), reported whether or not removal
    /// succeeds.
    done: Option<String>,
    /// What the removal did, reported only when it succeeded.
    removed: String,
}

/// Removes the worktree and its branch, records the session in place, and
/// says where its next request works.
fn finish(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    removal: &Removal<'_>,
) -> Result<String, String> {
    let done = removal
        .done
        .as_ref()
        .map(|done| format!("{done} "))
        .unwrap_or_default();
    crate::integrations::worktree::remove_session_worktree(
        project_root,
        session_id,
        removal.expected_tip,
    )
    .map_err(|error| {
        format!("{done}Removing the session worktree did not finish: {error}. Run /land to see what remains.")
    })?;
    let removed = &removal.removed;
    store
        .update_session(session_id, |session| {
            session.workspace = Some(SessionWorkspace::in_place(removal.reason));
            Ok(())
        })
        .map_err(|error| {
            format!(
                "{done}{removed} Recording that the session now works in your folder failed: {error}"
            )
        })?;
    let next = super::workspace::stored_session_workspace(project_root, store, session_id);
    let next = if next.is_worktree() {
        format!(
            "Its next request works in a new worktree, because {}.",
            next.reason
        )
    } else {
        "Its next request works in your folder.".to_string()
    };
    Ok(format!("{done}{removed} {next}"))
}

/// A branch topic for a merge that branches first: the session's name or
/// plan goal, or its id.
fn landing_topic(store: &SessionStore, session_id: &str) -> String {
    let session = store.load_result(session_id).ok().flatten();
    session
        .as_ref()
        .and_then(|session| session.display_name.clone())
        .or_else(|| {
            session
                .as_ref()
                .and_then(|session| session.plan.as_ref())
                .map(|plan| plan.goal.clone())
        })
        .filter(|topic| !topic.trim().is_empty())
        .map_or_else(
            || format!("land session {}", session_id.get(..8).unwrap_or(session_id)),
            |topic| format!("land {topic}"),
        )
}

fn discarded(preview: &LandPreview) -> String {
    let mut dropped = Vec::new();
    if preview.commit_count > 0 {
        dropped.push(count(preview.commit_count, "commit"));
    }
    if !preview.uncommitted.is_empty() {
        dropped.push(count(preview.uncommitted.len(), "uncommitted change"));
    }
    if !preview.ignored.is_empty() {
        dropped.push(count(preview.ignored.len(), "ignored path"));
    }
    if dropped.is_empty() {
        format!(
            "Discarded the session worktree and {}; your folder already had all of its work.",
            preview.branch
        )
    } else {
        format!(
            "Discarded the session worktree and {}, dropping {}.",
            preview.branch,
            dropped.join(" and ")
        )
    }
}

fn count(number: usize, noun: &str) -> String {
    if number == 1 {
        format!("1 {noun}")
    } else {
        format!("{number} {noun}s")
    }
}

pub(crate) fn format_preview(preview: &LandPreview) -> String {
    let into = preview
        .folder_branch
        .as_deref()
        .unwrap_or("a detached HEAD");
    let mut lines = vec![
        format!("Session worktree: {}", preview.worktree.display()),
        format!(
            "Branch {}: {} your folder ({into}) does not have.",
            preview.branch,
            count(preview.commit_count, "commit")
        ),
    ];
    lines.extend(preview.commits.iter().map(|commit| format!("  {commit}")));
    if preview.commit_count > preview.commits.len() {
        lines.push(format!(
            "  … {} more",
            preview.commit_count - preview.commits.len()
        ));
    }
    if preview.uncommitted.is_empty() {
        lines.push("No uncommitted changes in the worktree.".to_string());
    } else {
        lines.push(format!(
            "Uncommitted in the worktree ({}):",
            preview.uncommitted.len()
        ));
        lines.extend(
            preview
                .uncommitted
                .iter()
                .take(20)
                .map(|line| format!("  {line}")),
        );
        if preview.uncommitted.len() > 20 {
            lines.push(format!("  … {} more", preview.uncommitted.len() - 20));
        }
    }
    if !preview.ignored.is_empty() {
        lines.push(format!(
            "Ignored in the worktree, removed with it ({}): {}",
            preview.ignored.len(),
            preview
                .ignored
                .iter()
                .take(10)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if preview.ignored.len() > 10 {
            lines.push(format!("  … {} more", preview.ignored.len() - 10));
        }
    }
    if !preview.stat.is_empty() {
        lines.push(format!("Changes against {into}:"));
        lines.extend(
            preview
                .stat
                .lines()
                .take(30)
                .map(|line| format!("  {line}")),
        );
    }
    lines.push(String::new());
    let merge_into = if preview.folder_protected {
        format!("into a new nib/ branch, because {into} is protected")
    } else {
        format!("into {into}")
    };
    if !preview.uncommitted.is_empty() {
        lines.push(format!(
            "/land commit <message>    commit the uncommitted changes to {}",
            preview.branch
        ));
    }
    lines.push(format!(
        "/land merge {}         merge {} {merge_into}, then remove the worktree",
        preview.code, preview.branch
    ));
    lines.push(format!(
        "/land discard {}       delete the worktree and its branch",
        preview.code
    ));
    lines.join("\n")
}

#[cfg(test)]
#[path = "land_tests.rs"]
mod tests;

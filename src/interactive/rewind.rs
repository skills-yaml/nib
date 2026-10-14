//! `/rewind`: list file checkpoints and restore files to one (T080 phase 4).

use super::*;
use crate::session::{MessageOrigin, MessageProvenance, SessionMessage};
use crate::tools::checkpoints::{Checkpoint, CheckpointKind, CheckpointStore, RewindReport};

const MAX_LISTED_CHECKPOINTS: usize = 20;
const MAX_LISTED_PATHS: usize = 20;

pub(crate) fn format_checkpoints(store: &SessionStore, session_id: &str) -> Result<String, String> {
    let checkpoints = CheckpointStore::for_session(store.sessions_dir(), session_id)?.list()?;
    if checkpoints.is_empty() {
        return Ok("No file checkpoints in this session yet. nib saves one before the first file edit of each turn.".to_string());
    }
    let mut lines = vec![
        "Checkpoints, newest first. /rewind <n> restores the files edited since checkpoint n to their state before it:".to_string(),
    ];
    let mut seen = Vec::new();
    for checkpoint in checkpoints.iter().rev() {
        if seen.contains(&checkpoint.number) {
            continue;
        }
        seen.push(checkpoint.number);
        if seen.len() > MAX_LISTED_CHECKPOINTS {
            lines.push(format!(
                "  ... {} older checkpoint(s)",
                count_numbers(&checkpoints) - MAX_LISTED_CHECKPOINTS
            ));
            break;
        }
        lines.push(format!("  {}", checkpoint_line(&checkpoints, checkpoint)));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn rewind_files(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    number: u64,
) -> Result<String, String> {
    let mut allowed = vec![project_root.to_path_buf()];
    match crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |worktree| Ok(worktree.to_path_buf()),
    ) {
        Ok(Some(worktree)) => allowed.push(worktree),
        Ok(None) => {}
        // A session that stopped using a worktree nib cannot load (`/land
        // discard`) still rewinds its edits in the folder.
        Err(_)
            if !super::workspace::stored_session_workspace(project_root, store, session_id)
                .is_worktree() => {}
        Err(error) => return Err(error),
    }
    let checkpoints = CheckpointStore::for_session(store.sessions_dir(), session_id)?;
    let label = checkpoints
        .list()?
        .into_iter()
        .find(|checkpoint| checkpoint.number == number)
        .map(|checkpoint| checkpoint.label);
    let report = checkpoints.rewind(number, &allowed)?;
    if !report.restored.is_empty() || !report.removed.is_empty() {
        record_rewind(store, session_id, &report)?;
    }
    Ok(format_report(&report, label.as_deref().unwrap_or("")))
}

fn record_rewind(
    store: &SessionStore,
    session_id: &str,
    report: &RewindReport,
) -> Result<(), String> {
    let mut note = format!(
        "The user rewound files to their state before checkpoint #{}. The conversation was not rewound, so earlier edits to these files are undone.",
        report.number
    );
    if !report.restored.is_empty() {
        note.push_str(&format!(" Restored: {}.", path_list(&report.restored)));
    }
    if !report.removed.is_empty() {
        note.push_str(&format!(" Removed: {}.", path_list(&report.removed)));
    }
    note.push_str(" Re-read a file before editing it.");
    store
        .update_session(session_id, |session| {
            let index = session.messages.len();
            session.messages.push(SessionMessage {
                index,
                role: "user".to_string(),
                content: note,
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.message_provenance.push(MessageProvenance {
                message_index: index,
                origin: MessageOrigin::RuntimeContinuation,
            });
            Ok(())
        })
        .map_err(|error| format!("files were rewound, but the session note failed: {error}"))?;
    store
        .record_event(
            session_id,
            "files_rewound",
            serde_json::json!({
                "checkpoint": report.number,
                "undo_checkpoint": report.undo_number,
                "restored": report.restored,
                "removed": report.removed,
                "skipped": report.skipped.iter().map(|(path, _)| path).collect::<Vec<_>>(),
            }),
        )
        .map_err(|error| format!("files were rewound, but the audit event failed: {error}"))?;
    Ok(())
}

fn format_report(report: &RewindReport, label: &str) -> String {
    let mut lines = vec![if label.is_empty() {
        format!("Rewound files to before checkpoint #{}.", report.number)
    } else {
        format!(
            "Rewound files to before checkpoint #{} ({label:?}).",
            report.number
        )
    }];
    if report.restored.is_empty() && report.removed.is_empty() && report.skipped.is_empty() {
        lines.push("No file needed restoring.".to_string());
    }
    if !report.restored.is_empty() {
        lines.push(format!("Restored: {}", path_list(&report.restored)));
    }
    if !report.removed.is_empty() {
        lines.push(format!("Removed: {}", path_list(&report.removed)));
    }
    for (path, reason) in report.skipped.iter().take(MAX_LISTED_PATHS) {
        lines.push(format!("Not restored: {path} ({reason})"));
    }
    if report.skipped.len() > MAX_LISTED_PATHS {
        lines.push(format!(
            "Not restored: {} more",
            report.skipped.len() - MAX_LISTED_PATHS
        ));
    }
    if report.commands {
        lines.push(
            "Commands ran in these turns; files they changed were not restored. Check /diff."
                .to_string(),
        );
    }
    if let Some(undo) = report.undo_number {
        lines.push(format!("Undo with /rewind {undo}."));
    }
    lines.join("\n")
}

fn checkpoint_line(all: &[Checkpoint], checkpoint: &Checkpoint) -> String {
    let files: usize = all
        .iter()
        .filter(|entry| entry.number == checkpoint.number)
        .map(|entry| entry.files.len())
        .sum();
    let commands = all
        .iter()
        .any(|entry| entry.number == checkpoint.number && entry.commands);
    let time = checkpoint
        .created_at
        .with_timezone(&chrono::Local)
        .format("%H:%M");
    let label = match checkpoint.kind {
        CheckpointKind::Turn => format!("{:?}", checkpoint.label),
        CheckpointKind::Rewind => checkpoint.label.clone(),
    };
    let mut line = format!(
        "#{} {time} {label}, {files} file{}",
        checkpoint.number,
        if files == 1 { "" } else { "s" }
    );
    if commands {
        line.push_str(", commands ran (not restored)");
    }
    line
}

fn count_numbers(checkpoints: &[Checkpoint]) -> usize {
    let mut numbers: Vec<u64> = checkpoints.iter().map(|c| c.number).collect();
    numbers.dedup();
    numbers.len()
}

fn path_list(paths: &[String]) -> String {
    let mut listed: Vec<&str> = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(String::as_str)
        .collect();
    let more = paths.len().saturating_sub(MAX_LISTED_PATHS);
    let more_text = format!("and {more} more");
    if more > 0 {
        listed.push(&more_text);
    }
    listed.join(", ")
}

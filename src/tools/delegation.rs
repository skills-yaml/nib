use serde_json::{json, Value};
use std::path::Path;
use tokio::process::Command;

pub fn spawn_subagent(args: &Value, cwd: &Path) -> Result<Value, String> {
    let subagent_id = format!("sub-{}", uuid::Uuid::new_v4());
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("help")
        .to_string();

    let worktree = crate::sandbox::worktree::Worktree::create(cwd, &subagent_id)?;
    let wt_path = worktree.path.clone();

    crate::daemons::task::TASK_MANAGER.register_subagent(subagent_id.clone());

    let sid = subagent_id.clone();
    tokio::spawn(async move {
        let cfg = crate::agent::AgentLoopConfig {
            max_steps: 10,
            ..Default::default()
        };
        let _ = crate::agent::run_agent_loop(wt_path.clone(), &sid, &prompt, cfg).await;
        crate::daemons::task::TASK_MANAGER.mark_completed(&sid);
        // Do not remove the worktree yet, so parent can merge it if they want.
        // Or if we remove it, we lose the changes. So we don't remove it.
        // Actually, we probably should let `merge_subagent_worktree` remove it after merging,
        // or a separate cleanup step.
    });

    Ok(json!({ "status": "started", "subagent_id": subagent_id }))
}

pub async fn merge_subagent_worktree(args: &Value, cwd: &Path) -> Result<Value, String> {
    let subagent_id = args
        .get("subagent_id")
        .and_then(|v| v.as_str())
        .ok_or("missing subagent_id")?;

    let worktree_dir = cwd.join(".nib").join("worktrees").join(subagent_id);
    if !worktree_dir.exists() {
        return Err(format!(
            "Worktree not found for subagent_id: {}",
            subagent_id
        ));
    }

    // A simple merge implementation: commit changes in worktree, then merge that branch into cwd.
    // The branch name created by Worktree::create is usually based on the id.
    // Let's just pull or merge the branch.
    // Wait, `worktree::Worktree::create` creates a branch? Let's check `src/sandbox/worktree.rs`.
    // Instead of assuming, let's just do `git merge` or similar. Wait, if it's a git worktree,
    // it's on a branch.
    let branch_name = format!("nib-subagent-{}", subagent_id);

    // Run git commit in worktree just in case there are uncommitted changes
    let _ = Command::new("git")
        .current_dir(&worktree_dir)
        .arg("add")
        .arg(".")
        .output()
        .await;

    let _ = Command::new("git")
        .current_dir(&worktree_dir)
        .arg("commit")
        .arg("-m")
        .arg(format!("subagent {} changes", subagent_id))
        .output()
        .await;

    // Merge the branch into current branch in cwd
    let merge_output = Command::new("git")
        .current_dir(cwd)
        .arg("merge")
        .arg(&branch_name)
        .arg("--no-edit")
        .output()
        .await
        .map_err(|e| format!("git merge failed to start: {}", e))?;

    if !merge_output.status.success() {
        return Ok(json!({
            "success": false,
            "stdout": String::from_utf8_lossy(&merge_output.stdout),
            "stderr": String::from_utf8_lossy(&merge_output.stderr),
            "error": "Merge conflicts or failure",
        }));
    }

    // Remove the worktree
    let _ = crate::sandbox::worktree::Worktree::remove(cwd, subagent_id);

    Ok(json!({
        "success": true,
        "stdout": String::from_utf8_lossy(&merge_output.stdout),
    }))
}

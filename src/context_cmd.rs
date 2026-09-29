use clap::Args;
use nib::context::assemble_profile_context;
use nib::session::SessionStore;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ContextArgs {
    #[arg(default_value = ".")]
    pub path: String,

    #[arg(short, long)]
    pub task: Option<String>,

    /// Inspect a live session request snapshot instead of assembling project context.
    #[arg(long)]
    pub session: Option<String>,

    /// Emit bounded JSON. Project preview uses kind=project_preview; session inspect uses kind=live_request.
    #[arg(long)]
    pub json: bool,
}

pub fn run_context(args: &ContextArgs) -> Result<(), String> {
    let path = PathBuf::from(&args.path);
    if let Some(session_id) = args.session.as_deref() {
        return inspect_session(&path, session_id, args.json);
    }
    let ctx = assemble_profile_context(&path, args.task.as_deref())?;
    if args.json {
        println!("{}", nib::context::snapshot::project_preview_json(&ctx));
    } else {
        println!("{ctx}");
    }
    Ok(())
}

fn inspect_session(project_root: &Path, session_id: &str, json: bool) -> Result<(), String> {
    let store = SessionStore::for_project(project_root)
        .map_err(|error| format!("failed to open sessions: {error}"))?;
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} was not found"))?;
    if session.id != session_id {
        return Err("session projection does not match the requested session".to_string());
    }
    let snapshot = nib::context::snapshot::latest_from_session(&session);
    if json {
        println!(
            "{}",
            nib::context::snapshot::inspect_json(&session, snapshot.as_ref())
        );
    } else {
        println!(
            "{}",
            nib::context::snapshot::inspect_text(&session, snapshot.as_ref(), true)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn context_command_assembles_profile_context_with_and_without_task() {
        let project = tempdir().expect("project");
        std::fs::write(project.path().join("AGENTS.md"), "runtime context rule")
            .expect("AGENTS fixture");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("config");

        for task in [Some("inspect context".to_string()), None] {
            run_context(&ContextArgs {
                path: project.path().to_string_lossy().into_owned(),
                task,
                session: None,
                json: false,
            })
            .expect("context command");
        }
    }

    #[test]
    fn context_command_reports_invalid_config() {
        let project = tempdir().expect("project");
        let paths = nib::config::config_paths(project.path());
        std::fs::create_dir_all(&paths.nib_dir).expect("config directory");
        std::fs::write(&paths.toml, "not = [valid").expect("invalid config");
        assert!(run_context(&ContextArgs {
            path: project.path().to_string_lossy().into_owned(),
            task: Some("inspect".to_string()),
            session: None,
            json: false,
        })
        .is_err());
    }

    #[test]
    fn session_inspect_json_is_not_a_project_preview() {
        let project = tempdir().expect("project");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("config");
        let store = SessionStore::for_project(project.path()).expect("store");
        let session = store.try_create_session().expect("session");
        let snapshot = nib::context::snapshot::snapshot_from_bounded_input(
            "execution",
            "sent",
            "configured",
            8_192,
            &nib::context::budget::BoundedLlmInput {
                messages: vec![json!({"role":"user","content":"goal"})],
                tools: None,
                approximate_tokens: 40,
                raw_message_count: 1,
                raw_tool_count: 0,
                included_tool_count: 0,
            },
        );
        store
            .record_event(
                &session.id,
                "context_bounded",
                snapshot.to_event_details("run-1"),
            )
            .expect("snapshot");

        inspect_session(project.path(), &session.id, true).expect("session inspect");
        let preview = nib::context::snapshot::project_preview_json("assembled");
        assert_eq!(preview["kind"], "project_preview");
    }

    #[test]
    fn session_inspect_rejects_a_missing_or_foreign_session() {
        let project = tempdir().expect("project");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("config");
        let error = inspect_session(project.path(), "missing-session", false).expect_err("missing");
        assert!(error.contains("was not found"));
    }

    #[test]
    fn json_project_preview_keeps_compatibility_kind() {
        let project = tempdir().expect("project");
        std::fs::write(project.path().join("AGENTS.md"), "runtime context rule")
            .expect("AGENTS fixture");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("config");
        run_context(&ContextArgs {
            path: project.path().to_string_lossy().into_owned(),
            task: None,
            session: None,
            json: true,
        })
        .expect("json preview");
    }
}

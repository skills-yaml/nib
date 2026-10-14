//! Fork a transcript before its latest user request without copying execution authority.
use crate::session::SessionStore;

pub(crate) fn fork_before_last_user(
    store: &SessionStore,
    id: &str,
) -> Result<(String, String), String> {
    let _lease = store.try_acquire_run_lease(id).map_err(|e| e.to_string())?;
    let source = store
        .load_result(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "session no longer exists".to_string())?;
    let cut = source
        .messages
        .iter()
        .rposition(|m| m.role == "user")
        .ok_or_else(|| "no previous user message to edit".to_string())?;
    let draft = source.messages[cut].content.clone();
    let created = store.try_create_session().map_err(|e| e.to_string())?;
    store
        .update_session(&created.id, |session| {
            session.messages = source.messages[..cut].to_vec();
            session.message_provenance = source
                .message_provenance
                .iter()
                .filter(|p| p.message_index < cut)
                .cloned()
                .collect();
            session.human_intent = source
                .human_intent
                .iter()
                .filter(|p| p.source_message_index.is_some_and(|i| i < cut))
                .cloned()
                .collect();
            session.active_skills = source.active_skills.clone();
            session.display_name = source.display_name.clone();
            session.permission_mode = source.permission_mode.clone().filter(|m| m != "auto");
            // A fork of a worktree session gets its own worktree.
            session.workspace = source.workspace.clone();
            session.forked_from = Some(source.id.clone());
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok((created.id, draft))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionMessage;
    #[test]
    fn edit_fork_keeps_prefix_without_execution_authority_or_original_changes() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::at_dir(root.path().join("sessions"));
        let session = store.try_create_session().unwrap();
        store
            .update_session(&session.id, |s| {
                for (role, content) in [
                    ("user", "first"),
                    ("assistant", "answer"),
                    ("user", "edit me"),
                    ("assistant", "later"),
                ] {
                    s.messages.push(SessionMessage {
                        index: s.messages.len(),
                        role: role.into(),
                        content: content.into(),
                        timestamp: None,
                        attachments: vec![],
                    });
                }
                s.plan = Some(crate::session::Plan::new("previous execution", vec![]));
                s.tool_calls.push(crate::session::ToolCallRecord::default());
                s.queued_follow_ups.push(crate::session::QueuedFollowUp {
                    id: "old-queued".into(),
                    text: "later work".into(),
                    created_at: chrono::Utc::now(),
                    source: "composer".into(),
                });
                s.permission_mode = Some("auto".into());
                s.summary = Some("includes later answer".into());
                Ok(())
            })
            .unwrap();
        let before = store.load_result(&session.id).unwrap().unwrap();
        let (id, draft) = fork_before_last_user(&store, &session.id).unwrap();
        assert_eq!(draft, "edit me");
        let fork = store.load_result(&id).unwrap().unwrap();
        assert_eq!(fork.messages, before.messages[..2]);
        assert_eq!(fork.forked_from.as_deref(), Some(session.id.as_str()));
        assert!(
            fork.plan.is_none() && fork.tool_calls.is_empty() && fork.queued_follow_ups.is_empty()
        );
        assert!(fork.permission_mode.is_none() && fork.summary.is_none());
        assert_eq!(store.load_result(&session.id).unwrap().unwrap(), before);
        let _lease = store.try_acquire_run_lease(&session.id).unwrap();
        assert!(fork_before_last_user(&store, &session.id).is_err());
    }
    #[test]
    fn edit_fork_without_user_message_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::at_dir(root.path().join("sessions"));
        let session = store.try_create_session().unwrap();
        assert!(fork_before_last_user(&store, &session.id).is_err());
    }
}

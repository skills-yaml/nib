use super::*;
#[test]
fn editor_success_failure_and_invalid_output_recover_draft() {
    assert_eq!(
        edit_draft_with("original", "editor", |_, p| {
            std::fs::write(p, "edited\r\ntext")?;
            Ok(true)
        })
        .unwrap(),
        "edited\ntext"
    );
    assert!(edit_draft_with("original", "editor", |_, _| Ok(false)).is_err());
    assert!(edit_draft_with("original", "", |_, _| panic!("must not invoke")).is_err());
    assert!(edit_draft_with("original", "editor", |_, p| {
        std::fs::write(p, [0xff])?;
        Ok(true)
    })
    .is_err());
}
#[cfg(unix)]
#[test]
fn editor_symlink_replacement_is_rejected() {
    assert!(edit_draft_with("original", "editor", |_, p| {
        std::fs::remove_file(p)?;
        std::os::unix::fs::symlink("/etc/passwd", p)?;
        Ok(true)
    })
    .is_err());
}

#[test]
fn editor_terminal_resume_is_attempted_on_suspend_and_launch_failures() {
    let calls = std::cell::Cell::new(0);
    let result = editor_handoff(
        || Err(io::Error::other("suspend failed")),
        || panic!("must not launch"),
        || {
            calls.set(calls.get() + 1);
            Ok(())
        },
    );
    assert!(result.unwrap().is_err());
    assert_eq!(calls.get(), 1);
    assert!(editor_handoff(
        || Ok(()),
        || Err(io::Error::other("launch failed")),
        || Ok(())
    )
    .unwrap()
    .is_err());
    assert!(editor_handoff(
        || Ok(()),
        || Ok(true),
        || Err(io::Error::other("resume failed"))
    )
    .is_err());
}

#[test]
fn clear_view_preserves_persisted_session_and_removes_live_output() {
    let root = tempdir().unwrap();
    let store = SessionStore::at_dir(root.path().join("sessions"));
    let session = store.try_create_session().unwrap();
    let mut timeline = ActiveTimeline::from_session(&session, vec![]);
    timeline.push_status("visible evidence".into());
    timeline.live.text = "last output".into();
    clear_visible_transcript(&mut timeline);
    assert!(timeline.activities.is_empty() && timeline.live.text.is_empty());
    assert_eq!(store.load_result(&session.id).unwrap().unwrap(), session);
}

#[test]
fn completion_consumes_tab_before_running_queue_binding() {
    use crate::config::keymap::{KeyAction, TuiConfig};
    let keymap = TuiConfig::default();
    assert_eq!(
        keymap.resolve("running", KeyCode::Tab, KeyModifiers::NONE),
        Some(KeyAction::Queue)
    );
    let mut composer = Composer::from_text("/sta");
    let mut menu = CompletionMenu::default();
    menu.sync(&composer.input);
    assert_eq!(
        menu.handle_key(&mut composer, KeyCode::Tab),
        CompletionKeyResult::Consumed
    );
    assert!(composer.input.starts_with("/status"));
}

#[cfg(unix)]
#[test]
fn editor_directory_protects_replacement_files_and_backups() {
    use std::os::unix::fs::PermissionsExt;
    edit_draft_with("private draft", "editor", |_, path| {
        let parent = path.parent().unwrap();
        assert_eq!(std::fs::metadata(parent)?.permissions().mode() & 0o077, 0);
        let backup = parent.join("backup");
        std::fs::write(&backup, "private backup")?;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o644))?;
        // The private containing directory still prevents access to backups.
        assert_eq!(std::fs::metadata(parent)?.permissions().mode() & 0o077, 0);
        Ok(true)
    })
    .unwrap();
}

#[test]
fn completion_respects_newlines_unbound_enter_and_semantic_shortcuts() {
    use crate::config::keymap::{KeyAction, TuiConfig};
    let mut map = TuiConfig::default();
    let mut composer = Composer::from_text("/sta");
    let mut menu = CompletionMenu::default();
    menu.sync(&composer.input);
    let action = map.resolve("composer", KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(
        completion_keymap_action(
            &mut menu,
            &mut composer,
            action,
            KeyCode::Enter,
            KeyModifiers::SHIFT
        ),
        CompletionKeyResult::Ignored
    );
    assert_eq!(
        composer_keymap_action(&mut composer, action, KeyCode::Enter, KeyModifiers::SHIFT),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "/sta\n");
    map.keymap
        .entry("composer".into())
        .or_default()
        .insert("send".into(), vec![]);
    composer.set_text("/sta".into());
    menu.sync(&composer.input);
    let action = map.resolve("composer", KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(action, None);
    assert_eq!(
        completion_keymap_action(
            &mut menu,
            &mut composer,
            action,
            KeyCode::Enter,
            KeyModifiers::NONE
        ),
        CompletionKeyResult::Ignored
    );
    assert_eq!(
        composer_keymap_action(&mut composer, action, KeyCode::Enter, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "/sta");
    for (code, expected) in [
        ('a', KeyAction::Start),
        ('e', KeyAction::End),
        ('g', KeyAction::Editor),
        ('l', KeyAction::ClearView),
    ] {
        let action = map.resolve("composer", KeyCode::Char(code), KeyModifiers::CONTROL);
        assert_eq!(action, Some(expected));
        assert_eq!(
            completion_keymap_action(
                &mut menu,
                &mut composer,
                action,
                KeyCode::Char(code),
                KeyModifiers::CONTROL
            ),
            CompletionKeyResult::Ignored
        );
    }
    map.keymap
        .get_mut("composer")
        .unwrap()
        .insert("send".into(), vec!["alt-s".into()]);
    let action = map.resolve("composer", KeyCode::Char('s'), KeyModifiers::ALT);
    assert_eq!(
        completion_keymap_action(
            &mut menu,
            &mut composer,
            action,
            KeyCode::Char('s'),
            KeyModifiers::ALT
        ),
        CompletionKeyResult::Submit
    );
    assert!(composer.input.starts_with("/status"));
}

#[test]
fn running_file_completion_preserves_current_turn_steering_action() {
    use crate::config::keymap::{KeyAction, TuiConfig};
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("README.md"), "reference").unwrap();
    let mut composer = Composer::from_text("review @REA");
    let mut menu = CompletionMenu::default();
    menu.sync_for(&composer.input, Some(root.path()));
    let action = TuiConfig::default().resolve("running", KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(action, Some(KeyAction::Steer));
    assert_eq!(
        completion_keymap_action(
            &mut menu,
            &mut composer,
            action,
            KeyCode::Enter,
            KeyModifiers::NONE
        ),
        CompletionKeyResult::Submit
    );
    assert_eq!(action, Some(KeyAction::Steer));
    assert!(composer.input.contains("@README.md"));
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SteerCurrent(&composer.input)
        ),
        InteractionReduction::SteerCurrent(composer.input.clone())
    );
}

#[test]
fn modified_composer_shortcuts_do_not_answer_pending_approvals() {
    let (reply, mut receive) = oneshot::channel();
    let mut approval = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".into(),
            arguments: json!({"command":"task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply,
    ));
    let mut question = None;
    handle_pending_interaction_key_with_modifiers(
        &mut approval,
        &mut question,
        KeyCode::Char('y'),
        KeyModifiers::ALT,
    );
    handle_pending_interaction_key_with_modifiers(
        &mut approval,
        &mut question,
        KeyCode::Enter,
        KeyModifiers::ALT,
    );
    assert!(approval.is_some());
    assert!(receive.try_recv().is_err());
    handle_pending_interaction_key_with_modifiers(
        &mut approval,
        &mut question,
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    );
    assert!(!receive.try_recv().unwrap().granted);
}

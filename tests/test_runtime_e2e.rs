use nib::agent::r#loop::{run_agent_loop, AgentLoopConfig};
use nib::config::{save_config, LlmConfig};
use nib::session::SessionStore;
use std::path::PathBuf;
use tempfile::tempdir;

#[tokio::test]
async fn test_full_agent_runtime_cycle() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    
    // Set up mock provider
    let mut cfg = LlmConfig::default();
    cfg.active_provider = Some("mock".to_string());
    save_config(dir.path(), &cfg).unwrap();

    let cfg = AgentLoopConfig {
        max_steps: 5,
        auto_approve: true,
        ..Default::default()
    };

    // Run the agent loop simulating prompt -> context -> mock LLM -> execute -> memory update
    let summary = run_agent_loop(dir.path().to_path_buf(), &session.id, "run e2e test", cfg)
        .await
        .unwrap();

    // Verify it took steps and generated tool calls
    assert!(summary.steps_taken > 0);
    
    // Check that state machine transitions hit the "assistant" and "tool" states
    let final_session = store.load(&session.id).unwrap();
    assert!(final_session.messages.len() > 1);
    
    // First message should be the user goal
    assert_eq!(final_session.messages[0].role, "user");
    assert_eq!(final_session.messages[0].content, "run e2e test");

    // Because mock returns a tool call first, there should be an assistant message, then tool
    let has_assistant = final_session.messages.iter().any(|m| m.role == "assistant");
    assert!(has_assistant, "No assistant message found");
    
    let has_tool = final_session.messages.iter().any(|m| m.role == "tool");
    assert!(has_tool, "No tool message found");
}

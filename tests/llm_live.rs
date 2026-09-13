//! Credential-gated live LLM provider/model qualification (T023).
//!
//! Deterministic parser, planner, allowlist, and report tests in this target run during
//! ordinary CI. The network entrypoint is ignored and also requires explicit environment
//! acknowledgements before it constructs an HTTP client.

mod llm_live_support;

fn repository_text(relative: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|error| panic!("failed to read {relative}: {error}"))
        .replace("\r\n", "\n")
}

#[test]
fn selected_live_ci_contract_is_protected_and_reported() {
    let workflow = repository_text(".github/workflows/llm-live.yml");
    let taskfile = repository_text("Taskfile.yml");

    assert!(workflow.contains("          - selected\n"));
    assert!(workflow.contains("catalog | canary | selected | full"));
    assert!(workflow.contains("NIB_LIVE_SCHEDULE_MODE"));
    assert!(workflow.contains(".schema_version == 2"));
    assert!(workflow.contains(".source_revision == $revision"));
    assert!(workflow.contains(".selected_suite.matrix_sha256"));
    assert!(workflow.contains("Provider reports do not match the requested provider set."));
    assert!(workflow.contains("Provider reports do not have unique run identities."));
    assert!(workflow.contains("Selected provider reports do not share identical suite provenance."));
    assert!(workflow.contains(".scenario == \"complete_text\""));
    assert!(workflow.contains(".scenario == \"streamed_text\""));
    assert!(workflow.contains(".scenario == \"single_tool_continuation\""));
    assert!(workflow.contains(".scenario == \"parallel_tool_continuation\""));
    assert!(workflow.contains(".not_applicable_scenarios[]"));
    assert!(workflow.contains("      - name: Upload sanitized live reports\n        if: always()"));
    assert!(workflow.contains("          if-no-files-found: ignore\n"));
    assert!(!workflow.contains("\n  pull_request:"));
    assert!(!workflow.contains("\n  push:"));
    assert!(taskfile.contains("  test:llm-live:offline:\n"));
    assert!(taskfile.contains("  test:llm-live:selected:\n"));
    assert!(taskfile.contains("MODE: selected"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit live-network acknowledgement and may incur provider costs"]
async fn live_llm_qualification() {
    let published = llm_live_support::run_from_environment()
        .await
        .unwrap_or_else(|error| panic!("live LLM qualification failed safely: {error}"));
    println!("live LLM qualification JSON: {}", published.json.display());
    println!(
        "live LLM qualification summary: {}",
        published.markdown.display()
    );
    assert!(published.passed, "live LLM qualification did not pass");
}

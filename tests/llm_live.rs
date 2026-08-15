//! Credential-gated live LLM provider/model qualification (T023).
//!
//! Deterministic parser, planner, allowlist, and report tests in this target run during
//! ordinary CI. The network entrypoint is ignored and also requires explicit environment
//! acknowledgements before it constructs an HTTP client.

mod llm_live_support;

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

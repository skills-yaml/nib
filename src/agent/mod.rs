//! Agent loop and prompt assembly.

use std::future::Future;

pub mod r#loop;
pub mod planner;
pub mod state;

const AGENT_RUNTIME_WORKER_STACK_BYTES: usize = 4 * 1024 * 1024;

#[doc(hidden)]
pub fn build_agent_runtime(error_context: &'static str) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(AGENT_RUNTIME_WORKER_STACK_BYTES)
        .enable_all()
        .build()
        .map_err(|error| format!("{error_context}: {error}"))
}

#[doc(hidden)]
pub fn block_on_agent_runtime_worker<F>(
    runtime: &tokio::runtime::Runtime,
    future: F,
    worker_label: &'static str,
) -> Result<F::Output, String>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let worker = runtime.spawn(future);
    runtime.block_on(worker).map_err(|error| {
        if error.is_cancelled() {
            format!("{worker_label} was cancelled")
        } else {
            format!("{worker_label} panicked")
        }
    })
}

pub use r#loop::{
    exact_run_steering_channel, run_agent_loop, run_agent_loop_for_profile, AgentLoopConfig,
    AgentRunSummary, CancellationSignal, ExactRunSteeringHandle, ExactRunSteeringReceiver,
    QuestionHandler, MAX_STEERING_INPUT_BYTES,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_agent_runtime_polls_root_future_off_caller_thread() {
        assert_eq!(AGENT_RUNTIME_WORKER_STACK_BYTES, 4 * 1024 * 1024);
        let runtime = build_agent_runtime("test runtime").expect("runtime");
        let caller = std::thread::current().id();
        let worker = block_on_agent_runtime_worker(
            &runtime,
            async move { std::thread::current().id() },
            "test worker",
        )
        .expect("worker result");

        assert_ne!(worker, caller);
    }

    #[test]
    fn agent_runtime_join_failure_is_bounded_and_does_not_reflect_panic_payload() {
        let runtime = build_agent_runtime("test runtime").expect("runtime");
        let error = block_on_agent_runtime_worker(
            &runtime,
            async move { panic!("private-panic-payload") },
            "test worker",
        )
        .expect_err("panic must fail the join");

        assert_eq!(error, "test worker panicked");
        assert!(!error.contains("private-panic-payload"));
    }
}

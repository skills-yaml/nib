//! Bounded progress detection and refresh of a continuation's public base prompt.
use super::*;
use std::collections::VecDeque;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProgressDecision {
    Continue,
    Correct,
    Stop,
}

#[derive(Default)]
pub(crate) struct SuccessfulProgressGuard {
    recent: VecDeque<String>,
    correction: Option<(Vec<String>, usize)>,
    step: Option<usize>,
}

impl SuccessfulProgressGuard {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn observe(
        &mut self,
        calls: &[ToolCallRequest],
        observations: &[Value],
        success: bool,
        step: usize,
    ) -> ProgressDecision {
        if !success
            || calls.is_empty()
            || calls.iter().all(is_poll_call)
            || self.step.is_some_and(|previous| step != previous)
        {
            self.reset();
        }
        self.step = Some(step);
        if !success || calls.is_empty() || calls.iter().all(is_poll_call) {
            return ProgressDecision::Continue;
        }
        let fingerprint = semantic_batch(calls, observations);
        if let Some((pattern, offset)) = self.correction.as_mut() {
            if pattern[*offset] == fingerprint {
                *offset += 1;
                return if *offset == pattern.len() {
                    ProgressDecision::Stop
                } else {
                    ProgressDecision::Continue
                };
            }
            self.correction = None;
            self.recent.clear();
        }
        self.recent.push_back(fingerprint);
        while self.recent.len() > 6 {
            self.recent.pop_front();
        }
        for width in [1, 2] {
            if self.recent.len() < width * 3 {
                continue;
            }
            let tail = self
                .recent
                .iter()
                .skip(self.recent.len() - width * 3)
                .collect::<Vec<_>>();
            if (width..tail.len()).all(|index| tail[index] == tail[index % width]) {
                self.correction = Some((
                    tail[..width].iter().map(|item| (*item).clone()).collect(),
                    0,
                ));
                return ProgressDecision::Correct;
            }
        }
        ProgressDecision::Continue
    }
}

fn is_poll_call(call: &ToolCallRequest) -> bool {
    matches!(
        (call.name.as_str(), call.arguments["action"].as_str()),
        ("manage_task", Some("get" | "list")) | ("manage_subagents", Some("status" | "list"))
    )
}

fn semantic_batch(calls: &[ToolCallRequest], observations: &[Value]) -> String {
    let outputs = observations
        .iter()
        .map(|observation| {
            let mut output = observation["output"].clone();
            if let Some(object) = output.as_object_mut() {
                if observation["tool"] == "run_terminal" {
                    object.remove("duration");
                }
                if observation["tool"] == "load_skill" {
                    object.remove("already_active");
                }
            }
            json!({"tool":observation["tool"], "success":observation["success"],
            "output":output, "error":observation["error"]})
        })
        .collect::<Vec<_>>();
    let semantic = json!({"calls":calls.iter().map(|call| json!({
        "name":call.name,"arguments":call.arguments})).collect::<Vec<_>>(), "results":outputs});
    format!("{:x}", Sha256::digest(semantic.to_string().as_bytes()))
}

/// Keep the original public conversation prefix while refreshing live instructions
/// and plan state. Native exchanges belong exclusively to the adapter continuation.
pub(crate) fn refresh_continuation_base(base: &mut Session, current: &Session) {
    base.plan.clone_from(&current.plan);
    base.active_skills.clone_from(&current.active_skills);
    base.skill_usage.clone_from(&current.skill_usage);
}

pub(crate) fn scope_proposal_key(call: &ToolCallRequest) -> String {
    let mut args = call.arguments.clone();
    if let Some(object) = args.as_object_mut() {
        object.remove("affected_paths");
        if object.get("cwd").is_some_and(|cwd| cwd == ".") {
            object.remove("cwd");
        }
    }
    format!(
        "{:x}",
        Sha256::digest(
            json!({"tool":call.name,"arguments":args})
                .to_string()
                .as_bytes()
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observe(
        guard: &mut SuccessfulProgressGuard,
        name: &str,
        value: usize,
        step: usize,
    ) -> ProgressDecision {
        guard.observe(
            &[ToolCallRequest::new(name, json!({}))],
            &[json!({"tool":name,"success":true,"output":{"value":value}})],
            true,
            step,
        )
    }

    #[test]
    fn successful_identical_and_alternating_cycles_have_one_correction() {
        for pattern in [vec!["git_status"], vec!["load_skill", "git_status"]] {
            let mut guard = SuccessfulProgressGuard::default();
            for index in 0..pattern.len() * 4 {
                let result = observe(&mut guard, pattern[index % pattern.len()], 0, 0);
                assert_eq!(
                    result,
                    if index + 1 == pattern.len() * 3 {
                        ProgressDecision::Correct
                    } else if index + 1 == pattern.len() * 4 {
                        ProgressDecision::Stop
                    } else {
                        ProgressDecision::Continue
                    }
                );
            }
        }
    }

    #[test]
    fn polling_does_not_exempt_repeated_non_polling_work_in_a_mixed_batch() {
        let mut guard = SuccessfulProgressGuard::default();
        let calls = [
            ToolCallRequest::new("git_status", json!({})),
            ToolCallRequest::new("manage_task", json!({"action":"get"})),
        ];
        for expected in [
            ProgressDecision::Continue,
            ProgressDecision::Continue,
            ProgressDecision::Correct,
            ProgressDecision::Stop,
        ] {
            assert_eq!(guard.observe(&calls, &[], true, 0), expected);
        }
    }

    #[test]
    fn changed_evidence_step_and_polling_break_cycles() {
        let mut guard = SuccessfulProgressGuard::default();
        for value in 0..10 {
            assert_eq!(
                observe(&mut guard, "git_status", value, 0),
                ProgressDecision::Continue
            );
        }
        guard.reset();
        for step in 0..10 {
            assert_eq!(
                observe(&mut guard, "git_status", 0, step),
                ProgressDecision::Continue
            );
        }
        for _ in 0..10 {
            let call = ToolCallRequest::new("manage_task", json!({"action":"get"}));
            assert_eq!(
                guard.observe(&[call], &[], true, 0),
                ProgressDecision::Continue
            );
        }
    }
}

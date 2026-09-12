use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Planning,
    PlanApproval,
    BuildContext,
    Compression,
    InspectLlm,
    UserApproval,
    ToolExecute,
    UpdateMemory,
    Reconciliation,
    WaitingForUserInput,
    Done,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Planning => "planning",
            Self::PlanApproval => "plan_approval",
            Self::BuildContext => "build_context",
            Self::Compression => "compression",
            Self::InspectLlm => "inspect_llm",
            Self::UserApproval => "user_approval",
            Self::ToolExecute => "tool_execute",
            Self::UpdateMemory => "update_memory",
            Self::Reconciliation => "reconciliation",
            Self::WaitingForUserInput => "waiting_for_user_input",
            Self::Done => "done",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Idle, Self::Planning)
                | (Self::Idle, Self::PlanApproval)
                | (Self::Idle, Self::BuildContext)
                | (Self::Idle, Self::Reconciliation)
                | (Self::Planning, Self::PlanApproval)
                | (Self::Planning, Self::Reconciliation)
                | (Self::PlanApproval, Self::Planning)
                | (Self::PlanApproval, Self::BuildContext)
                | (Self::PlanApproval, Self::Reconciliation)
                | (Self::BuildContext, Self::Compression)
                | (Self::BuildContext, Self::Reconciliation)
                | (Self::Compression, Self::InspectLlm)
                | (Self::Compression, Self::Reconciliation)
                | (Self::InspectLlm, Self::BuildContext)
                | (Self::InspectLlm, Self::UpdateMemory)
                | (Self::InspectLlm, Self::Reconciliation)
                | (Self::UpdateMemory, Self::UserApproval)
                | (Self::UpdateMemory, Self::BuildContext)
                | (Self::UpdateMemory, Self::Reconciliation)
                | (Self::UserApproval, Self::ToolExecute)
                | (Self::UserApproval, Self::BuildContext)
                | (Self::UserApproval, Self::Reconciliation)
                | (Self::ToolExecute, Self::BuildContext)
                | (Self::ToolExecute, Self::WaitingForUserInput)
                | (Self::ToolExecute, Self::Reconciliation)
                | (Self::WaitingForUserInput, Self::BuildContext)
                | (Self::WaitingForUserInput, Self::Reconciliation)
                | (Self::Reconciliation, Self::Planning)
                | (Self::Reconciliation, Self::BuildContext)
                | (Self::Reconciliation, Self::Done)
        )
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_accepts_diagram_order() {
        let states = [
            AgentState::Idle,
            AgentState::Planning,
            AgentState::PlanApproval,
            AgentState::BuildContext,
            AgentState::Compression,
            AgentState::InspectLlm,
            AgentState::UpdateMemory,
            AgentState::UserApproval,
            AgentState::ToolExecute,
            AgentState::Reconciliation,
            AgentState::Done,
        ];
        for pair in states.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "invalid transition: {} -> {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn lifecycle_rejects_execution_before_plan_approval() {
        assert!(!AgentState::Planning.can_transition_to(AgentState::ToolExecute));
        assert!(!AgentState::PlanApproval.can_transition_to(AgentState::ToolExecute));
    }

    #[test]
    fn lifecycle_accepts_only_safe_steering_reentry_boundaries() {
        assert!(AgentState::PlanApproval.can_transition_to(AgentState::Planning));
        assert!(AgentState::InspectLlm.can_transition_to(AgentState::BuildContext));
        assert!(AgentState::UpdateMemory.can_transition_to(AgentState::BuildContext));
        assert!(AgentState::Reconciliation.can_transition_to(AgentState::Planning));
        assert!(AgentState::Reconciliation.can_transition_to(AgentState::BuildContext));
        assert!(AgentState::UserApproval.can_transition_to(AgentState::BuildContext));
        assert!(!AgentState::WaitingForUserInput.can_transition_to(AgentState::Planning));
    }
}

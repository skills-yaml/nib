#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Planning,
    BuildContext,
    InspectLlm,
    ToolExecute,
    UpdateMemory,
    WaitingForUserInput,
    Done,
}

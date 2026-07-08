#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    BuildContext,
    InspectLlm,
    ToolExecute,
    UpdateMemory,
    Done,
}

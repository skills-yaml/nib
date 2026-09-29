use super::*;

#[derive(Default)]
pub(crate) struct ContextCapturingApprovalHandler {
    pub(crate) context: Mutex<Option<ApprovalContext>>,
}

#[async_trait::async_trait]
impl ApprovalHandler for ContextCapturingApprovalHandler {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied()
    }

    async fn handle_approval_with_context(
        &self,
        _call: &ToolCall,
        _level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        *self.context.lock().expect("context lock") = Some(context.clone());
        ApprovalDecision::granted_user()
    }
}

#[path = "test_part_0.rs"]
mod test_part_0;

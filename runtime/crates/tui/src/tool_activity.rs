#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Requested,
    WaitingConfirmation,
    Running,
    Completed,
    Failed,
    Cancelled,
    Partial,
    PolicyDenied,
}

impl ToolStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::WaitingConfirmation => "waiting confirmation",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Partial => "partial",
            Self::PolicyDenied => "policy denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActivity {
    pub call_id: String,
    pub name: String,
    pub status: ToolStatus,
    pub summary: String,
}

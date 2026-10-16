#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub session_id: String,
    pub created_at: String,
    pub task_summary: Option<String>,
    pub expiration_reason: Option<String>,
    pub event_count: u64,
    pub message_count: u64,
    pub expired: bool,
}

/// A destructive history command must be repeated verbatim before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryDestructiveAction {
    Delete(String),
    Clear,
}

//! FEAT-037 本机跨会话消息工具。
//!
//! 工具只负责参数校验、policy 投影和脱敏；寻址、入站模式和投递由宿主实现的
//! [`SessionMessenger`] 决定。接收方始终按自身 policy 执行，发起方无法借此
//! 获得接收方的任何权限。

use crate::{AgentError, RegisteredTool};
use async_trait::async_trait;
use rivo_core::ToolDefinition;
use rivo_policy::{Operation, PolicyProfile, ToolRequest};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// 单条消息正文上限，与 A2A 消息上限保持一致。
pub const MAX_SESSION_MESSAGE_BYTES: usize = 64 * 1024;
pub const SESSION_LIST_PEERS_TOOL: &str = "session_list_peers";
pub const SESSION_SEND_TOOL: &str = "session_send";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPeer {
    pub session_id: String,
    /// `idle` / `busy` / `awaiting_approval`。
    pub status: String,
    pub policy_profile: PolicyProfile,
    /// `held` / `auto` / `reject`。
    pub inbound: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMessageKind {
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSendRequest {
    pub from_session_id: String,
    pub from_policy_profile: PolicyProfile,
    pub to: String,
    pub kind: SessionMessageKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDelivery {
    Delivered,
    Queued,
    HeldForApproval,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendOutcome {
    pub message_id: Option<String>,
    pub delivery: SessionDelivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 由宿主（当前仅 app-server）实现的本机会话消息总线。
#[async_trait]
pub trait SessionMessenger: Send + Sync {
    async fn list_peers(&self, from_session_id: &str) -> Result<Vec<SessionPeer>, String>;
    async fn send(&self, request: SessionSendRequest) -> Result<SessionSendOutcome, String>;
}

pub(crate) struct SessionListPeersTool {
    pub(crate) messenger: Arc<dyn SessionMessenger>,
    pub(crate) from_session_id: String,
}

#[async_trait]
impl RegisteredTool for SessionListPeersTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SESSION_LIST_PEERS_TOOL.into(),
            description: "列出同一宿主进程中可接收消息的其他本机会话及其状态、policy 和入站模式。只读，不返回会话内容。".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        }
    }

    fn request(&self, _arguments: &Value) -> ToolRequest {
        ToolRequest::read(SESSION_LIST_PEERS_TOOL, ".")
    }

    async fn execute(&self, _arguments: Value) -> Result<Value, AgentError> {
        let peers = self
            .messenger
            .list_peers(&self.from_session_id)
            .await
            .map_err(AgentError::Tool)?;
        Ok(json!({ "peers": peers }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArguments {
    to: String,
    message: String,
    #[serde(default = "default_kind")]
    kind: SessionMessageKind,
}

fn default_kind() -> SessionMessageKind {
    SessionMessageKind::Note
}

fn parse_send(arguments: &Value) -> Result<SendArguments, AgentError> {
    let parsed: SendArguments = serde_json::from_value(arguments.clone())
        .map_err(|error| AgentError::Tool(format!("session_send 参数无效: {error}")))?;
    if !valid_session_ref(&parsed.to) {
        return Err(AgentError::Tool("session_send 目标会话 ID 无效".into()));
    }
    if parsed.message.trim().is_empty() {
        return Err(AgentError::Tool("session_send 消息不能为空".into()));
    }
    if parsed.message.len() > MAX_SESSION_MESSAGE_BYTES {
        return Err(AgentError::Tool("session_message_too_large".into()));
    }
    Ok(parsed)
}

pub fn valid_session_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn message_sha256(message: &str) -> String {
    hex::encode(Sha256::digest(message.as_bytes()))
}

pub(crate) struct SessionSendTool {
    pub(crate) messenger: Arc<dyn SessionMessenger>,
    pub(crate) from_session_id: String,
    pub(crate) from_policy_profile: PolicyProfile,
}

#[async_trait]
impl RegisteredTool for SessionSendTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SESSION_SEND_TOOL.into(),
            description: "向同一宿主进程中的另一个本机会话投递一条消息。接收方空闲时开始新 Turn，忙碌时排队；接收方可能要求其用户批准后才处理。接收方按自身 policy 执行，不能借此获得额外权限。".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["to", "message"],
                "properties": {
                    "to": {"type": "string", "minLength": 1, "maxLength": 128},
                    "message": {"type": "string", "minLength": 1, "maxLength": MAX_SESSION_MESSAGE_BYTES},
                    "kind": {"enum": ["note"], "default": "note"}
                }
            }),
        }
    }

    fn request(&self, arguments: &Value) -> ToolRequest {
        // 确认请求绑定目标与正文 hash，避免批准后被替换为其他内容。
        let summary = parse_send(arguments)
            .map(|parsed| {
                format!(
                    "to={} kind=note message_sha256={} message_bytes={}",
                    parsed.to,
                    message_sha256(&parsed.message),
                    parsed.message.len()
                )
            })
            .unwrap_or_else(|_| "session_send 无效参数".into());
        ToolRequest {
            tool_name: SESSION_SEND_TOOL.into(),
            operation: Operation::SessionMessaging,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: summary,
            binary: false,
            item_count: 1,
            network: false,
        }
    }

    fn event_arguments(&self, arguments: &Value) -> Value {
        match parse_send(arguments) {
            Ok(parsed) => json!({
                "to": parsed.to,
                "kind": "note",
                "message_sha256": message_sha256(&parsed.message),
                "message_bytes": parsed.message.len(),
            }),
            Err(_) => json!({"redacted": true, "invalid": true}),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<Value, AgentError> {
        let parsed = parse_send(&arguments)?;
        if parsed.to == self.from_session_id {
            return Err(AgentError::Tool("session_send 不能发送给当前会话".into()));
        }
        let outcome = self
            .messenger
            .send(SessionSendRequest {
                from_session_id: self.from_session_id.clone(),
                from_policy_profile: self.from_policy_profile,
                to: parsed.to,
                kind: parsed.kind,
                message: parsed.message,
            })
            .await
            .map_err(AgentError::Tool)?;
        serde_json::to_value(outcome).map_err(|error| AgentError::Tool(error.to_string()))
    }
}

/// 子代理不继承任何会话消息工具。
pub(crate) fn is_session_messaging_tool(name: &str) -> bool {
    matches!(name, SESSION_LIST_PEERS_TOOL | SESSION_SEND_TOOL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingMessenger {
        sent: Mutex<Vec<SessionSendRequest>>,
    }

    #[async_trait]
    impl SessionMessenger for RecordingMessenger {
        async fn list_peers(&self, from: &str) -> Result<Vec<SessionPeer>, String> {
            Ok(vec![SessionPeer {
                session_id: format!("peer-of-{from}"),
                status: "idle".into(),
                policy_profile: PolicyProfile::Medium,
                inbound: "held".into(),
                parent_session_id: None,
            }])
        }
        async fn send(&self, request: SessionSendRequest) -> Result<SessionSendOutcome, String> {
            self.sent.lock().unwrap().push(request);
            Ok(SessionSendOutcome {
                message_id: Some("msg-1".into()),
                delivery: SessionDelivery::HeldForApproval,
                reason: None,
            })
        }
    }

    fn send_tool(messenger: Arc<RecordingMessenger>) -> SessionSendTool {
        SessionSendTool {
            messenger,
            from_session_id: "session-a".into(),
            from_policy_profile: PolicyProfile::Medium,
        }
    }

    #[test]
    fn send_request_binds_target_and_body_hash_without_body() {
        let tool = send_tool(Arc::new(RecordingMessenger::default()));
        let arguments = json!({"to": "session-b", "message": "secret body"});
        let request = tool.request(&arguments);
        assert_eq!(request.operation, Operation::SessionMessaging);
        assert!(request.arguments_summary.contains("to=session-b"));
        assert!(request
            .arguments_summary
            .contains(&message_sha256("secret body")));
        assert!(!request.arguments_summary.contains("secret body"));
        let changed = tool.request(&json!({"to": "session-b", "message": "other"}));
        assert_ne!(request.arguments_summary, changed.arguments_summary);
        let projected = tool.event_arguments(&arguments).to_string();
        assert!(!projected.contains("secret body"));
    }

    #[tokio::test]
    async fn send_forwards_sender_identity_and_rejects_invalid_targets() {
        let messenger = Arc::new(RecordingMessenger::default());
        let tool = send_tool(messenger.clone());
        let result = tool
            .execute(json!({"to": "session-b", "message": "hello"}))
            .await
            .unwrap();
        assert_eq!(result["delivery"], "held_for_approval");
        let sent = messenger.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].from_session_id, "session-a");
        assert_eq!(sent[0].from_policy_profile, PolicyProfile::Medium);
        assert_eq!(sent[0].to, "session-b");

        for invalid in [
            json!({"to": "session-a", "message": "self"}),
            json!({"to": "../x", "message": "path"}),
            json!({"to": "session-b", "message": "  "}),
            json!({"to": "session-b", "message": "x".repeat(MAX_SESSION_MESSAGE_BYTES + 1)}),
            json!({"to": "session-b", "message": "x", "kind": "request"}),
        ] {
            assert!(tool.execute(invalid).await.is_err());
        }
        assert_eq!(messenger.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_peers_is_read_only() {
        let tool = SessionListPeersTool {
            messenger: Arc::new(RecordingMessenger::default()),
            from_session_id: "session-a".into(),
        };
        assert_eq!(tool.request(&json!({})).operation, Operation::Read);
        let result = tool.execute(json!({})).await.unwrap();
        assert_eq!(result["peers"][0]["session_id"], "peer-of-session-a");
    }
}

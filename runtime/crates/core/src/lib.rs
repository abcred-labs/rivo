use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Deployment location of a Sidecar. This is descriptive metadata only; callers
/// must use the advertised capabilities instead of inferring permissions from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarDeploymentType {
    Desktop,
    LocalWorker,
    Cloud,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRootBinding {
    pub workspace_id: String,
    /// Opaque binding identity. The absolute root path never crosses the wire.
    pub binding_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CapabilitySummary {
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostCapabilitySummary {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_concurrent_tasks: u32,
    pub max_output_bytes: u64,
    pub max_observation_bytes: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 1,
            max_output_bytes: 256 * 1024,
            max_observation_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TransportCapabilities {
    pub app_server: bool,
    pub instance_control: bool,
    pub event_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LifecycleCapabilities {
    pub restart: bool,
    pub cancellation: bool,
    pub task_leases: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarNodeContext {
    pub node_id: String,
    pub deployment_type: SidecarDeploymentType,
    pub runtime_version: String,
    pub platform: String,
    pub workspace_id: String,
    pub workspace_root_binding: WorkspaceRootBinding,
    pub policy_profile: String,
    pub model_capabilities: CapabilitySummary,
    pub tool_capabilities: CapabilitySummary,
    pub host_capabilities: HostCapabilitySummary,
    pub resource_limits: ResourceLimits,
    pub transport_capabilities: TransportCapabilities,
    pub lifecycle_capabilities: LifecycleCapabilities,
}

impl SidecarNodeContext {
    /// Validate the bounded, secret-free projection that may be sent to clients.
    pub fn validate_public(&self) -> Result<(), &'static str> {
        for value in [
            self.node_id.as_str(),
            self.runtime_version.as_str(),
            self.platform.as_str(),
            self.workspace_id.as_str(),
            self.policy_profile.as_str(),
            self.workspace_root_binding.workspace_id.as_str(),
            self.workspace_root_binding.binding_id.as_str(),
            self.workspace_root_binding.display_name.as_str(),
        ] {
            if value.is_empty()
                || value.len() > 256
                || value
                    .chars()
                    .any(|character| character == '\\' || character == '/')
            {
                return Err("invalid sidecar context identifier");
            }
        }
        if self.workspace_root_binding.workspace_id != self.workspace_id {
            return Err("workspace binding does not match context");
        }
        if self.resource_limits.max_concurrent_tasks == 0
            || self.resource_limits.max_output_bytes == 0
            || self.resource_limits.max_observation_bytes == 0
        {
            return Err("invalid sidecar resource limits");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub kind: String,
    pub size: u64,
    pub hash: String,
    pub expires_at: String,
    pub download_scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarTaskState {
    Accepted,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    UnknownSideEffect,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub session_id: String,
    pub next_sequence: u64,
}

/// Persisted lease metadata. It is deliberately free of credentials and paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarTaskLease {
    pub task_id: String,
    pub intent_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub epoch: u64,
    pub expires_at: String,
    pub state: SidecarTaskState,
    pub last_event_sequence: u64,
    pub terminal_reason: Option<String>,
}

/// 根 Turn 内的稳定子代理标识。它不等同于 SessionId、TurnId 或模型提供方的请求 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

/// 子代理任务标识。一个任务仅属于一个根 Turn，首期不支持递归委派。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentTaskId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// 回灌给根 Agent 的有界子代理结果。完整子代理上下文不会回传或持久化为
/// 根 Agent 的工具观察，避免把独立任务的历史再次注入根上下文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentResult {
    pub agent_id: AgentId,
    pub task_id: AgentTaskId,
    pub state: AgentTaskState,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    pub truncated: bool,
    pub usage: SubagentUsage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub steps: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Url { url: String },
    Base64 { media_type: String, data: String },
}

/// 图片能力是 Agent 工具和 Provider 媒体接口之间的稳定契约。它不包含
/// Provider 密钥、签名 URL 或原始图片字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaCapability {
    /// Whether the active route accepts user supplied image inputs.
    #[serde(default)]
    pub input: bool,
    #[serde(default)]
    pub generate: bool,
    #[serde(default)]
    pub edit: bool,
    #[serde(default = "default_media_max_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_media_max_bytes")]
    pub max_output_bytes: u64,
}

impl Default for MediaCapability {
    fn default() -> Self {
        Self {
            input: false,
            generate: false,
            edit: false,
            max_input_bytes: default_media_max_bytes(),
            max_output_bytes: default_media_max_bytes(),
        }
    }
}

fn default_media_max_bytes() -> u64 {
    20 * 1024 * 1024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaOperation {
    Generate,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub asset_id: String,
    pub session_id: String,
    pub media_type: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaDelivery {
    LocalPath { path: String },
    SignedUrl { url: String, expires_at: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRequest {
    pub operation: MediaOperation,
    pub prompt: String,
    #[serde(default)]
    pub input_assets: Vec<MediaRef>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub output_media_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaResult {
    pub asset: MediaRef,
    pub delivery: MediaDelivery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: Vec<ContentPart>,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceSource {
    LocalFile {
        path: String,
        line_start: Option<u32>,
        line_end: Option<u32>,
    },
    Web {
        url: String,
        retrieved_at: String,
    },
    Mcp {
        server: String,
        uri: String,
        mime_type: Option<String>,
    },
    UserProvided {
        label: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub id: String,
    pub source: EvidenceSource,
    pub excerpt: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportClaim {
    pub text: String,
    pub evidence_ids: Vec<String>,
    #[serde(default)]
    pub uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResearchReport {
    pub summary: String,
    pub background: String,
    pub evidence: Vec<EvidenceRef>,
    pub claims: Vec<ReportClaim>,
    pub options: Vec<String>,
    pub risks: Vec<String>,
    pub recommendations: Vec<String>,
    pub validation_plan: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvidenceError {
    #[error("证据 ID 重复: {0}")]
    DuplicateEvidence(String),
    #[error("结论缺少证据且未标记为不确定: {0}")]
    MissingEvidence(String),
    #[error("结论引用了不存在的证据 {evidence_id}: {claim}")]
    UnknownEvidence { claim: String, evidence_id: String },
}

impl ResearchReport {
    pub fn validate(&self) -> Result<(), EvidenceError> {
        let mut ids = HashSet::new();
        for evidence in &self.evidence {
            if !ids.insert(evidence.id.as_str()) {
                return Err(EvidenceError::DuplicateEvidence(evidence.id.clone()));
            }
        }
        for claim in &self.claims {
            if claim.evidence_ids.is_empty() && !claim.uncertain {
                return Err(EvidenceError::MissingEvidence(claim.text.clone()));
            }
            for evidence_id in &claim.evidence_ids {
                if !ids.contains(evidence_id.as_str()) {
                    return Err(EvidenceError::UnknownEvidence {
                        claim: claim.text.clone(),
                        evidence_id: evidence_id.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn compact_evidence(&mut self, max_excerpt_chars: usize) {
        for evidence in &mut self.evidence {
            if evidence.excerpt.chars().count() > max_excerpt_chars {
                evidence.excerpt = evidence.excerpt.chars().take(max_excerpt_chars).collect();
                evidence.truncated = true;
            }
        }
    }

    pub fn evidence_map(&self) -> HashMap<&str, &EvidenceRef> {
        self.evidence
            .iter()
            .map(|evidence| (evidence.id.as_str(), evidence))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_context_is_secret_free_and_workspace_bound() {
        let context = SidecarNodeContext {
            node_id: "node-1".into(),
            deployment_type: SidecarDeploymentType::LocalWorker,
            runtime_version: "0.1.0".into(),
            platform: "windows-x86_64".into(),
            workspace_id: "workspace-1".into(),
            workspace_root_binding: WorkspaceRootBinding {
                workspace_id: "workspace-1".into(),
                binding_id: "binding-1".into(),
                display_name: "repo".into(),
            },
            policy_profile: "workspace_write".into(),
            model_capabilities: CapabilitySummary {
                capabilities: vec!["text".into()],
            },
            tool_capabilities: CapabilitySummary::default(),
            host_capabilities: HostCapabilitySummary::default(),
            resource_limits: ResourceLimits::default(),
            transport_capabilities: TransportCapabilities {
                app_server: true,
                instance_control: true,
                event_replay: true,
            },
            lifecycle_capabilities: LifecycleCapabilities {
                restart: true,
                cancellation: true,
                task_leases: true,
            },
        };
        assert_eq!(context.validate_public(), Ok(()));
        assert!(serde_json::to_string(&context)
            .unwrap()
            .contains("workspace-1"));
        assert!(!serde_json::to_string(&context).unwrap().contains("C:\\"));
    }

    #[test]
    fn sidecar_context_rejects_path_like_identifiers() {
        let mut context = SidecarNodeContext {
            node_id: "node-1".into(),
            deployment_type: SidecarDeploymentType::Desktop,
            runtime_version: "0.1.0".into(),
            platform: "windows".into(),
            workspace_id: "workspace-1".into(),
            workspace_root_binding: WorkspaceRootBinding {
                workspace_id: "workspace-1".into(),
                binding_id: "binding-1".into(),
                display_name: "repo".into(),
            },
            policy_profile: "read_only".into(),
            model_capabilities: CapabilitySummary::default(),
            tool_capabilities: CapabilitySummary::default(),
            host_capabilities: HostCapabilitySummary::default(),
            resource_limits: ResourceLimits::default(),
            transport_capabilities: TransportCapabilities::default(),
            lifecycle_capabilities: LifecycleCapabilities::default(),
        };
        context.workspace_id = "C:\\workspace".into();
        assert!(context.validate_public().is_err());
    }

    #[test]
    fn research_claims_must_reference_known_evidence() {
        let mut report = ResearchReport {
            evidence: vec![EvidenceRef {
                id: "e1".into(),
                source: EvidenceSource::LocalFile {
                    path: "README.md".into(),
                    line_start: Some(1),
                    line_end: Some(3),
                },
                excerpt: "evidence".into(),
                truncated: false,
            }],
            claims: vec![ReportClaim {
                text: "claim".into(),
                evidence_ids: vec!["e1".into()],
                uncertain: false,
            }],
            ..ResearchReport::default()
        };
        assert_eq!(report.validate(), Ok(()));

        report.claims[0].evidence_ids = vec!["missing".into()];
        assert!(matches!(
            report.validate(),
            Err(EvidenceError::UnknownEvidence { .. })
        ));
    }

    #[test]
    fn uncertain_claims_may_be_unreferenced() {
        let report = ResearchReport {
            claims: vec![ReportClaim {
                text: "unknown".into(),
                evidence_ids: vec![],
                uncertain: true,
            }],
            ..ResearchReport::default()
        };
        assert_eq!(report.validate(), Ok(()));
    }

    #[test]
    fn compression_preserves_evidence_identity_and_locator() {
        let mut report = ResearchReport {
            evidence: vec![EvidenceRef {
                id: "e1".into(),
                source: EvidenceSource::Web {
                    url: "https://example.com".into(),
                    retrieved_at: "2026-07-16T00:00:00Z".into(),
                },
                excerpt: "abcdefgh".into(),
                truncated: false,
            }],
            ..ResearchReport::default()
        };
        report.compact_evidence(4);
        assert_eq!(report.evidence[0].id, "e1");
        assert_eq!(report.evidence[0].excerpt, "abcd");
        assert!(report.evidence[0].truncated);
    }
}

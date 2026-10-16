use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyProfile {
    Strict,
    #[serde(alias = "workspace")]
    #[default]
    Medium,
    FullControl,
}

impl Display for PolicyProfile {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Strict => "strict",
            Self::Medium => "medium",
            Self::FullControl => "full_control",
        })
    }
}

impl PolicyProfile {
    /// 源码级兼容别名；序列化与新界面始终使用 `medium`。
    #[allow(non_upper_case_globals)]
    pub const Workspace: Self = Self::Medium;
}

impl FromStr for PolicyProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "strict" => Ok(Self::Strict),
            "medium" | "workspace" => Ok(Self::Medium),
            "full" | "full_control" => Ok(Self::FullControl),
            _ => Err(format!("不支持的 policy profile: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProfile {
    Research,
    Plan,
    Coding,
    Review,
    #[default]
    General,
    /// 仅用于读取 V1 早期配置。新的 MCP 授权由每个 Turn 的 server/toolset 决定。
    Mcp,
}

impl Display for CapabilityProfile {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Research => "research",
            Self::Plan => "plan",
            Self::Coding => "coding",
            Self::Review => "review",
            Self::General => "general",
            Self::Mcp => "mcp",
        })
    }
}

impl FromStr for CapabilityProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "research" => Ok(Self::Research),
            "plan" => Ok(Self::Plan),
            "coding" => Ok(Self::Coding),
            "review" => Ok(Self::Review),
            "general" => Ok(Self::General),
            "mcp" => Ok(Self::General),
            _ => Err(format!("不支持的 capability profile: {value}")),
        }
    }
}

/// 将反序列化旧配置得到的 `mcp` profile 收口为普通 general mode。
pub fn normalize_capability_profile(profile: CapabilityProfile) -> CapabilityProfile {
    match profile {
        CapabilityProfile::Mcp => CapabilityProfile::General,
        profile => profile,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    TextPatch,
    Write,
    Move,
    Delete,
    Process,
    Shell,
    Network,
    Browser,
    Computer,
    Mcp,
    /// 已由本机 allowlist 标记为只读的 MCP tool/resource。
    McpRead,
    /// 调用图片生成/编辑 Provider，并可能产生费用或外发图片内容。
    Media,
    /// 向已配对的 A2A peer 发送业务数据。它不是普通 Web fetch：严格模式
    /// 必须拒绝，其他模式也必须将精确的 peer/skill/内容 hash 交由本机确认。
    A2aDelegation,
    /// FEAT-037：向本机另一个 Session 投递消息或编排会话。接收方始终按自身
    /// policy 执行，因此该操作只授权“投递”，不授予接收方任何额外权限。
    SessionMessaging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathScope {
    #[default]
    WorkspaceOnly,
    FullControl,
}

/// 网络限制是 policy 的输出，而不是 Web/MCP 工具自行决定的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkScope {
    #[default]
    PublicHttpsOnly,
    Unrestricted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionConstraints {
    pub workspace_root: PathBuf,
    /// 只读工具与有副作用工具必须使用不同的解析器，避免 medium 的
    /// 任意本机读取能力外溢为工作区外写入权限。
    pub read_path_scope: PathScope,
    pub mutate_path_scope: PathScope,
    pub timeout_ms: u64,
    pub output_limit: usize,
    pub network_scope: NetworkScope,
    pub redact_environment: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub request_id: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub side_effects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow {
        constraints: ExecutionConstraints,
    },
    Deny {
        reason: String,
    },
    Confirm {
        request: ConfirmRequest,
        constraints: ExecutionConstraints,
    },
    Constrain {
        reason: String,
        constraints: ExecutionConstraints,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub tool_name: String,
    pub operation: Operation,
    pub targets: Vec<PathBuf>,
    pub program: Option<String>,
    pub args: Vec<String>,
    pub arguments_summary: String,
    pub binary: bool,
    pub item_count: usize,
    pub network: bool,
}

impl ToolRequest {
    pub fn read(tool_name: impl Into<String>, target: impl Into<PathBuf>) -> Self {
        Self {
            tool_name: tool_name.into(),
            operation: Operation::Read,
            targets: vec![target.into()],
            program: None,
            args: vec![],
            arguments_summary: "只读访问".into(),
            binary: false,
            item_count: 1,
            network: false,
        }
    }

    pub fn media(tool_name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            operation: Operation::Media,
            targets: Vec::new(),
            program: None,
            args: Vec::new(),
            arguments_summary: summary.into(),
            binary: true,
            item_count: 1,
            network: true,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("无法确定工作区路径: {0}")]
    InvalidWorkspace(String),
    #[error("目标路径越出工作区: {0}")]
    WorkspaceEscape(String),
}

#[derive(Debug, Clone)]
pub struct PolicyEvaluator {
    workspace_root: PathBuf,
    canonical_workspace: PathBuf,
    default_timeout_ms: u64,
    default_output_limit: usize,
}

impl PolicyEvaluator {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let workspace_root = absolute_lexical(workspace_root.as_ref()).map_err(|error| {
            PolicyError::InvalidWorkspace(format!("{}: {error}", workspace_root.as_ref().display()))
        })?;
        let canonical_workspace = workspace_root.canonicalize().map_err(|error| {
            PolicyError::InvalidWorkspace(format!("{}: {error}", workspace_root.display()))
        })?;
        Ok(Self {
            workspace_root,
            canonical_workspace,
            default_timeout_ms: 120_000,
            default_output_limit: 1_048_576,
        })
    }

    pub fn evaluate(
        &self,
        profile: PolicyProfile,
        capability: CapabilityProfile,
        request: &ToolRequest,
    ) -> PolicyDecision {
        if let Err(error) =
            self.validate_targets(profile, capability, request.operation, &request.targets)
        {
            return PolicyDecision::Deny {
                reason: error.to_string(),
            };
        }

        let constraints = ExecutionConstraints {
            workspace_root: self.workspace_root.clone(),
            read_path_scope: if matches!(capability, CapabilityProfile::Plan) {
                PathScope::WorkspaceOnly
            } else if matches!(profile, PolicyProfile::Medium | PolicyProfile::FullControl) {
                PathScope::FullControl
            } else {
                PathScope::WorkspaceOnly
            },
            mutate_path_scope: if matches!(profile, PolicyProfile::FullControl) {
                PathScope::FullControl
            } else {
                PathScope::WorkspaceOnly
            },
            timeout_ms: self.default_timeout_ms,
            output_limit: self.default_output_limit,
            network_scope: if matches!(profile, PolicyProfile::FullControl)
                && !matches!(capability, CapabilityProfile::Plan)
            {
                NetworkScope::Unrestricted
            } else {
                NetworkScope::PublicHttpsOnly
            },
            redact_environment: true,
        };

        // Plan 的能力边界独立于 permission ceiling：即使宿主给出 full_access，
        // 该模式也只允许本地读取、已登记的只读 MCP 和显式 HTTPS 读取。
        if matches!(capability, CapabilityProfile::Plan)
            && !matches!(
                request.operation,
                Operation::Read | Operation::McpRead | Operation::Network
            )
        {
            return PolicyDecision::Deny {
                reason: "plan capability 只允许只读调查工具".into(),
            };
        }

        // A2A 委派是跨信任边界的数据外流，不能被 Network 或 full_control 的
        // 一般语义放宽。ToolRequest 的 arguments_summary 必须包含 peer、skill、
        // 摘要和所有正文 hash，因此 ConfirmRequest 的 request_id 绑定精确内容。
        if matches!(request.operation, Operation::A2aDelegation) {
            return match profile {
                PolicyProfile::Strict => PolicyDecision::Deny {
                    reason: "严格模式不允许 A2A 数据外流".into(),
                },
                PolicyProfile::Medium | PolicyProfile::FullControl => {
                    self.confirm(request, constraints, "向已配对 A2A peer 外发业务数据")
                }
            };
        }

        if matches!(request.operation, Operation::SessionMessaging) {
            return match profile {
                PolicyProfile::Strict => PolicyDecision::Deny {
                    reason: "严格模式不允许向其他会话发送消息".into(),
                },
                PolicyProfile::Medium => {
                    self.confirm(request, constraints, "向本机其他会话投递消息")
                }
                PolicyProfile::FullControl => PolicyDecision::Allow { constraints },
            };
        }

        if matches!(request.operation, Operation::Media) {
            return match profile {
                PolicyProfile::Strict => PolicyDecision::Deny {
                    reason: "严格模式不允许图片生成或编辑".into(),
                },
                PolicyProfile::Medium | PolicyProfile::FullControl => self.confirm(
                    request,
                    constraints,
                    "调用图片 Provider，可能产生费用并外发媒体内容",
                ),
            };
        }

        match profile {
            PolicyProfile::Strict => match request.operation {
                Operation::Read | Operation::McpRead | Operation::Network => {
                    PolicyDecision::Allow { constraints }
                }
                _ => PolicyDecision::Deny {
                    reason: "严格模式只允许工作区内只读和显式启用的安全 Web 工具".into(),
                },
            },
            PolicyProfile::Medium => self.evaluate_medium(capability, request, constraints),
            PolicyProfile::FullControl => self.evaluate_full_control(request, constraints),
        }
    }

    pub fn request_id(&self, request: &ToolRequest) -> String {
        let encoded = serde_json::to_vec(request).expect("ToolRequest is serializable");
        hex::encode(Sha256::digest(encoded))
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn confirmation_matches(&self, request: &ToolRequest, request_id: &str) -> bool {
        self.request_id(request) == request_id
    }

    fn evaluate_medium(
        &self,
        capability: CapabilityProfile,
        request: &ToolRequest,
        constraints: ExecutionConstraints,
    ) -> PolicyDecision {
        if matches!(
            request.operation,
            Operation::TextPatch
                | Operation::Write
                | Operation::Move
                | Operation::Delete
                | Operation::Process
                | Operation::Shell
        ) && !matches!(capability, CapabilityProfile::Coding)
        {
            return PolicyDecision::Deny {
                reason: "当前能力 profile 未启用编码或进程操作".into(),
            };
        }
        match request.operation {
            Operation::Read | Operation::McpRead => PolicyDecision::Allow { constraints },
            Operation::Browser | Operation::Mcp | Operation::Network => {
                PolicyDecision::Allow { constraints }
            }
            Operation::Computer => {
                if request
                    .args
                    .iter()
                    .any(|arg| arg == "computer_read_only=true")
                {
                    PolicyDecision::Allow { constraints }
                } else {
                    self.confirm(request, constraints, "桌面输入或窗口状态改变需要逐次确认")
                }
            }
            Operation::Write
            | Operation::Move
            | Operation::Delete
            | Operation::Process
            | Operation::Shell
            | Operation::TextPatch => self.confirm(request, constraints, "操作具有显著副作用"),
            // `evaluate` 的前置分支已经为该操作固定了 A2A 专用提示；保留本
            // 分支使 Operation 扩展保持穷尽，且在未来重构时不意外降级为 Allow。
            Operation::A2aDelegation => {
                self.confirm(request, constraints, "向已配对 A2A peer 外发业务数据")
            }
            Operation::Media => self.confirm(
                request,
                constraints,
                "调用图片 Provider，可能产生费用并外发媒体内容",
            ),
            Operation::SessionMessaging => {
                self.confirm(request, constraints, "向本机其他会话投递消息")
            }
        }
    }

    fn evaluate_full_control(
        &self,
        request: &ToolRequest,
        constraints: ExecutionConstraints,
    ) -> PolicyDecision {
        // 原始 Shell 的解释器会赋予单个字符串完整的命令组合能力。即使用户
        // 选择 full_control，也必须让确认请求绑定命令正文，不能把该能力变成
        // 无人值守执行入口。
        if matches!(request.operation, Operation::Shell) {
            self.confirm(request, constraints, "原始 Shell 命令需要逐次确认")
        } else {
            PolicyDecision::Allow { constraints }
        }
    }

    fn confirm(
        &self,
        request: &ToolRequest,
        constraints: ExecutionConstraints,
        side_effect: &str,
    ) -> PolicyDecision {
        PolicyDecision::Confirm {
            request: ConfirmRequest {
                request_id: self.request_id(request),
                tool_name: request.tool_name.clone(),
                arguments_summary: request.arguments_summary.clone(),
                side_effects: vec![side_effect.into()],
            },
            constraints,
        }
    }

    fn validate_targets(
        &self,
        profile: PolicyProfile,
        capability: CapabilityProfile,
        operation: Operation,
        targets: &[PathBuf],
    ) -> Result<(), PolicyError> {
        if !matches!(capability, CapabilityProfile::Plan)
            && (matches!(profile, PolicyProfile::FullControl)
                || (matches!(profile, PolicyProfile::Medium)
                    && matches!(operation, Operation::Read)))
        {
            return Ok(());
        }
        for target in targets {
            let lexical = if target.is_absolute() {
                normalize_lexical(target)?
            } else {
                normalize_lexical(&self.workspace_root.join(target))?
            };
            if !lexical.starts_with(&self.workspace_root) {
                return Err(PolicyError::WorkspaceEscape(target.display().to_string()));
            }
            if let Some(existing) = nearest_existing_path(&lexical) {
                let canonical = existing
                    .canonicalize()
                    .map_err(|_| PolicyError::WorkspaceEscape(target.display().to_string()))?;
                if !canonical.starts_with(&self.canonical_workspace) {
                    return Err(PolicyError::WorkspaceEscape(target.display().to_string()));
                }
            }
        }
        Ok(())
    }
}

/// 按 policy 返回的路径作用域解析路径。相对路径始终以 workspace 为基准。
pub fn resolve_read_path(
    constraints: &ExecutionConstraints,
    path: &Path,
) -> Result<PathBuf, PolicyError> {
    let resolved = if path.is_absolute() {
        normalize_lexical(path)?
    } else {
        normalize_lexical(&constraints.workspace_root.join(path))?
    };
    if matches!(constraints.read_path_scope, PathScope::WorkspaceOnly) {
        if !resolved.starts_with(&constraints.workspace_root) {
            return Err(PolicyError::WorkspaceEscape(path.display().to_string()));
        }
        let canonical_workspace = constraints
            .workspace_root
            .canonicalize()
            .map_err(|error| PolicyError::InvalidWorkspace(error.to_string()))?;
        if let Some(existing) = nearest_existing_path(&resolved) {
            let canonical = existing
                .canonicalize()
                .map_err(|_| PolicyError::WorkspaceEscape(path.display().to_string()))?;
            if !canonical.starts_with(canonical_workspace) {
                return Err(PolicyError::WorkspaceEscape(path.display().to_string()));
            }
        }
    }
    Ok(resolved)
}

/// 解析可能改变系统状态的路径（写入、移动、删除或进程 cwd）。
pub fn resolve_mutate_path(
    constraints: &ExecutionConstraints,
    path: &Path,
) -> Result<PathBuf, PolicyError> {
    let resolved = if path.is_absolute() {
        normalize_lexical(path)?
    } else {
        normalize_lexical(&constraints.workspace_root.join(path))?
    };
    if matches!(constraints.mutate_path_scope, PathScope::WorkspaceOnly) {
        if !resolved.starts_with(&constraints.workspace_root) {
            return Err(PolicyError::WorkspaceEscape(path.display().to_string()));
        }
        let canonical_workspace = constraints
            .workspace_root
            .canonicalize()
            .map_err(|error| PolicyError::InvalidWorkspace(error.to_string()))?;
        if let Some(existing) = nearest_existing_path(&resolved) {
            let canonical = existing
                .canonicalize()
                .map_err(|_| PolicyError::WorkspaceEscape(path.display().to_string()))?;
            if !canonical.starts_with(canonical_workspace) {
                return Err(PolicyError::WorkspaceEscape(path.display().to_string()));
            }
        }
    }
    Ok(resolved)
}

pub fn process_is_allowlisted(program: &str, args: &[String]) -> bool {
    let name = Path::new(program)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    match name.as_str() {
        "cargo" => {
            matches!(
                args.first().map(String::as_str),
                Some("check" | "test" | "build" | "clippy")
            ) || (args.first().map(String::as_str) == Some("fmt")
                && args.iter().any(|arg| arg == "--check"))
        }
        "git" => matches!(args.first().map(String::as_str), Some("status" | "diff")),
        "rg" | "ripgrep" => true,
        _ => false,
    }
}

/// 会话授权只覆盖中等模式中的两个有界风险类别。一次性确认仍严格使用
/// request_id（包含完整、经脱敏的请求绑定）。
pub fn session_grant_key(request: &ToolRequest) -> Option<&'static str> {
    match request.operation {
        Operation::TextPatch | Operation::Write | Operation::Move | Operation::Delete => {
            Some("workspace_file_mutation")
        }
        Operation::Process | Operation::Shell => Some("workspace_process_execution"),
        _ => None,
    }
}

fn absolute_lexical(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        normalize_lexical(path).map_err(std::io::Error::other)
    } else {
        normalize_lexical(&std::env::current_dir()?.join(path)).map_err(std::io::Error::other)
    }
}

fn normalize_lexical(path: &Path) -> Result<PathBuf, PolicyError> {
    let mut prefix: Option<OsString> = None;
    let mut rooted = false;
    let mut parts: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_os_string()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(PolicyError::WorkspaceEscape(path.display().to_string()));
                }
            }
            Component::Normal(value) => parts.push(value.to_os_string()),
        }
    }
    let mut result = PathBuf::new();
    if let Some(prefix) = prefix {
        result.push(prefix);
    }
    if rooted {
        result.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    result.extend(parts);
    Ok(result)
}

fn nearest_existing_path(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_request(program: &str, args: &[&str]) -> ToolRequest {
        ToolRequest {
            tool_name: "run_process".into(),
            operation: Operation::Process,
            targets: vec![PathBuf::from(".")],
            program: Some(program.into()),
            args: args.iter().map(|value| (*value).into()).collect(),
            arguments_summary: format!("{program} {}", args.join(" ")),
            binary: false,
            item_count: 1,
            network: false,
        }
    }

    #[test]
    fn strict_rejects_path_escape() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let request = ToolRequest::read("read_file", "../secret.txt");
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Research, &request),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn medium_confirms_mutating_processes() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let cargo = process_request("cargo", &["test"]);
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Coding, &cargo),
            PolicyDecision::Confirm { .. }
        ));

        let shell = ToolRequest {
            operation: Operation::Shell,
            ..process_request("powershell", &["Remove-Item", "x"])
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Coding, &shell),
            PolicyDecision::Confirm { .. }
        ));
    }

    #[test]
    fn confirmations_are_bound_to_exact_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let request = process_request("npm", &["install"]);
        let request_id = policy.request_id(&request);
        assert!(policy.confirmation_matches(&request, &request_id));
        let changed = process_request("npm", &["install", "other-package"]);
        assert!(!policy.confirmation_matches(&changed, &request_id));
    }

    #[test]
    fn strict_mode_only_allows_reads() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let read = ToolRequest::read("read_file", "README.md");
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Research, &read),
            PolicyDecision::Allow { .. }
        ));
        let process = process_request("cargo", &["check"]);
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Coding, &process),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn strict_allows_explicit_safe_web_reads_but_not_browser_or_mcp() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let web = ToolRequest {
            tool_name: "web_fetch".into(),
            operation: Operation::Network,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "https://example.com".into(),
            binary: false,
            item_count: 1,
            network: true,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Research, &web),
            PolicyDecision::Allow { .. }
        ));
        let mcp_read = ToolRequest {
            tool_name: "mcp__server__read".into(),
            operation: Operation::McpRead,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "read-only MCP query".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Plan, &mcp_read),
            PolicyDecision::Allow { .. }
        ));
        let mcp_action = ToolRequest {
            operation: Operation::Mcp,
            ..mcp_read.clone()
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Plan, &mcp_action),
            PolicyDecision::Deny { .. }
        ));
        for operation in [
            Operation::Write,
            Operation::Process,
            Operation::Shell,
            Operation::Browser,
            Operation::Computer,
            Operation::Mcp,
            Operation::Media,
            Operation::A2aDelegation,
            Operation::SessionMessaging,
        ] {
            let request = ToolRequest {
                operation,
                ..mcp_read.clone()
            };
            assert!(matches!(
                policy.evaluate(
                    PolicyProfile::FullControl,
                    CapabilityProfile::Plan,
                    &request
                ),
                PolicyDecision::Deny { .. }
            ));
        }
    }

    #[test]
    fn plan_read_scope_remains_workspace_only_under_full_control() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest::read("read_file", outside.path());
        assert!(matches!(
            policy.evaluate(
                PolicyProfile::FullControl,
                CapabilityProfile::Plan,
                &request
            ),
            PolicyDecision::Deny { .. }
        ));
        let inside = ToolRequest::read("read_file", "README.md");
        assert!(matches!(
            policy.evaluate(PolicyProfile::FullControl, CapabilityProfile::Plan, &inside),
            PolicyDecision::Allow { constraints }
                if constraints.read_path_scope == PathScope::WorkspaceOnly
        ));
    }

    #[test]
    fn plan_network_scope_remains_restricted_under_full_control() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest {
            tool_name: "web_fetch".into(),
            operation: Operation::Network,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "https://example.com".into(),
            binary: false,
            item_count: 1,
            network: true,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::FullControl, CapabilityProfile::Plan, &request),
            PolicyDecision::Allow { constraints }
                if constraints.network_scope == NetworkScope::PublicHttpsOnly
        ));
    }

    #[test]
    fn medium_allows_external_read_but_denies_external_mutation() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        assert!(matches!(
            policy.evaluate(
                PolicyProfile::Medium,
                CapabilityProfile::Research,
                &ToolRequest::read("read_file", outside.path())
            ),
            PolicyDecision::Allow { .. }
        ));
        let write = ToolRequest {
            tool_name: "write_file".into(),
            operation: Operation::Write,
            targets: vec![outside.path().to_path_buf()],
            program: None,
            args: vec![],
            arguments_summary: "write outside".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Coding, &write),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn full_control_never_requires_confirmation_for_registered_operations() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let process = process_request("powershell", &["-Command", "Get-Date"]);
        assert!(matches!(
            policy.evaluate(
                PolicyProfile::FullControl,
                CapabilityProfile::Coding,
                &process
            ),
            PolicyDecision::Allow { .. }
        ));
    }

    #[test]
    fn full_control_still_confirms_raw_shell() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let shell = ToolRequest {
            operation: Operation::Shell,
            ..process_request("sh", &["-c", "echo safe"])
        };
        assert!(matches!(
            policy.evaluate(
                PolicyProfile::FullControl,
                CapabilityProfile::Coding,
                &shell
            ),
            PolicyDecision::Confirm { .. }
        ));
    }

    #[test]
    fn a2a_delegation_is_denied_in_strict_and_confirmed_in_every_other_profile() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest {
            tool_name: "a2a_delegate".into(),
            operation: Operation::A2aDelegation,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "peer=peer-1 skill=research message_sha256=abc artifacts=[]".into(),
            binary: false,
            item_count: 1,
            network: true,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Research, &request),
            PolicyDecision::Deny { .. }
        ));
        for profile in [PolicyProfile::Medium, PolicyProfile::FullControl] {
            let PolicyDecision::Confirm {
                request: confirmation,
                ..
            } = policy.evaluate(profile, CapabilityProfile::Research, &request)
            else {
                panic!("A2A delegation must require explicit confirmation");
            };
            assert_eq!(confirmation.request_id, policy.request_id(&request));
        }
    }

    #[test]
    fn session_messaging_is_denied_in_strict_confirmed_in_medium_and_allowed_in_full_control() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest {
            tool_name: "session_send".into(),
            operation: Operation::SessionMessaging,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "to=session-2 kind=note message_sha256=abc".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        for capability in [CapabilityProfile::Research, CapabilityProfile::Coding] {
            assert!(matches!(
                policy.evaluate(PolicyProfile::Strict, capability, &request),
                PolicyDecision::Deny { .. }
            ));
            let PolicyDecision::Confirm {
                request: confirmation,
                ..
            } = policy.evaluate(PolicyProfile::Medium, capability, &request)
            else {
                panic!("medium session messaging must require confirmation");
            };
            assert_eq!(confirmation.request_id, policy.request_id(&request));
            assert!(matches!(
                policy.evaluate(PolicyProfile::FullControl, capability, &request),
                PolicyDecision::Allow { .. }
            ));
        }
    }

    #[test]
    fn browser_is_explicitly_allowed_outside_strict_without_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let browser = ToolRequest {
            tool_name: "browser_click".into(),
            operation: Operation::Browser,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "browser action".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::General, &browser),
            PolicyDecision::Allow { .. }
        ));
        assert!(matches!(
            policy.evaluate(
                PolicyProfile::FullControl,
                CapabilityProfile::General,
                &browser
            ),
            PolicyDecision::Allow { .. }
        ));
        assert!(matches!(
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::General, &browser),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn research_profile_cannot_escalate_to_write_by_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(temp.path()).unwrap();
        let request = ToolRequest {
            tool_name: "apply_patch".into(),
            operation: Operation::TextPatch,
            targets: vec![PathBuf::from("README.md")],
            program: None,
            args: vec![],
            arguments_summary: "edit".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Research, &request),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn medium_allows_explicit_mcp_access() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest {
            tool_name: "mcp__server__tool".into(),
            operation: Operation::Mcp,
            targets: vec![],
            program: None,
            args: vec![],
            arguments_summary: "mcp".into(),
            binary: false,
            item_count: 1,
            network: false,
        };
        assert!(matches!(
            policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Research, &request),
            PolicyDecision::Allow { .. }
        ));
    }

    #[test]
    fn full_control_returns_a_full_control_path_scope() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside").unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest::read("read_file", outside.path());
        let PolicyDecision::Allow { constraints } = policy.evaluate(
            PolicyProfile::FullControl,
            CapabilityProfile::Research,
            &request,
        ) else {
            panic!("full control should allow read")
        };
        assert_eq!(constraints.read_path_scope, PathScope::FullControl);
        assert_eq!(
            resolve_read_path(&constraints, outside.path()).unwrap(),
            outside.path()
        );
    }

    #[test]
    fn compatibility_aliases_allowlist_and_session_grants_are_explicit() {
        assert_eq!(
            "workspace".parse::<PolicyProfile>().unwrap(),
            PolicyProfile::Medium
        );
        assert_eq!(
            "full".parse::<PolicyProfile>().unwrap(),
            PolicyProfile::FullControl
        );
        assert!("unsafe".parse::<PolicyProfile>().is_err());
        assert_eq!(
            "mcp".parse::<CapabilityProfile>().unwrap(),
            CapabilityProfile::General
        );
        assert_eq!(
            normalize_capability_profile(CapabilityProfile::Mcp),
            CapabilityProfile::General
        );
        assert!("unknown".parse::<CapabilityProfile>().is_err());

        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<String>>()
        };
        assert!(process_is_allowlisted("cargo", &strings(&["check"])));
        assert!(process_is_allowlisted(
            "cargo.exe",
            &strings(&["fmt", "--check"])
        ));
        assert!(!process_is_allowlisted("cargo", &strings(&["fmt"])));
        assert!(process_is_allowlisted("git", &strings(&["diff"])));
        assert!(!process_is_allowlisted("git", &strings(&["commit"])));
        assert!(process_is_allowlisted("ripgrep", &strings(&["todo"])));
        assert!(!process_is_allowlisted(
            "powershell",
            &strings(&["Get-Date"])
        ));

        for operation in [
            Operation::TextPatch,
            Operation::Write,
            Operation::Move,
            Operation::Delete,
        ] {
            let request = ToolRequest {
                operation,
                ..ToolRequest::read("write_file", "file.txt")
            };
            assert_eq!(session_grant_key(&request), Some("workspace_file_mutation"));
        }
        for operation in [Operation::Process, Operation::Shell] {
            let request = ToolRequest {
                operation,
                ..ToolRequest::read("run_process", "file.txt")
            };
            assert_eq!(
                session_grant_key(&request),
                Some("workspace_process_execution")
            );
        }
        assert_eq!(
            session_grant_key(&ToolRequest::read("read_file", "file.txt")),
            None
        );
    }

    #[test]
    fn workspace_scoped_resolvers_reject_escape_and_normalize_relative_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let request = ToolRequest::read("read_file", "nested/../safe.txt");
        let PolicyDecision::Allow { constraints } =
            policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Research, &request)
        else {
            panic!("strict read should be allowed")
        };
        assert_eq!(
            resolve_read_path(&constraints, Path::new("nested/../safe.txt")).unwrap(),
            workspace.path().join("safe.txt")
        );
        assert!(resolve_mutate_path(&constraints, Path::new("../escape.txt")).is_err());
        assert!(normalize_lexical(Path::new("../../escape.txt")).is_err());
    }

    #[test]
    fn profile_names_and_operation_matrix_stay_explicit() {
        assert_eq!(PolicyProfile::Strict.to_string(), "strict");
        assert_eq!(PolicyProfile::Medium.to_string(), "medium");
        assert_eq!(PolicyProfile::FullControl.to_string(), "full_control");
        for (value, expected) in [
            ("research", CapabilityProfile::Research),
            ("plan", CapabilityProfile::Plan),
            ("coding", CapabilityProfile::Coding),
            ("review", CapabilityProfile::Review),
            ("general", CapabilityProfile::General),
        ] {
            assert_eq!(value.parse::<CapabilityProfile>().unwrap(), expected);
            assert_eq!(expected.to_string(), value);
        }

        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        for operation in [
            Operation::Read,
            Operation::TextPatch,
            Operation::Write,
            Operation::Move,
            Operation::Delete,
            Operation::Process,
            Operation::Shell,
            Operation::Network,
            Operation::Browser,
            Operation::Computer,
            Operation::Mcp,
            Operation::McpRead,
            Operation::Media,
        ] {
            let request = ToolRequest {
                tool_name: format!("{operation:?}"),
                operation,
                targets: vec![PathBuf::from("inside.txt")],
                program: Some("fixture".into()),
                args: vec!["arg".into()],
                arguments_summary: "fixture".into(),
                binary: false,
                item_count: 1,
                network: matches!(operation, Operation::Network),
            };
            let strict =
                policy.evaluate(PolicyProfile::Strict, CapabilityProfile::Coding, &request);
            if matches!(
                operation,
                Operation::Read | Operation::McpRead | Operation::Network
            ) {
                assert!(matches!(strict, PolicyDecision::Allow { .. }));
            } else {
                assert!(matches!(strict, PolicyDecision::Deny { .. }));
            }

            let medium_general =
                policy.evaluate(PolicyProfile::Medium, CapabilityProfile::General, &request);
            if matches!(
                operation,
                Operation::Read
                    | Operation::Network
                    | Operation::Browser
                    | Operation::Mcp
                    | Operation::McpRead
            ) {
                assert!(matches!(medium_general, PolicyDecision::Allow { .. }));
            } else if matches!(operation, Operation::Media | Operation::Computer) {
                assert!(matches!(medium_general, PolicyDecision::Confirm { .. }));
            } else {
                assert!(matches!(medium_general, PolicyDecision::Deny { .. }));
            }

            let medium_coding =
                policy.evaluate(PolicyProfile::Medium, CapabilityProfile::Coding, &request);
            if matches!(
                operation,
                Operation::Media
                    | Operation::TextPatch
                    | Operation::Write
                    | Operation::Move
                    | Operation::Delete
                    | Operation::Process
                    | Operation::Shell
                    | Operation::Computer
            ) {
                assert!(matches!(medium_coding, PolicyDecision::Confirm { .. }));
            } else {
                assert!(matches!(medium_coding, PolicyDecision::Allow { .. }));
            }

            let full_control = policy.evaluate(
                PolicyProfile::FullControl,
                CapabilityProfile::General,
                &request,
            );
            if matches!(operation, Operation::Shell | Operation::Media) {
                assert!(matches!(full_control, PolicyDecision::Confirm { .. }));
            } else {
                assert!(matches!(full_control, PolicyDecision::Allow { .. }));
            }
        }
    }

    #[test]
    fn resolver_helpers_preserve_scope_and_existing_path_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("nested")).unwrap();
        std::fs::write(workspace.path().join("nested/file.txt"), "fixture").unwrap();
        let policy = PolicyEvaluator::new(workspace.path()).unwrap();
        let PolicyDecision::Allow { constraints } = policy.evaluate(
            PolicyProfile::Strict,
            CapabilityProfile::Research,
            &ToolRequest::read("read_file", "nested/file.txt"),
        ) else {
            panic!("strict local read should be allowed")
        };
        assert_eq!(
            resolve_read_path(&constraints, Path::new("nested/./file.txt")).unwrap(),
            workspace.path().join("nested/file.txt")
        );
        assert_eq!(
            resolve_mutate_path(&constraints, Path::new("nested/new.txt")).unwrap(),
            workspace.path().join("nested/new.txt")
        );
        assert!(resolve_read_path(&constraints, Path::new("../outside.txt")).is_err());
        assert!(resolve_mutate_path(&constraints, Path::new("../outside.txt")).is_err());
        assert_eq!(
            nearest_existing_path(&workspace.path().join("nested/missing/leaf.txt")),
            Some(workspace.path().join("nested"))
        );
        assert_eq!(
            normalize_lexical(Path::new("nested/../nested/file.txt")).unwrap(),
            PathBuf::from("nested/file.txt")
        );
    }
}

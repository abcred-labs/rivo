use async_trait::async_trait;
use regex::Regex;
use rivo_core::{EvidenceRef, EvidenceSource, ToolDefinition};
use rivo_policy::{
    resolve_mutate_path, resolve_read_path, CapabilityProfile, ConfirmRequest,
    ExecutionConstraints, Operation, PolicyDecision, PolicyEvaluator, PolicyProfile, ToolRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::sleep;

pub mod browser;
pub mod browser_extension;
pub mod browser_extension_host;
pub mod computer;
pub mod mcp;
pub mod oauth;
pub mod web;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("策略拒绝: {0}")]
    Denied(String),
    #[error("确认信息与当前请求不匹配")]
    ConfirmationMismatch,
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("参数错误: {0}")]
    InvalidArguments(String),
    #[error("命令执行超时")]
    Timeout,
    #[error("宿主能力不支持: {capability}")]
    Unsupported { capability: String },
    #[error("目标文件版本已变化：期望 {expected}，实际 {actual}")]
    VersionConflict { expected: String, actual: String },
    #[error("受保护路径不允许该操作")]
    ProtectedPath,
    #[error("批量操作失败，已回滚 {completed} 项；残留: {residuals:?}")]
    BulkRollback {
        completed: usize,
        residuals: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolExecution<T> {
    Completed { result: T },
    ConfirmationRequired { request: ConfirmRequest },
}

impl<T> ToolExecution<T> {
    fn map_completed<U>(self, map: impl FnOnce(T) -> U) -> ToolExecution<U> {
        match self {
            Self::Completed { result } => ToolExecution::Completed {
                result: map(result),
            },
            Self::ConfirmationRequired { request } => {
                ToolExecution::ConfirmationRequired { request }
            }
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, arguments: Value) -> Result<Value, ToolError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: PathBuf,
    #[serde(default = "default_read_limit")]
    pub max_bytes: usize,
}

fn default_read_limit() -> usize {
    256 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub path: PathBuf,
    pub content: String,
    pub truncated: bool,
    /// 整个文件（而非截断前缀）的 SHA-256，供后续写入作为版本前置条件。
    #[serde(default)]
    pub sha256: String,
    pub evidence: EvidenceRef,
}

impl ReadFileResult {
    pub fn into_evidence(self, id: impl Into<String>) -> EvidenceRef {
        EvidenceRef {
            id: id.into(),
            source: EvidenceSource::LocalFile {
                path: self.path.to_string_lossy().into_owned(),
                line_start: None,
                line_end: None,
            },
            excerpt: evidence_excerpt(&self.content),
            truncated: self.truncated,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirRequest {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDirResult {
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub pattern: String,
    pub path: PathBuf,
    #[serde(default = "default_match_limit")]
    pub max_matches: usize,
}

fn default_match_limit() -> usize {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub evidence: Vec<EvidenceRef>,
}

impl SearchMatch {
    pub fn into_evidence(self, id: impl Into<String>) -> EvidenceRef {
        EvidenceRef {
            id: id.into(),
            source: EvidenceSource::LocalFile {
                path: self.path.to_string_lossy().into_owned(),
                line_start: Some(self.line as u32),
                line_end: Some(self.line as u32),
            },
            excerpt: evidence_excerpt(&self.text),
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProcessRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_output_limit")]
    pub output_limit: usize,
}

/// 原始 Shell 只在用户为当前 Turn 显式启用后才会注册。环境变量只接受名称，
/// 执行器从当前环境复制对应值，避免模型经工具参数注入密钥正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellExecRequest {
    pub command: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub shell: ShellKind,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_output_limit")]
    pub output_limit: usize,
    #[serde(default)]
    pub environment_allowlist: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellKind {
    #[default]
    Default,
    PowerShell,
    Sh,
}

fn default_timeout_ms() -> u64 {
    120_000
}

fn default_output_limit() -> usize {
    1024 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchRequest {
    pub path: PathBuf,
    pub expected_file_sha256: String,
    pub expected: String,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: PathBuf,
    pub content: String,
    #[serde(default)]
    pub expected_file_sha256: Option<String>,
    #[serde(default)]
    pub expected_absent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteBinaryRequest {
    pub path: PathBuf,
    pub content: Vec<u8>,
    #[serde(default)]
    pub expected_file_sha256: Option<String>,
    #[serde(default)]
    pub expected_absent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePathRequest {
    pub source: PathBuf,
    pub destination: PathBuf,
    #[serde(default)]
    pub expected_source_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePathRequest {
    pub path: PathBuf,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashManifest {
    pub original_path: PathBuf,
    pub trash_path: PathBuf,
    pub sha256: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkOperation {
    WriteFile(WriteFileRequest),
    Move(MovePathRequest),
    Delete(DeletePathRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkFileOpsRequest {
    pub operations: Vec<BulkOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkFileOpsResult {
    pub completed: usize,
    pub rolled_back: bool,
    pub manifests: Vec<TrashManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchResult {
    pub path: PathBuf,
    pub diff: String,
    pub had_preexisting_changes: bool,
}

#[derive(Debug, Clone)]
pub struct ToolRuntime {
    policy: PolicyEvaluator,
    workspace_root: PathBuf,
    policy_profile: PolicyProfile,
    capability_profile: CapabilityProfile,
}

impl ToolRuntime {
    pub fn new(
        workspace: impl AsRef<Path>,
        policy_profile: PolicyProfile,
        capability_profile: CapabilityProfile,
    ) -> Result<Self, rivo_policy::PolicyError> {
        let workspace_root = workspace.as_ref().to_path_buf();
        Ok(Self {
            policy: PolicyEvaluator::new(&workspace_root)?,
            workspace_root,
            policy_profile,
            capability_profile,
        })
    }

    pub fn read_file(
        &self,
        request: ReadFileRequest,
    ) -> Result<ToolExecution<ReadFileResult>, ToolError> {
        let policy_request = ToolRequest::read("read_file", &request.path);
        let constraints = match self.authorize(&policy_request, None)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let path = resolve_read_tool_path(&constraints, &request.path)?;
        let limit = request.max_bytes.min(1024 * 1024);
        let file = fs::File::open(&path)?;
        let mut bytes = Vec::with_capacity(limit.saturating_add(1));
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > limit;
        let mut prefix = bytes;
        if truncated {
            prefix.truncate(limit);
        }
        let content = bounded_utf8(prefix)?;
        let sha256 = sha256_file(&path)?;
        let evidence = ReadFileResult {
            path: path.clone(),
            content: content.clone(),
            truncated,
            sha256,
            evidence: EvidenceRef {
                id: stable_evidence_id("read", &path.to_string_lossy(), &content),
                source: EvidenceSource::LocalFile {
                    path: path.to_string_lossy().into_owned(),
                    line_start: None,
                    line_end: None,
                },
                excerpt: evidence_excerpt(&content),
                truncated,
            },
        };
        Ok(ToolExecution::Completed { result: evidence })
    }

    pub fn list_dir(
        &self,
        request: ListDirRequest,
    ) -> Result<ToolExecution<ListDirResult>, ToolError> {
        let policy_request = ToolRequest::read("list_dir", &request.path);
        let constraints = match self.authorize(&policy_request, None)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let path = resolve_read_tool_path(&constraints, &request.path)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(path)? {
            if entries.len() >= 1000 {
                truncated = true;
                break;
            }
            let entry = entry?;
            entries.push(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: entry.file_type()?.is_dir(),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(ToolExecution::Completed {
            result: ListDirResult { entries, truncated },
        })
    }

    pub fn search(&self, request: SearchRequest) -> Result<ToolExecution<SearchResult>, ToolError> {
        let policy_request = ToolRequest::read("search", &request.path);
        let constraints = match self.authorize(&policy_request, None)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let regex = Regex::new(&request.pattern)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let root = resolve_read_tool_path(&constraints, &request.path)?;
        let mut matches = Vec::new();
        let mut state = SearchState::new(request.max_matches.min(1000));
        search_path(&root, &regex, 0, &mut state)?;
        matches.append(&mut state.matches);
        let evidence = matches
            .iter()
            .map(|item| {
                item.clone().into_evidence(stable_evidence_id(
                    "search",
                    &item.path.to_string_lossy(),
                    &format!("{}:{}:{}", item.line, item.path.display(), item.text),
                ))
            })
            .collect();
        Ok(ToolExecution::Completed {
            result: SearchResult {
                matches,
                truncated: state.truncated,
                files_scanned: state.files_scanned,
                bytes_scanned: state.bytes_scanned,
                evidence,
            },
        })
    }

    pub async fn run_process(
        &self,
        request: RunProcessRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<ProcessResult>, ToolError> {
        let policy_request = process_policy_request(&request);
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let cwd = resolve_mutate_tool_path(&constraints, &request.cwd)?;
        let timeout_ms = request.timeout_ms.min(constraints.timeout_ms);
        let output_limit = request.output_limit.min(constraints.output_limit);
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);
        let mut child = command.spawn()?;
        let mut process_tree = ProcessTreeGuard::attach(&child)?;
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::InvalidArguments("无法捕获 stdout".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::InvalidArguments("无法捕获 stderr".into()))?;
        let (overflow_tx, mut overflow_rx) = tokio::sync::mpsc::unbounded_channel();
        let stdout_task =
            tokio::spawn(read_limited(stdout_pipe, output_limit, overflow_tx.clone()));
        let stderr_task = tokio::spawn(read_limited(stderr_pipe, output_limit, overflow_tx));
        let timer = sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(timer);
        let (status, overflowed, timed_out) = tokio::select! {
            status = child.wait() => (Some(status?), false, false),
            _ = &mut timer => {
                process_tree.terminate()?;
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                (status, false, true)
            }
            Some(()) = overflow_rx.recv() => {
                process_tree.terminate()?;
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                (status, true, false)
            }
        };
        // A completed parent may still have descendants holding our pipes.
        process_tree.terminate()?;
        let stdout_read = stdout_task
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))??;
        let stderr_read = stderr_task
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))??;
        if timed_out {
            return Err(ToolError::Timeout);
        }
        let secrets = sensitive_environment_values();
        let (stdout, stdout_truncated) =
            bounded_redacted_text(&stdout_read.bytes, output_limit, &secrets);
        let (stderr, stderr_truncated) =
            bounded_redacted_text(&stderr_read.bytes, output_limit, &secrets);
        Ok(ToolExecution::Completed {
            result: ProcessResult {
                exit_code: status.and_then(|status| status.code()),
                stdout,
                stderr,
                truncated: overflowed
                    || stdout_read.overflowed
                    || stderr_read.overflowed
                    || stdout_truncated
                    || stderr_truncated,
            },
        })
    }

    pub async fn shell_exec(
        &self,
        request: ShellExecRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<ProcessResult>, ToolError> {
        if !matches!(self.capability_profile, CapabilityProfile::Coding) {
            return Err(ToolError::InvalidArguments(
                "shell_exec 仅可在 coding capability profile 中启用".into(),
            ));
        }
        if matches!(self.policy_profile, PolicyProfile::Strict) {
            return Err(ToolError::InvalidArguments(
                "strict policy 不允许启用 shell_exec".into(),
            ));
        }
        ensure_shell_capability(request.shell)?;
        if request.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments("Shell 命令不能为空".into()));
        }
        if request.command.chars().count() > 32_768 {
            return Err(ToolError::InvalidArguments("Shell 命令超过长度上限".into()));
        }
        if request.environment_allowlist.len() > 32 {
            return Err(ToolError::InvalidArguments(
                "Shell 环境变量 allowlist 超过数量上限".into(),
            ));
        }
        for name in &request.environment_allowlist {
            if !valid_environment_name(name) {
                return Err(ToolError::InvalidArguments(format!(
                    "环境变量名称无效: {name}"
                )));
            }
        }
        let policy_request = shell_policy_request(&request);
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let cwd = resolve_mutate_tool_path(&constraints, &request.cwd)?;
        let timeout_ms = request.timeout_ms.min(constraints.timeout_ms);
        let output_limit = request.output_limit.min(constraints.output_limit);
        let (program, args) = shell_program_and_args(request.shell, &request.command)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);
        // 原始 Shell 不继承任意环境。保留最小平台运行基线，再允许用户显式
        // 指定安全名称；值仍经过输出脱敏，不进入事件或审计正文。
        command.env_clear();
        for name in shell_base_environment_names() {
            copy_environment(&mut command, name);
        }
        for name in &request.environment_allowlist {
            copy_environment(&mut command, name);
        }
        let mut child = command.spawn()?;
        let mut process_tree = ProcessTreeGuard::attach(&child)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::InvalidArguments("无法捕获 stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::InvalidArguments("无法捕获 stderr".into()))?;
        let (overflow_tx, mut overflow_rx) = tokio::sync::mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(read_limited(stdout, output_limit, overflow_tx.clone()));
        let stderr_task = tokio::spawn(read_limited(stderr, output_limit, overflow_tx));
        let timer = sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(timer);
        let (status, overflowed, timed_out) = tokio::select! {
            status = child.wait() => (Some(status?), false, false),
            _ = &mut timer => { process_tree.terminate()?; let _ = child.kill().await; (child.wait().await.ok(), false, true) },
            Some(()) = overflow_rx.recv() => { process_tree.terminate()?; let _ = child.kill().await; (child.wait().await.ok(), true, false) },
        };
        process_tree.terminate()?;
        let stdout_read = stdout_task
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))??;
        let stderr_read = stderr_task
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))??;
        if timed_out {
            return Err(ToolError::Timeout);
        }
        let secrets = sensitive_environment_values_for(&request.environment_allowlist);
        let (stdout, stdout_truncated) =
            bounded_redacted_text(&stdout_read.bytes, output_limit, &secrets);
        let (stderr, stderr_truncated) =
            bounded_redacted_text(&stderr_read.bytes, output_limit, &secrets);
        Ok(ToolExecution::Completed {
            result: ProcessResult {
                exit_code: status.and_then(|status| status.code()),
                stdout,
                stderr,
                truncated: overflowed
                    || stdout_read.overflowed
                    || stderr_read.overflowed
                    || stdout_truncated
                    || stderr_truncated,
            },
        })
    }

    pub fn apply_patch(
        &self,
        request: ApplyPatchRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<PatchResult>, ToolError> {
        let file_state = git_path_state(&self.workspace_root, &request.path)?;
        let had_preexisting_changes = !matches!(file_state, GitFileState::TrackedClean);
        let policy_request = ToolRequest {
            tool_name: "apply_patch".into(),
            // A text patch remains a text patch even when it touches a file with
            // pre-existing edits.  Keeping its policy identity stable lets the
            // Agent's confirmation bind to the executor's exact request.
            operation: Operation::TextPatch,
            targets: vec![request.path.clone()],
            program: None,
            args: vec![],
            arguments_summary: format!("修改 {}", request.path.display()),
            binary: false,
            item_count: 1,
            network: false,
        };
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(constraints) => constraints,
            Authorization::Confirmation(request) => {
                return Ok(ToolExecution::ConfirmationRequired { request });
            }
        };
        let path = resolve_mutate_tool_path(&constraints, &request.path)?;
        let before = fs::read_to_string(&path)?;
        let actual_hash = sha256_text(&before);
        if !hash_matches(&actual_hash, &request.expected_file_sha256) {
            return Err(ToolError::InvalidArguments(format!(
                "文件版本已变化：期望 {}，实际 {}",
                request.expected_file_sha256, actual_hash
            )));
        }
        let occurrences = before.matches(&request.expected).count();
        if occurrences != 1 {
            return Err(ToolError::InvalidArguments(format!(
                "expected 文本应恰好出现一次，实际为 {occurrences} 次"
            )));
        }
        let after = before.replacen(&request.expected, &request.replacement, 1);
        let mut temporary = tempfile::NamedTempFile::new_in(
            path.parent()
                .ok_or_else(|| ToolError::InvalidArguments("目标文件缺少父目录".into()))?,
        )?;
        temporary.write_all(after.as_bytes())?;
        temporary.as_file().sync_all()?;
        let metadata = fs::metadata(&path)?;
        fs::set_permissions(temporary.path(), metadata.permissions())?;
        let current = fs::read_to_string(&path)?;
        let current_hash = sha256_text(&current);
        if !hash_matches(&current_hash, &request.expected_file_sha256) {
            return Err(ToolError::InvalidArguments("文件在写入前被并行修改".into()));
        }
        persist_temporary(temporary, &path)?;
        Ok(ToolExecution::Completed {
            result: PatchResult {
                path,
                diff: simple_diff(&request.expected, &request.replacement),
                had_preexisting_changes,
            },
        })
    }

    pub fn write_file(
        &self,
        request: WriteFileRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<PathBuf>, ToolError> {
        if request.content.len() > 1024 * 1024 {
            return Err(ToolError::InvalidArguments("文本写入超过 1 MiB".into()));
        }
        let policy_request = ToolRequest {
            tool_name: "write_file".into(),
            operation: Operation::Write,
            targets: vec![request.path.clone()],
            program: None,
            args: Vec::new(),
            arguments_summary: format!("写入 {}", request.path.display()),
            binary: false,
            item_count: 1,
            network: false,
        };
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(value) => value,
            Authorization::Confirmation(value) => {
                return Ok(ToolExecution::ConfirmationRequired { request: value })
            }
        };
        let path = resolve_mutate_tool_path(&constraints, &request.path)?;
        let initial_version = file_version(&path)?;
        validate_expected_state(
            &path,
            request.expected_absent,
            request.expected_file_sha256.as_deref(),
        )?;
        atomic_replace_checked(
            &path,
            request.content.as_bytes(),
            request.expected_absent,
            request.expected_file_sha256.as_deref(),
            initial_version.as_deref(),
        )?;
        Ok(ToolExecution::Completed { result: path })
    }

    pub fn write_binary(
        &self,
        request: WriteBinaryRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<PathBuf>, ToolError> {
        if request.content.len() > 10 * 1024 * 1024 {
            return Err(ToolError::InvalidArguments("二进制写入超过 10 MiB".into()));
        }
        let policy_request = ToolRequest {
            tool_name: "write_binary".into(),
            operation: Operation::Write,
            targets: vec![request.path.clone()],
            program: None,
            args: Vec::new(),
            arguments_summary: format!("二进制写入 {}", request.path.display()),
            binary: true,
            item_count: 1,
            network: false,
        };
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(value) => value,
            Authorization::Confirmation(value) => {
                return Ok(ToolExecution::ConfirmationRequired { request: value })
            }
        };
        let path = resolve_mutate_tool_path(&constraints, &request.path)?;
        let initial_version = file_version(&path)?;
        validate_expected_state(
            &path,
            request.expected_absent,
            request.expected_file_sha256.as_deref(),
        )?;
        atomic_replace_checked(
            &path,
            &request.content,
            request.expected_absent,
            request.expected_file_sha256.as_deref(),
            initial_version.as_deref(),
        )?;
        Ok(ToolExecution::Completed { result: path })
    }

    pub fn move_path(
        &self,
        request: MovePathRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<PathBuf>, ToolError> {
        let policy_request = ToolRequest {
            tool_name: "move_path".into(),
            operation: Operation::Move,
            targets: vec![request.source.clone(), request.destination.clone()],
            program: None,
            args: Vec::new(),
            arguments_summary: format!(
                "移动 {} -> {}",
                request.source.display(),
                request.destination.display()
            ),
            binary: false,
            item_count: 1,
            network: false,
        };
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(value) => value,
            Authorization::Confirmation(value) => {
                return Ok(ToolExecution::ConfirmationRequired { request: value })
            }
        };
        let source = resolve_mutate_tool_path(&constraints, &request.source)?;
        let destination = resolve_mutate_tool_path(&constraints, &request.destination)?;
        if destination.exists() {
            return Err(ToolError::InvalidArguments("移动目标已存在".into()));
        }
        ensure_same_filesystem(&source, destination.parent().unwrap_or(Path::new(".")))?;
        if let Some(expected) = expected_hash(request.expected_source_sha256.as_deref()) {
            let actual = sha256_file(&source)?;
            if !hash_matches(&actual, expected) {
                return Err(version_conflict(Some(expected), Some(&actual)));
            }
        }
        fs::rename(&source, &destination).map_err(ToolError::Io)?;
        Ok(ToolExecution::Completed {
            result: destination,
        })
    }

    pub fn delete_path(
        &self,
        request: DeletePathRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<TrashManifest>, ToolError> {
        if request.turn_id.is_empty()
            || request.turn_id.contains(['/', '\\'])
            || request.turn_id.contains("..")
        {
            return Err(ToolError::InvalidArguments("无效 turn_id".into()));
        }
        let policy_request = ToolRequest {
            tool_name: "delete_path".into(),
            operation: Operation::Delete,
            targets: vec![request.path.clone()],
            program: None,
            args: Vec::new(),
            arguments_summary: format!("删除 {}", request.path.display()),
            binary: false,
            item_count: 1,
            network: false,
        };
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(value) => value,
            Authorization::Confirmation(value) => {
                return Ok(ToolExecution::ConfirmationRequired { request: value })
            }
        };
        let source = resolve_mutate_tool_path(&constraints, &request.path)?;
        let workspace = &constraints.workspace_root;
        let trash_root = workspace.join(".rivo").join("trash");
        if is_protected_internal_path(&source, workspace) || source.starts_with(&trash_root) {
            return Err(ToolError::ProtectedPath);
        }
        let target_dir = trash_root.join(&request.turn_id);
        fs::create_dir_all(&target_dir)?;
        let name = source.file_name().ok_or(ToolError::ProtectedPath)?;
        let trash_path = target_dir.join(name);
        if trash_path.exists() {
            return Err(ToolError::InvalidArguments("回收站目标已存在".into()));
        }
        let digest = sha256_file(&source)?;
        fs::rename(&source, &trash_path)?;
        let original_path = source.clone();
        let manifest = TrashManifest {
            original_path,
            trash_path: trash_path.clone(),
            sha256: digest,
            turn_id: request.turn_id,
        };
        let manifest_path = target_dir.join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        if let Err(error) = atomic_replace(&manifest_path, &manifest_bytes) {
            let _ = fs::rename(&trash_path, &manifest.original_path);
            return Err(error);
        }
        Ok(ToolExecution::Completed { result: manifest })
    }

    pub fn bulk_file_ops(
        &self,
        request: BulkFileOpsRequest,
        confirmation_id: Option<&str>,
    ) -> Result<ToolExecution<BulkFileOpsResult>, ToolError> {
        if request.operations.is_empty() || request.operations.len() > 100 {
            return Err(ToolError::InvalidArguments(
                "bulk 操作数量必须为 1..=100".into(),
            ));
        }
        let policy_request = bulk_policy_request(&request.operations);
        let constraints = match self.authorize(&policy_request, confirmation_id)? {
            Authorization::Allowed(value) => value,
            Authorization::Confirmation(value) => {
                return Ok(ToolExecution::ConfirmationRequired { request: value })
            }
        };
        let mut seen = std::collections::HashSet::new();
        for operation in &request.operations {
            let (targets, expected) = match operation {
                BulkOperation::WriteFile(value) => {
                    let path = resolve_mutate_tool_path(&constraints, &value.path)?;
                    (
                        vec![path.clone()],
                        validate_expected_state(
                            &path,
                            value.expected_absent,
                            value.expected_file_sha256.as_deref(),
                        ),
                    )
                }
                BulkOperation::Move(value) => {
                    let source = resolve_mutate_tool_path(&constraints, &value.source)?;
                    let destination = resolve_mutate_tool_path(&constraints, &value.destination)?;
                    if destination.exists() {
                        return Err(ToolError::InvalidArguments("bulk 移动目标已存在".into()));
                    }
                    if let Some(expected) = expected_hash(value.expected_source_sha256.as_deref()) {
                        let actual = sha256_file(&source)?;
                        if !hash_matches(&actual, expected) {
                            return Err(version_conflict(Some(expected), Some(&actual)));
                        }
                    }
                    (vec![source], Ok(()))
                }
                BulkOperation::Delete(value) => {
                    let source = resolve_mutate_tool_path(&constraints, &value.path)?;
                    if value.turn_id.is_empty()
                        || value.turn_id.contains(['/', '\\'])
                        || value.turn_id.contains("..")
                    {
                        return Err(ToolError::InvalidArguments("无效 turn_id".into()));
                    }
                    (vec![source], Ok(()))
                }
            };
            expected?;
            for target in targets {
                if !seen.insert(target.clone()) {
                    return Err(ToolError::InvalidArguments(format!(
                        "bulk 目标重复: {}",
                        target.display()
                    )));
                }
            }
        }
        let mut completed = 0;
        let mut manifests = Vec::new();
        let mut undo = Vec::new();
        for operation in request.operations {
            let undo_item = match &operation {
                BulkOperation::WriteFile(value) => {
                    let path = resolve_mutate_tool_path(&constraints, &value.path)?;
                    BulkUndo::Write {
                        path: path.clone(),
                        previous: path.is_file().then(|| fs::read(&path)).transpose()?,
                        permissions: fs::metadata(&path).ok().map(|m| m.permissions()),
                    }
                }
                BulkOperation::Move(value) => BulkUndo::Move {
                    source: resolve_mutate_tool_path(&constraints, &value.source)?,
                    destination: resolve_mutate_tool_path(&constraints, &value.destination)?,
                },
                BulkOperation::Delete(_) => BulkUndo::Delete { manifest: None },
            };
            let result: Result<ToolExecution<()>, ToolError> = match operation {
                BulkOperation::WriteFile(value) => {
                    let token = self.policy.request_id(&bulk_write_request(&value));
                    self.write_file(value, Some(&token))
                        .map(|execution| execution.map_completed(|_| ()))
                }
                BulkOperation::Move(value) => {
                    let token = self.policy.request_id(&bulk_move_request(&value));
                    self.move_path(value, Some(&token))
                        .map(|execution| execution.map_completed(|_| ()))
                }
                BulkOperation::Delete(value) => {
                    let token = self.policy.request_id(&bulk_delete_request(&value));
                    match self.delete_path(value, Some(&token)) {
                        Err(error) => Err(error),
                        Ok(ToolExecution::Completed { result }) => {
                            manifests.push(result.clone());
                            undo.push(BulkUndo::Delete {
                                manifest: Some(result),
                            });
                            Ok(ToolExecution::Completed { result: () })
                        }
                        Ok(ToolExecution::ConfirmationRequired { request }) => {
                            Ok(ToolExecution::ConfirmationRequired { request })
                        }
                    }
                }
            };
            let result = match result {
                Ok(value) => value,
                Err(_) => {
                    let residuals = rollback_bulk(&mut undo);
                    return Err(ToolError::BulkRollback {
                        completed,
                        residuals,
                    });
                }
            };
            match result {
                ToolExecution::Completed { .. } => {
                    completed += 1;
                    if !matches!(&undo_item, BulkUndo::Delete { .. }) {
                        undo.push(undo_item);
                    }
                }
                ToolExecution::ConfirmationRequired { .. } => {
                    let residuals = rollback_bulk(&mut undo);
                    return Err(ToolError::BulkRollback {
                        completed,
                        residuals,
                    });
                }
            }
        }
        Ok(ToolExecution::Completed {
            result: BulkFileOpsResult {
                completed,
                rolled_back: false,
                manifests,
            },
        })
    }

    fn authorize(
        &self,
        request: &ToolRequest,
        confirmation_id: Option<&str>,
    ) -> Result<Authorization, ToolError> {
        match self
            .policy
            .evaluate(self.policy_profile, self.capability_profile, request)
        {
            PolicyDecision::Allow { constraints }
            | PolicyDecision::Constrain { constraints, .. } => {
                Ok(Authorization::Allowed(constraints))
            }
            PolicyDecision::Deny { reason } => Err(ToolError::Denied(reason)),
            PolicyDecision::Confirm {
                request: confirmation,
                constraints,
            } => match confirmation_id {
                Some(id) if self.policy.confirmation_matches(request, id) => {
                    Ok(Authorization::Allowed(constraints))
                }
                Some(_) => Err(ToolError::ConfirmationMismatch),
                None => Ok(Authorization::Confirmation(confirmation)),
            },
        }
    }
}

fn ensure_same_filesystem(source: &Path, destination_parent: &Path) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(source)?.dev() != fs::metadata(destination_parent)?.dev() {
            return Err(ToolError::InvalidArguments("不支持跨文件系统移动".into()));
        }
    }
    #[cfg(windows)]
    {
        let source = fs::canonicalize(source)?;
        let destination_parent = fs::canonicalize(destination_parent)?;
        let source_prefix = source.components().next();
        let destination_prefix = destination_parent.components().next();
        if source_prefix.is_some() && source_prefix != destination_prefix {
            return Err(ToolError::InvalidArguments("不支持跨卷移动".into()));
        }
    }
    Ok(())
}

#[derive(Debug)]
enum BulkUndo {
    Write {
        path: PathBuf,
        previous: Option<Vec<u8>>,
        permissions: Option<fs::Permissions>,
    },
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    Delete {
        manifest: Option<TrashManifest>,
    },
}

impl BulkUndo {
    fn path(&self) -> PathBuf {
        match self {
            Self::Write { path, .. } | Self::Move { source: path, .. } => path.clone(),
            Self::Delete {
                manifest: Some(manifest),
            } => manifest.original_path.clone(),
            Self::Delete { manifest: None } => PathBuf::from("<unknown>"),
        }
    }
}

fn rollback_bulk(undo: &mut Vec<BulkUndo>) -> Vec<PathBuf> {
    let mut residuals = Vec::new();
    while let Some(item) = undo.pop() {
        let path = item.path();
        let result = match item {
            BulkUndo::Write {
                path,
                previous,
                permissions,
            } => match previous {
                Some(bytes) => atomic_replace(&path, &bytes).and_then(|_| {
                    if let Some(mode) = permissions {
                        fs::set_permissions(&path, mode)?;
                    }
                    Ok(())
                }),
                None => fs::remove_file(&path).map_err(ToolError::Io),
            },
            BulkUndo::Move {
                source,
                destination,
            } => fs::rename(destination, source).map_err(ToolError::Io),
            BulkUndo::Delete { manifest } => match manifest {
                Some(manifest) => fs::rename(&manifest.trash_path, &manifest.original_path)
                    .map_err(ToolError::Io)
                    .and_then(|_| {
                        fs::remove_file(
                            manifest
                                .trash_path
                                .parent()
                                .unwrap_or(Path::new("."))
                                .join("manifest.json"),
                        )
                        .map_err(ToolError::Io)
                    }),
                None => Ok(()),
            },
        };
        if result.is_err() {
            residuals.push(path);
        }
    }
    residuals
}

enum Authorization {
    Allowed(ExecutionConstraints),
    Confirmation(ConfirmRequest),
}

pub fn bulk_policy_request(operations: &[BulkOperation]) -> ToolRequest {
    let mut targets = Vec::new();
    for operation in operations {
        match operation {
            BulkOperation::WriteFile(value) => targets.push(value.path.clone()),
            BulkOperation::Move(value) => {
                targets.push(value.source.clone());
                targets.push(value.destination.clone());
            }
            BulkOperation::Delete(value) => targets.push(value.path.clone()),
        }
    }
    ToolRequest {
        tool_name: "bulk_file_ops".into(),
        operation: Operation::Write,
        targets,
        program: None,
        args: Vec::new(),
        arguments_summary: format!("{} 项文件操作", operations.len()),
        binary: true,
        item_count: operations.len(),
        network: false,
    }
}

fn bulk_write_request(value: &WriteFileRequest) -> ToolRequest {
    ToolRequest {
        tool_name: "write_file".into(),
        operation: Operation::Write,
        targets: vec![value.path.clone()],
        program: None,
        args: Vec::new(),
        arguments_summary: format!("写入 {}", value.path.display()),
        binary: false,
        item_count: 1,
        network: false,
    }
}

fn bulk_move_request(value: &MovePathRequest) -> ToolRequest {
    ToolRequest {
        tool_name: "move_path".into(),
        operation: Operation::Move,
        targets: vec![value.source.clone(), value.destination.clone()],
        program: None,
        args: Vec::new(),
        arguments_summary: format!(
            "移动 {} -> {}",
            value.source.display(),
            value.destination.display()
        ),
        binary: false,
        item_count: 1,
        network: false,
    }
}

fn bulk_delete_request(value: &DeletePathRequest) -> ToolRequest {
    ToolRequest {
        tool_name: "delete_path".into(),
        operation: Operation::Delete,
        targets: vec![value.path.clone()],
        program: None,
        args: Vec::new(),
        arguments_summary: format!("删除 {}", value.path.display()),
        binary: false,
        item_count: 1,
        network: false,
    }
}

fn validate_expected_state(
    path: &Path,
    expected_absent: bool,
    expected_sha256: Option<&str>,
) -> Result<(), ToolError> {
    // 只在需要比较时计算哈希；目标可能是目录，由后续写入报告真实错误。
    if expected_absent && path.exists() {
        return Err(version_conflict(
            None,
            Some(
                file_version(path)
                    .ok()
                    .flatten()
                    .as_deref()
                    .unwrap_or("已存在"),
            ),
        ));
    }
    if let Some(expected) = expected_hash(expected_sha256) {
        let actual = file_version(path)?;
        if !actual
            .as_deref()
            .is_some_and(|current| hash_matches(current, expected))
        {
            return Err(version_conflict(Some(expected), actual.as_deref()));
        }
    }
    Ok(())
}

/// 冲突信息只含哈希或“不存在”，模型可据此直接重试而无需猜测版本。
fn version_conflict(expected: Option<&str>, actual: Option<&str>) -> ToolError {
    let describe = |value: Option<&str>| value.unwrap_or("不存在").to_owned();
    ToolError::VersionConflict {
        expected: describe(expected),
        actual: describe(actual),
    }
}

/// 模型常把可选字符串填成空串；空白哈希等同于未提供前置版本。
fn expected_hash(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn hash_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected.trim())
}

fn atomic_replace(path: &Path, content: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidArguments("目标缺少父目录".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(temporary.path(), metadata.permissions())?;
    }
    persist_temporary(temporary, path)?;
    Ok(())
}

fn atomic_replace_checked(
    path: &Path,
    content: &[u8],
    expected_absent: bool,
    expected_sha256: Option<&str>,
    initial_version: Option<&str>,
) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidArguments("目标缺少父目录".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(temporary.path(), metadata.permissions())?;
    }
    let current_version = file_version(path)?;
    if expected_absent {
        if current_version.is_some() {
            return Err(version_conflict(None, current_version.as_deref()));
        }
    } else if let Some(expected) = expected_hash(expected_sha256) {
        if !current_version
            .as_deref()
            .is_some_and(|current| hash_matches(current, expected))
        {
            return Err(version_conflict(Some(expected), current_version.as_deref()));
        }
    } else if current_version.as_deref() != initial_version {
        return Err(version_conflict(
            initial_version,
            current_version.as_deref(),
        ));
    }
    persist_temporary(temporary, path)
}

fn persist_temporary(temporary: tempfile::NamedTempFile, path: &Path) -> Result<(), ToolError> {
    #[cfg(not(windows))]
    {
        temporary
            .persist(path)
            .map_err(|error| ToolError::Io(error.error))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let temporary_path = temporary.into_temp_path();
        let source = temporary_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let destination = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            if MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ) == 0
            {
                return Err(ToolError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

fn is_protected_internal_path(path: &Path, workspace: &Path) -> bool {
    let protected = [
        workspace.to_path_buf(),
        workspace.join(".git"),
        workspace.join(".rivo"),
    ];
    #[cfg(windows)]
    {
        let value = path.to_string_lossy().to_ascii_lowercase();
        protected.iter().enumerate().any(|(index, root)| {
            let root = root.to_string_lossy().to_ascii_lowercase();
            value == root || (index > 0 && value.starts_with(&(root + "\\")))
        })
    }
    #[cfg(not(windows))]
    {
        path == protected[0]
            || protected[1..]
                .iter()
                .any(|root| path == root || path.starts_with(root))
    }
}

fn sha256_file(path: &Path) -> Result<String, ToolError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn file_version(path: &Path) -> Result<Option<String>, ToolError> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(sha256_file(path)?))
}

fn process_policy_request(request: &RunProcessRequest) -> ToolRequest {
    ToolRequest {
        tool_name: "run_process".into(),
        operation: Operation::Process,
        targets: vec![request.cwd.clone()],
        program: Some(request.program.clone()),
        args: request.args.clone(),
        arguments_summary: format!("{} {}", request.program, request.args.join(" ")),
        binary: false,
        item_count: 1,
        network: false,
    }
}

pub fn shell_policy_request(request: &ShellExecRequest) -> ToolRequest {
    let (program, args) = shell_program_and_args(request.shell, &request.command)
        .unwrap_or_else(|_| ("shell".into(), vec![request.command.clone()]));
    ToolRequest {
        tool_name: "shell_exec".into(),
        operation: rivo_policy::Operation::Shell,
        targets: vec![request.cwd.clone()],
        program: Some(program),
        args,
        arguments_summary: format!("Shell: {}", command_summary(&request.command)),
        binary: false,
        item_count: 1,
        network: false,
    }
}

fn command_summary(command: &str) -> String {
    const MAX: usize = 160;
    let mut summary = command.chars().take(MAX).collect::<String>();
    if command.chars().count() > MAX {
        summary.push('…');
    }
    summary
}

fn shell_program_and_args(
    kind: ShellKind,
    command: &str,
) -> Result<(String, Vec<String>), ToolError> {
    match kind {
        ShellKind::PowerShell => Ok((
            "powershell".into(),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                command.into(),
            ],
        )),
        ShellKind::Sh => Ok(("sh".into(), vec!["-c".into(), command.into()])),
        ShellKind::Default => {
            #[cfg(windows)]
            {
                Ok((
                    "powershell".into(),
                    vec![
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-Command".into(),
                        command.into(),
                    ],
                ))
            }
            #[cfg(not(windows))]
            {
                Ok(("sh".into(), vec!["-c".into(), command.into()]))
            }
        }
    }
}

fn ensure_shell_capability(kind: ShellKind) -> Result<(), ToolError> {
    match kind {
        ShellKind::Default => Ok(()),
        ShellKind::PowerShell if cfg!(windows) => Ok(()),
        ShellKind::Sh if cfg!(unix) => Ok(()),
        ShellKind::PowerShell => Err(ToolError::Unsupported {
            capability: "shell.powershell".into(),
        }),
        ShellKind::Sh => Err(ToolError::Unsupported {
            capability: "shell.posix".into(),
        }),
    }
}

fn shell_base_environment_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATH",
            "PATHEXT",
            "TEMP",
            "TMP",
            "USERPROFILE",
        ]
    }
    #[cfg(not(windows))]
    {
        &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"]
    }
}

fn copy_environment(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

fn valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn resolve_read_tool_path(
    constraints: &ExecutionConstraints,
    path: &Path,
) -> Result<PathBuf, ToolError> {
    resolve_read_path(constraints, path).map_err(|error| ToolError::Denied(error.to_string()))
}

fn resolve_mutate_tool_path(
    constraints: &ExecutionConstraints,
    path: &Path,
) -> Result<PathBuf, ToolError> {
    resolve_mutate_path(constraints, path).map_err(|error| ToolError::Denied(error.to_string()))
}

struct SearchState {
    matches: Vec<SearchMatch>,
    truncated: bool,
    files_scanned: usize,
    bytes_scanned: u64,
    max_matches: usize,
    started: std::time::Instant,
}

impl SearchState {
    fn new(max_matches: usize) -> Self {
        Self {
            matches: Vec::new(),
            truncated: false,
            files_scanned: 0,
            bytes_scanned: 0,
            max_matches,
            started: std::time::Instant::now(),
        }
    }
}

fn search_path(
    path: &Path,
    regex: &Regex,
    depth: usize,
    state: &mut SearchState,
) -> Result<(), ToolError> {
    if state.matches.len() >= state.max_matches
        || state.files_scanned >= 10_000
        || state.bytes_scanned >= 64 * 1024 * 1024
        || depth > 64
        || state.started.elapsed() > Duration::from_secs(30)
    {
        state.truncated = true;
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            search_path(&entry?.path(), regex, depth + 1, state)?;
            if state.truncated {
                break;
            }
        }
        return Ok(());
    }
    state.files_scanned += 1;
    let remaining = (64_usize * 1024 * 1024).saturating_sub(state.bytes_scanned as usize);
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((remaining as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > remaining {
        state.truncated = true;
        bytes.truncate(remaining);
    }
    state.bytes_scanned += bytes.len() as u64;
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(());
    };
    for (index, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            state.matches.push(SearchMatch {
                path: path.to_path_buf(),
                line: index + 1,
                text: line.chars().take(16 * 1024).collect(),
            });
            if state.matches.len() >= state.max_matches {
                state.truncated = true;
                break;
            }
        }
    }
    Ok(())
}

struct LimitedRead {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        // Every tool process owns a separate process group so timeout and
        // output-limit cancellation cannot leave grandchildren behind.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = command;
    #[cfg(windows)]
    {
        // Suspend the first thread until it belongs to our Job Object. This
        // prevents descendants from escaping during spawn/assignment.
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    }
}

struct ProcessTreeGuard {
    armed: bool,
    #[cfg(unix)]
    group_id: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTreeGuard {
    fn attach(child: &tokio::process::Child) -> Result<Self, std::io::Error> {
        #[cfg(unix)]
        {
            Ok(Self {
                armed: true,
                group_id: child
                    .id()
                    .ok_or_else(|| std::io::Error::other("tool process already exited"))?
                    as libc::pid_t,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::System::Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            };
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows_sys::Win32::System::Threading::{
                OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
            };
            // All handles have one owner. Closing the job kills its remaining
            // members even when a cancelled Future never polls us again.
            unsafe {
                let raw_job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if raw_job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = OwnedHandle::from_raw_handle(raw_job);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    raw_job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let process = child
                    .raw_handle()
                    .ok_or_else(|| std::io::Error::other("tool process already exited"))?;
                if AssignProcessToJobObject(raw_job, process) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let pid = child
                    .id()
                    .ok_or_else(|| std::io::Error::other("tool process already exited"))?;
                let raw_snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                if raw_snapshot == INVALID_HANDLE_VALUE {
                    return Err(std::io::Error::last_os_error());
                }
                let snapshot = OwnedHandle::from_raw_handle(raw_snapshot);
                let mut entry: THREADENTRY32 = std::mem::zeroed();
                entry.dwSize = std::mem::size_of_val(&entry) as u32;
                let mut has_thread = Thread32First(snapshot.as_raw_handle(), &mut entry) != 0;
                while has_thread {
                    if entry.th32OwnerProcessID == pid {
                        let raw_thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                        if raw_thread.is_null() {
                            return Err(std::io::Error::last_os_error());
                        }
                        let thread = OwnedHandle::from_raw_handle(raw_thread);
                        let suspended = ResumeThread(thread.as_raw_handle());
                        if suspended == u32::MAX {
                            return Err(std::io::Error::last_os_error());
                        }
                        if suspended == 1 {
                            return Ok(Self { armed: true, job });
                        }
                        if suspended > 1 {
                            return Err(std::io::Error::other(
                                "unexpected tool thread suspension count",
                            ));
                        }
                    }
                    has_thread = Thread32Next(snapshot.as_raw_handle(), &mut entry) != 0;
                }
                Err(std::io::Error::other(
                    "tool process suspended thread unavailable",
                ))
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Err(std::io::Error::other(
                "process tree cancellation unsupported",
            ))
        }
    }

    fn terminate(&mut self) -> Result<(), std::io::Error> {
        if !self.armed {
            return Ok(());
        }
        #[cfg(unix)]
        if unsafe { libc::kill(-self.group_id, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                #[cfg(target_os = "macos")]
                if error.raw_os_error() == Some(libc::EPERM)
                    && !macos_process_group_has_live_members(self.group_id)?
                {
                    // Darwin may reject signalling a group whose only members
                    // are already zombies. Confirm that state in the kernel;
                    // never ignore EPERM while a live member remains.
                    self.armed = false;
                    return Ok(());
                }
                return Err(error);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        self.armed = false;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn macos_process_group_has_live_members(group_id: libc::pid_t) -> Result<bool, std::io::Error> {
    const PROC_PGRP_ONLY: u32 = 2;
    // Bounded inspection fails closed if a tool group exceeds this limit.
    let mut pids = [0_i32; 4096];
    let bytes = unsafe {
        *libc::__error() = 0;
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            group_id as u32,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as i32,
        )
    };
    if bytes == 0 {
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code != 0 && code != libc::ESRCH)
        {
            return Err(error);
        }
    }
    if bytes < 0 || bytes as usize >= std::mem::size_of_val(&pids) {
        return Err(std::io::Error::other("cannot inspect tool process group"));
    }
    for pid in pids
        .into_iter()
        .take(bytes as usize / std::mem::size_of::<i32>())
        .filter(|pid| *pid > 0)
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                std::mem::size_of_val(&info) as i32,
            )
        };
        if read != std::mem::size_of_val(&info) as i32 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error);
        }
        if info.pbi_pgid == group_id as u32 && info.pbi_status != libc::SZOMB {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        // Drop cannot await. Unix group signalling and Windows Job Objects
        // enforce the same ownership boundary during Future cancellation.
        let _ = self.terminate();
    }
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    overflow_tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> Result<LimitedRead, std::io::Error> {
    let mut output = Vec::with_capacity(limit.min(1024 * 1024));
    let mut buffer = vec![0_u8; 8192];
    let mut overflowed = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            overflowed = true;
            let _ = overflow_tx.send(());
            break;
        }
    }
    Ok(LimitedRead {
        bytes: output,
        overflowed,
    })
}

fn bounded_utf8(mut bytes: Vec<u8>) -> Result<String, ToolError> {
    loop {
        match String::from_utf8(bytes) {
            Ok(text) => return Ok(text),
            Err(error) => {
                let valid_up_to = error.utf8_error().valid_up_to();
                if error.utf8_error().error_len().is_some() {
                    return Err(ToolError::InvalidArguments(
                        "read_file 只支持 UTF-8 文本".into(),
                    ));
                }
                bytes = error.into_bytes();
                bytes.truncate(valid_up_to);
            }
        }
    }
}

fn bounded_redacted_text(bytes: &[u8], limit: usize, secrets: &[String]) -> (String, bool) {
    let truncated = bytes.len() > limit;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
    for secret in secrets {
        text = text.replace(secret, "[REDACTED]");
    }
    (text, truncated)
}

fn sensitive_environment_values() -> Vec<String> {
    std::env::vars()
        .filter(|(key, value)| {
            value.len() >= 4
                && ["KEY", "TOKEN", "SECRET", "PASSWORD", "COOKIE"]
                    .iter()
                    .any(|marker| key.to_ascii_uppercase().contains(marker))
        })
        .map(|(_, value)| value)
        .collect()
}

/// 除常见敏感名称外，Shell 明确 allowlist 的所有非空值也必须脱敏。名称本身
/// 不能可靠地表达敏感性，且该 allowlist 是唯一能被子进程继承的额外环境来源。
fn sensitive_environment_values_for(allowlist: &[String]) -> Vec<String> {
    let mut values = sensitive_environment_values();
    for name in allowlist {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() && !values.iter().any(|existing| existing == &value) {
                values.push(value);
            }
        }
    }
    values
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitFileState {
    TrackedClean,
    TrackedDirty,
    Untracked,
    Ignored,
    NotRepository,
}

fn git_path_state(workspace: &Path, path: &Path) -> Result<GitFileState, ToolError> {
    let output = std::process::Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "--ignored=matching",
            "--untracked-files=all",
            "--",
        ])
        .arg(path)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| ToolError::InvalidArguments(format!("无法执行 git 状态检查: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(128) && stderr.contains("not a git repository") {
            return Ok(GitFileState::NotRepository);
        }
        return Err(ToolError::InvalidArguments(format!(
            "git 状态检查失败: {stderr}"
        )));
    }
    let status = String::from_utf8_lossy(&output.stdout);
    if status.lines().any(|line| line.starts_with("!!")) {
        Ok(GitFileState::Ignored)
    } else if status.lines().any(|line| line.starts_with("??")) {
        Ok(GitFileState::Untracked)
    } else if status.trim().is_empty() {
        Ok(GitFileState::TrackedClean)
    } else {
        Ok(GitFileState::TrackedDirty)
    }
}

fn simple_diff(before: &str, after: &str) -> String {
    format!("--- before\n-{before}\n+++ after\n+{after}\n")
}

pub fn sha256_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn evidence_excerpt(text: &str) -> String {
    text.chars().take(4096).collect()
}

fn stable_evidence_id(kind: &str, path: &str, locator: &str) -> String {
    format!(
        "local-{kind}-{}",
        sha256_text(&format!("{kind}\n{path}\n{locator}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivo_core::{ReportClaim, ResearchReport};

    #[test]
    fn read_file_is_limited_and_workspace_scoped() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "abcdef").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Research,
        )
        .unwrap();
        let result = runtime
            .read_file(ReadFileRequest {
                path: "a.txt".into(),
                max_bytes: 3,
            })
            .unwrap();
        let ToolExecution::Completed { result } = result else {
            panic!("read should be allowed")
        };
        assert_eq!(result.content, "abc");
        assert!(result.truncated);

        assert!(runtime
            .read_file(ReadFileRequest {
                path: "../secret.txt".into(),
                max_bytes: 10,
            })
            .is_err());
    }

    #[test]
    fn binary_writes_and_bulk_policy_requests_preserve_expected_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let medium = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Medium,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = WriteBinaryRequest {
            path: "artifact.bin".into(),
            content: vec![0, 159, 146, 150],
            expected_file_sha256: None,
            expected_absent: true,
        };
        assert!(matches!(
            medium.write_binary(request.clone(), None).unwrap(),
            ToolExecution::ConfirmationRequired { .. }
        ));

        let full = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let written = full.write_binary(request.clone(), None).unwrap();
        let ToolExecution::Completed { result } = written else {
            panic!("full control binary write must complete");
        };
        assert_eq!(result, temp.path().join("artifact.bin"));
        assert_eq!(fs::read(&result).unwrap(), request.content);
        assert!(full.write_binary(request, None).is_err());
        assert!(full
            .write_binary(
                WriteBinaryRequest {
                    path: "too-large.bin".into(),
                    content: vec![0; 10 * 1024 * 1024 + 1],
                    expected_file_sha256: None,
                    expected_absent: true,
                },
                None,
            )
            .is_err());

        let write = WriteFileRequest {
            path: "notes.txt".into(),
            content: "notes".into(),
            expected_file_sha256: None,
            expected_absent: true,
        };
        let move_request = MovePathRequest {
            source: "notes.txt".into(),
            destination: "archive/notes.txt".into(),
            expected_source_sha256: None,
        };
        let delete = DeletePathRequest {
            path: "archive/notes.txt".into(),
            turn_id: "turn-1".into(),
        };
        let bulk = bulk_policy_request(&[
            BulkOperation::WriteFile(write.clone()),
            BulkOperation::Move(move_request.clone()),
            BulkOperation::Delete(delete.clone()),
        ]);
        assert_eq!(bulk.tool_name, "bulk_file_ops");
        assert_eq!(bulk.targets.len(), 4);
        assert_eq!(bulk_write_request(&write).operation, Operation::Write);
        assert_eq!(bulk_move_request(&move_request).operation, Operation::Move);
        assert_eq!(bulk_delete_request(&delete).operation, Operation::Delete);
    }

    #[tokio::test]
    async fn non_allowlisted_process_requires_bound_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = RunProcessRequest {
            program: "unknown-installer".into(),
            args: vec!["install".into()],
            cwd: ".".into(),
            timeout_ms: 100,
            output_limit: 100,
        };
        let pending = runtime.run_process(request.clone(), None).await.unwrap();
        let ToolExecution::ConfirmationRequired { request: confirm } = pending else {
            panic!("command should require confirmation")
        };
        let changed = RunProcessRequest {
            args: vec!["install".into(), "other".into()],
            ..request
        };
        assert!(matches!(
            runtime
                .run_process(changed, Some(&confirm.request_id))
                .await,
            Err(ToolError::ConfirmationMismatch)
        ));
    }

    #[tokio::test]
    async fn shell_exec_requires_bound_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = ShellExecRequest {
            command: "echo shell-ok".into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            // PowerShell startup on shared Windows runners can exceed one
            // second while the workspace test suite is running in parallel;
            // this test is about confirmation binding, not startup latency.
            timeout_ms: 30_000,
            output_limit: 1024,
            environment_allowlist: vec![],
        };
        let pending = runtime.shell_exec(request.clone(), None).await.unwrap();
        let ToolExecution::ConfirmationRequired { request: approval } = pending else {
            panic!("shell must require confirmation");
        };
        let completed = runtime
            .shell_exec(request, Some(&approval.request_id))
            .await
            .unwrap();
        let ToolExecution::Completed { result } = completed else {
            panic!("confirmed shell must run");
        };
        assert!(result.stdout.contains("shell-ok"));
    }

    #[tokio::test]
    async fn shell_exec_rejects_invalid_bounds_before_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let invalid_name = ShellExecRequest {
            command: "echo should-not-run".into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            timeout_ms: 1_000,
            output_limit: 1024,
            environment_allowlist: vec!["BAD-NAME".into()],
        };
        assert!(matches!(
            runtime.shell_exec(invalid_name, None).await,
            Err(ToolError::InvalidArguments(message)) if message.contains("环境变量")
        ));

        let too_many = ShellExecRequest {
            command: "echo should-not-run".into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            timeout_ms: 1_000,
            output_limit: 1024,
            environment_allowlist: (0..33).map(|index| format!("SAFE_{index}")).collect(),
        };
        assert!(matches!(
            runtime.shell_exec(too_many, None).await,
            Err(ToolError::InvalidArguments(message)) if message.contains("数量上限")
        ));
    }

    #[tokio::test]
    async fn shell_exec_has_defense_in_depth_for_capability_and_strict_policy() {
        let temp = tempfile::tempdir().unwrap();
        let request = ShellExecRequest {
            command: "echo should-not-run".into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            timeout_ms: 1_000,
            output_limit: 1024,
            environment_allowlist: vec![],
        };
        for (policy, capability, expected) in [
            (
                PolicyProfile::FullControl,
                CapabilityProfile::General,
                "coding",
            ),
            (PolicyProfile::Strict, CapabilityProfile::Coding, "strict"),
        ] {
            let runtime = ToolRuntime::new(temp.path(), policy, capability).unwrap();
            assert!(matches!(
                runtime.shell_exec(request.clone(), None).await,
                Err(ToolError::InvalidArguments(message)) if message.contains(expected)
            ));
        }
    }

    #[tokio::test]
    async fn shell_exec_reports_cross_platform_host_capability_as_unsupported() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let unsupported = if cfg!(windows) {
            ShellKind::Sh
        } else {
            ShellKind::PowerShell
        };
        let request = ShellExecRequest {
            command: "echo should-not-run".into(),
            cwd: temp.path().into(),
            shell: unsupported,
            timeout_ms: 1_000,
            output_limit: 1024,
            environment_allowlist: vec![],
        };
        assert!(matches!(
            runtime.shell_exec(request, None).await,
            Err(ToolError::Unsupported { capability })
                if capability == if cfg!(windows) { "shell.posix" } else { "shell.powershell" }
        ));
    }

    #[tokio::test]
    async fn shell_exec_marks_fast_output_overflow_and_enforces_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let overflow_command = if cfg!(windows) {
            "$text = 'x' * 8192; Write-Output $text"
        } else {
            "yes x | head -c 8192"
        };
        let overflow_request = ShellExecRequest {
            command: overflow_command.into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            // Allow the platform shell to start under a busy CI runner before
            // asserting the output bound and timeout behavior below.
            timeout_ms: 30_000,
            output_limit: 64,
            environment_allowlist: vec![],
        };
        let ToolExecution::ConfirmationRequired { request: approval } = runtime
            .shell_exec(overflow_request.clone(), None)
            .await
            .unwrap()
        else {
            panic!("shell overflow request must require confirmation")
        };
        let ToolExecution::Completed { result } = runtime
            .shell_exec(overflow_request, Some(&approval.request_id))
            .await
            .unwrap()
        else {
            panic!("confirmed shell overflow request must complete")
        };
        assert!(result.truncated);
        assert!(result.stdout.len() <= 64);

        let timeout_command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 1000"
        } else {
            "sleep 1"
        };
        let timeout_request = ShellExecRequest {
            command: timeout_command.into(),
            cwd: temp.path().into(),
            shell: ShellKind::Default,
            timeout_ms: 100,
            output_limit: 1024,
            environment_allowlist: vec![],
        };
        let ToolExecution::ConfirmationRequired { request: approval } = runtime
            .shell_exec(timeout_request.clone(), None)
            .await
            .unwrap()
        else {
            panic!("shell timeout request must require confirmation")
        };
        assert!(matches!(
            runtime
                .shell_exec(timeout_request, Some(&approval.request_id))
                .await,
            Err(ToolError::Timeout)
        ));
    }

    #[test]
    fn shell_allowlisted_values_are_added_to_redaction_set() {
        const NAME: &str = "RIVO_TEST_NONSTANDARD_VALUE";
        const VALUE: &str = "not-a-standard-secret";
        std::env::set_var(NAME, VALUE);
        let values = sensitive_environment_values_for(&[NAME.into()]);
        std::env::remove_var(NAME);
        assert!(values.iter().any(|value| value == VALUE));
        let (text, truncated) = bounded_redacted_text(VALUE.as_bytes(), 128, &values);
        assert!(!truncated);
        assert_eq!(text, "[REDACTED]");
    }

    // A real descendant inherits the tool's output pipes. Cancelling only the
    // immediate child would leave it running after the Agent drops the tool.
    #[test]
    fn process_tree_leaf_fixture() {
        if !Path::new("process-tree-fixture").exists() {
            return;
        }
        fs::write("leaf.pid", std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    // This fixture deliberately exits before its descendant. The owning tool
    // must reap that descendant rather than wait indefinitely on its pipes.
    #[allow(clippy::zombie_processes)]
    fn process_tree_parent_fixture() {
        if !Path::new("process-tree-fixture").exists() {
            return;
        }
        let mut leaf = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::process_tree_leaf_fixture", "--nocapture"])
            .spawn()
            .unwrap();
        while !Path::new("leaf.pid").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        match fs::read_to_string("process-tree-fixture").unwrap().as_str() {
            "parent_exit" => return,
            "overflow" => println!("{}", "x".repeat(8192)),
            _ => {}
        }
        leaf.wait().unwrap();
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn cancelling_tool_future_reaps_process_descendants() {
        check_process_tree_cleanup("cancel").await;
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn timeout_reaps_process_descendants() {
        check_process_tree_cleanup("timeout").await;
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn output_overflow_reaps_process_descendants() {
        check_process_tree_cleanup("overflow").await;
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn completed_parent_reaps_descendants_holding_output_pipes() {
        check_process_tree_cleanup("parent_exit").await;
    }

    #[cfg(any(unix, windows))]
    async fn check_process_tree_cleanup(end: &str) {
        for use_shell in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let cwd = temp.path().join("进程取消");
            fs::create_dir(&cwd).unwrap();
            fs::write(cwd.join("process-tree-fixture"), end).unwrap();
            let runtime = ToolRuntime::new(
                temp.path(),
                PolicyProfile::Medium,
                CapabilityProfile::Coding,
            )
            .unwrap();
            let program = std::env::current_exe().unwrap();
            let process = RunProcessRequest {
                program: program.to_string_lossy().into_owned(),
                args: vec![
                    "--exact".into(),
                    "tests::process_tree_parent_fixture".into(),
                    "--nocapture".into(),
                ],
                cwd: cwd.clone(),
                timeout_ms: if end == "timeout" { 5_000 } else { 30_000 },
                output_limit: if end == "overflow" { 512 } else { 8192 },
            };
            let shell = ShellExecRequest {
                command: if cfg!(windows) {
                    format!(
                        "& '{}' --exact tests::process_tree_parent_fixture --nocapture",
                        program.display().to_string().replace('\'', "''")
                    )
                } else {
                    format!(
                        "'{}' --exact tests::process_tree_parent_fixture --nocapture",
                        program.display().to_string().replace('\'', "'\\''")
                    )
                },
                cwd,
                shell: ShellKind::Default,
                timeout_ms: process.timeout_ms,
                output_limit: process.output_limit,
                environment_allowlist: vec![],
            };
            let approval = if use_shell {
                let ToolExecution::ConfirmationRequired { request } =
                    runtime.shell_exec(shell.clone(), None).await.unwrap()
                else {
                    panic!("shell confirmation required")
                };
                request
            } else {
                let ToolExecution::ConfirmationRequired { request } =
                    runtime.run_process(process.clone(), None).await.unwrap()
                else {
                    panic!("process confirmation required")
                };
                request
            };
            let running = tokio::spawn(async move {
                if use_shell {
                    runtime.shell_exec(shell, Some(&approval.request_id)).await
                } else {
                    runtime
                        .run_process(process, Some(&approval.request_id))
                        .await
                }
            });
            let pid_path = temp.path().join("进程取消/leaf.pid");
            let pid = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(pid) = fs::read_to_string(&pid_path)
                        .ok()
                        .and_then(|value| value.parse::<u32>().ok())
                    {
                        return Some(pid);
                    }
                    if running.is_finished() {
                        return None;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let Some(pid) = pid else {
                panic!(
                    "fixture failed to start (shell={use_shell}): {:?}",
                    running.await
                );
            };
            if end == "cancel" {
                running.abort();
                assert!(running.await.unwrap_err().is_cancelled());
            } else {
                let result = tokio::time::timeout(Duration::from_secs(10), running)
                    .await
                    .unwrap()
                    .unwrap();
                if end == "timeout" {
                    assert!(matches!(result, Err(ToolError::Timeout)));
                } else {
                    let ToolExecution::Completed { result } = result.unwrap() else {
                        panic!("confirmed fixture must complete")
                    };
                    assert_eq!(result.truncated, end == "overflow");
                }
            }
            let exited = tokio::time::timeout(Duration::from_secs(5), async {
                while process_is_running(pid) {
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            // Also clean up the fixture if the assertion catches a regression.
            if exited.is_err() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                #[cfg(windows)]
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            assert!(
                exited.is_ok(),
                "tool cleanup left descendant running (shell={use_shell}, end={end})"
            );
        }
    }

    #[cfg(unix)]
    fn process_is_running(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&output.stdout);
        !status.trim().is_empty() && !status.trim().starts_with('Z')
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            running
        }
    }

    #[tokio::test]
    async fn allowlisted_process_output_overflow_is_bounded_and_returns_promptly() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..256 {
            fs::write(temp.path().join(format!("file-{index:04}.txt")), "x").unwrap();
        }
        let git_init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(git_init.success());
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::FullControl,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let execution = runtime
            .run_process(
                RunProcessRequest {
                    program: "git".into(),
                    args: vec![
                        "status".into(),
                        "--short".into(),
                        "--untracked-files=all".into(),
                    ],
                    cwd: ".".into(),
                    timeout_ms: 5_000,
                    output_limit: 64,
                },
                None,
            )
            .await
            .unwrap();
        let ToolExecution::Completed { result } = execution else {
            panic!("allowlisted git status should execute");
        };
        assert!(result.truncated);
        assert!(result.stdout.len() <= 64);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn patch_checks_expected_file_version() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "old").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = ApplyPatchRequest {
            path: "a.txt".into(),
            expected_file_sha256: sha256_text("old"),
            expected: "old".into(),
            replacement: "new".into(),
        };
        let pending = runtime.apply_patch(request.clone(), None).unwrap();
        let ToolExecution::ConfirmationRequired { request: confirm } = pending else {
            panic!("non-repository writes require confirmation")
        };
        let result = runtime
            .apply_patch(request, Some(&confirm.request_id))
            .unwrap();
        assert!(matches!(result, ToolExecution::Completed { .. }));
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn patch_rejects_a_stale_file_version() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "changed").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = ApplyPatchRequest {
            path: "a.txt".into(),
            expected_file_sha256: sha256_text("old"),
            expected: "changed".into(),
            replacement: "new".into(),
        };
        let pending = runtime.apply_patch(request.clone(), None).unwrap();
        let ToolExecution::ConfirmationRequired { request: confirm } = pending else {
            panic!("non-repository writes require confirmation")
        };
        let result = runtime.apply_patch(request, Some(&confirm.request_id));
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn utf8_truncation_keeps_valid_character_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "你好世界").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Research,
        )
        .unwrap();
        let result = runtime
            .read_file(ReadFileRequest {
                path: "a.txt".into(),
                max_bytes: 4,
            })
            .unwrap();
        let ToolExecution::Completed { result } = result else {
            panic!("read should be allowed")
        };
        assert_eq!(result.content, "你");
        assert_eq!(
            result.evidence.source,
            EvidenceSource::LocalFile {
                path: temp.path().join("a.txt").to_string_lossy().into_owned(),
                line_start: None,
                line_end: None,
            }
        );
        assert!(result.evidence.truncated);
    }

    #[test]
    fn readonly_results_include_stable_local_evidence() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("notes.txt"), "needle\nother").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Research,
        )
        .unwrap();

        let ToolExecution::Completed { result: first } = runtime
            .read_file(ReadFileRequest {
                path: "notes.txt".into(),
                max_bytes: 1024,
            })
            .unwrap()
        else {
            panic!("read should be allowed")
        };
        let ToolExecution::Completed { result: second } = runtime
            .read_file(ReadFileRequest {
                path: "notes.txt".into(),
                max_bytes: 1024,
            })
            .unwrap()
        else {
            panic!("read should be allowed")
        };
        assert_eq!(first.evidence.id, second.evidence.id);
        assert_eq!(first.evidence.excerpt, first.content);

        let ToolExecution::Completed { result } = runtime
            .search(SearchRequest {
                pattern: "needle".into(),
                path: ".".into(),
                max_matches: 10,
            })
            .unwrap()
        else {
            panic!("search should be allowed")
        };
        assert_eq!(result.matches.len(), result.evidence.len());
        assert!(result.evidence.iter().all(|evidence| matches!(
            evidence.source,
            EvidenceSource::LocalFile {
                line_start: Some(1),
                line_end: Some(1),
                ..
            }
        )));
        assert!(result.evidence[0].id.starts_with("local-search-"));
    }

    #[test]
    fn evidence_excerpt_is_bounded_without_shortening_read_content() {
        let temp = tempfile::tempdir().unwrap();
        let content = "x".repeat(8 * 1024);
        fs::write(temp.path().join("large.txt"), &content).unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Research,
        )
        .unwrap();
        let ToolExecution::Completed { result } = runtime
            .read_file(ReadFileRequest {
                path: "large.txt".into(),
                max_bytes: 16 * 1024,
            })
            .unwrap()
        else {
            panic!("read should be allowed")
        };
        assert_eq!(result.content.len(), content.len());
        assert_eq!(result.evidence.excerpt.chars().count(), 4096);
    }

    #[test]
    fn tool_observations_form_a_valid_research_evidence_chain() {
        let evidence = SearchMatch {
            path: PathBuf::from("README.md"),
            line: 3,
            text: "Rivo is a general agent".into(),
        }
        .into_evidence("e1");
        let report = ResearchReport {
            evidence: vec![evidence],
            claims: vec![ReportClaim {
                text: "项目定位为通用 Agent".into(),
                evidence_ids: vec!["e1".into()],
                uncertain: false,
            }],
            ..ResearchReport::default()
        };
        assert_eq!(report.validate(), Ok(()));
    }

    #[test]
    fn list_and_search_results_are_bounded_and_mark_partial_results() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..1002 {
            fs::write(temp.path().join(format!("{index}.txt")), "needle").unwrap();
        }
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Research,
        )
        .unwrap();
        let ToolExecution::Completed { result } = runtime
            .list_dir(ListDirRequest { path: ".".into() })
            .unwrap()
        else {
            panic!("list should be allowed")
        };
        assert_eq!(result.entries.len(), 1000);
        assert!(result.truncated);
        let ToolExecution::Completed { result } = runtime
            .search(SearchRequest {
                pattern: "needle".into(),
                path: ".".into(),
                max_matches: 1000,
            })
            .unwrap()
        else {
            panic!("search should be allowed")
        };
        assert_eq!(result.matches.len(), 1000);
        assert!(result.truncated);
        assert!(result.files_scanned <= 10_000);
    }

    #[test]
    fn coding_file_operations_require_confirmation_and_are_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("old.txt"), "old").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let write = WriteFileRequest {
            path: "new.txt".into(),
            content: "new".into(),
            expected_file_sha256: None,
            expected_absent: true,
        };
        let ToolExecution::ConfirmationRequired {
            request: write_confirmation,
        } = runtime.write_file(write.clone(), None).unwrap()
        else {
            panic!("write requires confirmation")
        };
        assert!(matches!(
            runtime
                .write_file(write, Some(&write_confirmation.request_id))
                .unwrap(),
            ToolExecution::Completed { .. }
        ));
        assert_eq!(
            fs::read_to_string(temp.path().join("new.txt")).unwrap(),
            "new"
        );

        let move_request = MovePathRequest {
            source: "new.txt".into(),
            destination: "moved.txt".into(),
            expected_source_sha256: Some(sha256_text("new")),
        };
        let ToolExecution::ConfirmationRequired {
            request: move_confirmation,
        } = runtime.move_path(move_request.clone(), None).unwrap()
        else {
            panic!("move requires confirmation")
        };
        assert!(matches!(
            runtime
                .move_path(move_request, Some(&move_confirmation.request_id))
                .unwrap(),
            ToolExecution::Completed { .. }
        ));
        let delete_request = DeletePathRequest {
            path: "moved.txt".into(),
            turn_id: "turn-1".into(),
        };
        let ToolExecution::ConfirmationRequired {
            request: delete_confirmation,
        } = runtime.delete_path(delete_request.clone(), None).unwrap()
        else {
            panic!("delete requires confirmation")
        };
        let ToolExecution::Completed { result: manifest } = runtime
            .delete_path(delete_request, Some(&delete_confirmation.request_id))
            .unwrap()
        else {
            panic!("delete should complete")
        };
        assert!(
            !manifest.trash_path.exists()
                || manifest.trash_path.to_string_lossy().contains("trash")
        );
        assert!(temp
            .path()
            .join(".rivo/trash/turn-1/manifest.json")
            .exists());
    }

    #[test]
    fn filesystem_guard_accepts_paths_on_the_same_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        fs::write(&source, "content").unwrap();
        assert!(ensure_same_filesystem(&source, temp.path()).is_ok());
    }

    #[test]
    fn bulk_preflight_and_failure_roll_back_completed_writes() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "old").unwrap();
        fs::create_dir(temp.path().join("directory")).unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let request = BulkFileOpsRequest {
            operations: vec![
                BulkOperation::WriteFile(WriteFileRequest {
                    path: "a.txt".into(),
                    content: "new".into(),
                    expected_file_sha256: Some(sha256_text("old")),
                    expected_absent: false,
                }),
                BulkOperation::WriteFile(WriteFileRequest {
                    path: "directory".into(),
                    content: "cannot replace directory".into(),
                    expected_file_sha256: None,
                    expected_absent: false,
                }),
            ],
        };
        let pending = runtime.bulk_file_ops(request.clone(), None).unwrap();
        let ToolExecution::ConfirmationRequired {
            request: confirmation,
        } = pending
        else {
            panic!("bulk write should require confirmation")
        };
        let result = runtime.bulk_file_ops(request, Some(&confirmation.request_id));
        assert!(
            matches!(result, Err(ToolError::BulkRollback { completed: 1, residuals }) if residuals.is_empty())
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "old"
        );
    }

    #[test]
    fn delete_protects_git_descendants_and_atomic_replace_overwrites_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".git").join("objects")).unwrap();
        fs::write(temp.path().join(".git/objects/keep"), "keep").unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        let delete = DeletePathRequest {
            path: ".git/objects/keep".into(),
            turn_id: "turn".into(),
        };
        let pending = runtime.delete_path(delete, None).unwrap();
        let ToolExecution::ConfirmationRequired { request } = pending else {
            panic!("delete should require confirmation")
        };
        assert!(matches!(
            runtime.delete_path(
                DeletePathRequest {
                    path: ".git/objects/keep".into(),
                    turn_id: "turn".into(),
                },
                Some(&request.request_id)
            ),
            Err(ToolError::ProtectedPath)
        ));

        let write = WriteFileRequest {
            path: "note.txt".into(),
            content: "first".into(),
            expected_file_sha256: None,
            expected_absent: false,
        };
        let pending = runtime.write_file(write, None).unwrap();
        let ToolExecution::ConfirmationRequired { request } = pending else {
            panic!("write should require confirmation")
        };
        runtime
            .write_file(
                WriteFileRequest {
                    path: "note.txt".into(),
                    content: "first".into(),
                    expected_file_sha256: None,
                    expected_absent: false,
                },
                Some(&request.request_id),
            )
            .unwrap();
        let pending = runtime
            .write_file(
                WriteFileRequest {
                    path: "note.txt".into(),
                    content: "second".into(),
                    expected_file_sha256: Some(sha256_text("first")),
                    expected_absent: false,
                },
                None,
            )
            .unwrap();
        let ToolExecution::ConfirmationRequired { request } = pending else {
            panic!("second write should require confirmation")
        };
        runtime
            .write_file(
                WriteFileRequest {
                    path: "note.txt".into(),
                    content: "second".into(),
                    expected_file_sha256: Some(sha256_text("first")),
                    expected_absent: false,
                },
                Some(&request.request_id),
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("note.txt")).unwrap(),
            "second"
        );
    }

    fn approved_write(runtime: &ToolRuntime, request: WriteFileRequest) -> Result<(), ToolError> {
        let pending = runtime.write_file(request.clone(), None)?;
        let ToolExecution::ConfirmationRequired {
            request: confirmation,
        } = pending
        else {
            panic!("write should require confirmation")
        };
        runtime
            .write_file(request, Some(&confirmation.request_id))
            .map(|_| ())
    }

    #[test]
    fn write_preconditions_ignore_blank_hashes_and_accept_read_file_versions() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = ToolRuntime::new(
            temp.path(),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        )
        .unwrap();
        approved_write(
            &runtime,
            WriteFileRequest {
                path: "new.txt".into(),
                content: "first".into(),
                expected_file_sha256: Some(String::new()),
                expected_absent: false,
            },
        )
        .unwrap();
        let ToolExecution::Completed { result: read } = runtime
            .read_file(ReadFileRequest {
                path: "new.txt".into(),
                max_bytes: 2,
            })
            .unwrap()
        else {
            panic!("read should complete")
        };
        assert!(read.truncated);
        assert_eq!(read.sha256, sha256_text("first"));
        approved_write(
            &runtime,
            WriteFileRequest {
                path: "new.txt".into(),
                content: "second".into(),
                expected_file_sha256: Some(read.sha256.to_ascii_uppercase()),
                expected_absent: false,
            },
        )
        .unwrap();
        assert!(matches!(
            approved_write(
                &runtime,
                WriteFileRequest {
                    path: "new.txt".into(),
                    content: "third".into(),
                    expected_file_sha256: Some(read.sha256),
                    expected_absent: false,
                },
            ),
            Err(ToolError::VersionConflict { ref expected, ref actual })
                if *expected == sha256_text("first") && *actual == sha256_text("second")
        ));
        assert_eq!(
            fs::read_to_string(temp.path().join("new.txt")).unwrap(),
            "second"
        );
        let missing = approved_write(
            &runtime,
            WriteFileRequest {
                path: "missing.txt".into(),
                content: "x".into(),
                expected_file_sha256: Some("abc".into()),
                expected_absent: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(missing, "目标文件版本已变化：期望 abc，实际 不存在");
    }
}

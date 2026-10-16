//! V1 instance-control primitives shared by CLI, app-server and process-control skill.

use fs2::FileExt;
use rand_core::{OsRng, RngCore};
use rivo_core::{
    CapabilitySummary, HostCapabilitySummary, LifecycleCapabilities, ResourceLimits,
    SidecarDeploymentType, SidecarNodeContext, TransportCapabilities, WorkspaceRootBinding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "rivo-instance-v1";
pub const REGISTRY_FILE: &str = "instances.json";
pub const REGISTRY_FORMAT_VERSION: u32 = 1;
const SIDECAR_NODE_ID_FILE: &str = "sidecar-node-id";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessControlSkill {
    pub name: String,
    pub version: String,
    #[serde(rename = "default")]
    pub default_enabled: bool,
    pub read_operations: Vec<String>,
    pub mutating_operations: Vec<String>,
    pub scopes: Vec<ControlScope>,
}

/// Frozen V1 default Skill contract. The executor still applies policy at the
/// request boundary; this definition is the loader-facing capability manifest.
pub fn default_process_control_skill() -> ProcessControlSkill {
    ProcessControlSkill {
        name: "process-control".into(),
        version: "1".into(),
        default_enabled: true,
        read_operations: vec![
            "health/read".into(),
            "instance/list".into(),
            "instance/read".into(),
            "thread/list".into(),
            "thread/read".into(),
        ],
        mutating_operations: vec![
            "instance/stop".into(),
            "instance/restart".into(),
            "thread/resume".into(),
            "thread/interrupt".into(),
            "thread/send".into(),
            "thread/fork".into(),
        ],
        scopes: vec![
            ControlScope::Read,
            ControlScope::Thread,
            ControlScope::Lifecycle,
            ControlScope::Lan,
        ],
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, ProcessControlSkill>,
}

impl SkillRegistry {
    pub fn load_defaults() -> Self {
        let skill = default_process_control_skill();
        let mut skills = BTreeMap::new();
        skills.insert(skill.name.clone(), skill);
        Self { skills }
    }

    pub fn get(&self, name: &str) -> Option<&ProcessControlSkill> {
        self.skills.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.skills.keys().map(String::as_str)
    }
}
pub fn registry_path(data_root: &Path) -> PathBuf {
    data_root.join("rivo").join("instances").join(REGISTRY_FILE)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceInfo {
    pub instance_id: String,
    pub pid: u32,
    pub protocol_version: String,
    pub endpoint: String,
    pub workspace: String,
    pub started_at_unix_ms: u128,
    pub state: InstanceState,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcEnvelope<T> {
    pub protocol_version: String,
    pub request_id: String,
    pub method: String,
    pub params: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceListResponse {
    pub instances: Vec<InstanceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlScope {
    Read,
    Thread,
    Lifecycle,
    Lan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlRequest {
    pub scope: ControlScope,
    pub confirmation: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanControlEnvelope {
    pub peer_id: String,
    pub target_instance_id: String,
    pub method: String,
    pub params: Value,
    pub request_id: String,
    pub nonce: String,
    pub expires_at_unix_ms: u128,
    pub control: ControlRequest,
}

#[derive(Debug, Error)]
pub enum InstanceControlError {
    #[error("invalid instance id")]
    InvalidInstanceId,
    #[error("invalid request")]
    InvalidRequest,
    #[error("confirmation is required for this operation")]
    ConfirmationRequired,
    #[error("instance registry I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("instance registry JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "instance registry format version {0} is newer than this Rivo version; the file was not modified"
    )]
    FutureRegistryFormat(u32),
    #[error("instance registry format version {0} is not supported")]
    UnsupportedRegistryFormat(u32),
    #[error("instance control transport is only available on Windows")]
    UnsupportedTransport,
    #[error("instance control response was not valid JSON-RPC")]
    InvalidResponse,
    #[error("workspace root is not a readable directory")]
    InvalidWorkspaceRoot,
    #[error("workspace root must not be a symbolic link")]
    WorkspaceRootSymlink,
    #[error("workspace root binding is outside the requested workspace")]
    WorkspaceBindingMismatch,
}

/// Load the stable identity of this local Sidecar, creating it exactly once
/// under its private data root. A client or Gateway never supplies this value.
/// The file is deliberately an opaque identifier and is never included in a
/// public path or credential projection.
pub fn load_or_create_node_id(data_root: &Path) -> Result<String, InstanceControlError> {
    let directory = data_root.join("rivo");
    fs::create_dir_all(&directory)?;
    let path = directory.join(SIDECAR_NODE_ID_FILE);

    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() {
            return Err(InstanceControlError::InvalidRequest);
        }
    }
    match fs::read_to_string(&path) {
        Ok(value) => {
            let value = value.trim();
            validate_instance_id(value)?;
            return Ok(value.to_owned());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mut random = [0_u8; 16];
    OsRng.fill_bytes(&mut random);
    let generated = format!("node-{}", hex::encode(random));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(generated.as_bytes())?;
            file.sync_all()?;
            Ok(generated)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let value = fs::read_to_string(&path)?;
            let value = value.trim();
            validate_instance_id(value)?;
            Ok(value.to_owned())
        }
        Err(error) => Err(error.into()),
    }
}

/// Canonicalize and validate the one Workspace root a Sidecar may execute in.
/// The check is intentionally performed at startup, before any task or tool is
/// accepted; request parameters cannot replace this binding.
pub fn validate_workspace_root(root: &Path) -> Result<PathBuf, InstanceControlError> {
    let metadata =
        fs::symlink_metadata(root).map_err(|_| InstanceControlError::InvalidWorkspaceRoot)?;
    if metadata.file_type().is_symlink() {
        return Err(InstanceControlError::WorkspaceRootSymlink);
    }
    if !metadata.is_dir() {
        return Err(InstanceControlError::InvalidWorkspaceRoot);
    }
    let canonical =
        fs::canonicalize(root).map_err(|_| InstanceControlError::InvalidWorkspaceRoot)?;
    if !canonical.is_dir() {
        return Err(InstanceControlError::InvalidWorkspaceRoot);
    }
    Ok(canonical)
}

/// Build the public capability projection from a validated local binding. The
/// canonical path is used only to derive an opaque binding id and never appears
/// in the returned structure.
pub fn sidecar_node_context(
    node_id: &str,
    deployment_type: SidecarDeploymentType,
    workspace_id: &str,
    workspace_root: &Path,
    policy_profile: &str,
    model_capabilities: CapabilitySummary,
    tool_capabilities: CapabilitySummary,
) -> Result<SidecarNodeContext, InstanceControlError> {
    validate_instance_id(node_id)?;
    validate_instance_id(workspace_id)?;
    if policy_profile.is_empty() || policy_profile.len() > 64 || policy_profile.contains('/') {
        return Err(InstanceControlError::InvalidRequest);
    }
    let canonical = validate_workspace_root(workspace_root)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(workspace_id.as_bytes());
    let binding_id = format!(
        "binding-{}",
        hex::encode(hasher.finalize())[..24].to_owned()
    );
    let context = SidecarNodeContext {
        node_id: node_id.to_owned(),
        deployment_type,
        runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: std::env::consts::OS.to_owned(),
        workspace_id: workspace_id.to_owned(),
        workspace_root_binding: WorkspaceRootBinding {
            workspace_id: workspace_id.to_owned(),
            binding_id,
            display_name: canonical
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace")
                .chars()
                .take(96)
                .collect(),
        },
        policy_profile: policy_profile.to_owned(),
        model_capabilities,
        tool_capabilities,
        host_capabilities: host_capabilities(),
        resource_limits: ResourceLimits::default(),
        transport_capabilities: TransportCapabilities {
            app_server: true,
            instance_control: true,
            event_replay: true,
        },
        lifecycle_capabilities: LifecycleCapabilities {
            restart: cfg!(windows),
            cancellation: true,
            task_leases: true,
        },
    };
    context
        .validate_public()
        .map_err(|_| InstanceControlError::InvalidRequest)?;
    Ok(context)
}

/// Return host capabilities as data. Consumers must gate actions on these
/// values instead of inferring support from the platform label.
pub fn host_capabilities() -> HostCapabilitySummary {
    let mut capabilities = vec![
        "workspace.path_normalization".to_owned(),
        "workspace.symlink_escape_check".to_owned(),
        "process.timeout".to_owned(),
        "process.output_limit".to_owned(),
    ];
    if cfg!(windows) {
        capabilities.extend([
            "process.tree_cancel".to_owned(),
            "terminal.conpty".to_owned(),
            "shell.powershell".to_owned(),
        ]);
    } else if cfg!(unix) {
        capabilities.extend([
            "process.group_cancel".to_owned(),
            "terminal.pty".to_owned(),
            "shell.posix".to_owned(),
        ]);
    }
    let unsupported = if cfg!(windows) {
        vec!["process.group_cancel".to_owned(), "terminal.pty".to_owned()]
    } else if cfg!(unix) {
        vec![
            "process.tree_cancel".to_owned(),
            "terminal.conpty".to_owned(),
        ]
    } else {
        vec![
            "process.tree_cancel".to_owned(),
            "process.group_cancel".to_owned(),
            "terminal.conpty".to_owned(),
            "terminal.pty".to_owned(),
        ]
    };
    HostCapabilitySummary {
        capabilities,
        unsupported,
    }
}

/// Resolve a path below the already-bound Workspace. Existing paths are fully
/// canonicalized; new paths are checked through their canonical parent so a
/// symlink cannot redirect a write outside the binding.
pub fn validate_workspace_path(
    root: &Path,
    candidate: &Path,
) -> Result<PathBuf, InstanceControlError> {
    let canonical_root = validate_workspace_root(root)?;
    let resolved = if candidate.exists() {
        fs::canonicalize(candidate).map_err(|_| InstanceControlError::InvalidWorkspaceRoot)?
    } else {
        let parent = candidate
            .parent()
            .ok_or(InstanceControlError::InvalidWorkspaceRoot)?;
        let canonical_parent =
            fs::canonicalize(parent).map_err(|_| InstanceControlError::InvalidWorkspaceRoot)?;
        canonical_parent.join(
            candidate
                .file_name()
                .ok_or(InstanceControlError::InvalidWorkspaceRoot)?,
        )
    };
    if !resolved.starts_with(&canonical_root) {
        return Err(InstanceControlError::WorkspaceBindingMismatch);
    }
    Ok(resolved)
}

pub fn validate_instance_id(value: &str) -> Result<(), InstanceControlError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(InstanceControlError::InvalidInstanceId);
    }
    Ok(())
}

pub fn validate_control(request: &ControlRequest) -> Result<(), InstanceControlError> {
    if request.idempotency_key.len() < 16 || request.idempotency_key.len() > 128 {
        return Err(InstanceControlError::InvalidRequest);
    }
    if !matches!(request.scope, ControlScope::Read) && !request.confirmation {
        return Err(InstanceControlError::ConfirmationRequired);
    }
    Ok(())
}

pub fn validate_lan_control(
    envelope: &LanControlEnvelope,
    now_unix_ms: u128,
) -> Result<(), InstanceControlError> {
    if envelope.peer_id.trim().is_empty()
        || envelope.peer_id.len() > 128
        || envelope.target_instance_id.trim().is_empty()
        || envelope.target_instance_id.len() > 96
        || envelope.method.trim().is_empty()
        || envelope.method.len() > 64
        || serde_json::to_vec(&envelope.params)
            .map(|bytes| bytes.len() > 32 * 1024)
            .unwrap_or(true)
        || envelope.request_id.trim().is_empty()
        || envelope.request_id.len() > 128
        || envelope.nonce.len() < 16
        || envelope.nonce.len() > 256
        || envelope.expires_at_unix_ms < now_unix_ms
        || envelope.expires_at_unix_ms.saturating_sub(now_unix_ms) > 5 * 60 * 1000
    {
        return Err(InstanceControlError::InvalidRequest);
    }
    if !matches!(envelope.control.scope, ControlScope::Lan) {
        return Err(InstanceControlError::InvalidRequest);
    }
    let skill = default_process_control_skill();
    if !skill
        .read_operations
        .iter()
        .chain(skill.mutating_operations.iter())
        .any(|operation| operation == &envelope.method)
    {
        return Err(InstanceControlError::InvalidRequest);
    }
    validate_control(&envelope.control)
}

pub fn instance_id_for(endpoint: &str, pid: u32, started_at_unix_ms: u128) -> String {
    let mut h = Sha256::new();
    h.update(endpoint.as_bytes());
    h.update(pid.to_le_bytes());
    h.update(started_at_unix_ms.to_le_bytes());
    hex::encode(h.finalize())[..32].to_owned()
}

/// Return whether a process identifier currently refers to a live process.
/// This is intentionally a liveness probe only; it does not grant control.
pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        type Handle = *mut std::ffi::c_void;
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
            fn CloseHandle(handle: Handle) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        // SAFETY: The handle is checked for null and closed before returning.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        unsafe { CloseHandle(handle) != 0 }
    }
    #[cfg(unix)]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(any(windows, unix)))]
    {
        true
    }
}

#[derive(Debug, Clone)]
pub struct InstanceRegistry {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceRegistryDocument {
    format_version: u32,
    instances: Vec<InstanceInfo>,
}

impl InstanceRegistry {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn load(&self) -> Result<Vec<InstanceInfo>, InstanceControlError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        self.read_instances().map(|(instances, _)| instances)
    }

    pub fn register(&self, instance: InstanceInfo) -> Result<(), InstanceControlError> {
        validate_instance_id(&instance.instance_id)?;
        self.with_lock(|| {
            let mut instances = self.load_for_mutation()?;
            instances.retain(|item| {
                item.instance_id != instance.instance_id && item.pid != instance.pid
            });
            instances.push(instance);
            self.write(&instances)
        })
    }

    pub fn unregister(&self, instance_id: &str) -> Result<(), InstanceControlError> {
        validate_instance_id(instance_id)?;
        self.with_lock(|| {
            let mut instances = self.load_for_mutation()?;
            instances.retain(|item| item.instance_id != instance_id);
            self.write(&instances)
        })
    }

    pub fn prune(
        &self,
        live_pids: impl Fn(u32) -> bool,
    ) -> Result<Vec<InstanceInfo>, InstanceControlError> {
        self.with_lock(|| {
            let mut instances = self.load_for_mutation()?;
            instances.retain(|item| live_pids(item.pid));
            self.write(&instances)?;
            Ok(instances)
        })
    }

    fn read_instances(&self) -> Result<(Vec<InstanceInfo>, bool), InstanceControlError> {
        let bytes = fs::read(&self.path)?;
        if let Ok(instances) = serde_json::from_slice::<Vec<InstanceInfo>>(&bytes) {
            return Ok((instances, true));
        }
        let document: InstanceRegistryDocument = serde_json::from_slice(&bytes)?;
        if document.format_version > REGISTRY_FORMAT_VERSION {
            return Err(InstanceControlError::FutureRegistryFormat(
                document.format_version,
            ));
        }
        if document.format_version != REGISTRY_FORMAT_VERSION {
            return Err(InstanceControlError::UnsupportedRegistryFormat(
                document.format_version,
            ));
        }
        Ok((document.instances, false))
    }

    fn load_for_mutation(&self) -> Result<Vec<InstanceInfo>, InstanceControlError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        match self.read_instances() {
            Ok((instances, _)) => Ok(instances),
            Err(InstanceControlError::Json(_)) => {
                self.backup_malformed_file()?;
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, InstanceControlError>,
    ) -> Result<T, InstanceControlError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let lock_path = self.path.with_extension("json.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock_exclusive()?;
        operation()
    }

    fn backup_malformed_file(&self) -> Result<(), InstanceControlError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();
        let backup = self
            .path
            .with_file_name(format!("instances.corrupt-{timestamp}.json"));
        fs::rename(&self.path, backup)?;
        Ok(())
    }

    fn write(&self, instances: &[InstanceInfo]) -> Result<(), InstanceControlError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&InstanceRegistryDocument {
            format_version: REGISTRY_FORMAT_VERSION,
            instances: instances.to_vec(),
        })?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

pub fn local_endpoint(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\rivo-{}", stable_home_hash(home)))
    }
    #[cfg(not(windows))]
    {
        home.join("instance-control.sock")
    }
}

/// Send one JSON-RPC request to a local instance. The client deliberately uses
/// one request per connection so a crashed or stale peer cannot retain resources.
#[cfg(windows)]
pub async fn request_named_pipe(
    endpoint: &str,
    request: &Value,
    timeout: std::time::Duration,
) -> Result<Value, InstanceControlError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;
    let endpoint = endpoint.to_owned();
    let mut client = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || ClientOptions::new().open(endpoint)),
    )
    .await
    .map_err(|_| {
        InstanceControlError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "named pipe connect timed out",
        ))
    })?
    .map_err(|error| InstanceControlError::Io(std::io::Error::other(error)))??;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    tokio::time::timeout(timeout, client.write_all(&bytes))
        .await
        .map_err(|_| {
            InstanceControlError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "named pipe write timed out",
            ))
        })??;
    client.flush().await.map_err(InstanceControlError::Io)?;
    let mut line = String::new();
    tokio::time::timeout(timeout, BufReader::new(client).read_line(&mut line))
        .await
        .map_err(|_| {
            InstanceControlError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "named pipe read timed out",
            ))
        })??;
    if line.len() > 64 * 1024 {
        return Err(InstanceControlError::InvalidRequest);
    }
    let response: Value = serde_json::from_str(&line)?;
    if !response.is_object() || response.get("jsonrpc") != Some(&Value::String("2.0".into())) {
        return Err(InstanceControlError::InvalidResponse);
    }
    Ok(response)
}

#[cfg(not(windows))]
pub async fn request_named_pipe(
    _endpoint: &str,
    _request: &Value,
    _timeout: std::time::Duration,
) -> Result<Value, InstanceControlError> {
    Err(InstanceControlError::UnsupportedTransport)
}

/// Serve one bounded JSON-lines request on the current user's Windows Named Pipe.
/// The callback owns protocol validation and authorization; this layer only provides
/// framing and bounded I/O so it can be reused by app-server and test hosts.
#[cfg(windows)]
pub async fn serve_named_pipe_once<F, Fut>(
    name: &str,
    handler: F,
) -> Result<(), InstanceControlError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(name)
        .map_err(InstanceControlError::Io)?;
    server.connect().await.map_err(InstanceControlError::Io)?;
    let (reader, mut writer) = tokio::io::split(server);
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .await
        .map_err(InstanceControlError::Io)?;
    if line.len() > 64 * 1024 {
        return Err(InstanceControlError::InvalidRequest);
    }
    let mut response = serde_json::to_vec(&handler(line).await)?;
    response.push(b'\n');
    writer
        .write_all(&response)
        .await
        .map_err(InstanceControlError::Io)?;
    writer.shutdown().await.map_err(InstanceControlError::Io)
}
#[cfg(windows)]
#[allow(dead_code)]
fn stable_home_hash(home: &Path) -> String {
    let mut h = Sha256::new();
    h.update(home.to_string_lossy().as_bytes());
    hex::encode(h.finalize())[..20].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_stable_and_bounded() {
        assert_eq!(instance_id_for("pipe", 7, 9).len(), 32);
        assert!(validate_instance_id("a_b-1").is_ok());
        assert!(validate_instance_id("bad/id").is_err());
    }

    #[test]
    fn sidecar_node_id_is_generated_once_and_persisted_locally() {
        let root = tempfile::tempdir().unwrap();
        let first = load_or_create_node_id(root.path()).unwrap();
        let second = load_or_create_node_id(root.path()).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("node-"));
        assert!(!first.contains('/') && !first.contains('\\'));
    }

    #[test]
    fn workspace_binding_is_canonical_and_does_not_expose_root() {
        let dir = tempfile::tempdir().unwrap();
        let context = sidecar_node_context(
            "node-1",
            SidecarDeploymentType::LocalWorker,
            "workspace-1",
            dir.path(),
            "workspace_write",
            CapabilitySummary::default(),
            CapabilitySummary::default(),
        )
        .unwrap();
        assert_eq!(context.workspace_id, "workspace-1");
        assert!(!serde_json::to_string(&context)
            .unwrap()
            .contains(&dir.path().to_string_lossy().to_string()));
        assert!(host_capabilities()
            .capabilities
            .iter()
            .any(|capability| capability == "workspace.path_normalization"));
    }

    #[test]
    fn workspace_path_cannot_escape_bound_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let escaped = outside.path().join("secret.txt");
        assert!(matches!(
            validate_workspace_path(dir.path(), &escaped),
            Err(InstanceControlError::WorkspaceBindingMismatch)
        ));
        let inside = dir.path().join("new.txt");
        assert_eq!(
            validate_workspace_path(dir.path(), &inside).unwrap(),
            fs::canonicalize(dir.path()).unwrap().join("new.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_workspace_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            validate_workspace_root(&link),
            Err(InstanceControlError::WorkspaceRootSymlink)
        ));
    }
    #[test]
    fn registry_is_atomic_and_prunable() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InstanceRegistry::new(dir.path().join(REGISTRY_FILE));
        let item = InstanceInfo {
            instance_id: "one".into(),
            pid: 7,
            protocol_version: PROTOCOL_VERSION.into(),
            endpoint: "pipe".into(),
            workspace: "w".into(),
            started_at_unix_ms: 1,
            state: InstanceState::Running,
            capabilities: vec!["thread_read".into()],
        };
        reg.register(item.clone()).unwrap();
        assert_eq!(reg.load().unwrap(), vec![item]);
        reg.prune(|_| false).unwrap();
        assert!(reg.load().unwrap().is_empty());
    }

    #[test]
    fn legacy_registry_is_upgraded_on_mutation_without_losing_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REGISTRY_FILE);
        let item = InstanceInfo {
            instance_id: "legacy".into(),
            pid: 7,
            protocol_version: PROTOCOL_VERSION.into(),
            endpoint: "pipe".into(),
            workspace: "w".into(),
            started_at_unix_ms: 1,
            state: InstanceState::Running,
            capabilities: vec![],
        };
        fs::write(&path, serde_json::to_vec(&vec![item.clone()]).unwrap()).unwrap();
        let registry = InstanceRegistry::new(&path);

        assert_eq!(registry.load().unwrap(), vec![item.clone()]);
        registry.prune(|_| true).unwrap();
        let document: InstanceRegistryDocument =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(document.format_version, REGISTRY_FORMAT_VERSION);
        assert_eq!(document.instances, vec![item]);
    }

    #[test]
    fn malformed_registry_is_backed_up_and_future_registry_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REGISTRY_FILE);
        let registry = InstanceRegistry::new(&path);
        fs::write(&path, b"{not-json").unwrap();
        registry.prune(|_| true).unwrap();
        assert!(path.exists());
        assert!(fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("instances.corrupt-")));

        let future = br#"{"format_version":2,"instances":[]}"#;
        fs::write(&path, future).unwrap();
        assert!(matches!(
            registry.prune(|_| true),
            Err(InstanceControlError::FutureRegistryFormat(2))
        ));
        assert_eq!(fs::read(&path).unwrap(), future);
    }

    #[test]
    fn zero_pid_is_never_reported_as_live() {
        assert!(!process_is_running(0));
    }
    #[test]
    fn mutating_scope_requires_confirmation() {
        let req = ControlRequest {
            scope: ControlScope::Lifecycle,
            confirmation: false,
            idempotency_key: "1234567890123456".into(),
        };
        assert!(matches!(
            validate_control(&req),
            Err(InstanceControlError::ConfirmationRequired)
        ));
    }

    #[test]
    fn default_process_control_manifest_is_read_only_by_default() {
        let skill = default_process_control_skill();
        assert!(skill.default_enabled);
        assert!(skill.read_operations.contains(&"instance/list".into()));
        assert!(skill.mutating_operations.contains(&"thread/fork".into()));
        assert!(skill.scopes.contains(&ControlScope::Lan));
    }

    #[test]
    fn skill_registry_loads_the_default_manifest_by_name() {
        let registry = SkillRegistry::load_defaults();
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            vec!["process-control"]
        );
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn lan_control_envelope_is_bounded_and_time_limited() {
        let envelope = LanControlEnvelope {
            peer_id: "peer-1".into(),
            target_instance_id: "instance-1".into(),
            method: "thread/read".into(),
            params: serde_json::json!({"thread_ref":"thread-1"}),
            request_id: "request-1".into(),
            nonce: "1234567890123456".into(),
            expires_at_unix_ms: 1_000,
            control: ControlRequest {
                scope: ControlScope::Lan,
                confirmation: true,
                idempotency_key: "1234567890123456".into(),
            },
        };
        assert!(validate_lan_control(&envelope, 900).is_ok());
        assert!(validate_lan_control(&envelope, 1_001).is_err());
        assert!(validate_lan_control(
            &LanControlEnvelope {
                expires_at_unix_ms: 301_001,
                ..envelope
            },
            1_000
        )
        .is_err());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn client_reports_unsupported_transport_off_windows() {
        let error = request_named_pipe(
            "\\\\.\\pipe\\rivo-test",
            &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"health/read"}),
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, InstanceControlError::UnsupportedTransport));
    }
}

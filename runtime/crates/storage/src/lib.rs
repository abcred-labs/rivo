use fs2::FileExt;
use rand_core::{OsRng, RngCore};
use rivo_a2a::{A2aPeerId, A2aTaskId, A2aTaskState, A2aTrustSnapshot, A2aTrustState};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use time::{format_description, format_description::well_known::Rfc3339, OffsetDateTime};

pub mod worker;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("本机历史存储暂不可用: {0}")]
    Unavailable(String),
    #[error("SQLite 错误: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("文件系统错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("序列化错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("历史记录不存在: {0}")]
    NotFound(String),
    #[error("历史会话已按本地留存策略过期: {0}")]
    Expired(String),
    #[error("不支持的 SQLite schema 版本: {0}；请显式受控重建会话，非会话数据必须保留；不会自动迁移或清空")]
    UnsupportedSchema(u64),
    #[error("历史事件损坏（session={session_id}, sequence={sequence}）：{message}")]
    CorruptEvent {
        session_id: String,
        sequence: u64,
        message: String,
    },
    #[error("A2A 状态无效: {0}")]
    InvalidA2aState(String),
    #[error("Artifact 参数无效: {0}")]
    InvalidArtifact(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub session_id: String,
    pub turn_id: String,
    pub sequence: u64,
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub session_id: String,
    pub turn_id: String,
    pub sequence: u64,
    pub role: String,
    pub body: String,
}

/// 用户选择与解析结果分离：Auto 永远不会被保存成上次选中的 Provider ID。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderSelection {
    #[default]
    Auto,
    Explicit {
        provider_id: String,
    },
}

impl ProviderSelection {
    pub fn explicit_id(&self) -> Option<&str> {
        match self {
            Self::Auto => None,
            Self::Explicit { provider_id } => Some(provider_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub vendor: String,
    pub model: String,
    pub provider: ProviderSelection,
}

/// 无秘密的历史路由快照，不构成恢复时的当前可用性证明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredModelRoute {
    pub selection: ModelSelection,
    pub provider_id: String,
    pub protocol: String,
    pub base_url: String,
    pub priority: u16,
}

/// FEAT-037 本机跨会话消息。`state` 为 `pending` / `held` / `delivered` /
/// `expired` / `rejected`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSessionMessage {
    pub message_id: String,
    pub from_session_id: String,
    pub to_session_id: String,
    pub kind: String,
    pub body: String,
    pub state: String,
    pub created_at: String,
    pub expires_at: String,
}

/// 消息默认有效期（分钟），与 FEAT-037 契约一致。
pub const SESSION_MESSAGE_TTL_MINUTES: u32 = 30;

fn session_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSessionMessage> {
    Ok(StoredSessionMessage {
        message_id: row.get(0)?,
        from_session_id: row.get(1)?,
        to_session_id: row.get(2)?,
        kind: row.get(3)?,
        body: row.get(4)?,
        state: row.get(5)?,
        created_at: row.get(6)?,
        expires_at: row.get(7)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAgentTask {
    pub task_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub label: String,
    pub state: String,
    pub summary: Option<String>,
    pub usage_json: Option<String>,
    pub terminal_reason: Option<String>,
}

/// Sidecar task recovery index. The event journal remains the source of truth;
/// this table only makes idempotency, leases and recovery bounded and queryable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSidecarTask {
    pub task_id: String,
    pub intent_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub state: String,
    pub lease_epoch: u64,
    pub expires_at: String,
    pub last_event_sequence: u64,
    /// Bounded JSON object keyed by tool_call_id. This is recovery metadata,
    /// never a tool argument or result body.
    pub tool_call_status: String,
    pub terminal_reason: Option<String>,
}

/// Independent lease index. Task rows retain idempotency and terminal state;
/// lease rows can be renewed or expired without rewriting that history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSidecarLease {
    pub task_id: String,
    pub lease_epoch: u64,
    pub expires_at: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEventCursor {
    pub session_id: String,
    pub next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredArtifact {
    pub artifact_id: String,
    pub node_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub kind: String,
    pub size: u64,
    pub hash: String,
    pub expires_at: String,
    pub download_scope: String,
    pub storage_key: String,
    pub state: String,
}

pub const MAX_SIDECAR_ARTIFACT_BYTES: u64 = 10 * 1024 * 1024;

/// OS ownership is released on crash. A second Sidecar sharing this data root
/// must not reinterpret the first process's live tasks as restart recovery.
pub struct SidecarRuntimeLease {
    _file: fs::File,
}

impl SidecarRuntimeLease {
    pub fn acquire(data_root: &Path) -> Result<Self, StorageError> {
        let directory = data_root.join("rivo");
        fs::create_dir_all(&directory)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("sidecar-runtime.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
            StorageError::Unavailable("sidecar data root already has an owner".into())
        })?;
        Ok(Self { _file: file })
    }
}
static ARTIFACT_READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ArtifactReadPermit;

impl ArtifactReadPermit {
    fn acquire() -> Result<Self, StorageError> {
        use std::sync::atomic::Ordering;
        ARTIFACT_READERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                StorageError::InvalidArtifact("artifact read concurrency limit exceeded".into())
            })
    }
}

impl Drop for ArtifactReadPermit {
    fn drop(&mut self) {
        ARTIFACT_READERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Runtime-owned artifact bytes. The filesystem path is private to this store;
/// callers exchange only the returned metadata and logical artifact id.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    workspace_binding: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub artifact_id: String,
    pub node_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub kind: String,
    pub expires_at: String,
    pub download_scope: String,
}

impl ArtifactStore {
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            root: workspace_root.as_ref().join(".rivo").join("artifacts"),
            workspace_binding: fs::canonicalize(workspace_root).ok(),
        }
    }

    fn validate_root(&self, create: bool) -> Result<(), StorageError> {
        let workspace = self
            .root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| StorageError::InvalidArtifact("invalid artifact root".into()))?;
        if artifact_link(&fs::symlink_metadata(workspace)?)
            || self.workspace_binding.as_ref() != Some(&fs::canonicalize(workspace)?)
        {
            return Err(StorageError::InvalidArtifact(
                "workspace binding changed".into(),
            ));
        }
        for path in [self.root.parent().unwrap(), self.root.as_path()] {
            if create && !path.exists() {
                match fs::create_dir(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let metadata = fs::symlink_metadata(path)?;
            if artifact_link(&metadata) || !metadata.is_dir() {
                return Err(StorageError::InvalidArtifact(
                    "artifact directory is not runtime-owned".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn put(
        &self,
        metadata: &ArtifactMetadata,
        bytes: &[u8],
    ) -> Result<StoredArtifact, StorageError> {
        validate_artifact_id(&metadata.artifact_id)?;
        validate_artifact_scope(metadata)?;
        if metadata.kind.is_empty()
            || metadata.kind.len() > 64
            || bytes.len() as u64 > MAX_SIDECAR_ARTIFACT_BYTES
        {
            return Err(StorageError::InvalidArtifact(
                "kind or size exceeds limit".into(),
            ));
        }
        self.validate_root(true)?;
        let hash = hex::encode(Sha256::digest(bytes));
        let path = self.root.join(&metadata.artifact_id);
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(&path)
            .map_err(|error| StorageError::Io(error.error))?;
        Ok(StoredArtifact {
            artifact_id: metadata.artifact_id.clone(),
            node_id: metadata.node_id.clone(),
            workspace_id: metadata.workspace_id.clone(),
            session_id: metadata.session_id.clone(),
            kind: metadata.kind.clone(),
            size: bytes.len() as u64,
            hash,
            expires_at: metadata.expires_at.clone(),
            download_scope: metadata.download_scope.clone(),
            storage_key: metadata.artifact_id.clone(),
            state: "ready".into(),
        })
    }

    pub fn read(&self, artifact: &StoredArtifact) -> Result<Vec<u8>, StorageError> {
        validate_artifact_id(&artifact.storage_key)?;
        validate_stored_artifact_scope(artifact)?;
        let expires_at = OffsetDateTime::parse(&artifact.expires_at, &Rfc3339)
            .map_err(|_| StorageError::InvalidArtifact("invalid artifact expiry".into()))?;
        if expires_at <= OffsetDateTime::now_utc() {
            return Err(StorageError::InvalidArtifact("artifact has expired".into()));
        }
        self.read_content(artifact)
    }

    fn read_content(&self, artifact: &StoredArtifact) -> Result<Vec<u8>, StorageError> {
        let _permit = ArtifactReadPermit::acquire()?;
        validate_artifact_id(&artifact.storage_key)?;
        if artifact.size > MAX_SIDECAR_ARTIFACT_BYTES {
            return Err(StorageError::InvalidArtifact(
                "artifact exceeds size limit".into(),
            ));
        }
        self.validate_root(false)?;
        let path = self.root.join(&artifact.storage_key);
        let metadata = fs::symlink_metadata(&path)?;
        if artifact_link(&metadata) || !metadata.is_file() {
            return Err(StorageError::InvalidArtifact(
                "artifact must be a regular runtime-owned file".into(),
            ));
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        if artifact_link(&metadata) || !metadata.is_file() || metadata.len() != artifact.size {
            return Err(StorageError::InvalidArtifact(
                "artifact file or size changed".into(),
            ));
        }
        let started = std::time::Instant::now();
        let mut bytes = Vec::with_capacity(artifact.size as usize);
        let mut chunk = [0; 8192];
        loop {
            if started.elapsed() > Duration::from_secs(2) {
                return Err(StorageError::InvalidArtifact(
                    "artifact read deadline exceeded".into(),
                ));
            }
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            if bytes.len() as u64 + count as u64 > artifact.size {
                return Err(StorageError::InvalidArtifact(
                    "artifact grew beyond registered size".into(),
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        if bytes.len() as u64 != artifact.size
            || hex::encode(Sha256::digest(&bytes)) != artifact.hash
        {
            return Err(StorageError::InvalidArtifact(
                "hash or size mismatch".into(),
            ));
        }
        Ok(bytes)
    }

    pub fn cleanup(&self, artifact: &StoredArtifact) -> Result<(), StorageError> {
        validate_artifact_id(&artifact.storage_key)?;
        let path = self.root.join(&artifact.storage_key);
        // Expiry does not prevent cleanup, but a substituted or modified user
        // file must never be deleted merely because it has an old artifact id.
        match self.read_content(artifact) {
            Ok(_) => {}
            Err(StorageError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(())
            }
            Err(error) => return Err(error),
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn artifact_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // includes directory junctions
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn validate_artifact_id(value: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(StorageError::InvalidArtifact(
            "artifact id is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_artifact_scope(metadata: &ArtifactMetadata) -> Result<(), StorageError> {
    for (name, value) in [
        ("node_id", metadata.node_id.as_str()),
        ("workspace_id", metadata.workspace_id.as_str()),
        ("session_id", metadata.session_id.as_str()),
    ] {
        if value.is_empty() || value.len() > 128 || value.contains(['/', '\\']) {
            return Err(StorageError::InvalidArtifact(format!(
                "{name} scope is invalid"
            )));
        }
    }
    if metadata.download_scope != "session" {
        return Err(StorageError::InvalidArtifact(
            "only session-scoped downloads are supported".into(),
        ));
    }
    let expires_at = OffsetDateTime::parse(&metadata.expires_at, &Rfc3339)
        .map_err(|_| StorageError::InvalidArtifact("invalid artifact expiry".into()))?;
    let _ = expires_at;
    Ok(())
}

fn validate_stored_artifact_scope(artifact: &StoredArtifact) -> Result<(), StorageError> {
    for (name, value) in [
        ("node_id", artifact.node_id.as_str()),
        ("workspace_id", artifact.workspace_id.as_str()),
        ("session_id", artifact.session_id.as_str()),
    ] {
        if value.is_empty() || value.len() > 128 || value.contains(['/', '\\']) {
            return Err(StorageError::InvalidArtifact(format!(
                "{name} scope is invalid"
            )));
        }
    }
    if artifact.download_scope != "session" {
        return Err(StorageError::InvalidArtifact(
            "only session-scoped downloads are supported".into(),
        ));
    }
    OffsetDateTime::parse(&artifact.expires_at, &Rfc3339)
        .map_err(|_| StorageError::InvalidArtifact("invalid artifact expiry".into()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGoal {
    pub goal_id: String,
    pub session_id: String,
    pub objective: String,
    pub status: String,
    pub budget: u64,
    pub used_budget: u64,
    pub revision: u64,
    pub last_evidence_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTaskPlan {
    pub plan_id: String,
    pub session_id: String,
    pub objective: String,
    pub revision: u64,
    pub status: String,
    pub payload: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 前端动作的完整本机审计记录。payload 由调用者提供；API Key 脱敏在
/// Session/CLI 边界完成，从而让数据库和导出使用相同规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiAuditEvent {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub action: String,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticIndexEntry {
    pub source: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub event_sequence: Option<u64>,
    pub audit_id: Option<i64>,
    pub created_at: String,
    pub stage: String,
    pub code: String,
    pub retryable: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub created_at: String,
    /// First persisted user prompt, bounded at query time for history lists.
    #[serde(default)]
    pub task_summary: Option<String>,
    #[serde(default)]
    pub expiration_reason: Option<String>,
    pub event_count: u64,
    pub message_count: u64,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub expired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSummary {
    pub session_id: String,
    pub turn_id: String,
    pub created_at: String,
    pub event_count: u64,
    pub message_count: u64,
}

/// A user-managed local folder project. The host UI owns presentation and
/// selection; storage only keeps the durable folder identity and ordering data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub display_name: String,
    pub workspace_root: String,
    pub is_default: bool,
    pub last_opened_at: String,
}

/// Public workspace terminology. `Project` remains as an internal storage
/// compatibility name while callers migrate to the workspace-first API.
pub type Workspace = Project;

/// A process-scoped shared lease for a Workspace. Readers can share the lease
/// so independent local sessions remain possible; destructive Workspace
/// removal requires an exclusive lease and therefore fails while any session
/// is still alive.
#[derive(Debug)]
pub struct WorkspaceLease {
    file: Option<fs::File>,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.as_ref() {
            let _ = fs2::FileExt::unlock(file);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScope {
    CurrentWorkspace(String),
    WorkspaceId(String),
    AllWorkspaces,
    Unscoped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub workspace: String,
    pub mode: String,
    pub body: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub id: String,
    pub workspace: String,
    pub mode: String,
    pub body: String,
    pub source: String,
    pub body_sha256: String,
}

/// 持久化的、可撤销的中等模式会话授权。只保存风险类别与边界，绝不保存
/// 工具正文、命令参数或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionGrant {
    pub session_id: String,
    pub workspace: String,
    pub policy: String,
    pub grant_key: String,
}

/// A2A 对端的本机信任记录。身份引用只指向 OS keychain；私钥、证书正文、
/// claim token 与远端请求正文绝不写入 SQLite。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredA2aPeer {
    pub snapshot: A2aTrustSnapshot,
    pub identity_ref: Option<String>,
}

/// A2A 任务恢复索引。它只用于幂等、取消和终态恢复，不能替代 Session history，
/// 因此不存远端 message、Artifact、路径或本机 SessionId/TurnId。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredA2aTask {
    pub peer_id: A2aPeerId,
    pub task_id: A2aTaskId,
    pub request_sha256: String,
    pub state: A2aTaskState,
    pub revision: Option<u64>,
}

pub struct Store {
    connection: Connection,
    path: PathBuf,
    _database_lease: Option<DatabaseLease>,
}

/// A process-wide lease for a database. Normal stores share the lease while
/// maintenance operations take it exclusively, so backup and replacement see
/// one stable application-level view of the database.
struct DatabaseLease {
    file: fs::File,
}

impl Drop for DatabaseLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub const DEFAULT_RETENTION_DAYS: u64 = 30;
pub const SCHEMA_VERSION: u64 = 18;
pub const DEFAULT_MAX_JOURNAL_RECORDS: u64 = 100_000;
pub const DEFAULT_MAX_JOURNAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Read-only storage facts that remain available even when the database cannot
/// be opened by the current session runtime.
#[derive(Debug, Clone, Serialize)]
pub struct StorageHealth {
    pub path: String,
    pub exists: bool,
    pub schema_version: Option<u64>,
    pub tables: Vec<String>,
    pub quick_check: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseBackup {
    pub source: String,
    pub destination: String,
    pub schema_version: Option<u64>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomaticMigrationReport {
    pub already_current: bool,
    pub backup: Option<DatabaseBackup>,
    pub bound_session_count: u64,
    pub unscoped_session_count: u64,
}

/// Inspect a database without calling `Store::open`, so diagnostics continue
/// to work when the session schema is intentionally rejected.
pub fn inspect_database(path: impl AsRef<Path>) -> StorageHealth {
    let path = path.as_ref();
    let mut health = StorageHealth {
        path: path.display().to_string(),
        exists: path.is_file(),
        schema_version: None,
        tables: Vec::new(),
        quick_check: None,
        error: None,
    };
    if !health.exists {
        return health;
    }
    let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(error) => {
            health.error = Some(error.to_string());
            return health;
        }
    };
    health.schema_version = connection
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .ok();
    match connection.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name") {
        Ok(mut statement) => match statement.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => health.tables = rows.filter_map(Result::ok).collect(),
            Err(error) => health.error = Some(error.to_string()),
        },
        Err(error) => health.error = Some(error.to_string()),
    }
    health.quick_check = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .ok();
    health
}

/// Create a consistent SQLite online backup through a same-directory temporary
/// file. The source is read only and a pre-existing destination is rejected.
pub fn backup_database(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<DatabaseBackup, StorageError> {
    let source = source.as_ref();
    if !source.is_file() {
        return Err(StorageError::Unavailable(format!(
            "数据库不存在: {}",
            source.display()
        )));
    }
    let _lease = acquire_database_lease(source, false)?;
    backup_database_unlocked(source, destination.as_ref())
}

fn backup_database_unlocked(
    source: &Path,
    destination: &Path,
) -> Result<DatabaseBackup, StorageError> {
    if !source.is_file() {
        return Err(StorageError::Unavailable(format!(
            "数据库不存在: {}",
            source.display()
        )));
    }
    if destination.exists() {
        return Err(StorageError::Unavailable(format!(
            "备份目标已存在: {}",
            destination.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        StorageError::Unavailable(format!("备份目标没有父目录: {}", destination.display()))
    })?;
    if !parent.is_dir() {
        return Err(StorageError::Unavailable(format!(
            "备份目录不存在: {}",
            parent.display()
        )));
    }
    let temporary = temporary_path(destination, "backup");
    if temporary.exists() {
        return Err(StorageError::Unavailable(format!(
            "备份临时目标已存在: {}",
            temporary.display()
        )));
    }
    let result = (|| -> Result<DatabaseBackup, StorageError> {
        let source_connection =
            Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut destination_connection = Connection::open(&temporary)?;
        let backup =
            rusqlite::backup::Backup::new(&source_connection, &mut destination_connection)?;
        backup.run_to_completion(128, Duration::from_millis(10), None)?;
        drop(backup);
        drop(destination_connection);
        let backup_hash = file_sha256(&temporary)?;
        if inspect_database(&temporary).quick_check.as_deref() != Some("ok") {
            return Err(StorageError::Unavailable(
                "备份校验失败：SQLite quick_check 未通过".into(),
            ));
        }
        fs::rename(&temporary, destination)
            .map_err(|error| StorageError::Unavailable(error.to_string()))?;
        Ok(DatabaseBackup {
            source: source.display().to_string(),
            destination: destination.display().to_string(),
            schema_version: inspect_database(source).schema_version,
            sha256: backup_hash,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Rebuilds a schema 14 database as the current schema after a verified external
/// backup. Session-owned rows are deliberately not copied.
pub fn reset_schema_14_sessions(
    path: impl AsRef<Path>,
    backup: impl AsRef<Path>,
) -> Result<(), StorageError> {
    let path = path.as_ref();
    let backup = backup.as_ref();
    let _lease = acquire_database_lease(path, true)?;
    reset_schema_14_sessions_locked(path, backup)
}

fn reset_schema_14_sessions_locked(path: &Path, backup: &Path) -> Result<(), StorageError> {
    let health = inspect_database(path);
    if health.schema_version != Some(14) {
        return Err(StorageError::UnsupportedSchema(
            health.schema_version.unwrap_or(0),
        ));
    }
    if !backup.is_file() {
        return Err(StorageError::Unavailable(format!(
            "找不到已验证备份: {}",
            backup.display()
        )));
    }
    let backup_health = inspect_database(backup);
    if backup_health.schema_version != Some(14)
        || backup_health.quick_check.as_deref() != Some("ok")
    {
        return Err(StorageError::Unavailable(
            "备份校验失败：备份不是完整的 schema 14 SQLite 快照".into(),
        ));
    }
    let temporary = temporary_path(path, "reset");
    let displaced = temporary_path(path, "pre-reset");
    if temporary.exists() || displaced.exists() {
        return Err(StorageError::Unavailable(
            "已有未完成的会话重建临时文件".into(),
        ));
    }
    let result = (|| -> Result<(), StorageError> {
        let target_store = Store::open(&temporary)?;
        let target = &target_store.connection;
        target.execute(
            "ATTACH DATABASE ?1 AS legacy",
            params![path.to_string_lossy().as_ref()],
        )?;
        let transaction = target.unchecked_transaction()?;
        for table in [
            "projects",
            "memories",
            "memory_candidates",
            "a2a_peers",
            "a2a_tasks",
            "a2a_audit",
        ] {
            copy_shared_columns(target, &transaction, table)?;
        }
        transaction.execute("INSERT INTO memories_fts(id, workspace, mode, body) SELECT id, workspace, mode, body FROM memories", [])?;
        transaction.commit()?;
        target.execute_batch("DETACH DATABASE legacy")?;
        target_store.close()?;

        // Keep the original in place until the fresh database is complete.
        fs::rename(path, &displaced)
            .map_err(|error| StorageError::Unavailable(error.to_string()))?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::rename(&displaced, path);
            return Err(StorageError::Unavailable(error.to_string()));
        }
        let _ = fs::remove_file(&displaced);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        if displaced.exists() && !path.exists() {
            let _ = fs::rename(&displaced, path);
        }
    }
    result
}

/// Explicitly upgrades the current schema 17 database to schema 18. The
/// caller must provide a verified backup; the source is replaced only after a
/// complete schema-18 copy has been built and checked.
pub fn migrate_schema_17_workspaces(
    path: impl AsRef<Path>,
    backup: impl AsRef<Path>,
) -> Result<(), StorageError> {
    let path = path.as_ref();
    let backup = backup.as_ref();
    let _lease = acquire_database_lease(path, true)?;
    migrate_schema_17_workspaces_locked(path, backup).map(|_| ())
}

/// Automatically creates a verified backup and upgrades schema 17 in one
/// exclusive maintenance operation. Schema 18 is idempotent and does not
/// create another backup.
pub fn auto_migrate_schema_17_workspaces(
    path: impl AsRef<Path>,
) -> Result<AutomaticMigrationReport, StorageError> {
    let path = path.as_ref();
    if !path.is_file() {
        return Err(StorageError::Unavailable(format!(
            "数据库不存在: {}",
            path.display()
        )));
    }
    let _lease = acquire_database_lease(path, true)?;
    let source_health = inspect_database(path);
    if source_health.schema_version == Some(SCHEMA_VERSION) {
        if source_health.quick_check.as_deref() != Some("ok") {
            return Err(StorageError::Unavailable(
                "源数据库校验失败：SQLite quick_check 未通过".into(),
            ));
        }
        return Ok(AutomaticMigrationReport {
            already_current: true,
            backup: None,
            bound_session_count: 0,
            unscoped_session_count: 0,
        });
    }
    if source_health.schema_version != Some(17) {
        return Err(StorageError::UnsupportedSchema(
            source_health.schema_version.unwrap_or(0),
        ));
    }
    if source_health.quick_check.as_deref() != Some("ok") {
        return Err(StorageError::Unavailable(
            "源数据库校验失败：SQLite quick_check 未通过".into(),
        ));
    }
    let backup_path = automatic_backup_path(path)?;
    let backup = backup_database_unlocked(path, &backup_path)?;
    let (bound_session_count, unscoped_session_count) =
        migrate_schema_17_workspaces_locked(path, &backup_path)?;
    Ok(AutomaticMigrationReport {
        already_current: false,
        backup: Some(backup),
        bound_session_count,
        unscoped_session_count,
    })
}

fn migrate_schema_17_workspaces_locked(
    path: &Path,
    backup: &Path,
) -> Result<(u64, u64), StorageError> {
    let source_health = inspect_database(path);
    if source_health.schema_version == Some(18) {
        return Ok((0, 0));
    }
    if source_health.schema_version != Some(17) {
        return Err(StorageError::UnsupportedSchema(
            inspect_database(path).schema_version.unwrap_or(0),
        ));
    }
    if source_health.quick_check.as_deref() != Some("ok") {
        return Err(StorageError::Unavailable(
            "源数据库校验失败：SQLite quick_check 未通过".into(),
        ));
    }
    let backup_health = inspect_database(backup);
    if !backup.is_file()
        || backup_health.schema_version != Some(17)
        || backup_health.quick_check.as_deref() != Some("ok")
    {
        return Err(StorageError::Unavailable(
            "备份校验失败：需要完整的 schema 17 SQLite 快照".into(),
        ));
    }
    let temporary = temporary_path(path, "workspace-migrate");
    let displaced = temporary_path(path, "pre-workspace-migrate");
    if temporary.exists() || displaced.exists() {
        return Err(StorageError::Unavailable(
            "已有未完成的 workspace 数据迁移临时文件".into(),
        ));
    }
    let result = (|| -> Result<(u64, u64), StorageError> {
        let target_store = Store::open(&temporary)?;
        let target = &target_store.connection;
        target.execute(
            "ATTACH DATABASE ?1 AS legacy",
            params![path.to_string_lossy().as_ref()],
        )?;
        let transaction = target.unchecked_transaction()?;
        for table in [
            "projects",
            "sessions",
            "turns",
            "events",
            "messages",
            "memories",
            "memory_candidates",
            "ui_audit_events",
            "diagnostic_index",
            "expired_sessions",
            "session_permission_grants",
            "agent_tasks",
            "a2a_peers",
            "a2a_tasks",
            "a2a_audit",
            "goals",
            "task_plans",
        ] {
            copy_shared_columns(target, &transaction, table)?;
        }
        normalize_migrated_projects(&transaction)?;
        let legacy_sessions = {
            let mut statement = transaction
                .prepare("SELECT id, workspace_root, project_id FROM legacy.sessions")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (session_id, workspace_root, project_id) in legacy_sessions {
            if project_id.is_some() {
                continue;
            }
            let Some(workspace_root) = workspace_root else {
                continue;
            };
            let Ok(root) = normalize_workspace_path(Path::new(&workspace_root)) else {
                continue;
            };
            let identity = workspace_identity(&root);
            let existing = transaction
                .query_row(
                    "SELECT id FROM projects WHERE workspace_root=?1",
                    params![root.display().to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let workspace_id = if let Some(id) = existing {
                id
            } else {
                let id = format!("workspace-legacy-{}", &sha256_text(&identity)[..24]);
                let display_name = root
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                transaction.execute(
                    "INSERT OR IGNORE INTO projects(id, display_name, workspace_root, is_default, last_opened_at) VALUES(?1, ?2, ?3, 0, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                    params![id, display_name, root.display().to_string()],
                )?;
                id
            };
            transaction.execute(
                "UPDATE sessions SET project_id=?2, workspace_root=?3 WHERE id=?1 AND project_id IS NULL",
                params![session_id, workspace_id, root.display().to_string()],
            )?;
        }
        transaction.execute(
            "INSERT INTO memories_fts(id, workspace, mode, body)\
             SELECT id, workspace, mode, body FROM memories",
            [],
        )?;
        transaction.execute("UPDATE schema_version SET version=18", [])?;
        let bound_session_count = transaction.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id IS NOT NULL",
            [],
            |row| row.get::<_, u64>(0),
        )?;
        let unscoped_session_count = transaction.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id IS NULL",
            [],
            |row| row.get::<_, u64>(0),
        )?;
        transaction.commit()?;
        target.execute_batch("DETACH DATABASE legacy")?;
        target_store.close()?;

        fs::rename(path, &displaced)
            .map_err(|error| StorageError::Unavailable(error.to_string()))?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::rename(&displaced, path);
            return Err(StorageError::Unavailable(error.to_string()));
        }
        let _ = fs::remove_file(&displaced);
        Ok((bound_session_count, unscoped_session_count))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        if displaced.exists() && !path.exists() {
            let _ = fs::rename(&displaced, path);
        }
    }
    result
}

fn normalize_migrated_projects(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let projects = {
        let mut statement =
            transaction.prepare("SELECT id, workspace_root FROM projects ORDER BY id")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut identities = std::collections::HashMap::new();
    for (project_id, workspace_root) in projects {
        let Ok(root) = normalize_workspace_path(Path::new(&workspace_root)) else {
            transaction.execute(
                "UPDATE sessions SET project_id=NULL WHERE project_id=?1",
                params![project_id],
            )?;
            transaction.execute("DELETE FROM projects WHERE id=?1", params![project_id])?;
            continue;
        };
        let identity = workspace_identity(&root);
        if let Some(existing_id) = identities.get(&identity) {
            transaction.execute(
                "UPDATE sessions SET project_id=?1, workspace_root=(SELECT workspace_root FROM projects WHERE id=?1) WHERE project_id=?2",
                params![existing_id, project_id],
            )?;
            transaction.execute("DELETE FROM projects WHERE id=?1", params![project_id])?;
            continue;
        }
        let canonical_root = root.display().to_string();
        transaction.execute(
            "UPDATE projects SET workspace_root=?2 WHERE id=?1",
            params![project_id, canonical_root],
        )?;
        transaction.execute(
            "UPDATE sessions SET workspace_root=?2 WHERE project_id=?1",
            params![project_id, canonical_root],
        )?;
        identities.insert(identity, project_id);
    }
    Ok(())
}

fn copy_shared_columns(
    connection: &Connection,
    target: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<(), StorageError> {
    let source_columns = table_columns(connection, &format!("legacy.{table}"))?;
    if source_columns.is_empty() {
        return Ok(());
    }
    let target_columns = table_columns(connection, table)?;
    let columns = target_columns
        .into_iter()
        .filter(|column| source_columns.contains(column))
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(());
    }
    let joined = columns.join(", ");
    target.execute_batch(&format!(
        "INSERT INTO {table} ({joined}) SELECT {joined} FROM legacy.{table};"
    ))?;
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, StorageError> {
    let pragma = if let Some((database, table)) = table.split_once('.') {
        format!("PRAGMA {database}.table_info({table})")
    } else {
        format!("PRAGMA table_info({table})")
    };
    let mut statement = connection.prepare(&pragma)?;
    let columns = statement
        .query_map([], |row| row.get(1))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(columns)
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        display_name: row.get(1)?,
        workspace_root: row.get(2)?,
        is_default: row.get::<_, i64>(3)? != 0,
        last_opened_at: row.get(4)?,
    })
}

/// Workspace paths are resolved before they become durable identities. The
/// database stores the canonical display form; identity comparison applies
/// Windows' case-insensitive filesystem semantics.
pub fn normalize_workspace_path(path: &Path) -> Result<PathBuf, StorageError> {
    let canonical = path.canonicalize().map_err(|error| {
        StorageError::Unavailable(format!(
            "Workspace 目录无法解析: {} ({error})",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(StorageError::Unavailable(format!(
            "Workspace 不是目录: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn workspace_identity(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn workspace_lock_path(database_path: &Path, workspace_id: &str) -> PathBuf {
    let parent = database_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(format!(".workspace-{}.lock", sha256_text(workspace_id)))
}

fn temporary_path(path: &Path, suffix: &str) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("db");
    path.with_extension(format!("{extension}.{suffix}.tmp"))
}

fn file_sha256(path: &Path) -> Result<String, StorageError> {
    let bytes = fs::read(path).map_err(|error| StorageError::Unavailable(error.to_string()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn database_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("rivo.db");
    path.with_file_name(format!("{file_name}.lock"))
}

fn acquire_database_lease(
    path: &Path,
    exclusive: bool,
) -> Result<Option<DatabaseLease>, StorageError> {
    if path == Path::new(":memory:") {
        return Ok(None);
    }
    let lock_path = database_lock_path(path);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            StorageError::Unavailable(format!(
                "无法打开数据库维护锁: {} ({error})",
                lock_path.display()
            ))
        })?;
    let lock_result = if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)
    } else {
        fs2::FileExt::lock_shared(&file)
    };
    lock_result.map_err(|error| {
        StorageError::Unavailable(format!(
            "数据库正在被其他 Rivo 进程使用: {} ({error})",
            lock_path.display()
        ))
    })?;
    Ok(Some(DatabaseLease { file }))
}

fn automatic_backup_path(path: &Path) -> Result<PathBuf, StorageError> {
    automatic_backup_path_at(path, OffsetDateTime::now_utc())
}

fn automatic_backup_path_at(
    path: &Path,
    timestamp: OffsetDateTime,
) -> Result<PathBuf, StorageError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| {
            StorageError::Unavailable(format!("数据库没有可用的备份目录: {}", path.display()))
        })?;
    if !parent.is_dir() {
        return Err(StorageError::Unavailable(format!(
            "数据库备份目录不存在: {}",
            parent.display()
        )));
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("rivo");
    let format =
        format_description::parse_borrowed::<1>("[year][month][day]-[hour][minute][second]")
            .map_err(|error| StorageError::Unavailable(format!("无法生成备份时间戳: {error}")))?;
    let timestamp = timestamp
        .format(&format)
        .map_err(|error| StorageError::Unavailable(format!("无法生成备份时间戳: {error}")))?;
    for suffix in 0..1000_u32 {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let candidate = parent.join(format!("{stem}-schema-17-backup-{timestamp}{suffix}.db"));
        if !candidate.exists() && !temporary_path(&candidate, "backup").exists() {
            return Ok(candidate);
        }
    }
    Err(StorageError::Unavailable(
        "无法生成不冲突的自动备份路径".into(),
    ))
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let database_lease = acquire_database_lease(&path, false)?;
        let connection = Connection::open(&path)?;
        let store = Self {
            connection,
            path,
            _database_lease: database_lease,
        };
        store.initialize_schema()?;
        store.ensure_session_messages_table()?;
        store.ensure_sidecar_tables()?;
        store.mark_interrupted_agent_tasks()?;
        Ok(store)
    }
    pub fn open_memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        let store = Self {
            connection,
            path: PathBuf::from(":memory:"),
            _database_lease: None,
        };
        store.initialize_schema()?;
        store.ensure_session_messages_table()?;
        store.ensure_sidecar_tables()?;
        store.mark_interrupted_agent_tasks()?;
        Ok(store)
    }

    /// Open a store for a long-lived runtime process without treating every
    /// short-lived read connection as a process restart. App-server handlers
    /// use this variant because they open a bounded connection per request;
    /// startup recovery is performed by the owning process once.
    pub fn open_runtime(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let database_lease = acquire_database_lease(&path, false)?;
        let connection = Connection::open(&path)?;
        let store = Self {
            connection,
            path,
            _database_lease: database_lease,
        };
        store.initialize_schema()?;
        store.ensure_session_messages_table()?;
        store.ensure_sidecar_tables()?;
        Ok(store)
    }

    /// Explicitly close the SQLite connection and preserve any close failure as
    /// a structured storage error. Normal drop remains sufficient for callers
    /// that do not need to observe the result.
    pub fn close(self) -> Result<(), StorageError> {
        self.connection
            .close()
            .map_err(|(_, error)| StorageError::Sqlite(error))
    }
    fn initialize_schema(&self) -> Result<(), StorageError> {
        const CURRENT_SCHEMA_VERSION: u64 = SCHEMA_VERSION;
        // 在任何 DDL、清理或任务状态写入之前拒绝旧库，包括没有版本表的旧库。
        let has_schema: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version')", [], |r| r.get(0))?;
        if has_schema {
            let version: u64 = self.connection.query_row(
                "SELECT version FROM schema_version LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            if version != CURRENT_SCHEMA_VERSION {
                return Err(StorageError::UnsupportedSchema(version));
            }
            self.connection.execute_batch("PRAGMA foreign_keys=ON;")?;
            self.validate_current_schema()?;
            return Ok(());
        }
        let has_tables: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')", [], |r| r.get(0))?;
        if has_tables {
            return Err(StorageError::UnsupportedSchema(0));
        }
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute_batch("PRAGMA foreign_keys=ON;
             CREATE TABLE schema_version(version INTEGER NOT NULL);
             INSERT INTO schema_version(version) VALUES(18);
             CREATE TABLE projects(id TEXT PRIMARY KEY, display_name TEXT NOT NULL, workspace_root TEXT NOT NULL UNIQUE, is_default INTEGER NOT NULL DEFAULT 0 CHECK(is_default IN (0,1)), last_opened_at TEXT NOT NULL);
             CREATE UNIQUE INDEX projects_single_default ON projects(is_default) WHERE is_default=1;
             CREATE INDEX projects_recently_opened ON projects(last_opened_at DESC);
             CREATE TABLE sessions(id TEXT PRIMARY KEY, project_id TEXT NULL, created_at TEXT NOT NULL, model_route TEXT NULL, workspace_root TEXT NULL, FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE SET NULL);
             CREATE TABLE turns(session_id TEXT NOT NULL, turn_id TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(session_id, turn_id), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE);
             CREATE TABLE events(session_id TEXT NOT NULL, turn_id TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00Z', PRIMARY KEY(session_id, sequence), FOREIGN KEY(session_id, turn_id) REFERENCES turns(session_id, turn_id) ON DELETE CASCADE);
             CREATE TABLE messages(session_id TEXT NOT NULL, turn_id TEXT NOT NULL, sequence INTEGER NOT NULL, role TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(session_id, sequence), FOREIGN KEY(session_id, turn_id) REFERENCES turns(session_id, turn_id) ON DELETE CASCADE);
             CREATE TABLE memories(id TEXT PRIMARY KEY, workspace TEXT NOT NULL, mode TEXT NOT NULL, body TEXT NOT NULL, source TEXT NOT NULL);
             CREATE TABLE memory_candidates(id TEXT PRIMARY KEY, workspace TEXT NOT NULL, mode TEXT NOT NULL, body TEXT NOT NULL, source TEXT NOT NULL, body_sha256 TEXT NOT NULL);
             CREATE VIRTUAL TABLE memories_fts USING fts5(id UNINDEXED, workspace, mode, body);
             CREATE TABLE ui_audit_events(id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NULL, turn_id TEXT NULL, action TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
             CREATE INDEX ui_audit_events_created_at ON ui_audit_events(created_at); CREATE INDEX ui_audit_events_session ON ui_audit_events(session_id, turn_id);
             CREATE TABLE diagnostic_index(id INTEGER PRIMARY KEY AUTOINCREMENT, source TEXT NOT NULL, session_id TEXT NULL, turn_id TEXT NULL, event_sequence INTEGER NULL, audit_id INTEGER NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), stage TEXT NOT NULL, code TEXT NOT NULL, retryable INTEGER NOT NULL DEFAULT 0, summary TEXT NOT NULL, UNIQUE(source, session_id, turn_id, event_sequence, audit_id));
             CREATE INDEX diagnostic_index_created_at ON diagnostic_index(created_at DESC); CREATE INDEX diagnostic_index_session ON diagnostic_index(session_id, turn_id, created_at DESC);
             CREATE TABLE expired_sessions(id TEXT PRIMARY KEY, created_at TEXT NOT NULL, expired_at TEXT NOT NULL, event_count INTEGER NOT NULL DEFAULT 0, message_count INTEGER NOT NULL DEFAULT 0, reason TEXT NOT NULL); CREATE INDEX expired_sessions_created_at ON expired_sessions(created_at DESC);
             CREATE TABLE session_permission_grants(session_id TEXT NOT NULL, workspace TEXT NOT NULL, policy TEXT NOT NULL, grant_key TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), PRIMARY KEY(session_id,workspace,policy,grant_key), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE); CREATE INDEX session_permission_grants_session ON session_permission_grants(session_id);
             CREATE TABLE agent_tasks(task_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, session_id TEXT NOT NULL, turn_id TEXT NOT NULL, label TEXT NOT NULL, state TEXT NOT NULL, summary TEXT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), usage_json TEXT NULL, terminal_reason TEXT NULL, FOREIGN KEY(session_id,turn_id) REFERENCES turns(session_id,turn_id) ON DELETE CASCADE); CREATE INDEX agent_tasks_session_turn ON agent_tasks(session_id,turn_id,created_at);
             CREATE TABLE a2a_peers(peer_id TEXT PRIMARY KEY, endpoint TEXT NOT NULL, spki_sha256 TEXT NOT NULL, agent_card_sha256 TEXT NOT NULL, protocol_version TEXT NOT NULL, skill_ids_json TEXT NOT NULL, state TEXT NOT NULL, identity_ref TEXT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), interface_url TEXT NULL, protocol_binding TEXT NULL, tenant TEXT NULL, agent_card_id TEXT NULL, agent_card_jcs_sha256 TEXT NULL, spec_revision TEXT NULL, wire_protocol_version TEXT NULL, status TEXT NULL, approved_at TEXT NULL);
             CREATE TABLE a2a_tasks(peer_id TEXT NOT NULL, task_id TEXT NOT NULL, request_sha256 TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), PRIMARY KEY(peer_id,task_id), FOREIGN KEY(peer_id) REFERENCES a2a_peers(peer_id) ON DELETE RESTRICT); CREATE INDEX a2a_tasks_peer_state ON a2a_tasks(peer_id,state,updated_at); CREATE TABLE a2a_audit(id INTEGER PRIMARY KEY AUTOINCREMENT, peer_id TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
             CREATE TABLE goals(goal_id TEXT PRIMARY KEY, session_id TEXT NOT NULL UNIQUE, objective TEXT NOT NULL, status TEXT NOT NULL, budget INTEGER NOT NULL, used_budget INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 1, last_evidence_summary TEXT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE); CREATE INDEX goals_status ON goals(status);
             CREATE TABLE task_plans(plan_id TEXT NOT NULL, session_id TEXT NOT NULL, objective TEXT NOT NULL, revision INTEGER NOT NULL, status TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(plan_id,revision), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE); CREATE INDEX task_plans_session_idx ON task_plans(session_id,updated_at);")?;
        transaction.commit()?;
        Ok(())
    }

    fn validate_current_schema(&self) -> Result<(), StorageError> {
        const REQUIRED_TABLES: &[&str] = &[
            "projects",
            "sessions",
            "turns",
            "events",
            "messages",
            "memories",
            "memory_candidates",
            "memories_fts",
            "ui_audit_events",
            "diagnostic_index",
            "expired_sessions",
            "session_permission_grants",
            "agent_tasks",
            "a2a_peers",
            "a2a_tasks",
            "a2a_audit",
            "goals",
            "task_plans",
        ];
        for table in REQUIRED_TABLES {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                params![table],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StorageError::UnsupportedSchema(SCHEMA_VERSION));
            }
        }
        for (table, column) in [
            ("sessions", "project_id"),
            ("turns", "turn_id"),
            ("events", "turn_id"),
            ("messages", "turn_id"),
            ("ui_audit_events", "turn_id"),
            ("diagnostic_index", "turn_id"),
            ("agent_tasks", "turn_id"),
        ] {
            let mut statement = self
                .connection
                .prepare(&format!("PRAGMA table_info({table})"))?;
            let has_column = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .any(|existing| existing == column);
            if !has_column {
                return Err(StorageError::UnsupportedSchema(SCHEMA_VERSION));
            }
        }
        Ok(())
    }

    fn mark_interrupted_agent_tasks(&self) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE agent_tasks SET state='interrupted', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE state IN ('queued', 'running')",
            [],
        )?;
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_session(&self, session_id: &str) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO sessions(id, created_at) VALUES(?1, strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(id) DO NOTHING",
            params![session_id],
        )?;
        Ok(())
    }

    /// Resolve a local directory to its durable Workspace identity. Existing
    /// projects are deliberately reused so legacy references remain valid.
    pub fn ensure_workspace(
        &self,
        root_path: impl AsRef<Path>,
        display_name: Option<&str>,
    ) -> Result<Workspace, StorageError> {
        let root = normalize_workspace_path(root_path.as_ref())?;
        let identity = workspace_identity(&root);
        if let Some(existing) = self
            .list_workspaces()?
            .into_iter()
            .find(|workspace| workspace_identity(Path::new(&workspace.workspace_root)) == identity)
        {
            return Ok(existing);
        }
        let mut random_id = [0_u8; 16];
        OsRng.fill_bytes(&mut random_id);
        let id = format!("workspace-{}", hex::encode(random_id));
        let name = display_name
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                root.file_name()
                    .map(|value| value.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root.display().to_string());
        let workspace = Workspace {
            id,
            display_name: name,
            workspace_root: root.display().to_string(),
            is_default: self.list_workspaces()?.is_empty(),
            last_opened_at: String::new(),
        };
        self.save_workspace(&workspace)?;
        self.get_workspace(&workspace.id)
    }

    pub fn get_workspace(&self, workspace_id: &str) -> Result<Workspace, StorageError> {
        self.connection
            .query_row(
                "SELECT id, display_name, workspace_root, is_default, last_opened_at FROM projects WHERE id=?1",
                params![workspace_id],
                project_from_row,
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(workspace_id.into()))
    }

    pub fn find_workspace_by_path(
        &self,
        root_path: impl AsRef<Path>,
    ) -> Result<Option<Workspace>, StorageError> {
        let root = normalize_workspace_path(root_path.as_ref())?;
        let identity = workspace_identity(&root);
        Ok(self
            .list_workspaces()?
            .into_iter()
            .find(|workspace| workspace_identity(Path::new(&workspace.workspace_root)) == identity))
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, StorageError> {
        self.list_projects()
    }

    pub fn acquire_workspace_lease(
        &self,
        workspace_id: &str,
    ) -> Result<WorkspaceLease, StorageError> {
        self.get_workspace(workspace_id)?;
        if self.path == Path::new(":memory:") {
            return Ok(WorkspaceLease { file: None });
        }
        let path = workspace_lock_path(&self.path, workspace_id);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                StorageError::Unavailable(format!(
                    "无法打开 Workspace 活动租约: {} ({error})",
                    path.display()
                ))
            })?;
        fs2::FileExt::lock_shared(&file).map_err(|error| {
            StorageError::Unavailable(format!(
                "无法取得 Workspace 活动租约: {} ({error})",
                path.display()
            ))
        })?;
        Ok(WorkspaceLease { file: Some(file) })
    }

    pub fn touch_workspace(&self, workspace_id: &str) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        if tx.execute(
            "UPDATE projects SET is_default=0 WHERE id<>?1 AND is_default=1",
            params![workspace_id],
        )? > 0
        {
            tx.execute("UPDATE projects SET is_default=1, last_opened_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1", params![workspace_id])?;
        } else {
            tx.execute("UPDATE projects SET is_default=1, last_opened_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1", params![workspace_id])?;
        }
        if tx.changes() == 0 {
            tx.rollback()?;
            return Err(StorageError::NotFound(workspace_id.into()));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rename_workspace(
        &self,
        workspace_id: &str,
        display_name: &str,
    ) -> Result<(), StorageError> {
        if display_name.trim().is_empty() {
            return Err(StorageError::Unavailable("Workspace 名称不能为空".into()));
        }
        let changed = self.connection.execute(
            "UPDATE projects SET display_name=?2 WHERE id=?1",
            params![workspace_id, display_name.trim()],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound(workspace_id.into()));
        }
        Ok(())
    }

    pub fn rebind_workspace(
        &self,
        workspace_id: &str,
        root_path: impl AsRef<Path>,
    ) -> Result<Workspace, StorageError> {
        let root = normalize_workspace_path(root_path.as_ref())?;
        if let Some(existing) = self.find_workspace_by_path(&root)? {
            if existing.id != workspace_id {
                return Err(StorageError::Unavailable(
                    "新路径已经属于另一个 Workspace".into(),
                ));
            }
        }
        let changed = self.connection.execute(
            "UPDATE projects SET workspace_root=?2 WHERE id=?1",
            params![workspace_id, root.display().to_string()],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound(workspace_id.into()));
        }
        self.get_workspace(workspace_id)
    }

    pub fn delete_workspace_cascade(&self, workspace_id: &str) -> Result<(), StorageError> {
        self.get_workspace(workspace_id)?;
        if self.path != Path::new(":memory:") {
            let lock_path = workspace_lock_path(&self.path, workspace_id);
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|error| {
                    StorageError::Unavailable(format!(
                        "无法检查 Workspace 活动状态: {} ({error})",
                        lock_path.display()
                    ))
                })?;
            if let Err(error) = lock.try_lock_exclusive() {
                return Err(StorageError::Unavailable(
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        "Workspace 正在运行，不能删除".into()
                    } else {
                        format!("Workspace 正在运行或无法锁定: {error}")
                    },
                ));
            }
        }
        let tx = self.connection.unchecked_transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
            params![workspace_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StorageError::NotFound(workspace_id.into()));
        }
        tx.execute(
            "DELETE FROM diagnostic_index WHERE session_id IN (SELECT id FROM sessions WHERE project_id=?1)",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM ui_audit_events WHERE session_id IN (SELECT id FROM sessions WHERE project_id=?1)",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE project_id=?1",
            params![workspace_id],
        )?;
        tx.execute("DELETE FROM projects WHERE id=?1", params![workspace_id])?;
        let has_default: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE is_default=1)",
            [],
            |row| row.get(0),
        )?;
        if !has_default {
            tx.execute("UPDATE projects SET is_default=1 WHERE id=(SELECT id FROM projects ORDER BY last_opened_at DESC, id LIMIT 1)", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn session_ids_for_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM sessions WHERE project_id=?1")?;
        let rows = statement.query_map(params![workspace_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn attach_unscoped_session(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<(), StorageError> {
        self.get_workspace(workspace_id)?;
        let changed = self.connection.execute("UPDATE sessions SET project_id=?2, workspace_root=(SELECT workspace_root FROM projects WHERE id=?2) WHERE id=?1 AND project_id IS NULL", params![session_id, workspace_id])?;
        if changed == 0 {
            if !self.has_session(session_id)? {
                return Err(StorageError::NotFound(session_id.into()));
            }
            return Err(StorageError::Unavailable(
                "Session 已经归属 Workspace，或不存在".into(),
            ));
        }
        Ok(())
    }

    pub fn session_workspace_id(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        Ok(self
            .connection
            .query_row(
                "SELECT project_id FROM sessions WHERE id=?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn bind_session_to_workspace(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<(), StorageError> {
        self.get_workspace(workspace_id)?;
        self.ensure_session(session_id)?;
        self.connection.execute("UPDATE sessions SET project_id=?2, workspace_root=(SELECT workspace_root FROM projects WHERE id=?2) WHERE id=?1", params![session_id, workspace_id])?;
        Ok(())
    }

    pub fn save_goal(&self, goal: &StoredGoal) -> Result<(), StorageError> {
        if goal.goal_id.trim().is_empty() || goal.session_id.trim().is_empty() {
            return Err(StorageError::NotFound("Goal 标识不能为空".into()));
        }
        if goal.objective.trim().is_empty() {
            return Err(StorageError::NotFound("Goal 目标不能为空".into()));
        }
        self.connection.execute(
            "INSERT INTO goals(goal_id,session_id,objective,status,budget,used_budget,revision,last_evidence_summary,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(session_id) DO UPDATE SET objective=excluded.objective,status=excluded.status,budget=excluded.budget,
               used_budget=excluded.used_budget,revision=excluded.revision,last_evidence_summary=excluded.last_evidence_summary,
               updated_at=excluded.updated_at",
            params![goal.goal_id, goal.session_id, goal.objective, goal.status, goal.budget as i64,
                goal.used_budget as i64, goal.revision as i64, goal.last_evidence_summary, goal.created_at, goal.updated_at],
        )?;
        Ok(())
    }

    pub fn goal(&self, session_id: &str) -> Result<Option<StoredGoal>, StorageError> {
        Ok(self.connection.query_row(
            "SELECT goal_id,session_id,objective,status,budget,used_budget,revision,last_evidence_summary,created_at,updated_at FROM goals WHERE session_id=?1",
            params![session_id], row_to_goal,
        ).optional()?)
    }

    pub fn delete_goal(&self, session_id: &str) -> Result<bool, StorageError> {
        Ok(self
            .connection
            .execute("DELETE FROM goals WHERE session_id=?1", params![session_id])?
            > 0)
    }

    pub fn save_task_plan(&self, plan: &StoredTaskPlan) -> Result<(), StorageError> {
        if plan.plan_id.trim().is_empty()
            || plan.session_id.trim().is_empty()
            || plan.objective.trim().is_empty()
        {
            return Err(StorageError::NotFound("任务计划标识和目标不能为空".into()));
        }
        self.connection.execute(
            "INSERT INTO task_plans(plan_id,session_id,objective,revision,status,payload,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(plan_id,revision) DO UPDATE SET
               session_id=excluded.session_id, objective=excluded.objective,
               status=excluded.status, payload=excluded.payload,
               created_at=excluded.created_at, updated_at=excluded.updated_at",
            params![plan.plan_id, plan.session_id, plan.objective, plan.revision as i64,
                plan.status, plan.payload, plan.created_at, plan.updated_at],
        )?;
        Ok(())
    }

    pub fn task_plan(
        &self,
        session_id: &str,
        plan_id: &str,
        revision: Option<u64>,
    ) -> Result<Option<StoredTaskPlan>, StorageError> {
        let mut stmt = self.connection.prepare(if revision.is_some() {
            "SELECT plan_id,session_id,objective,revision,status,payload,created_at,updated_at FROM task_plans WHERE session_id=?1 AND plan_id=?2 AND revision=?3"
        } else {
            "SELECT plan_id,session_id,objective,revision,status,payload,created_at,updated_at FROM task_plans WHERE session_id=?1 AND plan_id=?2 ORDER BY revision DESC LIMIT 1"
        })?;
        let value = if let Some(revision) = revision {
            stmt.query_row(
                params![session_id, plan_id, revision as i64],
                row_to_task_plan,
            )
            .optional()?
        } else {
            stmt.query_row(params![session_id, plan_id], row_to_task_plan)
                .optional()?
        };
        Ok(value)
    }

    pub fn task_plans(&self, session_id: &str) -> Result<Vec<StoredTaskPlan>, StorageError> {
        let mut stmt = self.connection.prepare("SELECT plan_id,session_id,objective,revision,status,payload,created_at,updated_at FROM task_plans WHERE session_id=?1 ORDER BY updated_at DESC, revision DESC")?;
        let rows = stmt.query_map(params![session_id], row_to_task_plan)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn update_task_plan_status(
        &self,
        session_id: &str,
        plan_id: &str,
        revision: u64,
        status: &str,
        payload: &str,
        updated_at: &str,
    ) -> Result<bool, StorageError> {
        Ok(self.connection.execute("UPDATE task_plans SET status=?1, payload=?2, updated_at=?3 WHERE session_id=?4 AND plan_id=?5 AND revision=?6", params![status, payload, updated_at, session_id, plan_id, revision as i64])? > 0)
    }

    pub fn save_a2a_peer(&self, peer: &StoredA2aPeer) -> Result<(), StorageError> {
        validate_stored_a2a_peer(peer)?;
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM a2a_peers WHERE peer_id=?1",
                params![peer.snapshot.peer_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if existing.as_deref() == Some("revoked") && peer.snapshot.status != A2aTrustState::Revoked
        {
            return Err(StorageError::InvalidA2aState(
                "本机撤销的 peer 必须经过显式重新配对，不能被发现或刷新恢复".into(),
            ));
        }
        self.connection.execute(
            "INSERT INTO a2a_peers(peer_id, endpoint, spki_sha256, agent_card_sha256, protocol_version, skill_ids_json, state, interface_url, protocol_binding, tenant, agent_card_id, agent_card_jcs_sha256, spec_revision, wire_protocol_version, status, approved_at, identity_ref)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?2, ?8, ?9, ?10, ?4, ?11, ?5, ?7, ?12, ?13)
             ON CONFLICT(peer_id) DO UPDATE SET interface_url=excluded.interface_url,
                 protocol_binding=excluded.protocol_binding, tenant=excluded.tenant,
                 spki_sha256=excluded.spki_sha256, agent_card_id=excluded.agent_card_id,
                 agent_card_jcs_sha256=excluded.agent_card_jcs_sha256,
                 skill_ids_json=excluded.skill_ids_json, spec_revision=excluded.spec_revision,
                 wire_protocol_version=excluded.wire_protocol_version, status=excluded.status,
                 approved_at=excluded.approved_at, identity_ref=excluded.identity_ref,
                 updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![
                peer.snapshot.peer_id.0,
                peer.snapshot.interface_url,
                peer.snapshot.spki_sha256,
                peer.snapshot.agent_card_jcs_sha256,
                peer.snapshot.wire_protocol_version,
                serde_json::to_string(&peer.snapshot.allowed_skills)?,
                a2a_trust_state_name(peer.snapshot.status),
                peer.snapshot.protocol_binding,
                peer.snapshot.tenant,
                peer.snapshot.agent_card_id,
                peer.snapshot.spec_revision,
                peer.snapshot.approved_at,
                peer.identity_ref,
            ],
        )?;
        Ok(())
    }

    pub fn a2a_peer(&self, peer_id: &A2aPeerId) -> Result<Option<StoredA2aPeer>, StorageError> {
        Ok(self
            .connection
            .query_row(
                "SELECT peer_id, interface_url, protocol_binding, tenant, spki_sha256, agent_card_id, agent_card_jcs_sha256, skill_ids_json, spec_revision, wire_protocol_version, status, approved_at, identity_ref
                 FROM a2a_peers WHERE peer_id=?1",
                params![peer_id.0],
                row_to_a2a_peer,
            )
            .optional()?)
    }

    pub fn a2a_peers(&self) -> Result<Vec<StoredA2aPeer>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT peer_id, interface_url, protocol_binding, tenant, spki_sha256, agent_card_id, agent_card_jcs_sha256, skill_ids_json, spec_revision, wire_protocol_version, status, approved_at, identity_ref
             FROM a2a_peers ORDER BY peer_id",
        )?;
        let peers = statement
            .query_map([], row_to_a2a_peer)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(peers)
    }

    /// 将新 Card/TLS 信息与现有快照比较。任何变化都会将 trusted peer 降级为
    /// suspended；调用方需要明确重新批准，而不能继续使用旧 mTLS 身份。
    #[allow(clippy::too_many_arguments)] // 与冻结的完整 trust snapshot 一一对应。
    pub fn reconcile_a2a_peer(
        &self,
        peer_id: &A2aPeerId,
        interface_url: &str,
        protocol_binding: &str,
        tenant: Option<String>,
        spki_sha256: &str,
        agent_card_id: &str,
        agent_card_jcs_sha256: &str,
        allowed_skills: std::collections::BTreeSet<String>,
        spec_revision: &str,
        wire_protocol_version: &str,
    ) -> Result<StoredA2aPeer, StorageError> {
        let existing = self
            .a2a_peer(peer_id)?
            .ok_or_else(|| StorageError::NotFound(format!("A2A peer {}", peer_id.0)))?;
        let peer = StoredA2aPeer {
            snapshot: existing.snapshot.reconcile(
                interface_url,
                protocol_binding,
                tenant,
                spki_sha256,
                agent_card_id,
                agent_card_jcs_sha256,
                allowed_skills,
                spec_revision,
                wire_protocol_version,
            ),
            identity_ref: existing.identity_ref,
        };
        self.save_a2a_peer(&peer)?;
        Ok(peer)
    }

    pub fn suspend_a2a_peer(&self, peer_id: &A2aPeerId) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE a2a_peers SET status='suspended' WHERE peer_id=?1 AND status='trusted'",
            params![peer_id.0],
        )?;
        Ok(())
    }

    /// 本机撤销优先：同一事务中阻断未来连接、取消可取消 task，并留下不含正文
    /// 或 token 的最小审计。返回 false 表示 peer 不存在。
    pub fn revoke_a2a_peer(&self, peer_id: &A2aPeerId) -> Result<bool, StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE a2a_peers SET status='revoked', identity_ref=NULL, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE peer_id=?1 AND status<>'revoked'",
            params![peer_id.0],
        )?;
        if changed == 0 {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM a2a_peers WHERE peer_id=?1)",
                params![peer_id.0],
                |row| row.get(0),
            )?;
            transaction.commit()?;
            return Ok(exists);
        }
        transaction.execute(
            "UPDATE a2a_tasks SET state='CANCELED', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE peer_id=?1 AND state IN ('SUBMITTED', 'WORKING', 'INPUT_REQUIRED')",
            params![peer_id.0],
        )?;
        transaction.execute(
            "INSERT INTO a2a_audit(peer_id, action, detail) VALUES(?1, 'peer_revoked', 'local_owner_revoke')",
            params![peer_id.0],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn save_a2a_task(&self, task: &StoredA2aTask) -> Result<(), StorageError> {
        if task.peer_id.0.trim().is_empty()
            || task.task_id.0.trim().is_empty()
            || !is_lower_hex_sha256(&task.request_sha256)
        {
            return Err(StorageError::InvalidA2aState(
                "A2A task 索引字段无效".into(),
            ));
        }
        let peer_state: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM a2a_peers WHERE peer_id=?1",
                params![task.peer_id.0],
                |row| row.get(0),
            )
            .optional()?;
        match peer_state.as_deref() {
            Some("revoked") => {
                return Err(StorageError::InvalidA2aState(
                    "已撤销 peer 不能写入 A2A task".into(),
                ));
            }
            Some(_) => {}
            None => {
                return Err(StorageError::NotFound(format!(
                    "A2A peer {}",
                    task.peer_id.0
                )));
            }
        }
        if let Some(existing) = self.a2a_task(&task.peer_id, &task.task_id)? {
            if existing.request_sha256 != task.request_sha256 {
                return Err(StorageError::InvalidA2aState(
                    "同一 peer/task 的 request hash 冲突".into(),
                ));
            }
            if existing.state.terminal() && existing.state != task.state {
                return Err(StorageError::InvalidA2aState(
                    "A2A task 终态不可回退或改写".into(),
                ));
            }
            if let (Some(previous), Some(next)) = (existing.revision, task.revision) {
                if next < previous {
                    return Err(StorageError::InvalidA2aState(
                        "A2A task revision 不能回退".into(),
                    ));
                }
                if next == previous && existing.state != task.state {
                    return Err(StorageError::InvalidA2aState(
                        "A2A task 相同 revision 不能改写状态".into(),
                    ));
                }
            }
        }
        self.connection.execute(
            "INSERT INTO a2a_tasks(peer_id, task_id, request_sha256, state, revision)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_id, task_id) DO UPDATE SET request_sha256=excluded.request_sha256,
                 state=excluded.state, revision=excluded.revision, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![
                task.peer_id.0,
                task.task_id.0,
                task.request_sha256,
                a2a_task_state_name(task.state),
                task.revision.map(|value| value as i64),
            ],
        )?;
        Ok(())
    }

    pub fn a2a_task(
        &self,
        peer_id: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<Option<StoredA2aTask>, StorageError> {
        Ok(self
            .connection
            .query_row(
                "SELECT peer_id, task_id, request_sha256, state, revision FROM a2a_tasks WHERE peer_id=?1 AND task_id=?2",
                params![peer_id.0, task_id.0],
                row_to_a2a_task,
            )
            .optional()?)
    }

    pub fn save_agent_task(&self, task: &StoredAgentTask) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO agent_tasks(task_id, agent_id, session_id, turn_id, label, state, summary, usage_json, terminal_reason)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(task_id) DO UPDATE SET state=excluded.state, summary=excluded.summary,
             usage_json=excluded.usage_json, terminal_reason=excluded.terminal_reason,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![
                task.task_id,
                task.agent_id,
                task.session_id,
                task.turn_id,
                task.label,
                task.state,
                task.summary,
                task.usage_json,
                task.terminal_reason,
            ],
        )?;
        Ok(())
    }

    pub fn update_agent_task(
        &self,
        task_id: &str,
        state: &str,
        summary: Option<&str>,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE agent_tasks SET state=?2, summary=COALESCE(?3, summary),
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE task_id=?1",
            params![task_id, state, summary],
        )?;
        Ok(())
    }

    pub fn finish_agent_task(
        &self,
        task_id: &str,
        state: &str,
        summary: &str,
        usage_json: &str,
        terminal_reason: &str,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE agent_tasks SET state=?2, summary=?3, usage_json=?4, terminal_reason=?5,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE task_id=?1",
            params![task_id, state, summary, usage_json, terminal_reason],
        )?;
        Ok(())
    }

    pub fn agent_tasks(&self, session_id: &str) -> Result<Vec<StoredAgentTask>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT task_id, agent_id, session_id, turn_id, label, state, summary, usage_json, terminal_reason
             FROM agent_tasks WHERE session_id=?1 ORDER BY created_at, task_id",
        )?;
        let tasks = statement
            .query_map(params![session_id], |row| {
                Ok(StoredAgentTask {
                    task_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    turn_id: row.get(3)?,
                    label: row.get(4)?,
                    state: row.get(5)?,
                    summary: row.get(6)?,
                    usage_json: row.get(7)?,
                    terminal_reason: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;
        Ok(tasks)
    }

    /// FEAT-037 的追加表。它不提升 schema 版本：旧二进制忽略该表，新二进制
    /// 在打开 schema 18 数据库时按需创建，因此可安全降级。
    fn ensure_session_messages_table(&self) -> Result<(), StorageError> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_messages(message_id TEXT PRIMARY KEY, from_session_id TEXT NOT NULL, to_session_id TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), expires_at TEXT NOT NULL, FOREIGN KEY(to_session_id) REFERENCES sessions(id) ON DELETE CASCADE);
             CREATE INDEX IF NOT EXISTS session_messages_to_state ON session_messages(to_session_id, state, created_at);",
        )?;
        Ok(())
    }

    /// Sidecar tables are additive to schema 18. Older binaries ignore them,
    /// while newer binaries create them lazily without altering user history.
    fn ensure_sidecar_tables(&self) -> Result<(), StorageError> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sidecar_tasks(
                task_id TEXT PRIMARY KEY,
                intent_id TEXT NOT NULL UNIQUE,
                workspace_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                state TEXT NOT NULL,
                lease_epoch INTEGER NOT NULL,
                expires_at TEXT NOT NULL,
                last_event_sequence INTEGER NOT NULL DEFAULT 0,
                tool_call_status TEXT NOT NULL DEFAULT '{}',
                intent_fingerprint TEXT NOT NULL DEFAULT '',
                terminal_reason TEXT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE INDEX IF NOT EXISTS sidecar_tasks_session_state ON sidecar_tasks(session_id, state, updated_at);
            CREATE TABLE IF NOT EXISTS sidecar_leases(
                task_id TEXT PRIMARY KEY,
                lease_epoch INTEGER NOT NULL,
                expires_at TEXT NOT NULL,
                state TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                FOREIGN KEY(task_id) REFERENCES sidecar_tasks(task_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS sidecar_leases_expiry ON sidecar_leases(expires_at, state);
            CREATE TABLE IF NOT EXISTS sidecar_event_cursors(
                session_id TEXT PRIMARY KEY,
                next_sequence INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE TABLE IF NOT EXISTS sidecar_artifacts(
                artifact_id TEXT PRIMARY KEY,
                node_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                hash TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                download_scope TEXT NOT NULL,
                storage_key TEXT NOT NULL,
                state TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS sidecar_artifacts_scope ON sidecar_artifacts(node_id, workspace_id, session_id, state);",
        )?;
        // The table is additive to schema 18 and may already exist from an
        // earlier binary. Add the new recovery column without touching the
        // schema version or rewriting existing task rows.
        let has_tool_call_status = self
            .connection
            .prepare("PRAGMA table_info(sidecar_tasks)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|name| name == "tool_call_status");
        if !has_tool_call_status {
            self.connection.execute(
                "ALTER TABLE sidecar_tasks ADD COLUMN tool_call_status TEXT NOT NULL DEFAULT '{}'",
                [],
            )?;
        }
        let has_intent_fingerprint = self
            .connection
            .prepare("PRAGMA table_info(sidecar_tasks)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "intent_fingerprint");
        if !has_intent_fingerprint {
            self.connection.execute(
                "ALTER TABLE sidecar_tasks ADD COLUMN intent_fingerprint TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        Ok(())
    }

    pub fn accept_sidecar_task(
        &self,
        task: &StoredSidecarTask,
        lease: &StoredSidecarLease,
        fingerprint: &str,
    ) -> Result<(), StorageError> {
        if task.task_id != lease.task_id
            || task.lease_epoch != lease.lease_epoch
            || fingerprint.len() != 64
            || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(StorageError::Unavailable(
                "invalid sidecar acceptance metadata".into(),
            ));
        }
        let transaction = self.connection.unchecked_transaction()?;
        self.save_sidecar_task(task)?;
        self.save_sidecar_lease(lease)?;
        transaction.execute(
            "UPDATE sidecar_tasks SET intent_fingerprint=?2 WHERE task_id=?1",
            params![task.task_id, fingerprint],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn sidecar_intent_fingerprint(
        &self,
        task_id: &str,
    ) -> Result<Option<String>, StorageError> {
        self.connection
            .query_row(
                "SELECT intent_fingerprint FROM sidecar_tasks WHERE task_id=?1",
                params![task_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn save_sidecar_task(&self, task: &StoredSidecarTask) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO sidecar_tasks(task_id,intent_id,workspace_id,session_id,turn_id,state,lease_epoch,expires_at,last_event_sequence,tool_call_status,terminal_reason)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                task.task_id,
                task.intent_id,
                task.workspace_id,
                task.session_id,
                task.turn_id,
                task.state,
                task.lease_epoch,
                task.expires_at,
                task.last_event_sequence,
                task.tool_call_status,
                task.terminal_reason,
            ],
        )?;
        Ok(())
    }

    pub fn save_sidecar_lease(&self, lease: &StoredSidecarLease) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO sidecar_leases(task_id,lease_epoch,expires_at,state)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(task_id) DO UPDATE SET lease_epoch=excluded.lease_epoch,
             expires_at=excluded.expires_at,state=excluded.state,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![
                lease.task_id,
                lease.lease_epoch,
                lease.expires_at,
                lease.state
            ],
        )?;
        Ok(())
    }

    pub fn sidecar_lease(&self, task_id: &str) -> Result<Option<StoredSidecarLease>, StorageError> {
        self.connection
            .query_row(
                "SELECT task_id,lease_epoch,expires_at,state FROM sidecar_leases WHERE task_id=?1",
                params![task_id],
                |row| {
                    Ok(StoredSidecarLease {
                        task_id: row.get(0)?,
                        lease_epoch: row.get(1)?,
                        expires_at: row.get(2)?,
                        state: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Expire accepted/running leases using SQLite's clock and atomically
    /// expose the same terminal state through the task index.
    pub fn expire_sidecar_leases(&self) -> Result<usize, StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE sidecar_leases SET state='expired',updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE state IN ('accepted','running') AND julianday(expires_at) <= julianday('now')",
            [],
        )?;
        let changed = transaction.execute(
            "UPDATE sidecar_tasks SET state='expired',terminal_reason='lease_expired',updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE task_id IN (SELECT task_id FROM sidecar_leases WHERE state='expired')
             AND state IN ('accepted','running')",
            [],
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn sidecar_task(&self, task_id: &str) -> Result<Option<StoredSidecarTask>, StorageError> {
        self.connection
            .query_row(
                "SELECT task_id,intent_id,workspace_id,session_id,turn_id,state,lease_epoch,expires_at,last_event_sequence,tool_call_status,terminal_reason FROM sidecar_tasks WHERE task_id=?1",
                params![task_id],
                sidecar_task_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn sidecar_task_by_intent(
        &self,
        intent_id: &str,
    ) -> Result<Option<StoredSidecarTask>, StorageError> {
        self.connection
            .query_row(
                "SELECT task_id,intent_id,workspace_id,session_id,turn_id,state,lease_epoch,expires_at,last_event_sequence,tool_call_status,terminal_reason FROM sidecar_tasks WHERE intent_id=?1",
                params![intent_id],
                sidecar_task_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn sidecar_task_by_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<StoredSidecarTask>, StorageError> {
        self.connection
            .query_row(
                "SELECT task_id,intent_id,workspace_id,session_id,turn_id,state,lease_epoch,expires_at,last_event_sequence,tool_call_status,terminal_reason FROM sidecar_tasks WHERE session_id=?1 AND turn_id=?2 ORDER BY created_at DESC LIMIT 1",
                params![session_id, turn_id],
                sidecar_task_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn update_sidecar_task(
        &self,
        task_id: &str,
        state: &str,
        lease_epoch: u64,
        last_event_sequence: u64,
        terminal_reason: Option<&str>,
    ) -> Result<bool, StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE sidecar_tasks SET state=?2,last_event_sequence=MAX(last_event_sequence,?4),
             terminal_reason=CASE WHEN state IN ('accepted','running') THEN ?5 ELSE terminal_reason END,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE task_id=?1 AND lease_epoch=?3 AND (state='accepted' OR (state='running' AND ?2!='accepted') OR state=?2)",
            params![task_id, state, lease_epoch, last_event_sequence, terminal_reason],
        )?;
        if changed == 1 {
            transaction.execute(
                "UPDATE sidecar_leases SET state=?3,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE task_id=?1 AND lease_epoch=?2",
                params![task_id, lease_epoch, state],
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Persist only bounded tool lifecycle metadata. Tool arguments, results
    /// and credentials stay in the event/session projection boundaries.
    pub fn update_sidecar_tool_status(
        &self,
        task_id: &str,
        tool_call_status: &str,
    ) -> Result<bool, StorageError> {
        if tool_call_status.len() > 16 * 1024 {
            return Err(StorageError::Unavailable(
                "sidecar tool status is too large".to_owned(),
            ));
        }
        let changed = self.connection.execute(
            "UPDATE sidecar_tasks SET tool_call_status=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE task_id=?1",
            params![task_id, tool_call_status],
        )?;
        Ok(changed == 1)
    }

    pub fn recover_sidecar_tasks(&self) -> Result<(), StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE sidecar_tasks SET
                state=CASE WHEN state='running' THEN 'unknown_side_effect' ELSE 'interrupted' END,
                terminal_reason=CASE WHEN state='running' THEN 'sidecar_restart_unknown_side_effect' ELSE 'sidecar_restart_interrupted' END,
                updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE state IN ('accepted','running')",
            [],
        )?;
        transaction.execute(
            "UPDATE sidecar_leases SET state=CASE WHEN state='running' THEN 'unknown_side_effect' ELSE 'interrupted' END, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE state IN ('accepted','running')",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A transport disconnect is not an explicit cancellation. Persist the
    /// uncertainty so a later owner must inspect and explicitly resume or
    /// abandon the task instead of replaying a tool call automatically.
    pub fn mark_sidecar_tasks_unknown_side_effect(&self) -> Result<usize, StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE sidecar_tasks SET state='unknown_side_effect',
                terminal_reason='client_disconnect_unknown_side_effect',
                updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE state IN ('accepted','running')",
            [],
        )?;
        transaction.execute(
            "UPDATE sidecar_leases SET state='unknown_side_effect',
                updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE state IN ('accepted','running')",
            [],
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn save_event_cursor(&self, cursor: &StoredEventCursor) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO sidecar_event_cursors(session_id,next_sequence) VALUES(?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET next_sequence=MAX(next_sequence,excluded.next_sequence),updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![cursor.session_id, cursor.next_sequence],
        )?;
        Ok(())
    }

    pub fn event_cursor(
        &self,
        session_id: &str,
    ) -> Result<Option<StoredEventCursor>, StorageError> {
        self.connection
            .query_row(
                "SELECT session_id,next_sequence FROM sidecar_event_cursors WHERE session_id=?1",
                params![session_id],
                |row| {
                    Ok(StoredEventCursor {
                        session_id: row.get(0)?,
                        next_sequence: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn save_artifact(&self, artifact: &StoredArtifact) -> Result<(), StorageError> {
        validate_artifact_id(&artifact.artifact_id)?;
        validate_stored_artifact_scope(artifact)?;
        if artifact.kind.is_empty()
            || artifact.kind.len() > 64
            || artifact.size > MAX_SIDECAR_ARTIFACT_BYTES
            || artifact.hash.len() != 64
            || !artifact.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(StorageError::InvalidArtifact(
                "artifact metadata is invalid".into(),
            ));
        }
        self.connection.execute(
            "INSERT INTO sidecar_artifacts(artifact_id,node_id,workspace_id,session_id,kind,size,hash,expires_at,download_scope,storage_key,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![artifact.artifact_id,artifact.node_id,artifact.workspace_id,artifact.session_id,artifact.kind,artifact.size,artifact.hash,artifact.expires_at,artifact.download_scope,artifact.storage_key,artifact.state],
        )?;
        Ok(())
    }

    pub fn artifact_for_scope(
        &self,
        artifact_id: &str,
        node_id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<StoredArtifact>, StorageError> {
        self.connection
            .query_row(
                "SELECT artifact_id,node_id,workspace_id,session_id,kind,size,hash,expires_at,download_scope,storage_key,state FROM sidecar_artifacts WHERE artifact_id=?1 AND node_id=?2 AND workspace_id=?3 AND session_id=?4",
                params![artifact_id,node_id,workspace_id,session_id],
                artifact_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// List runtime-owned artifacts for one authorized Sidecar session. The
    /// caller still performs filesystem cleanup through `ArtifactStore`; this
    /// query only returns metadata and never exposes a path to the wire.
    pub fn artifacts_for_scope(
        &self,
        node_id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT artifact_id,node_id,workspace_id,session_id,kind,size,hash,expires_at,download_scope,storage_key,state
             FROM sidecar_artifacts
             WHERE node_id=?1 AND workspace_id=?2 AND session_id=?3 AND state != 'deleted'
             ORDER BY artifact_id ASC",
        )?;
        let rows = statement.query_map(
            params![node_id, workspace_id, session_id],
            artifact_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Return artifacts whose retention deadline has passed. Expiry is a
    /// metadata transition; callers must remove only the runtime-owned file
    /// and record whether that removal succeeded.
    pub fn expired_artifacts(
        &self,
        node_id: &str,
        workspace_id: &str,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT artifact_id,node_id,workspace_id,session_id,kind,size,hash,expires_at,download_scope,storage_key,state
             FROM sidecar_artifacts
             WHERE node_id=?1 AND workspace_id=?2 AND state != 'deleted'
               AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
             ORDER BY artifact_id ASC",
        )?;
        let rows = statement.query_map(params![node_id, workspace_id], artifact_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Sessions whose tasks ended in a state that owns no further runtime
    /// work. A Sidecar startup uses this index after restart recovery to clean
    /// leftover runtime-owned artifacts from a crashed or failed node.
    pub fn sidecar_sessions_needing_artifact_cleanup(&self) -> Result<Vec<String>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT session_id FROM sidecar_tasks
             WHERE state IN ('failed','cancelled','interrupted','unknown_side_effect','expired')
             ORDER BY session_id ASC",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(StorageError::from)
    }

    pub fn update_artifact_state(
        &self,
        artifact_id: &str,
        node_id: &str,
        workspace_id: &str,
        session_id: &str,
        state: &str,
    ) -> Result<bool, StorageError> {
        let changed = self.connection.execute(
            "UPDATE sidecar_artifacts SET state=?5 WHERE artifact_id=?1 AND node_id=?2 AND workspace_id=?3 AND session_id=?4",
            params![artifact_id, node_id, workspace_id, session_id, state],
        )?;
        Ok(changed == 1)
    }

    /// 写入一条新消息并返回其持久化视图。过期时间由数据库时钟计算。
    pub fn insert_session_message(
        &self,
        message_id: &str,
        from_session_id: &str,
        to_session_id: &str,
        kind: &str,
        body: &str,
        state: &str,
    ) -> Result<StoredSessionMessage, StorageError> {
        self.connection.execute(
            "INSERT INTO session_messages(message_id, from_session_id, to_session_id, kind, body, state, expires_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ','now', ?7))",
            params![
                message_id,
                from_session_id,
                to_session_id,
                kind,
                body,
                state,
                format!("+{SESSION_MESSAGE_TTL_MINUTES} minutes"),
            ],
        )?;
        self.session_message(message_id)?
            .ok_or_else(|| StorageError::Unavailable("会话消息写入后无法读取".into()))
    }

    pub fn session_message(
        &self,
        message_id: &str,
    ) -> Result<Option<StoredSessionMessage>, StorageError> {
        self.connection
            .query_row(
                "SELECT message_id, from_session_id, to_session_id, kind, body, state, created_at, expires_at
                 FROM session_messages WHERE message_id=?1",
                params![message_id],
                session_message_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// 条件状态迁移：仅当当前状态属于 `from` 且未过期（迁出到 `expired` 除外）
    /// 时更新，返回是否发生迁移。用于防止重复投递或批准已过期消息。
    pub fn transition_session_message(
        &self,
        message_id: &str,
        from: &[&str],
        to: &str,
    ) -> Result<bool, StorageError> {
        let from = serde_json::to_string(from)
            .map_err(|error| StorageError::Unavailable(format!("会话消息状态无效: {error}")))?;
        let changed = self.connection.execute(
            "UPDATE session_messages SET state=?3, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE message_id=?1 AND state IN (SELECT value FROM json_each(?2))
             AND (?3='expired' OR expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            params![message_id, from, to],
        )?;
        Ok(changed == 1)
    }

    /// 列出接收方的消息，按创建顺序（FIFO）。`state` 为空时返回全部。
    pub fn session_messages_for(
        &self,
        to_session_id: &str,
        state: Option<&str>,
    ) -> Result<Vec<StoredSessionMessage>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT message_id, from_session_id, to_session_id, kind, body, state, created_at, expires_at
             FROM session_messages WHERE to_session_id=?1 AND (?2 IS NULL OR state=?2)
             ORDER BY created_at, message_id",
        )?;
        let messages = statement
            .query_map(params![to_session_id, state], session_message_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;
        Ok(messages)
    }

    /// 将已超过有效期的 `pending` / `held` 消息标记为 `expired`，返回数量。
    pub fn expire_session_messages(&self) -> Result<u64, StorageError> {
        let changed = self.connection.execute(
            "UPDATE session_messages SET state='expired', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE state IN ('pending','held') AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
        )?;
        Ok(changed as u64)
    }

    pub fn save_session_permission_grant(
        &self,
        grant: &SessionPermissionGrant,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES(?1, datetime('now'))",
            params![grant.session_id],
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO session_permission_grants(session_id, workspace, policy, grant_key) VALUES(?1, ?2, ?3, ?4)",
            params![grant.session_id, grant.workspace, grant.policy, grant.grant_key],
        )?;
        Ok(())
    }

    pub fn session_permission_grants(
        &self,
        session_id: &str,
        workspace: &str,
        policy: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT grant_key FROM session_permission_grants WHERE session_id=?1 AND workspace=?2 AND policy=?3 ORDER BY grant_key",
        )?;
        let rows = statement.query_map(params![session_id, workspace, policy], |row| row.get(0))?;
        let grants = rows.collect::<Result<Vec<String>, _>>()?;
        Ok(grants)
    }

    pub fn clear_session_permission_grants(&self, session_id: &str) -> Result<u64, StorageError> {
        Ok(self.connection.execute(
            "DELETE FROM session_permission_grants WHERE session_id=?1",
            params![session_id],
        )? as u64)
    }
    pub fn append_event(&self, event: &StoredEvent) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES(?1, datetime('now'))",
            params![event.session_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO turns(session_id, turn_id, created_at) VALUES(?1, ?2, datetime('now'))",
            params![event.session_id, event.turn_id],
        )?;
        tx.execute("INSERT INTO events(session_id, turn_id, sequence, kind, body, created_at) VALUES(?1, ?2, ?3, ?4, ?5, datetime('now'))", params![event.session_id, event.turn_id, event.sequence, event.kind, event.body])?;
        if matches!(event.kind.as_str(), "error" | "failure") {
            let (stage, code, retryable, summary) = diagnostic_fields(&event.body);
            tx.execute(
                "INSERT OR IGNORE INTO diagnostic_index(source, session_id, turn_id, event_sequence, audit_id, stage, code, retryable, summary)
                 VALUES('session_event', ?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
                params![event.session_id, event.turn_id, event.sequence, stage, code, retryable, summary],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn append_ui_audit(&self, event: &UiAuditEvent) -> Result<i64, StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO ui_audit_events(session_id, turn_id, action, payload) VALUES(?1, ?2, ?3, ?4)",
            params![event.session_id, event.turn_id, event.action, event.payload],
        )?;
        let id = tx.last_insert_rowid();
        if event.action == "failure" {
            let (stage, code, retryable, summary) = diagnostic_fields(&event.payload);
            tx.execute(
                "INSERT OR IGNORE INTO diagnostic_index(source, session_id, turn_id, event_sequence, audit_id, stage, code, retryable, summary)
                 VALUES('ui_audit', ?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
                params![event.session_id, event.turn_id, id, stage, code, retryable, summary],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn list_diagnostics(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DiagnosticIndexEntry>, StorageError> {
        let limit = limit.clamp(1, 100_000) as i64;
        let sql = if session_id.is_some() {
            "SELECT source, session_id, turn_id, event_sequence, audit_id, created_at, stage, code, retryable, summary FROM diagnostic_index WHERE session_id=?1 ORDER BY id DESC LIMIT ?2"
        } else {
            "SELECT source, session_id, turn_id, event_sequence, audit_id, created_at, stage, code, retryable, summary FROM diagnostic_index ORDER BY id DESC LIMIT ?1"
        };
        let mut statement = self.connection.prepare(sql)?;
        let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<DiagnosticIndexEntry> {
            Ok(DiagnosticIndexEntry {
                source: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                event_sequence: row
                    .get::<_, Option<i64>>(3)?
                    .map(|value| value.max(0) as u64),
                audit_id: row.get(4)?,
                created_at: row.get(5)?,
                stage: row.get(6)?,
                code: row.get(7)?,
                retryable: row.get::<_, i64>(8)? != 0,
                summary: row.get(9)?,
            })
        };
        let rows = if let Some(session_id) = session_id {
            statement.query_map(params![session_id, limit], read)?
        } else {
            statement.query_map(params![limit], read)?
        };
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn clear_diagnostics(&self) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM diagnostic_index", [])?;
        tx.execute("DELETE FROM ui_audit_events", [])?;
        tx.execute("DELETE FROM expired_sessions", [])?;
        tx.commit()?;
        Ok(())
    }

    /// 返回已经持久化的本机 journal；调用方仍必须在导出边界按已知 API Key
    /// 做掩码。完整 system prompt 和持久指令不属于可持久化数据，绝不可通过
    /// 本接口或诊断包导出。
    pub fn full_diagnostic_journal(&self) -> Result<Value, StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        let sessions = {
            let mut statement = tx.prepare("SELECT s.id, s.created_at, s.workspace_root, (SELECT COUNT(*) FROM events e WHERE e.session_id=s.id), (SELECT COUNT(*) FROM messages m WHERE m.session_id=s.id) FROM sessions s ORDER BY s.created_at DESC, s.id DESC")?;
            let rows = statement.query_map([], |row| {
                Ok(SessionSummary {
                    session_id: row.get(0)?,
                    project_id: None,
                    created_at: row.get(1)?,
                    workspace_root: row.get(2)?,
                    task_summary: None,
                    expiration_reason: None,
                    event_count: row.get::<_, i64>(3)?.max(0) as u64,
                    message_count: row.get::<_, i64>(4)?.max(0) as u64,
                    expired: false,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut session_values = Vec::with_capacity(sessions.len());
        for session in sessions {
            let events = {
                let mut statement = tx.prepare("SELECT session_id, turn_id, sequence, kind, body FROM events WHERE session_id=?1 ORDER BY sequence")?;
                let rows = statement.query_map(params![session.session_id], |row| {
                    Ok(StoredEvent {
                        session_id: row.get(0)?,
                        turn_id: row.get(1)?,
                        sequence: row.get::<_, i64>(2)?.max(0) as u64,
                        kind: row.get(3)?,
                        body: row.get(4)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let messages = {
                let mut statement = tx.prepare("SELECT session_id, turn_id, sequence, role, body FROM messages WHERE session_id=?1 ORDER BY sequence")?;
                let rows = statement.query_map(params![session.session_id], |row| {
                    Ok(StoredMessage {
                        session_id: row.get(0)?,
                        turn_id: row.get(1)?,
                        sequence: row.get::<_, i64>(2)?.max(0) as u64,
                        role: row.get(3)?,
                        body: row.get(4)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            session_values.push(serde_json::json!({
                "summary": session,
                "events": events,
                "messages": messages,
            }));
        }
        let audits = {
            let mut audit_statement = tx.prepare(
                "SELECT id, session_id, turn_id, action, payload, created_at FROM ui_audit_events ORDER BY id",
            )?;
            let rows = audit_statement.query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, i64>(0)?, "session_id": row.get::<_, Option<String>>(1)?,
                    "turn_id": row.get::<_, Option<String>>(2)?, "action": row.get::<_, String>(3)?,
                    "payload": row.get::<_, String>(4)?, "created_at": row.get::<_, String>(5)?,
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let diagnostics = {
            let mut statement = tx.prepare("SELECT source, session_id, turn_id, event_sequence, audit_id, created_at, stage, code, retryable, summary FROM diagnostic_index ORDER BY id DESC LIMIT 100000")?;
            let rows = statement.query_map([], |row| {
                Ok(DiagnosticIndexEntry {
                    source: row.get(0)?,
                    session_id: row.get(1)?,
                    turn_id: row.get(2)?,
                    event_sequence: row
                        .get::<_, Option<i64>>(3)?
                        .map(|value| value.max(0) as u64),
                    audit_id: row.get(4)?,
                    created_at: row.get(5)?,
                    stage: row.get(6)?,
                    code: row.get(7)?,
                    retryable: row.get::<_, i64>(8)? != 0,
                    summary: row.get(9)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let value = serde_json::json!({
            "sessions": session_values,
            "ui_audit_events": audits,
            "diagnostics": diagnostics,
        });
        tx.commit()?;
        Ok(value)
    }

    pub fn append_message(&self, message: &StoredMessage) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES(?1, datetime('now'))",
            params![message.session_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO turns(session_id, turn_id, created_at) VALUES(?1, ?2, datetime('now'))",
            params![message.session_id, message.turn_id],
        )?;
        tx.execute(
            "INSERT INTO messages(session_id, turn_id, sequence, role, body) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![message.session_id, message.turn_id, message.sequence, message.role, message.body],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replay_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT session_id, turn_id, sequence, role, body FROM messages
             WHERE session_id=?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                session_id: row.get(0)?,
                turn_id: row.get(1)?,
                sequence: row.get(2)?,
                role: row.get(3)?,
                body: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.project_id, s.created_at, s.workspace_root,
                    (SELECT substr(m.body, 1, 160) FROM messages m
                     WHERE m.session_id=s.id AND m.role='user' ORDER BY m.sequence LIMIT 1),
                    (SELECT COUNT(*) FROM events e WHERE e.session_id=s.id),
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id=s.id), 0, NULL
             FROM sessions s
             UNION ALL
             SELECT id, NULL, created_at, NULL, NULL, event_count, message_count, 1, reason FROM expired_sessions
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                project_id: row.get(1)?,
                created_at: row.get(2)?,
                workspace_root: row.get(3)?,
                task_summary: row.get(4)?,
                event_count: row.get(5)?,
                message_count: row.get(6)?,
                expired: row.get::<_, i64>(7)? != 0,
                expiration_reason: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Workspace-scoped history facade. The legacy no-argument method remains
    /// for compatibility with older callers and returns the unfiltered view.
    pub fn list_sessions_scoped(
        &self,
        scope: SessionScope,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let sessions = self.list_sessions()?;
        match scope {
            SessionScope::AllWorkspaces => Ok(sessions
                .into_iter()
                .filter(|session| session.project_id.is_some())
                .collect()),
            SessionScope::Unscoped => Ok(sessions
                .into_iter()
                .filter(|session| session.project_id.is_none())
                .collect()),
            SessionScope::WorkspaceId(workspace_id) => Ok(sessions
                .into_iter()
                .filter(|session| session.project_id.as_deref() == Some(workspace_id.as_str()))
                .collect()),
            SessionScope::CurrentWorkspace(value) => {
                let Some(workspace_id) = self.resolve_workspace_ref(&value)? else {
                    return Ok(Vec::new());
                };
                Ok(sessions
                    .into_iter()
                    .filter(|session| session.project_id.as_deref() == Some(workspace_id.as_str()))
                    .collect())
            }
        }
    }

    pub fn resolve_workspace_ref(&self, value: &str) -> Result<Option<String>, StorageError> {
        if self.get_workspace(value).is_ok() {
            return Ok(Some(value.to_owned()));
        }
        Ok(self
            .find_workspace_by_path(value)?
            .map(|workspace| workspace.id))
    }

    pub fn has_session(&self, session_id: &str) -> Result<bool, StorageError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    pub fn is_expired_session(&self, session_id: &str) -> Result<bool, StorageError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM expired_sessions WHERE id=?1)",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    pub fn model_route(&self, session_id: &str) -> Result<Option<StoredModelRoute>, StorageError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT model_route FROM sessions WHERE id=?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StorageError::from)?;
        json.map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .transpose()
    }

    pub fn set_model_route(
        &self,
        session_id: &str,
        route: Option<&StoredModelRoute>,
    ) -> Result<(), StorageError> {
        let json = route.map(serde_json::to_string).transpose()?;
        let transaction = self.connection.unchecked_transaction()?;
        self.connection.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES(?1, datetime('now'))",
            params![session_id],
        )?;
        self.connection.execute(
            "UPDATE sessions SET model_route=?2 WHERE id=?1",
            params![session_id, json],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES(?1, datetime('now'))",
            params![session_id],
        )?;
        self.connection.execute(
            "UPDATE sessions SET workspace_root=?2,
             project_id=(SELECT id FROM projects WHERE workspace_root=?2)
             WHERE id=?1",
            params![session_id, workspace_root],
        )?;
        Ok(())
    }

    pub fn list_turns(&self, session_id: &str) -> Result<Vec<TurnSummary>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT t.session_id, t.turn_id, t.created_at,
                    (SELECT COUNT(*) FROM events e WHERE e.session_id=t.session_id AND e.turn_id=t.turn_id),
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id=t.session_id AND m.turn_id=t.turn_id)
             FROM turns t WHERE t.session_id=?1 ORDER BY t.created_at DESC, t.turn_id DESC",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(TurnSummary {
                session_id: row.get(0)?,
                turn_id: row.get(1)?,
                created_at: row.get(2)?,
                event_count: row.get(3)?,
                message_count: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_project(&self, project: &Project) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        if project.is_default {
            tx.execute(
                "UPDATE projects SET is_default=0 WHERE id<>?1",
                params![project.id],
            )?;
        }
        tx.execute(
            "INSERT INTO projects(id, display_name, workspace_root, is_default, last_opened_at)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                display_name=excluded.display_name,
                workspace_root=excluded.workspace_root,
                is_default=excluded.is_default,
                last_opened_at=excluded.last_opened_at",
            params![
                project.id,
                project.display_name,
                project.workspace_root,
                project.is_default,
                project.last_opened_at,
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET project_id=?1 WHERE workspace_root=?2",
            params![project.id, project.workspace_root],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_workspace(&self, workspace: &Workspace) -> Result<(), StorageError> {
        self.save_project(workspace)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT id, display_name, workspace_root, is_default, last_opened_at
             FROM projects ORDER BY is_default DESC, last_opened_at DESC, id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                display_name: row.get(1)?,
                workspace_root: row.get(2)?,
                is_default: row.get::<_, i64>(3)? != 0,
                last_opened_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn delete_project(&self, project_id: &str) -> Result<(), StorageError> {
        if self
            .connection
            .execute("DELETE FROM projects WHERE id=?1", params![project_id])?
            == 0
        {
            return Err(StorageError::NotFound(project_id.into()));
        }
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        let deleted = tx.execute("DELETE FROM sessions WHERE id=?1", params![session_id])?;
        let expired_deleted = tx.execute(
            "DELETE FROM expired_sessions WHERE id=?1",
            params![session_id],
        )?;
        if deleted == 0 && expired_deleted == 0 {
            tx.rollback()?;
            return Err(StorageError::NotFound(session_id.into()));
        }
        tx.execute(
            "DELETE FROM events WHERE session_id=?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE session_id=?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM diagnostic_index WHERE session_id=?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM ui_audit_events WHERE session_id=?1",
            params![session_id],
        )?;
        tx.execute("DELETE FROM turns WHERE session_id=?1", params![session_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn database_size(&self) -> Result<u64, StorageError> {
        if self.path == Path::new(":memory:") {
            return Ok(0);
        }
        Ok(std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0))
    }

    /// 清理完整 journal 时总是按 session 删除，避免留下无法恢复的半段会话。
    /// 返回删除的 session/audit 数量。字节限制按有效正文的逻辑大小计算；SQLite
    /// 文件的空闲页可由后续维护命令回收，不影响留存语义。
    pub fn prune_journal(
        &self,
        retention_days: u64,
        max_records: u64,
        max_bytes: u64,
    ) -> Result<u64, StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        let mut removed = 0;
        let cutoff = format!("-{} days", retention_days.max(1));
        let expired = {
            let mut statement = tx.prepare("SELECT id FROM sessions WHERE created_at < datetime('now', ?1) ORDER BY created_at, id")?;
            let rows = statement.query_map(params![cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for session_id in expired {
            record_expired_session(&tx, &session_id)?;
            delete_session_journal(&tx, &session_id)?;
            removed += 1;
        }
        while journal_usage(&tx)?.0 > max_records || journal_usage(&tx)?.1 > max_bytes {
            let next = tx
                .query_row(
                    "SELECT id FROM sessions ORDER BY created_at, id LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(session_id) = next else {
                break;
            };
            record_expired_session(&tx, &session_id)?;
            delete_session_journal(&tx, &session_id)?;
            removed += 1;
        }
        tx.commit()?;
        self.compact_if_needed(max_bytes)?;
        Ok(removed)
    }
    pub fn replay(&self, session_id: &str) -> Result<Vec<StoredEvent>, StorageError> {
        let mut statement = self.connection.prepare("SELECT session_id, turn_id, sequence, kind, body FROM events WHERE session_id=?1 ORDER BY sequence")?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(StoredEvent {
                session_id: row.get(0)?,
                turn_id: row.get(1)?,
                sequence: row.get(2)?,
                kind: row.get(3)?,
                body: row.get(4)?,
            })
        })?;
        let events = rows.collect::<Result<Vec<_>, _>>()?;
        for event in &events {
            if let Err(error) = serde_json::from_str::<serde_json::Value>(&event.body) {
                return Err(StorageError::CorruptEvent {
                    session_id: event.session_id.clone(),
                    sequence: event.sequence,
                    message: error.to_string(),
                });
            }
        }
        Ok(events)
    }

    pub fn last_event_sequence(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<u64, StorageError> {
        let last: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id=?1 AND turn_id=?2",
            params![session_id, turn_id],
            |row| row.get(0),
        )?;
        Ok(last.max(0) as u64)
    }
    pub fn next_message_sequence(&self, session_id: &str) -> Result<u64, StorageError> {
        let next: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(sequence) + 1, 0) FROM messages WHERE session_id=?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(next.max(0) as u64)
    }
    pub fn next_event_sequence(&self, session_id: &str) -> Result<u64, StorageError> {
        let next: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(sequence) + 1, 0) FROM events WHERE session_id=?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(next.max(0) as u64)
    }
    pub fn save_memory(&self, memory: &Memory) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO memories(id, workspace, mode, body, source) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                memory.id,
                memory.workspace,
                memory.mode,
                memory.body,
                memory.source
            ],
        )?;
        tx.execute(
            "INSERT INTO memories_fts(id, workspace, mode, body) VALUES(?1, ?2, ?3, ?4)",
            params![memory.id, memory.workspace, memory.mode, memory.body],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_memory_candidate(&self, candidate: &MemoryCandidate) -> Result<(), StorageError> {
        if candidate.body_sha256 != sha256_text(&candidate.body) {
            return Err(StorageError::NotFound("memory candidate hash 无效".into()));
        }
        self.connection.execute(
            "INSERT OR REPLACE INTO memory_candidates(id, workspace, mode, body, source, body_sha256)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                candidate.id,
                candidate.workspace,
                candidate.mode,
                candidate.body,
                candidate.source,
                candidate.body_sha256
            ],
        )?;
        Ok(())
    }

    pub fn get_memory_candidate(&self, id: &str) -> Result<Option<MemoryCandidate>, StorageError> {
        self.connection
            .query_row(
                "SELECT id, workspace, mode, body, source, body_sha256 FROM memory_candidates WHERE id=?1",
                params![id],
                |row| {
                    Ok(MemoryCandidate {
                        id: row.get(0)?,
                        workspace: row.get(1)?,
                        mode: row.get(2)?,
                        body: row.get(3)?,
                        source: row.get(4)?,
                        body_sha256: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }
    pub fn confirm_memory(
        &self,
        candidate: &MemoryCandidate,
        body_sha256: &str,
    ) -> Result<Memory, StorageError> {
        if candidate.body_sha256 != body_sha256
            || candidate.body_sha256 != sha256_text(&candidate.body)
        {
            return Err(StorageError::NotFound("memory candidate 已变化".into()));
        }
        let memory = Memory {
            id: candidate.id.clone(),
            workspace: candidate.workspace.clone(),
            mode: candidate.mode.clone(),
            body: candidate.body.clone(),
            source: candidate.source.clone(),
        };
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO memories(id, workspace, mode, body, source) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                memory.id,
                memory.workspace,
                memory.mode,
                memory.body,
                memory.source
            ],
        )?;
        tx.execute(
            "INSERT INTO memories_fts(id, workspace, mode, body) VALUES(?1, ?2, ?3, ?4)",
            params![memory.id, memory.workspace, memory.mode, memory.body],
        )?;
        tx.execute(
            "DELETE FROM memory_candidates WHERE id=?1",
            params![candidate.id],
        )?;
        tx.commit()?;
        Ok(memory)
    }
    pub fn reject_memory(&self, candidate_id: &str) -> Result<(), StorageError> {
        self.connection.execute(
            "DELETE FROM memory_candidates WHERE id=?1",
            params![candidate_id],
        )?;
        Ok(())
    }
    pub fn recall(
        &self,
        workspace: &str,
        mode: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, StorageError> {
        let limit = limit.min(5);
        let mut statement = self.connection.prepare("SELECT id, workspace, mode, body, source FROM memories WHERE workspace=?1 AND mode=?2 ORDER BY rowid DESC LIMIT ?3")?;
        let rows = statement.query_map(params![workspace, mode, limit as i64], |row| {
            Ok(Memory {
                id: row.get(0)?,
                workspace: row.get(1)?,
                mode: row.get(2)?,
                body: row.get(3)?,
                source: row.get(4)?,
            })
        })?;
        let mut total = 0usize;
        let mut result = Vec::new();
        for memory in rows.collect::<Result<Vec<_>, _>>()? {
            if result.len() >= 5 || total.saturating_add(memory.body.len()) > 4096 {
                break;
            }
            total += memory.body.len();
            result.push(memory);
        }
        Ok(result)
    }

    pub fn list_memories(
        &self,
        workspace: &str,
        mode: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>, StorageError> {
        let limit = limit.min(50);
        let mut result = Vec::new();
        if let Some(mode) = mode {
            let mut statement = self.connection.prepare(
                "SELECT id, workspace, mode, body, source FROM memories
                 WHERE workspace=?1 AND mode=?2 ORDER BY rowid DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(params![workspace, mode, limit as i64], |row| {
                Ok(Memory {
                    id: row.get(0)?,
                    workspace: row.get(1)?,
                    mode: row.get(2)?,
                    body: row.get(3)?,
                    source: row.get(4)?,
                })
            })?;
            for row in rows {
                result.push(row?);
            }
        } else {
            let mut statement = self.connection.prepare(
                "SELECT id, workspace, mode, body, source FROM memories
                 WHERE workspace=?1 ORDER BY rowid DESC LIMIT ?2",
            )?;
            let rows = statement.query_map(params![workspace, limit as i64], |row| {
                Ok(Memory {
                    id: row.get(0)?,
                    workspace: row.get(1)?,
                    mode: row.get(2)?,
                    body: row.get(3)?,
                    source: row.get(4)?,
                })
            })?;
            for row in rows {
                result.push(row?);
            }
        }
        Ok(result)
    }

    pub fn list_memory_candidates(
        &self,
        workspace: &str,
        limit: usize,
    ) -> Result<Vec<MemoryCandidate>, StorageError> {
        let limit = limit.min(50);
        let mut statement = self.connection.prepare(
            "SELECT id, workspace, mode, body, source, body_sha256
             FROM memory_candidates WHERE workspace=?1 ORDER BY rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![workspace, limit as i64], |row| {
            Ok(MemoryCandidate {
                id: row.get(0)?,
                workspace: row.get(1)?,
                mode: row.get(2)?,
                body: row.get(3)?,
                source: row.get(4)?,
                body_sha256: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_memory(&self, id: &str) -> Result<Option<Memory>, StorageError> {
        self.connection
            .query_row(
                "SELECT id, workspace, mode, body, source FROM memories WHERE id=?1",
                params![id],
                |row| {
                    Ok(Memory {
                        id: row.get(0)?,
                        workspace: row.get(1)?,
                        mode: row.get(2)?,
                        body: row.get(3)?,
                        source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn delete_memory_scoped(&self, workspace: &str, id: &str) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT id FROM memories WHERE id=?1 AND workspace=?2",
                params![id, workspace],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StorageError::NotFound(id.into()));
        }
        tx.execute(
            "DELETE FROM memories WHERE id=?1 AND workspace=?2",
            params![id, workspace],
        )?;
        tx.execute("DELETE FROM memories_fts WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_memory(&self, id: &str) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        let source: Option<String> = tx
            .query_row(
                "SELECT body FROM memories WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if source.is_none() {
            return Err(StorageError::NotFound(id.into()));
        }
        tx.execute("DELETE FROM memories WHERE id=?1", params![id])?;
        tx.execute("DELETE FROM memories_fts WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn clear_history(&self) -> Result<(), StorageError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM events", [])?;
        tx.execute("DELETE FROM messages", [])?;
        tx.execute("DELETE FROM turns", [])?;
        tx.execute("DELETE FROM sessions", [])?;
        tx.execute("DELETE FROM diagnostic_index", [])?;
        tx.execute("DELETE FROM ui_audit_events", [])?;
        tx.commit()?;
        self.compact_if_needed(0)?;
        Ok(())
    }

    /// Reclaims SQLite free pages after retention cleanup.  The logical journal
    /// limit remains transactionally enforced above; compaction keeps the on-disk
    /// file from growing indefinitely after large sessions are removed.
    fn compact_if_needed(&self, max_bytes: u64) -> Result<(), StorageError> {
        if self.path == Path::new(":memory:") {
            return Ok(());
        }
        if max_bytes == 0 || self.database_size()? > max_bytes {
            self.connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
        Ok(())
    }
}

fn delete_session_journal(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM diagnostic_index WHERE session_id=?1",
        params![session_id],
    )?;
    tx.execute(
        "DELETE FROM ui_audit_events WHERE session_id=?1",
        params![session_id],
    )?;
    tx.execute("DELETE FROM sessions WHERE id=?1", params![session_id])?;
    Ok(())
}

fn record_expired_session(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO expired_sessions(id, created_at, expired_at, event_count, message_count, reason)
         SELECT ?1, s.created_at, strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                (SELECT COUNT(*) FROM events WHERE session_id=?1),
                (SELECT COUNT(*) FROM messages WHERE session_id=?1),
                'local_retention_policy'
         FROM sessions s WHERE s.id=?1",
        params![session_id],
    )?;
    Ok(())
}

fn journal_usage(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<(u64, u64)> {
    let events: (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(body)), 0) FROM events",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let messages: (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(body)), 0) FROM messages",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let audit: (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM ui_audit_events",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((
        (events.0 + messages.0 + audit.0).max(0) as u64,
        (events.1 + messages.1 + audit.1).max(0) as u64,
    ))
}

fn diagnostic_fields(body: &str) -> (String, String, bool, String) {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let details = value.get("details").unwrap_or(&Value::Null);
    let stage = details
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let code = details
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let retryable = details
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| details.get("cause").and_then(Value::as_str))
        .unwrap_or("未知错误");
    (stage, code, retryable, truncate_diagnostic(summary, 512))
}

fn truncate_diagnostic(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

pub fn sha256_text(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn validate_stored_a2a_peer(peer: &StoredA2aPeer) -> Result<(), StorageError> {
    if peer.snapshot.peer_id.0.trim().is_empty() {
        return Err(StorageError::InvalidA2aState("peer_id 不能为空".into()));
    }
    // PSK transport has no durable client credential by design: its secret,
    // ephemeral key, and session key never leave process memory. mTLS peers
    // still require an OS-keychain identity reference.
    if peer.snapshot.status == A2aTrustState::Trusted
        && peer.snapshot.protocol_binding != "PSK"
        && peer
            .identity_ref
            .as_deref()
            .is_none_or(|identity_ref| identity_ref.trim().is_empty())
    {
        return Err(StorageError::InvalidA2aState(
            "trusted peer 必须有 OS keychain identity_ref".into(),
        ));
    }
    if peer.snapshot.status == A2aTrustState::Trusted {
        peer.snapshot
            .validate_transport()
            .map_err(|error| StorageError::InvalidA2aState(error.to_string()))?;
    }
    Ok(())
}

fn a2a_trust_state_name(state: A2aTrustState) -> &'static str {
    match state {
        A2aTrustState::Candidate => "candidate",
        A2aTrustState::Pending => "pending",
        A2aTrustState::Trusted => "trusted",
        A2aTrustState::Suspended => "suspended",
        A2aTrustState::Revoked => "revoked",
    }
}

fn a2a_trust_state_from_name(value: &str) -> Result<A2aTrustState, StorageError> {
    match value {
        "candidate" => Ok(A2aTrustState::Candidate),
        "pending" => Ok(A2aTrustState::Pending),
        "trusted" => Ok(A2aTrustState::Trusted),
        "suspended" => Ok(A2aTrustState::Suspended),
        "revoked" => Ok(A2aTrustState::Revoked),
        _ => Err(StorageError::InvalidA2aState(format!(
            "未知 peer state: {value}"
        ))),
    }
}

fn a2a_task_state_name(state: A2aTaskState) -> &'static str {
    match state {
        A2aTaskState::Submitted => "SUBMITTED",
        A2aTaskState::Working => "WORKING",
        A2aTaskState::InputRequired => "INPUT_REQUIRED",
        A2aTaskState::Completed => "COMPLETED",
        A2aTaskState::Failed => "FAILED",
        A2aTaskState::Canceled => "CANCELED",
        A2aTaskState::Rejected => "REJECTED",
    }
}

fn a2a_task_state_from_name(value: &str) -> Result<A2aTaskState, StorageError> {
    match value {
        "SUBMITTED" => Ok(A2aTaskState::Submitted),
        "WORKING" => Ok(A2aTaskState::Working),
        "INPUT_REQUIRED" => Ok(A2aTaskState::InputRequired),
        "COMPLETED" => Ok(A2aTaskState::Completed),
        "FAILED" => Ok(A2aTaskState::Failed),
        "CANCELED" => Ok(A2aTaskState::Canceled),
        "REJECTED" => Ok(A2aTaskState::Rejected),
        _ => Err(StorageError::InvalidA2aState(format!(
            "未知 task state: {value}"
        ))),
    }
}

fn invalid_a2a_column(error: StorageError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn row_to_a2a_peer(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredA2aPeer> {
    let skills: std::collections::BTreeSet<String> =
        serde_json::from_str(&row.get::<_, String>(7)?)
            .map_err(StorageError::from)
            .map_err(invalid_a2a_column)?;
    let status =
        a2a_trust_state_from_name(&row.get::<_, String>(10)?).map_err(invalid_a2a_column)?;
    Ok(StoredA2aPeer {
        snapshot: A2aTrustSnapshot {
            peer_id: A2aPeerId(row.get(0)?),
            interface_url: row.get(1)?,
            protocol_binding: row.get(2)?,
            tenant: row.get(3)?,
            spki_sha256: row.get(4)?,
            agent_card_id: row.get(5)?,
            agent_card_jcs_sha256: row.get(6)?,
            allowed_skills: skills,
            spec_revision: row.get(8)?,
            wire_protocol_version: row.get(9)?,
            status,
            approved_at: row.get(11)?,
        },
        identity_ref: row.get(12)?,
    })
}

fn row_to_goal(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredGoal> {
    Ok(StoredGoal {
        goal_id: row.get(0)?,
        session_id: row.get(1)?,
        objective: row.get(2)?,
        status: row.get(3)?,
        budget: row.get::<_, i64>(4)? as u64,
        used_budget: row.get::<_, i64>(5)? as u64,
        revision: row.get::<_, i64>(6)? as u64,
        last_evidence_summary: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_task_plan(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTaskPlan> {
    Ok(StoredTaskPlan {
        plan_id: row.get(0)?,
        session_id: row.get(1)?,
        objective: row.get(2)?,
        revision: row.get::<_, i64>(3)? as u64,
        status: row.get(4)?,
        payload: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn sidecar_task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSidecarTask> {
    Ok(StoredSidecarTask {
        task_id: row.get(0)?,
        intent_id: row.get(1)?,
        workspace_id: row.get(2)?,
        session_id: row.get(3)?,
        turn_id: row.get(4)?,
        state: row.get(5)?,
        lease_epoch: row.get(6)?,
        expires_at: row.get(7)?,
        last_event_sequence: row.get(8)?,
        tool_call_status: row.get(9)?,
        terminal_reason: row.get(10)?,
    })
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredArtifact> {
    Ok(StoredArtifact {
        artifact_id: row.get(0)?,
        node_id: row.get(1)?,
        workspace_id: row.get(2)?,
        session_id: row.get(3)?,
        kind: row.get(4)?,
        size: row.get(5)?,
        hash: row.get(6)?,
        expires_at: row.get(7)?,
        download_scope: row.get(8)?,
        storage_key: row.get(9)?,
        state: row.get(10)?,
    })
}

fn row_to_a2a_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredA2aTask> {
    let revision = row
        .get::<_, Option<i64>>(4)?
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                invalid_a2a_column(StorageError::InvalidA2aState(
                    "A2A task revision 不能为负数".into(),
                ))
            })
        })
        .transpose()?;
    Ok(StoredA2aTask {
        peer_id: A2aPeerId(row.get(0)?),
        task_id: A2aTaskId(row.get(1)?),
        request_sha256: row.get(2)?,
        state: a2a_task_state_from_name(&row.get::<_, String>(3)?).map_err(invalid_a2a_column)?,
        revision,
    })
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_14_backup_and_reset_preserve_non_session_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let backup = directory.path().join("rivo-schema-14.db");
        let store = Store::open(&path).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO projects(id, display_name, workspace_root, is_default, last_opened_at) VALUES('project-1', 'Project', 'C:/workspace', 1, '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO memories(id, workspace, mode, body, source) VALUES('memory-1', 'C:/workspace', 'coding', 'preserve me', 'test')",
                [],
            )
            .unwrap();
        store
            .save_a2a_peer(&a2a_peer(A2aTrustState::Pending))
            .unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "legacy-session".into(),
                turn_id: "legacy-turn".into(),
                sequence: 1,
                kind: "message".into(),
                body: "must remain only in backup".into(),
            })
            .unwrap();
        store
            .connection
            .execute("UPDATE schema_version SET version=14", [])
            .unwrap();
        drop(store);

        let before = std::fs::read(&path).unwrap();
        let info = backup_database(&path, &backup).unwrap();
        assert_eq!(info.schema_version, Some(14));
        assert_eq!(info.sha256, file_sha256(&backup).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(inspect_database(&backup).quick_check.as_deref(), Some("ok"));
        assert!(backup_database(&path, &backup).is_err());

        reset_schema_14_sessions(&path, &backup).unwrap();
        assert_eq!(inspect_database(&path).schema_version, Some(SCHEMA_VERSION));
        assert_eq!(inspect_database(&backup).schema_version, Some(14));
        let rebuilt = Store::open(&path).unwrap();
        assert_eq!(
            rebuilt
                .connection
                .query_row("SELECT COUNT(*) FROM projects", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            rebuilt.recall("C:/workspace", "coding", 5).unwrap().len(),
            1
        );
        assert_eq!(rebuilt.a2a_peers().unwrap().len(), 1);
        assert!(rebuilt.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn reset_refuses_unverified_backup_and_non_schema_14_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let backup = directory.path().join("backup.db");
        Store::open(&path).unwrap();
        std::fs::write(&backup, "not a database").unwrap();
        assert!(matches!(
            reset_schema_14_sessions(&path, &backup),
            Err(StorageError::UnsupportedSchema(18))
        ));
    }
    #[test]
    fn migration_replay_and_memory_scope_work() {
        let store = Store::open_memory().unwrap();
        let has_event_timestamp: bool = store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('events') WHERE name='created_at')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_event_timestamp);
        store
            .append_event(&StoredEvent {
                session_id: "s".into(),
                turn_id: "r".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        assert_eq!(store.replay("s").unwrap().len(), 1);
        let candidate = MemoryCandidate {
            id: "m".into(),
            workspace: "w".into(),
            mode: "research".into(),
            body: "fact".into(),
            source: "s".into(),
            body_sha256: sha256_text("fact"),
        };
        store
            .confirm_memory(&candidate, &candidate.body_sha256)
            .unwrap();
        assert_eq!(store.recall("w", "research", 5).unwrap().len(), 1);
    }

    #[test]
    fn session_messages_are_additive_to_schema_18_and_transition_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rivo.db");
        {
            let store = Store::open(&path).unwrap();
            // 模拟由旧二进制创建、尚无追加表的 schema 18 数据库。
            store
                .connection
                .execute("DROP TABLE session_messages", [])
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(inspect_database(&path).schema_version, Some(SCHEMA_VERSION));
        store.ensure_session("receiver").unwrap();
        let message = store
            .insert_session_message("msg-1", "sender", "receiver", "note", "hello", "held")
            .unwrap();
        assert_eq!(message.state, "held");
        assert!(message.expires_at > message.created_at);
        store
            .insert_session_message("msg-2", "sender", "receiver", "note", "second", "pending")
            .unwrap();
        assert!(store
            .insert_session_message("msg-3", "sender", "missing", "note", "x", "pending")
            .is_err());

        assert!(store
            .transition_session_message("msg-1", &["held"], "pending")
            .unwrap());
        assert!(!store
            .transition_session_message("msg-1", &["held"], "pending")
            .unwrap());
        let pending = store
            .session_messages_for("receiver", Some("pending"))
            .unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["msg-1", "msg-2"]
        );

        store
            .connection
            .execute(
                "UPDATE session_messages SET expires_at='2000-01-01T00:00:00.000Z' WHERE message_id='msg-2'",
                [],
            )
            .unwrap();
        assert!(!store
            .transition_session_message("msg-2", &["pending"], "delivered")
            .unwrap());
        assert_eq!(store.expire_session_messages().unwrap(), 1);
        assert_eq!(
            store.session_message("msg-2").unwrap().unwrap().state,
            "expired"
        );

        store.delete_session("receiver").unwrap();
        assert!(store
            .session_messages_for("receiver", None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn diagnostic_index_tracks_full_events_and_audit_records() {
        let store = Store::open_memory().unwrap();
        store.append_event(&StoredEvent {
            session_id: "s".into(), turn_id: "r".into(), sequence: 1, kind: "failure".into(),
            body: serde_json::json!({"type":"failure","message":"provider failed","details":{"stage":"stream","code":"provider_stream_failed","retryable":true,"cause":"full cause"}}).to_string(),
        }).unwrap();
        store.append_ui_audit(&UiAuditEvent {
            session_id: Some("s".into()), turn_id: Some("r".into()), action: "failure".into(),
            payload: serde_json::json!({"message":"ui failed","details":{"stage":"tui","code":"input","retryable":false}}).to_string(),
        }).unwrap();
        let entries = store.list_diagnostics(Some("s"), 10).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|entry| entry.code == "provider_stream_failed" && entry.retryable));
        let export = store.full_diagnostic_journal().unwrap();
        assert!(export["sessions"][0]["events"][0]["body"]
            .as_str()
            .unwrap()
            .contains("full cause"));
        store.clear_diagnostics().unwrap();
        assert!(store.list_diagnostics(None, 10).unwrap().is_empty());
    }

    #[test]
    fn prune_removes_complete_session_and_compacts_file_store() {
        let path = std::env::temp_dir().join(format!("rivo-journal-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "old".into(),
                turn_id: "turn".into(),
                sequence: 1,
                kind: "message".into(),
                body: "full prompt body".into(),
            })
            .unwrap();
        assert!(store.has_session("old").unwrap());
        assert_eq!(
            store
                .prune_journal(30, 0, DEFAULT_MAX_JOURNAL_BYTES)
                .unwrap(),
            1
        );
        assert!(!store.has_session("old").unwrap());
        assert!(store.replay("old").unwrap().is_empty());
        let expired = store.list_sessions().unwrap();
        let expired_entry = expired
            .iter()
            .find(|entry| entry.session_id == "old" && entry.expired)
            .unwrap();
        assert!(expired_entry.expiration_reason.is_some());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_history_lists_deletes_and_replays_messages() {
        let store = Store::open_memory().unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "s2".into(),
                turn_id: "r2".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "s2".into(),
                turn_id: "r2".into(),
                sequence: 2,
                role: "user".into(),
                body: "hello".into(),
            })
            .unwrap();
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions[0].session_id, "s2");
        assert_eq!(sessions[0].task_summary.as_deref(), Some("hello"));
        assert_eq!(sessions[0].event_count, 1);
        assert_eq!(sessions[0].message_count, 1);
        assert_eq!(store.replay_messages("s2").unwrap()[0].body, "hello");
        assert!(store.has_session("s2").unwrap());
        assert!(!store.has_session("missing").unwrap());
        store.delete_session("s2").unwrap();
        assert!(store.replay("s2").unwrap().is_empty());
        assert!(matches!(
            store.delete_session("s2"),
            Err(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn session_model_route_preserves_auto_and_explicit_selection() {
        let store = Store::open_memory().unwrap();
        let mut route = StoredModelRoute {
            selection: ModelSelection {
                vendor: "openai".into(),
                model: "gpt-5".into(),
                provider: ProviderSelection::Auto,
            },
            provider_id: "gateway".into(),
            protocol: "openai_responses".into(),
            base_url: "https://example.invalid/v1".into(),
            priority: 100,
        };
        store
            .set_model_route("route-session", Some(&route))
            .unwrap();
        assert_eq!(
            store.model_route("route-session").unwrap(),
            Some(route.clone())
        );
        route.selection.provider = ProviderSelection::Explicit {
            provider_id: "gateway".into(),
        };
        store
            .set_model_route("route-session", Some(&route))
            .unwrap();
        assert_eq!(store.model_route("route-session").unwrap(), Some(route));
        store.set_model_route("route-session", None).unwrap();
        assert!(store.model_route("route-session").unwrap().is_none());
        let columns = store
            .connection
            .prepare("PRAGMA table_info(sessions)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.contains(&"model_route".into()));
        assert!(!columns.contains(&"provider_profile".into()));
    }

    #[test]
    fn projects_have_one_default_and_unique_workspace_roots() {
        let store = Store::open_memory().unwrap();
        store
            .save_project(&Project {
                id: "default".into(),
                display_name: "Default project".into(),
                workspace_root: "C:/work/default".into(),
                is_default: true,
                last_opened_at: "2026-07-19T00:00:00Z".into(),
            })
            .unwrap();
        store
            .save_project(&Project {
                id: "other".into(),
                display_name: "Other project".into(),
                workspace_root: "C:/work/other".into(),
                is_default: true,
                last_opened_at: "2026-07-19T01:00:00Z".into(),
            })
            .unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "other");
        assert!(projects[0].is_default);
        assert!(!projects[1].is_default);
        assert!(store
            .save_project(&Project {
                id: "duplicate-root".into(),
                display_name: "Duplicate".into(),
                workspace_root: "C:/work/other".into(),
                is_default: false,
                last_opened_at: "2026-07-19T02:00:00Z".into(),
            })
            .is_err());
        store.delete_project("other").unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn workspace_api_deduplicates_paths_and_scopes_sessions() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = Store::open_memory().unwrap();
        let first = store.ensure_workspace(root.path(), None).unwrap();
        let second = store
            .ensure_workspace(root.path().join("."), Some("renamed"))
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.display_name, first.display_name);
        let other_workspace = store.ensure_workspace(other.path(), None).unwrap();
        store.bind_session_to_workspace("s1", &first.id).unwrap();
        store
            .bind_session_to_workspace("s2", &other_workspace.id)
            .unwrap();
        store.ensure_session("legacy").unwrap();
        assert_eq!(
            store
                .list_sessions_scoped(SessionScope::WorkspaceId(first.id.clone()))
                .unwrap()
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1"]
        );
        assert_eq!(
            store
                .list_sessions_scoped(SessionScope::WorkspaceId(other_workspace.id.clone()))
                .unwrap()
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s2"]
        );
        assert_eq!(
            store
                .list_sessions_scoped(SessionScope::Unscoped)
                .unwrap()
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["legacy"]
        );
        store.attach_unscoped_session("legacy", &first.id).unwrap();
        assert_eq!(
            store
                .list_sessions_scoped(SessionScope::Unscoped)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn deleting_workspace_cascades_and_reselects_default() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let store = Store::open_memory().unwrap();
        let first = store.ensure_workspace(first_root.path(), None).unwrap();
        let second = store.ensure_workspace(second_root.path(), None).unwrap();
        store
            .bind_session_to_workspace("session", &first.id)
            .unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "session".into(),
                turn_id: "turn".into(),
                sequence: 0,
                role: "user".into(),
                body: "body".into(),
            })
            .unwrap();
        store.delete_workspace_cascade(&first.id).unwrap();
        assert!(!store.has_session("session").unwrap());
        assert!(store
            .list_workspaces()
            .unwrap()
            .iter()
            .any(|workspace| workspace.id == second.id && workspace.is_default));
    }

    #[test]
    fn deleting_workspace_is_rejected_while_a_session_holds_a_lease() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("rivo.db")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let workspace = store.ensure_workspace(root.path(), None).unwrap();
        let lease = store.acquire_workspace_lease(&workspace.id).unwrap();

        let error = store.delete_workspace_cascade(&workspace.id).unwrap_err();
        assert!(error.to_string().contains("正在运行"));
        assert!(store.get_workspace(&workspace.id).is_ok());

        drop(lease);
        store.delete_workspace_cascade(&workspace.id).unwrap();
        assert!(store.get_workspace(&workspace.id).is_err());
    }

    #[test]
    fn schema_17_workspace_migration_binds_valid_roots_and_keeps_unknown_unscoped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.db");
        let backup = directory.path().join("legacy-backup.db");
        let valid_root = tempfile::tempdir().unwrap();
        {
            let store = Store::open(&path).unwrap();
            store
                .connection
                .execute("UPDATE schema_version SET version=17", [])
                .unwrap();
            store.ensure_session("valid").unwrap();
            store
                .set_workspace_root("valid", valid_root.path().to_string_lossy().as_ref())
                .unwrap();
            store.ensure_session("unknown").unwrap();
            store
                .set_workspace_root("unknown", "C:/definitely/missing")
                .unwrap();
            let raw_root = valid_root.path().join(".");
            store
                .connection
                .execute(
                    "INSERT INTO projects(id, display_name, workspace_root, is_default, last_opened_at) VALUES('legacy-project', 'Legacy', ?1, 0, '2026-01-01T00:00:00Z')",
                    params![raw_root.to_string_lossy().as_ref()],
                )
                .unwrap();
            store.ensure_session("project-session").unwrap();
            store
                .connection
                .execute(
                    "UPDATE sessions SET project_id='legacy-project', workspace_root=?1 WHERE id='project-session'",
                    params![raw_root.to_string_lossy().as_ref()],
                )
                .unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO projects(id, display_name, workspace_root, is_default, last_opened_at) VALUES('invalid-project', 'Invalid', 'C:/definitely/missing-project', 0, '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
            store.ensure_session("invalid-project-session").unwrap();
            store
                .connection
                .execute(
                    "UPDATE sessions SET project_id='invalid-project', workspace_root='C:/definitely/missing-project' WHERE id='invalid-project-session'",
                    [],
                )
                .unwrap();
        }
        backup_database(&path, &backup).unwrap();
        migrate_schema_17_workspaces(&path, &backup).unwrap();
        let migrated = Store::open(&path).unwrap();
        assert!(migrated.session_workspace_id("valid").unwrap().is_some());
        assert_eq!(migrated.session_workspace_id("unknown").unwrap(), None);
        assert_eq!(
            migrated
                .session_workspace_id("project-session")
                .unwrap()
                .as_deref(),
            Some("legacy-project")
        );
        assert_eq!(
            migrated
                .session_workspace_id("invalid-project-session")
                .unwrap(),
            None
        );
        assert_eq!(
            migrated
                .get_workspace("legacy-project")
                .unwrap()
                .workspace_root,
            valid_root
                .path()
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        drop(migrated);
        migrate_schema_17_workspaces(&path, &backup).unwrap();
    }

    #[test]
    fn automatic_schema_17_migration_creates_backup_and_reports_scope_counts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let workspace = tempfile::tempdir().unwrap();
        {
            let store = Store::open(&path).unwrap();
            store.ensure_session("legacy").unwrap();
            store
                .set_workspace_root("legacy", workspace.path().to_string_lossy().as_ref())
                .unwrap();
            store
                .append_event(&StoredEvent {
                    session_id: "legacy".into(),
                    turn_id: "turn".into(),
                    sequence: 1,
                    kind: "started".into(),
                    body: "{\"ok\":true}".into(),
                })
                .unwrap();
            store
                .append_message(&StoredMessage {
                    session_id: "legacy".into(),
                    turn_id: "turn".into(),
                    sequence: 1,
                    role: "user".into(),
                    body: "keep this history".into(),
                })
                .unwrap();
            store
                .connection
                .execute("UPDATE schema_version SET version=17", [])
                .unwrap();
        }

        let report = auto_migrate_schema_17_workspaces(&path).unwrap();
        assert!(!report.already_current);
        assert_eq!(report.bound_session_count, 1);
        assert_eq!(report.unscoped_session_count, 0);
        let backup = report.backup.unwrap();
        let backup_path = PathBuf::from(&backup.destination);
        assert!(backup_path.is_file());
        assert!(backup_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("rivo-schema-17-backup-"));
        assert_eq!(inspect_database(&backup_path).schema_version, Some(17));
        assert_eq!(inspect_database(&path).schema_version, Some(18));

        let migrated = Store::open(&path).unwrap();
        assert_eq!(migrated.list_sessions().unwrap().len(), 1);
        assert!(migrated.session_workspace_id("legacy").unwrap().is_some());
        assert_eq!(migrated.replay("legacy").unwrap().len(), 1);
        assert_eq!(migrated.replay_messages("legacy").unwrap().len(), 1);
        drop(migrated);

        let second = auto_migrate_schema_17_workspaces(&path).unwrap();
        assert!(second.already_current);
        assert!(second.backup.is_none());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rivo-schema-17-backup-"))
                .count(),
            1
        );
    }

    #[test]
    fn automatic_schema_17_migration_refuses_an_existing_maintenance_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .connection
                .execute("UPDATE schema_version SET version=17", [])
                .unwrap();
        }
        let lease = acquire_database_lease(&path, true).unwrap();
        let error = auto_migrate_schema_17_workspaces(&path).unwrap_err();
        assert!(error.to_string().contains("数据库正在被其他 Rivo 进程使用"));
        assert_eq!(inspect_database(&path).schema_version, Some(17));
        drop(lease);
    }

    #[test]
    fn automatic_migration_rejects_non_schema_17_without_creating_backup() {
        for version in [14, 99] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("rivo.db");
            let store = Store::open(&path).unwrap();
            store
                .connection
                .execute("UPDATE schema_version SET version=?1", params![version])
                .unwrap();
            drop(store);
            let before = std::fs::read(&path).unwrap();

            assert!(matches!(
                auto_migrate_schema_17_workspaces(&path),
                Err(StorageError::UnsupportedSchema(value)) if value == version
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                std::fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("rivo-schema-17-backup-"))
                    .count(),
                0
            );
        }
    }

    #[test]
    fn automatic_backup_path_never_overwrites_an_existing_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let timestamp = OffsetDateTime::UNIX_EPOCH;
        let first = automatic_backup_path_at(&path, timestamp).unwrap();
        std::fs::write(&first, b"reserved").unwrap();
        let second = automatic_backup_path_at(&path, timestamp).unwrap();
        assert_ne!(first, second);
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-1.db"));
        assert_eq!(std::fs::read(&first).unwrap(), b"reserved");
    }

    #[test]
    fn session_workspace_root_is_available_to_history_consumers() {
        let store = Store::open_memory().unwrap();
        store
            .save_project(&Project {
                id: "default".into(),
                display_name: "Default project".into(),
                workspace_root: "C:/work/default".into(),
                is_default: true,
                last_opened_at: "2026-09-10T00:00:00Z".into(),
            })
            .unwrap();
        store
            .set_workspace_root("project-session", "C:/work/default")
            .unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "project-session".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        let sessions = store.list_sessions().unwrap();
        assert_eq!(
            sessions[0].workspace_root.as_deref(),
            Some("C:/work/default")
        );
        assert_eq!(sessions[0].project_id.as_deref(), Some("default"));
        assert_eq!(
            store.list_turns("project-session").unwrap(),
            vec![TurnSummary {
                session_id: "project-session".into(),
                turn_id: "turn-1".into(),
                created_at: store.list_turns("project-session").unwrap()[0]
                    .created_at
                    .clone(),
                event_count: 1,
                message_count: 0,
            }]
        );
    }

    #[test]
    fn memory_candidate_lifecycle_is_hash_bound() {
        let store = Store::open_memory().unwrap();
        let candidate = MemoryCandidate {
            id: "candidate".into(),
            workspace: "w".into(),
            mode: "research".into(),
            body: "remember this".into(),
            source: "session/turn/event".into(),
            body_sha256: sha256_text("remember this"),
        };
        store.save_memory_candidate(&candidate).unwrap();
        assert_eq!(
            store.get_memory_candidate("candidate").unwrap(),
            Some(candidate.clone())
        );
        assert!(store.confirm_memory(&candidate, "wrong").is_err());
        let memory = store
            .confirm_memory(&candidate, &candidate.body_sha256)
            .unwrap();
        assert_eq!(memory.body, "remember this");
        assert_eq!(store.get_memory_candidate("candidate").unwrap(), None);

        let rejected = MemoryCandidate {
            id: "rejected".into(),
            ..candidate
        };
        store.save_memory_candidate(&rejected).unwrap();
        store.reject_memory("rejected").unwrap();
        assert_eq!(store.get_memory_candidate("rejected").unwrap(), None);
    }

    #[test]
    fn memory_management_queries_and_delete_are_workspace_scoped() {
        let store = Store::open_memory().unwrap();
        store
            .save_memory(&Memory {
                id: "memory-w".into(),
                workspace: "w".into(),
                mode: "research".into(),
                body: "workspace w".into(),
                source: "source-w".into(),
            })
            .unwrap();
        store
            .save_memory(&Memory {
                id: "memory-x".into(),
                workspace: "x".into(),
                mode: "research".into(),
                body: "workspace x".into(),
                source: "source-x".into(),
            })
            .unwrap();
        let candidate = MemoryCandidate {
            id: "candidate-w".into(),
            workspace: "w".into(),
            mode: "research".into(),
            body: "candidate w".into(),
            source: "source-c".into(),
            body_sha256: sha256_text("candidate w"),
        };
        store.save_memory_candidate(&candidate).unwrap();

        assert_eq!(store.list_memories("w", None, 50).unwrap().len(), 1);
        assert_eq!(
            store.list_memories("x", None, 50).unwrap()[0].id,
            "memory-x"
        );
        assert_eq!(
            store.list_memory_candidates("w", 50).unwrap(),
            vec![candidate]
        );
        assert!(matches!(
            store.delete_memory_scoped("x", "memory-w"),
            Err(StorageError::NotFound(_))
        ));
        assert!(store.get_memory("memory-w").unwrap().is_some());
        store.delete_memory_scoped("w", "memory-w").unwrap();
        assert!(store.get_memory("memory-w").unwrap().is_none());
        assert!(store.get_memory("memory-x").unwrap().is_some());
    }

    #[test]
    fn future_schema_versions_are_rejected_without_rebuilding() {
        let store = Store::open_memory().unwrap();
        store
            .connection
            .execute("UPDATE schema_version SET version=99", [])
            .unwrap();
        let error = store.initialize_schema().unwrap_err();
        assert!(matches!(error, StorageError::UnsupportedSchema(99)));
    }

    #[test]
    fn incomplete_current_schema_is_rejected_without_rebuilding() {
        let store = Store::open_memory().unwrap();
        store.connection.execute("DROP TABLE turns", []).unwrap();

        let error = store.initialize_schema().unwrap_err();

        assert!(matches!(error, StorageError::UnsupportedSchema(18)));
        let exists: bool = store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='turns')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn old_schema_is_rejected_even_when_partially_modified() {
        let store = Store::open_memory().unwrap();
        store
            .connection
            .execute("UPDATE schema_version SET version=11", [])
            .unwrap();
        // Simulate a partially applied v12 schema: the first column already exists,
        // so the next migration attempt must fail explicitly rather than panic.
        store
            .connection
            .execute("DROP TABLE a2a_peers", [])
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TABLE a2a_peers(
                    peer_id TEXT PRIMARY KEY,
                    endpoint TEXT NOT NULL,
                    spki_sha256 TEXT NOT NULL,
                    agent_card_sha256 TEXT NOT NULL,
                    protocol_version TEXT NOT NULL,
                    skill_ids_json TEXT NOT NULL,
                    state TEXT NOT NULL,
                    identity_ref TEXT NULL,
                    interface_url TEXT NULL
                );",
            )
            .unwrap();

        let error = store.initialize_schema().unwrap_err();
        assert!(matches!(error, StorageError::UnsupportedSchema(11)));
        let version: u64 = store
            .connection
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 11);
    }

    #[test]
    fn readonly_connection_returns_structured_write_error_without_partial_event() {
        let store = Store::open_memory().unwrap();
        store
            .connection
            .execute_batch("PRAGMA query_only = ON;")
            .unwrap();
        let error = store
            .append_event(&StoredEvent {
                session_id: "readonly-session".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));
        assert!(store.replay("readonly-session").unwrap().is_empty());
    }

    #[test]
    fn opening_a_directory_returns_a_structured_sqlite_error() {
        let directory = tempfile::tempdir().unwrap();
        let error = match Store::open(directory.path()) {
            Ok(_) => panic!("opening a directory must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, StorageError::Sqlite(_)));
    }

    #[test]
    fn explicit_close_succeeds_for_a_healthy_store() {
        let store = Store::open_memory().unwrap();
        assert!(store.close().is_ok());
    }

    #[test]
    fn explicit_close_reports_an_active_transaction_as_structured_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("close-failure.db");
        let store = Store::open(&path).unwrap();
        let connection = store.connection;
        let statement = connection.prepare("SELECT 1").unwrap();
        std::mem::forget(statement);
        let error = connection.close().unwrap_err().1;
        assert!(matches!(StorageError::from(error), StorageError::Sqlite(_)));
    }

    #[test]
    fn page_limit_write_failure_is_structured_and_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("disk-full.db");
        let store = Store::open(&path).unwrap();
        let page_count: u64 = store
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        let _: u64 = store
            .connection
            .query_row(
                &format!("PRAGMA max_page_count = {page_count}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        let event = StoredEvent {
            session_id: "disk-full".into(),
            turn_id: "turn-1".into(),
            sequence: 1,
            kind: "observation".into(),
            body: "x".repeat(128 * 1024),
        };
        let error = store.append_event(&event).unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));
        assert!(store.replay("disk-full").unwrap().is_empty());
    }

    #[test]
    fn replay_locates_corrupt_event_without_skipping_history() {
        let store = Store::open_memory().unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "corrupt-session".into(),
                turn_id: "turn".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE events SET body=?1 WHERE session_id=?2 AND sequence=?3",
                params!["not-json", "corrupt-session", 1u64],
            )
            .unwrap();
        assert!(matches!(
            store.replay("corrupt-session"),
            Err(StorageError::CorruptEvent {
                session_id,
                sequence: 1,
                ..
            }) if session_id == "corrupt-session"
        ));
    }

    #[test]
    fn append_event_rolls_back_session_and_turn_on_sequence_conflict() {
        let store = Store::open_memory().unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "atomic-session".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();

        let error = store
            .append_event(&StoredEvent {
                session_id: "atomic-session".into(),
                turn_id: "turn-2".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));

        assert_eq!(
            store
                .last_event_sequence("atomic-session", "turn-1")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .last_event_sequence("atomic-session", "turn-2")
                .unwrap(),
            0
        );
        let turn_count: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id=?1 AND turn_id=?2",
                params!["atomic-session", "turn-2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(turn_count, 0);
        assert_eq!(store.replay("atomic-session").unwrap().len(), 1);
    }

    #[test]
    fn agent_tasks_are_persisted_and_unfinished_tasks_are_interrupted_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let store = Store::open(&path).unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        for (task_id, state) in [
            ("task-queued", "queued"),
            ("task-running", "running"),
            ("task-completed", "completed"),
        ] {
            store
                .save_agent_task(&StoredAgentTask {
                    task_id: task_id.into(),
                    agent_id: format!("agent-{task_id}"),
                    session_id: "session".into(),
                    turn_id: "turn".into(),
                    label: "fixture".into(),
                    state: state.into(),
                    summary: None,
                    usage_json: None,
                    terminal_reason: None,
                })
                .unwrap();
        }
        drop(store);

        let reopened = Store::open(&path).unwrap();
        let states = reopened
            .agent_tasks("session")
            .unwrap()
            .into_iter()
            .map(|task| (task.task_id, task.state))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(states["task-queued"], "interrupted");
        assert_eq!(states["task-running"], "interrupted");
        assert_eq!(states["task-completed"], "completed");
    }

    #[test]
    fn agent_task_completion_persists_usage_and_terminal_reason() {
        let store = Store::open_memory().unwrap();
        store
            .append_event(&StoredEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        store
            .save_agent_task(&StoredAgentTask {
                task_id: "task-1".into(),
                agent_id: "agent-1".into(),
                session_id: "session".into(),
                turn_id: "turn".into(),
                label: "bounded label".into(),
                state: "queued".into(),
                summary: None,
                usage_json: None,
                terminal_reason: None,
            })
            .unwrap();
        store.update_agent_task("task-1", "running", None).unwrap();
        store
            .finish_agent_task(
                "task-1",
                "completed",
                "bounded summary",
                r#"{"input_tokens":3,"output_tokens":5,"steps":2}"#,
                "completed",
            )
            .unwrap();

        let task = store.agent_tasks("session").unwrap().pop().unwrap();
        assert_eq!(task.state, "completed");
        assert_eq!(task.summary.as_deref(), Some("bounded summary"));
        assert_eq!(
            task.usage_json.as_deref(),
            Some(r#"{"input_tokens":3,"output_tokens":5,"steps":2}"#)
        );
        assert_eq!(task.terminal_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn sidecar_tables_are_additive_and_recover_unknown_side_effects() {
        let store = Store::open_memory().unwrap();
        store
            .save_sidecar_task(&StoredSidecarTask {
                task_id: "task-1".into(),
                intent_id: "intent-1".into(),
                workspace_id: "workspace-1".into(),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                state: "running".into(),
                lease_epoch: 1,
                expires_at: "2099-01-01T00:00:00Z".into(),
                last_event_sequence: 4,
                tool_call_status: r#"{"call-1":"running"}"#.into(),
                terminal_reason: None,
            })
            .unwrap();
        assert_eq!(
            store
                .sidecar_task("task-1")
                .unwrap()
                .unwrap()
                .tool_call_status,
            r#"{"call-1":"running"}"#
        );
        assert!(store
            .update_sidecar_tool_status("task-1", r#"{"call-1":"completed"}"#)
            .unwrap());
        assert_eq!(
            store
                .sidecar_task("task-1")
                .unwrap()
                .unwrap()
                .tool_call_status,
            r#"{"call-1":"completed"}"#
        );
        assert!(store
            .update_sidecar_task("task-1", "running", 1, 12, None)
            .unwrap());
        assert_eq!(
            store
                .sidecar_task("task-1")
                .unwrap()
                .unwrap()
                .last_event_sequence,
            12
        );
        store
            .save_sidecar_lease(&StoredSidecarLease {
                task_id: "task-1".into(),
                lease_epoch: 1,
                expires_at: "2099-01-01T00:00:00Z".into(),
                state: "running".into(),
            })
            .unwrap();
        store
            .save_event_cursor(&StoredEventCursor {
                session_id: "session-1".into(),
                next_sequence: 5,
            })
            .unwrap();
        store.recover_sidecar_tasks().unwrap();
        store
            .save_event_cursor(&StoredEventCursor {
                session_id: "session-1".into(),
                next_sequence: 3,
            })
            .unwrap();
        assert_eq!(
            store.sidecar_lease("task-1").unwrap().unwrap().state,
            "unknown_side_effect"
        );
        assert_eq!(
            store
                .sidecar_task_by_intent("intent-1")
                .unwrap()
                .unwrap()
                .state,
            "unknown_side_effect"
        );
        assert_eq!(
            store
                .sidecar_task_by_intent("intent-1")
                .unwrap()
                .unwrap()
                .terminal_reason
                .as_deref(),
            Some("sidecar_restart_unknown_side_effect")
        );
        assert_eq!(
            store
                .event_cursor("session-1")
                .unwrap()
                .unwrap()
                .next_sequence,
            5
        );
    }

    #[test]
    fn sidecar_lease_expiry_is_terminal_and_queryable() {
        let store = Store::open_memory().unwrap();
        store
            .save_sidecar_task(&StoredSidecarTask {
                task_id: "expired-task".into(),
                intent_id: "expired-intent".into(),
                workspace_id: "workspace-1".into(),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                state: "running".into(),
                lease_epoch: 2,
                expires_at: "2000-01-01T00:00:00Z".into(),
                last_event_sequence: 9,
                tool_call_status: "{}".into(),
                terminal_reason: None,
            })
            .unwrap();
        store
            .save_sidecar_lease(&StoredSidecarLease {
                task_id: "expired-task".into(),
                lease_epoch: 2,
                expires_at: "2000-01-01T00:00:00Z".into(),
                state: "running".into(),
            })
            .unwrap();
        assert_eq!(store.expire_sidecar_leases().unwrap(), 1);
        let task = store.sidecar_task("expired-task").unwrap().unwrap();
        assert_eq!(task.state, "expired");
        assert_eq!(task.terminal_reason.as_deref(), Some("lease_expired"));
        assert_eq!(
            store.sidecar_lease("expired-task").unwrap().unwrap().state,
            "expired"
        );
        // A late worker or stale lease owner cannot resurrect an expired task.
        assert!(!store
            .update_sidecar_task("expired-task", "running", 2, 10, None)
            .unwrap());
        assert!(!store
            .update_sidecar_task("expired-task", "completed", 1, 11, None)
            .unwrap());
        assert!(store
            .update_sidecar_task("expired-task", "expired", 2, 8, None)
            .unwrap());
        let task = store.sidecar_task("expired-task").unwrap().unwrap();
        assert_eq!(task.state, "expired");
        assert_eq!(task.last_event_sequence, 9);
        assert_eq!(task.terminal_reason.as_deref(), Some("lease_expired"));
        assert_eq!(
            store.sidecar_lease("expired-task").unwrap().unwrap().state,
            "expired"
        );
    }

    #[test]
    fn sidecar_disconnect_preserves_uncertainty_without_cancelling_tools() {
        let store = Store::open_memory().unwrap();
        for (task_id, intent_id, state) in [
            ("accepted-task", "accepted-intent", "accepted"),
            ("running-task", "running-intent", "running"),
        ] {
            store
                .save_sidecar_task(&StoredSidecarTask {
                    task_id: task_id.into(),
                    intent_id: intent_id.into(),
                    workspace_id: "workspace-1".into(),
                    session_id: "session-1".into(),
                    turn_id: task_id.into(),
                    state: state.into(),
                    lease_epoch: 1,
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    last_event_sequence: 0,
                    tool_call_status: "{}".into(),
                    terminal_reason: None,
                })
                .unwrap();
            store
                .save_sidecar_lease(&StoredSidecarLease {
                    task_id: task_id.into(),
                    lease_epoch: 1,
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    state: state.into(),
                })
                .unwrap();
        }
        assert_eq!(store.mark_sidecar_tasks_unknown_side_effect().unwrap(), 2);
        for task_id in ["accepted-task", "running-task"] {
            let task = store.sidecar_task(task_id).unwrap().unwrap();
            assert_eq!(task.state, "unknown_side_effect");
            assert_eq!(
                task.terminal_reason.as_deref(),
                Some("client_disconnect_unknown_side_effect")
            );
            assert_eq!(
                store.sidecar_lease(task_id).unwrap().unwrap().state,
                "unknown_side_effect"
            );
        }
    }

    #[test]
    fn sidecar_artifact_cleanup_queries_are_scope_bound_and_expiry_aware() {
        let store = Store::open_memory().unwrap();
        store
            .save_sidecar_task(&StoredSidecarTask {
                task_id: "failed-task".into(),
                intent_id: "failed-intent".into(),
                workspace_id: "workspace-1".into(),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                state: "failed".into(),
                lease_epoch: 1,
                expires_at: "2099-01-01T00:00:00Z".into(),
                last_event_sequence: 0,
                tool_call_status: "{}".into(),
                terminal_reason: Some("provider_failure".into()),
            })
            .unwrap();
        assert_eq!(
            store.sidecar_sessions_needing_artifact_cleanup().unwrap(),
            ["session-1"]
        );
        for (artifact_id, expires_at) in [
            ("expired-artifact", "2000-01-01T00:00:00Z"),
            ("live-artifact", "2099-01-01T00:00:00Z"),
        ] {
            store
                .save_artifact(&StoredArtifact {
                    artifact_id: artifact_id.into(),
                    node_id: "node-1".into(),
                    workspace_id: "workspace-1".into(),
                    session_id: "session-1".into(),
                    kind: "text/plain".into(),
                    size: 1,
                    hash: "0".repeat(64),
                    expires_at: expires_at.into(),
                    download_scope: "session".into(),
                    storage_key: artifact_id.into(),
                    state: "ready".into(),
                })
                .unwrap();
        }
        assert_eq!(
            store
                .artifacts_for_scope("node-1", "workspace-1", "session-1")
                .unwrap()
                .len(),
            2
        );
        let expired = store.expired_artifacts("node-1", "workspace-1").unwrap();
        assert_eq!(
            expired
                .iter()
                .map(|item| item.artifact_id.as_str())
                .collect::<Vec<_>>(),
            ["expired-artifact"]
        );
        store
            .update_artifact_state(
                "expired-artifact",
                "node-1",
                "workspace-1",
                "session-1",
                "deleted",
            )
            .unwrap();
        assert!(store
            .artifacts_for_scope("node-1", "workspace-1", "session-1")
            .unwrap()
            .iter()
            .all(|item| item.artifact_id != "expired-artifact"));
        assert!(store
            .expired_artifacts("other-node", "workspace-1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn runtime_reopen_does_not_recover_live_tasks_within_one_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rivo.db");
        let store = Store::open(&path).unwrap();
        store
            .save_sidecar_task(&StoredSidecarTask {
                task_id: "live-task".into(),
                intent_id: "live-intent".into(),
                workspace_id: "workspace-1".into(),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                state: "running".into(),
                lease_epoch: 1,
                expires_at: "2099-01-01T00:00:00Z".into(),
                last_event_sequence: 1,
                tool_call_status: "{}".into(),
                terminal_reason: None,
            })
            .unwrap();
        drop(store);
        let reopened = Store::open_runtime(&path).unwrap();
        assert_eq!(
            reopened.sidecar_task("live-task").unwrap().unwrap().state,
            "running"
        );
        drop(reopened);
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .sidecar_task("live-task")
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
    }

    #[test]
    fn sidecar_runtime_ownership_is_exclusive_and_released_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let owner = SidecarRuntimeLease::acquire(root.path()).unwrap();
        assert!(SidecarRuntimeLease::acquire(root.path()).is_err());
        drop(owner);
        assert!(SidecarRuntimeLease::acquire(root.path()).is_ok());
    }

    #[test]
    fn artifact_scope_is_enforced_without_exposing_storage_key() {
        let store = Store::open_memory().unwrap();
        store
            .save_artifact(&StoredArtifact {
                artifact_id: "artifact-1".into(),
                node_id: "node-1".into(),
                workspace_id: "workspace-1".into(),
                session_id: "session-1".into(),
                kind: "text".into(),
                size: 4,
                hash: "a".repeat(64),
                expires_at: "2099-01-01T00:00:00Z".into(),
                download_scope: "session".into(),
                storage_key: "internal/secret/path".into(),
                state: "ready".into(),
            })
            .unwrap();
        assert!(store
            .artifact_for_scope("artifact-1", "node-1", "workspace-1", "session-1")
            .unwrap()
            .is_some());
        assert!(store
            .artifact_for_scope("artifact-1", "node-1", "workspace-other", "session-1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn artifact_store_is_runtime_owned_and_hash_verified() {
        let workspace = tempfile::tempdir().unwrap();
        let artifacts = ArtifactStore::new(workspace.path());
        let artifact = artifacts
            .put(
                &ArtifactMetadata {
                    artifact_id: "artifact-1".into(),
                    node_id: "node-1".into(),
                    workspace_id: "workspace-1".into(),
                    session_id: "session-1".into(),
                    kind: "text".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    download_scope: "session".into(),
                },
                b"data",
            )
            .unwrap();
        assert_eq!(artifacts.read(&artifact).unwrap(), b"data");
        assert!(workspace.path().join(".rivo").join("artifacts").exists());
        assert!(artifacts
            .put(
                &ArtifactMetadata {
                    artifact_id: "workspace-scoped".into(),
                    node_id: "node-1".into(),
                    workspace_id: "workspace-1".into(),
                    session_id: "session-1".into(),
                    kind: "text".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    download_scope: "workspace".into(),
                },
                b"data",
            )
            .is_err());
        artifacts.cleanup(&artifact).unwrap();
        assert!(artifacts.read(&artifact).is_err());
        assert!(artifacts
            .put(
                &ArtifactMetadata {
                    artifact_id: "../escape".into(),
                    node_id: "n".into(),
                    workspace_id: "w".into(),
                    session_id: "s".into(),
                    kind: "text".into(),
                    expires_at: "never".into(),
                    download_scope: "session".into(),
                },
                b"x",
            )
            .is_err());
    }

    #[test]
    fn artifact_cleanup_failure_does_not_touch_user_files() {
        let workspace = tempfile::tempdir().unwrap();
        let user_file = workspace.path().join("user-data.txt");
        fs::write(&user_file, "keep me").unwrap();
        let artifacts = ArtifactStore::new(workspace.path());
        let artifact = artifacts
            .put(
                &ArtifactMetadata {
                    artifact_id: "cleanup-failure".into(),
                    node_id: "node-1".into(),
                    workspace_id: "workspace-1".into(),
                    session_id: "session-1".into(),
                    kind: "text".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    download_scope: "session".into(),
                },
                b"owned",
            )
            .unwrap();
        let artifact_path = workspace
            .path()
            .join(".rivo")
            .join("artifacts")
            .join("cleanup-failure");
        fs::remove_file(&artifact_path).unwrap();
        fs::create_dir(&artifact_path).unwrap();
        assert!(artifacts.cleanup(&artifact).is_err());
        assert_eq!(fs::read_to_string(user_file).unwrap(), "keep me");
        fs::remove_dir(artifact_path).unwrap();
    }

    fn artifact_test_metadata() -> ArtifactMetadata {
        ArtifactMetadata {
            artifact_id: "artifact-owned".into(),
            node_id: "node".into(),
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            kind: "text".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            download_scope: "session".into(),
        }
    }

    #[test]
    fn artifact_put_never_overwrites_and_cleanup_preserves_modified_content() {
        let workspace = tempfile::tempdir().unwrap();
        let artifacts = ArtifactStore::new(workspace.path());
        let metadata = artifact_test_metadata();
        let artifact = artifacts.put(&metadata, b"owned").unwrap();
        assert!(artifacts.put(&metadata, b"replacement").is_err());
        assert_eq!(artifacts.read(&artifact).unwrap(), b"owned");
        let path = workspace.path().join(".rivo/artifacts/artifact-owned");
        fs::write(&path, b"user-modified").unwrap();
        assert!(artifacts.read(&artifact).is_err());
        assert!(artifacts.cleanup(&artifact).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"user-modified");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SIDECAR_ARTIFACT_BYTES + 1)
            .unwrap();
        assert!(artifacts.read(&artifact).is_err());
        assert!(artifacts.cleanup(&artifact).is_err());
        assert!(path.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn artifact_rejects_substituted_directory_without_touching_user_files() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let artifacts = ArtifactStore::new(workspace.path());
        let artifact = artifacts.put(&artifact_test_metadata(), b"owned").unwrap();
        fs::rename(
            workspace.path().join(".rivo"),
            workspace.path().join("original-rivo"),
        )
        .unwrap();
        fs::create_dir(outside.path().join("artifacts")).unwrap();
        let user_file = outside.path().join("artifacts/artifact-owned");
        fs::write(&user_file, b"owned").unwrap();
        let link = workspace.path().join(".rivo");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "junction fixture must be available"
            );
        }
        assert!(artifacts.read(&artifact).is_err());
        assert!(artifacts.cleanup(&artifact).is_err());
        assert!(artifacts.put(&artifact_test_metadata(), b"new").is_err());
        assert_eq!(fs::read(user_file).unwrap(), b"owned");
    }

    #[test]
    fn old_database_is_rejected_without_touching_non_session_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v8.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "                      CREATE TABLE schema_version(version INTEGER NOT NULL);
                      INSERT INTO schema_version(version) VALUES(8);
                      CREATE TABLE sessions(id TEXT PRIMARY KEY, created_at TEXT NOT NULL);
                      CREATE TABLE preserved_data(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                      INSERT INTO preserved_data VALUES(1, 'memory-and-trust-sentinel');",
            )
            .unwrap();
        drop(connection);

        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StorageError::UnsupportedSchema(8))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let connection = Connection::open(&path).unwrap();
        let version: u64 = connection
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        let has_agent_tasks: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_tasks')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 8);
        assert!(!has_agent_tasks);
        let sentinel: String = connection
            .query_row("SELECT body FROM preserved_data WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sentinel, "memory-and-trust-sentinel");
    }

    fn a2a_peer(state: A2aTrustState) -> StoredA2aPeer {
        StoredA2aPeer {
            snapshot: A2aTrustSnapshot {
                peer_id: A2aPeerId("peer-1".into()),
                interface_url: "https://10.0.0.3:8443/a2a".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: None,
                spki_sha256: "a".repeat(64),
                agent_card_id: "card-1".into(),
                agent_card_jcs_sha256: "b".repeat(64),
                allowed_skills: ["research".into()].into_iter().collect(),
                spec_revision: rivo_a2a::SPEC_REVISION.into(),
                wire_protocol_version: "1.0".into(),
                status: state,
                approved_at: "2026-07-31T00:00:00Z".into(),
            },
            identity_ref: (state == A2aTrustState::Trusted).then(|| "a2a-peer-1".into()),
        }
    }

    #[test]
    fn trusted_psk_peer_needs_no_persistent_identity() {
        let store = Store::open_memory().unwrap();
        let mut peer = a2a_peer(A2aTrustState::Trusted);
        peer.snapshot.protocol_binding = "PSK".into();
        peer.identity_ref = None;
        store.save_a2a_peer(&peer).unwrap();
        assert_eq!(
            store
                .a2a_peer(&peer.snapshot.peer_id)
                .unwrap()
                .unwrap()
                .identity_ref,
            None
        );
    }

    #[test]
    fn a2a_peer_reconcile_suspends_and_local_revoke_cancels_active_tasks() {
        let store = Store::open_memory().unwrap();
        let peer = a2a_peer(A2aTrustState::Trusted);
        store.save_a2a_peer(&peer).unwrap();
        store
            .save_a2a_task(&StoredA2aTask {
                peer_id: peer.snapshot.peer_id.clone(),
                task_id: A2aTaskId("task-1".into()),
                request_sha256: "c".repeat(64),
                state: A2aTaskState::Working,
                revision: Some(3),
            })
            .unwrap();
        for conflicting in [
            StoredA2aTask {
                peer_id: peer.snapshot.peer_id.clone(),
                task_id: A2aTaskId("task-1".into()),
                request_sha256: "e".repeat(64),
                state: A2aTaskState::Working,
                revision: Some(3),
            },
            StoredA2aTask {
                peer_id: peer.snapshot.peer_id.clone(),
                task_id: A2aTaskId("task-1".into()),
                request_sha256: "c".repeat(64),
                state: A2aTaskState::Working,
                revision: Some(2),
            },
            StoredA2aTask {
                peer_id: peer.snapshot.peer_id.clone(),
                task_id: A2aTaskId("task-1".into()),
                request_sha256: "c".repeat(64),
                state: A2aTaskState::Completed,
                revision: Some(3),
            },
        ] {
            assert!(matches!(
                store.save_a2a_task(&conflicting),
                Err(StorageError::InvalidA2aState(_))
            ));
        }
        let changed = store
            .reconcile_a2a_peer(
                &peer.snapshot.peer_id,
                &peer.snapshot.interface_url,
                &peer.snapshot.protocol_binding,
                peer.snapshot.tenant.clone(),
                &"d".repeat(64),
                &peer.snapshot.agent_card_id,
                &peer.snapshot.agent_card_jcs_sha256,
                peer.snapshot.allowed_skills.clone(),
                &peer.snapshot.spec_revision,
                &peer.snapshot.wire_protocol_version,
            )
            .unwrap();
        assert_eq!(changed.snapshot.status, A2aTrustState::Suspended);
        assert!(store.revoke_a2a_peer(&peer.snapshot.peer_id).unwrap());
        assert_eq!(
            store
                .a2a_peer(&peer.snapshot.peer_id)
                .unwrap()
                .unwrap()
                .snapshot
                .status,
            A2aTrustState::Revoked
        );
        assert_eq!(
            store
                .a2a_task(&peer.snapshot.peer_id, &A2aTaskId("task-1".into()))
                .unwrap()
                .unwrap()
                .state,
            A2aTaskState::Canceled
        );
        assert!(matches!(
            store.save_a2a_peer(&a2a_peer(A2aTrustState::Trusted)),
            Err(StorageError::InvalidA2aState(_))
        ));
        assert!(matches!(
            store.save_a2a_task(&StoredA2aTask {
                peer_id: peer.snapshot.peer_id,
                task_id: A2aTaskId("task-1".into()),
                request_sha256: "c".repeat(64),
                state: A2aTaskState::Working,
                revision: Some(4),
            }),
            Err(StorageError::InvalidA2aState(_))
        ));
    }
}

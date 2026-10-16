use async_stream::stream;
use futures::{Stream, StreamExt};
use rivo_a2a::{A2aPeerId, KeyringA2aIdentityStore, KeyringMtlsTransport};
pub use rivo_agent::SubagentToolActivity;
pub use rivo_agent::{
    valid_session_ref, SessionDelivery, SessionMessageKind, SessionMessenger, SessionPeer,
    SessionSendOutcome, SessionSendRequest, MAX_SESSION_MESSAGE_BYTES,
};
use rivo_agent::{
    ConfirmationResponse, PendingConfirmation, PendingSubagentConfirmation,
    SubagentConfirmationResponse,
};
use rivo_core::{
    AgentId, AgentTaskId, AgentTaskState, ContentPart, ImageSource, MediaCapability, MediaDelivery,
    MediaOperation, MediaRef, MediaRequest, MediaResult, Message, Role, SubagentResult,
};
use rivo_llm::{
    create_client, create_media_client, LlmClient, MediaClient, MediaInput, MediaProviderRequest,
    ProviderConfig as LlmProviderConfig, ProviderRequest, ProviderStreamEvent, ToolChoice,
};
pub use rivo_llm::{ProviderKind, ReasoningEffort, StopReason, Usage};
use rivo_policy::{normalize_capability_profile, ExecutionConstraints};
pub use rivo_policy::{CapabilityProfile, PolicyProfile};
use rivo_prompts_management::{
    compose_prompt, load_persistent_instructions, PromptEnvelope, PromptInput,
};
use rivo_storage::{MemoryCandidate, SessionPermissionGrant, StoredTaskPlan};
use rivo_tools::mcp::McpRegistry;
pub use rivo_tools::mcp::McpServerConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

mod config;
mod credentials;
mod interaction;
mod model_routing;
mod persistence;
mod runtime;
#[cfg(test)]
mod started_route_tests;
pub use config::{
    config_file_path, injected_config_content, ConfigOverrides, CONFIG_CONTENT_ENV, CONFIG_VERSION,
};
pub use credentials::{
    credential_exists, credential_names, credentials_file_path, get_credential,
    migrate_credentials, set_credential, unset_credential, upgrade_credentials, CredentialStatus,
};
use interaction::{elicitation_to_interaction, interaction_outcome};
pub use model_routing::*;
use persistence::{
    active_agent_tasks, append_message, blocking_storage, confirm_memory, delete_memory,
    handle_management_intent, last_event_sequence, load_a2a_peers, next_event_sequence,
    persist_agent_task_event, persist_standalone_event, recall_memories, reject_memory,
    save_memory_candidate, save_model_selection, save_session_permission_grant,
};
pub use rivo_storage::{ModelSelection, ProviderSelection, StoredModelRoute};
use runtime::TurnContext;

/// The durable schema 6 document. Secrets are references only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub version: u32,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub vendors: BTreeMap<String, VendorCatalogEntry>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelCatalogEntry>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub mcp: McpFileConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            defaults: DefaultsConfig::default(),
            vendors: BTreeMap::new(),
            models: BTreeMap::new(),
            providers: BTreeMap::new(),
            mcp: McpFileConfig::default(),
            browser: BrowserConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryFormat {
    Text,
    Table,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    /// Canonical BCP 47 locale used by the UI and the next model turn.
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_format: Option<QueryFormat>,
    pub provider_id: Option<String>,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub capability: Option<CapabilityProfile>,
    pub policy: Option<PolicyProfile>,
    pub workspace: Option<PathBuf>,
    /// TUI-only turn defaults. Headless invocations must continue to opt in via flags.
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub browser: bool,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub mcp_toolsets: BTreeMap<String, BTreeSet<String>>,
    #[serde(default = "default_memory_recall")]
    pub memory_recall: bool,
    #[serde(default)]
    pub trace: bool,
    /// 内部子代理默认启用；可按 turn 关闭。
    #[serde(default = "default_subagents_enabled")]
    pub subagents_enabled: bool,
    #[serde(default = "default_max_subagents")]
    pub max_subagents: usize,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            locale: default_locale(),
            query_format: None,
            provider_id: None,
            vendor: None,
            model: None,
            capability: None,
            policy: None,
            workspace: None,
            network: false,
            browser: false,
            mcp_servers: Vec::new(),
            mcp_toolsets: BTreeMap::new(),
            memory_recall: default_memory_recall(),
            trace: false,
            subagents_enabled: default_subagents_enabled(),
            max_subagents: default_max_subagents(),
            budget: BudgetConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

fn default_locale() -> String {
    "en-US".into()
}

fn default_memory_recall() -> bool {
    true
}

fn persist_initial_locale(
    storage: &rivo_storage::Store,
    session_id: &str,
    locale: &str,
) -> Result<(), SessionError> {
    let event = AgentEvent::SessionLocaleApplied {
        locale: locale.to_owned(),
    };
    let body = serde_json::to_string(&event).map_err(|error| {
        SessionError::InvalidConfig(format!("locale journal 序列化失败: {error}"))
    })?;
    let sequence = storage.next_event_sequence(session_id)?;
    storage.append_event(&rivo_storage::StoredEvent {
        session_id: session_id.into(),
        turn_id: "session-locale".into(),
        sequence,
        kind: "session_locale_applied".into(),
        body,
    })?;
    Ok(())
}

fn default_subagents_enabled() -> bool {
    true
}

fn default_max_subagents() -> usize {
    5
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    #[serde(default = "default_browser_cdp_url")]
    pub cdp_url: String,
    /// Optional local Native Messaging broker pipe for a selected Chrome/Edge
    /// extension instance. CDP remains the default when this is absent.
    #[serde(default)]
    pub extension_pipe: Option<String>,
    #[serde(default)]
    pub extension_instance: Option<rivo_tools::browser_extension::BrowserInstanceRef>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            cdp_url: default_browser_cdp_url(),
            extension_pipe: None,
            extension_instance: None,
        }
    }
}

fn default_browser_cdp_url() -> String {
    "http://127.0.0.1:9222".into()
}

/// Publisher metadata is intentionally presentation-only. It has no transport
/// or credential semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorCatalogEntry {
    pub display_name: String,
}

/// Canonical model identity. Built-in entries are merged with this local
/// overlay; a local entry may add a model, and may override the protocol list
/// or priority of a built-in ID but not its vendor or display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogEntry {
    pub vendor: String,
    pub display_name: String,
    /// Protocols confirmed for this exact canonical model ID, in preference
    /// order. Routing always uses the first entry; Providers carry no protocol.
    pub supported_protocols: Vec<ProviderKind>,
    /// Reasoning levels explicitly confirmed for this exact canonical model ID.
    #[serde(default)]
    pub supported_reasoning_efforts: BTreeSet<ReasoningEffort>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: u16,
}

/// Provider-owned connection and authenticated discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: u16,
    /// Provider-owned, authenticated `/v1/models` discovery state.
    #[serde(default)]
    pub discovery: ModelDiscoveryCache,
}

fn default_enabled() -> bool {
    true
}

/// Immutable route selected before a turn begins. A transport failure never
/// re-resolves this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRoute {
    pub vendor: String,
    pub model: String,
    pub provider_id: String,
    pub protocol: ProviderKind,
    pub base_url: String,
    pub priority: u16,
}

/// A redacted, revisioned routing document used by every configuration surface.
/// The revision is derived from the complete configuration document, so a stale
/// UI draft can never overwrite an unrelated concurrent update.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingDraft {
    pub revision: String,
    pub config: FileConfig,
}

/// The impact report returned before a destructive routing change.  Callers
/// must echo both fields to `delete_routing_config` after an explicit confirm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingDeletePlan {
    pub revision: String,
    pub confirmation_token: String,
    pub target_kind: String,
    pub target_id: String,
    pub affected_models: Vec<AffectedModel>,
    pub impact_complete: bool,
    pub clears_default_route: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffectedModel {
    pub vendor: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDiscoveryCache {
    /// Fingerprint of the Provider connection used for this discovery.
    pub config_fingerprint: Option<String>,
    pub supported: Option<bool>,
    #[serde(default)]
    pub models: Vec<String>,
    pub last_checked_at: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpFileConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_duration_seconds")]
    pub max_duration_seconds: u64,
    #[serde(default = "default_context_limit_tokens")]
    pub context_limit_tokens: u64,
    #[serde(default = "default_max_observation_bytes")]
    pub max_observation_bytes: usize,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_retries: default_max_retries(),
            max_duration_seconds: default_max_duration_seconds(),
            context_limit_tokens: default_context_limit_tokens(),
            max_observation_bytes: default_max_observation_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            required: false,
            retention_days: default_retention_days(),
        }
    }
}

fn default_max_steps() -> usize {
    20
}
fn default_max_retries() -> usize {
    3
}
fn default_max_duration_seconds() -> u64 {
    30 * 60
}
fn default_context_limit_tokens() -> u64 {
    128_000
}
fn default_max_observation_bytes() -> usize {
    256 * 1024
}
fn default_retention_days() -> u64 {
    7
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub config_path: PathBuf,
    pub query_format: Option<QueryFormat>,
    pub selection: Option<ModelSelection>,
    pub selection_explicit: bool,
    pub route: Option<ResolvedRoute>,
    pub routing_vendors: BTreeMap<String, VendorCatalogEntry>,
    pub routing_models: BTreeMap<String, ModelCatalogEntry>,
    pub routing_providers: BTreeMap<String, ProviderConfig>,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub base_url: String,
    pub auth: AuthConfig,
    pub api_key_env: Option<String>,
    pub capability_profile: CapabilityProfile,
    pub policy_profile: PolicyProfile,
    pub workspace: PathBuf,
    pub mcp_servers: Vec<McpServerConfig>,
    pub browser: BrowserConfig,
    pub subagents_enabled: bool,
    pub max_subagents: usize,
    pub budget: BudgetConfig,
    pub logging: LoggingConfig,
    pub locale: String,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("无法读取配置文件 {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("配置文件格式错误: {0}")]
    ConfigParse(#[from] toml::de::Error),
    #[error("配置文件序列化错误: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),
    #[error("配置值错误: {0}")]
    InvalidConfig(String),
    #[error("配置 revision 冲突，请重新读取并确认")]
    RevisionConflict,
    #[error("环境变量 {key} 的值无效: {value}（{reason}）")]
    InvalidEnvironment {
        key: String,
        value: String,
        reason: String,
    },
    #[error("缺少模型选择，请选择目录中的 vendor/model")]
    MissingModel,
    #[error("未选择可用模型路由，请配置并刷新 Provider 后选择 vendor/model")]
    MissingRoute,
    #[error("Credential 引用 {0} 不存在，请先创建或修正 Provider 引用")]
    MissingApiKey(String),
    #[error("凭据存储错误: {0}")]
    Credential(String),
    #[error("会话运行时锁已失效: {0}")]
    RuntimeUnavailable(&'static str),
    #[error(transparent)]
    Provider(#[from] rivo_llm::ProviderError),
    #[error(transparent)]
    Policy(#[from] rivo_policy::PolicyError),
    #[error(transparent)]
    Storage(#[from] rivo_storage::StorageError),
}

fn storage_lock(
    store: &Arc<Mutex<rivo_storage::Store>>,
) -> Result<MutexGuard<'_, rivo_storage::Store>, rivo_storage::StorageError> {
    store.lock().map_err(|_| {
        rivo_storage::StorageError::Unavailable("本机历史存储锁已失效；请重新打开会话".into())
    })
}

fn runtime_lock<'a, T>(
    lock: &'a Mutex<T>,
    name: &'static str,
) -> Result<MutexGuard<'a, T>, SessionError> {
    lock.lock()
        .map_err(|_| SessionError::RuntimeUnavailable(name))
}

fn active_llm_snapshot(
    active_llm: &Arc<Mutex<Option<ActiveLlm>>>,
) -> Result<Option<ActiveLlm>, SessionError> {
    Ok(runtime_lock(active_llm, "LLM")?.clone())
}

fn replace_active_llm(
    active_llm: &Arc<Mutex<Option<ActiveLlm>>>,
    active: ActiveLlm,
) -> Result<(), SessionError> {
    *runtime_lock(active_llm, "LLM")? = Some(active);
    Ok(())
}

fn take_matching_pending_turn(
    pending: &Arc<Mutex<Option<PendingTurn>>>,
    request_id: &str,
) -> Result<Option<PendingTurn>, SessionError> {
    let mut pending = runtime_lock(pending, "等待确认状态")?;
    if pending
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
    {
        Ok(pending.take())
    } else {
        Ok(None)
    }
}

fn take_pending_turn(
    pending: &Arc<Mutex<Option<PendingTurn>>>,
) -> Result<Option<PendingTurn>, SessionError> {
    Ok(runtime_lock(pending, "等待确认状态")?.take())
}

fn has_pending_turn(pending: &Arc<Mutex<Option<PendingTurn>>>) -> Result<bool, SessionError> {
    Ok(runtime_lock(pending, "等待确认状态")?.is_some())
}

fn restore_pending_turn(
    pending: &Arc<Mutex<Option<PendingTurn>>>,
    pending_turn: PendingTurn,
) -> Result<(), SessionError> {
    let mut pending = runtime_lock(pending, "等待确认状态")?;
    if pending.is_some() {
        return Err(SessionError::RuntimeUnavailable("等待确认状态"));
    }
    *pending = Some(pending_turn);
    Ok(())
}

impl ResolvedConfig {
    pub fn with_model_selection(&self, selection: ModelSelection) -> Result<Self, SessionError> {
        let file = read_file_config(&self.config_path)?;
        self.with_selection_from_file(&file, selection)
    }

    fn with_selection_from_file(
        &self,
        file: &FileConfig,
        selection: ModelSelection,
    ) -> Result<Self, SessionError> {
        let route = resolve_available_route(
            file,
            Some(&selection.vendor),
            Some(&selection.model),
            selection.provider.explicit_id(),
        )?;
        let mut resolved = self.clone();
        resolved.auth = file.providers[&route.provider_id].auth.clone();
        resolved.provider = route.protocol;
        resolved.model = Some(route.model.clone());
        resolved.base_url = route.base_url.clone();
        resolved.route = Some(route);
        resolved.selection = Some(selection);
        resolved.routing_vendors = merged_vendor_catalog(file)?;
        resolved.routing_models = merged_model_catalog(file)?;
        resolved.routing_providers = file.providers.clone();
        Ok(resolved)
    }

    pub fn load(overrides: ConfigOverrides) -> Result<Self, SessionError> {
        let explicit_path = std::env::var_os("RIVO_CONFIG").map(PathBuf::from);
        let config_path = explicit_path.clone().or_else(config_file_path);
        let file = config_path
            .as_deref()
            .map(upgrade_file_config)
            .transpose()?
            .unwrap_or_default();
        let environment = std::env::vars().collect::<BTreeMap<_, _>>();
        Self::resolve_at(
            file,
            &environment,
            overrides,
            config_path.unwrap_or_else(|| PathBuf::from("config.toml")),
        )
    }

    pub fn resolve(
        file: FileConfig,
        environment: &BTreeMap<String, String>,
        overrides: ConfigOverrides,
    ) -> Result<Self, SessionError> {
        Self::resolve_at(file, environment, overrides, PathBuf::from("config.toml"))
    }

    pub fn resolve_at(
        file: FileConfig,
        environment: &BTreeMap<String, String>,
        overrides: ConfigOverrides,
        config_path: PathBuf,
    ) -> Result<Self, SessionError> {
        for key in [
            "RIVO_PROVIDER_PROFILE",
            "RIVO_PROVIDER",
            "RIVO_BASE_URL",
            "RIVO_API_KEY_ENV",
            "RIVO_CREDENTIAL_NAME",
        ] {
            if environment.contains_key(key) {
                return Err(SessionError::InvalidConfig(format!(
                    "旧模型连接环境变量 {key} 已移除；请移除该变量并通过 config provider 配置连接"
                )));
            }
        }
        let default_subagents_enabled = file.defaults.subagents_enabled;
        let default_max_subagents = file.defaults.max_subagents;
        if default_max_subagents == 0 {
            return Err(SessionError::InvalidConfig(
                "defaults.max_subagents 必须是正整数".into(),
            ));
        }
        model_routing::validate_model_routing_structure(&file)?;
        let configured_locale = overrides
            .locale
            .clone()
            .unwrap_or_else(|| file.defaults.locale.clone());
        let locale = rivo_i18n::LocaleId::parse(&configured_locale)
            .map_err(|error| SessionError::InvalidConfig(format!("locale 无效: {error}")))?
            .to_string();
        if let Some(env) = &overrides.api_key_env {
            validate_env_name(env, "--api-key-env")?;
        }
        if overrides.auto_provider && overrides.provider_id.is_some() {
            return Err(SessionError::InvalidConfig(
                "Auto 与显式 Provider 不能同时选择".into(),
            ));
        }
        let selection_explicit = overrides.vendor.is_some()
            || overrides.model.is_some()
            || overrides.provider_id.is_some()
            || overrides.auto_provider
            || ["RIVO_VENDOR", "RIVO_MODEL", "RIVO_PROVIDER_ID"]
                .iter()
                .any(|key| environment.contains_key(*key));
        let vendor = overrides
            .vendor
            .as_deref()
            .or_else(|| environment.get("RIVO_VENDOR").map(String::as_str))
            .or(file.defaults.vendor.as_deref());
        let model = overrides
            .model
            .as_deref()
            .or_else(|| environment.get("RIVO_MODEL").map(String::as_str))
            .or(file.defaults.model.as_deref());
        let provider_selection = if overrides.auto_provider {
            ProviderSelection::Auto
        } else {
            overrides
                .provider_id
                .clone()
                .or_else(|| environment.get("RIVO_PROVIDER_ID").cloned())
                .or_else(|| file.defaults.provider_id.clone())
                .map(|provider_id| ProviderSelection::Explicit { provider_id })
                .unwrap_or_default()
        };
        let has_catalog_selection = vendor.is_some() || model.is_some() || selection_explicit;
        let route = if has_catalog_selection {
            Some(resolve_available_route(
                &file,
                vendor,
                model,
                provider_selection.explicit_id(),
            )?)
        } else {
            None
        };
        let env_capability = parse_env(environment, "RIVO_PROFILE")?;
        let env_policy = parse_env(environment, "RIVO_POLICY")?;
        let browser = BrowserConfig {
            cdp_url: overrides
                .browser_cdp_url
                .or_else(|| environment.get("RIVO_BROWSER_CDP_URL").cloned())
                .unwrap_or(file.browser.cdp_url.clone()),
            extension_pipe: environment
                .get("RIVO_BROWSER_EXTENSION_PIPE")
                .cloned()
                .or_else(|| file.browser.extension_pipe.clone()),
            extension_instance: environment
                .get("RIVO_BROWSER_EXTENSION_INSTANCE")
                .and_then(|value| serde_json::from_str(value).ok())
                .or_else(|| file.browser.extension_instance.clone()),
        };
        if browser.extension_pipe.is_none() {
            rivo_tools::browser::BrowserClient::new(&browser.cdp_url)
                .map_err(|error| SessionError::InvalidConfig(error.to_string()))?;
        } else if browser.extension_instance.is_none() {
            return Err(SessionError::InvalidConfig(
                "启用浏览器扩展时必须提供 extension_instance".into(),
            ));
        }
        let provider = route
            .as_ref()
            .map(|r| r.protocol)
            .unwrap_or(ProviderKind::OpenAiChatCompletions);
        let model = route.as_ref().map(|r| r.model.clone());
        let base_url = route
            .as_ref()
            .map(|r| r.base_url.clone())
            .unwrap_or_default();
        let auth = route
            .as_ref()
            .map(|r| file.providers[&r.provider_id].auth.clone())
            .unwrap_or_default();
        let selection = route.as_ref().map(|r| ModelSelection {
            vendor: r.vendor.clone(),
            model: r.model.clone(),
            provider: provider_selection,
        });
        let capability_profile = normalize_capability_profile(
            overrides
                .capability_profile
                .or(env_capability)
                .or(file.defaults.capability)
                .unwrap_or(CapabilityProfile::General),
        );
        let requested_policy = overrides
            .policy_profile
            .or(env_policy)
            .or(file.defaults.policy);
        let policy_profile = requested_policy.unwrap_or({
            if matches!(capability_profile, CapabilityProfile::Plan) {
                PolicyProfile::Strict
            } else {
                PolicyProfile::Medium
            }
        });
        let workspace = overrides
            .workspace
            .or_else(|| environment.get("RIVO_WORKSPACE").map(PathBuf::from))
            .or(file.defaults.workspace.clone())
            .unwrap_or(std::env::current_dir().map_err(|error| {
                SessionError::InvalidConfig(format!("无法确定当前目录: {error}"))
            })?);
        let mut mcp_registry = McpRegistry::default();
        for server in file.mcp.servers.clone() {
            if server.name.trim().is_empty() {
                return Err(SessionError::InvalidConfig(
                    "MCP server name 不能为空".into(),
                ));
            }
            if server.enabled && server.allowed_capabilities.is_empty() {
                return Err(SessionError::InvalidConfig(format!(
                    "MCP server {} 未配置 allowed_capabilities",
                    server.name
                )));
            }
            for capability in &server.allowed_capabilities {
                if !matches!(capability.as_str(), "tools" | "resources" | "prompts") {
                    return Err(SessionError::InvalidConfig(format!(
                        "MCP server {} 的能力 {} 无效",
                        server.name, capability
                    )));
                }
            }
            for (name, toolset) in &server.toolsets {
                if name.trim().is_empty() || toolset.tools.is_empty() {
                    return Err(SessionError::InvalidConfig(format!(
                        "MCP server {} 的 toolset 必须具有名称和至少一个精确工具名",
                        server.name
                    )));
                }
            }
            mcp_registry
                .add(server)
                .map_err(|error| SessionError::InvalidConfig(error.to_string()))?;
        }
        validate_budget(&file.defaults.budget)?;
        if !(1..=30).contains(&file.defaults.logging.retention_days) {
            return Err(SessionError::InvalidConfig(
                "logging.retention_days 必须在 1..=30 之间".into(),
            ));
        }
        Ok(Self {
            config_path,
            query_format: file.defaults.query_format,
            selection,
            selection_explicit,
            route,
            routing_vendors: merged_vendor_catalog(&file)?,
            routing_models: merged_model_catalog(&file)?,
            routing_providers: file.providers,
            provider,
            model,
            base_url,
            auth,
            api_key_env: overrides.api_key_env,
            capability_profile,
            policy_profile,
            workspace,
            mcp_servers: mcp_registry.into_servers(),
            browser,
            subagents_enabled: default_subagents_enabled,
            max_subagents: default_max_subagents,
            budget: file.defaults.budget,
            logging: file.defaults.logging,
            locale,
        })
    }

    pub fn api_key(&self) -> Result<String, SessionError> {
        model_routing::provider_api_key(&self.config_path, &self.auth, self.api_key_env.as_deref())
    }

    /// 收集已配置 Provider 的认证值，仅供本机脱敏，不参与路由选择。
    pub fn known_api_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for (index, auth) in std::iter::once(&self.auth)
            .chain(
                self.routing_providers
                    .values()
                    .map(|provider| &provider.auth),
            )
            .enumerate()
        {
            let runtime_env = (index == 0)
                .then_some(self.api_key_env.as_deref())
                .flatten();
            if let Ok(value) = model_routing::provider_api_key(&self.config_path, auth, runtime_env)
            {
                if !value.is_empty() {
                    keys.push(value);
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn turn_limits(&self) -> rivo_agent::TurnLimits {
        rivo_agent::TurnLimits {
            max_steps: self.budget.max_steps,
            max_retries: self.budget.max_retries,
            max_duration: std::time::Duration::from_secs(self.budget.max_duration_seconds),
            context_limit_tokens: self.budget.context_limit_tokens,
            max_observation_bytes: self.budget.max_observation_bytes,
        }
    }
}

fn validate_credential_name(name: &str) -> Result<(), SessionError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(SessionError::InvalidConfig(
            "credential_name 只能包含 ASCII 字母、数字、_ 或 -，且长度为 1..=128".into(),
        ));
    }
    Ok(())
}

fn validate_env_name(value: &str, field: &str) -> Result<(), SessionError> {
    if value.trim().is_empty()
        || !value.chars().enumerate().all(|(index, character)| {
            (index == 0 && (character == '_' || character.is_ascii_alphabetic()))
                || (index > 0 && (character == '_' || character.is_ascii_alphanumeric()))
        })
    {
        return Err(SessionError::InvalidConfig(format!(
            "{field} 必须是合法环境变量名"
        )));
    }
    Ok(())
}

fn validate_budget(budget: &BudgetConfig) -> Result<(), SessionError> {
    if !(1..=100).contains(&budget.max_steps)
        || budget.max_retries > 10
        || !(1..=7200).contains(&budget.max_duration_seconds)
        || !(1024..=2_000_000).contains(&budget.context_limit_tokens)
        || !(1024..=4 * 1024 * 1024).contains(&budget.max_observation_bytes)
    {
        return Err(SessionError::InvalidConfig("budget 值超出允许范围".into()));
    }
    Ok(())
}

fn parse_env<T: FromStr>(
    environment: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<T>, SessionError>
where
    T::Err: std::fmt::Display,
{
    let Some(value) = environment.get(key) else {
        return Ok(None);
    };
    value
        .parse::<T>()
        .map(Some)
        .map_err(|error| SessionError::InvalidEnvironment {
            key: key.into(),
            value: value.clone(),
            reason: error.to_string(),
        })
}

/// 返回本次进程使用的数据根目录。`RIVO_DATA_DIR` 允许测试、便携运行和
/// 受控发布验收隔离 SQLite、日志与模型缓存。
pub fn data_root_dir() -> Option<PathBuf> {
    std::env::var_os("RIVO_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir)
}

/// Read-only parsing accepts only the current schema. Write-capable startup
/// paths use `upgrade_file_config` before calling this validator.
pub fn parse_file_config(content: &str) -> Result<FileConfig, SessionError> {
    let value: toml::Value = toml::from_str(content).map_err(config_parse_error)?;
    let version = value.get("version").and_then(toml::Value::as_integer);
    if version != Some(i64::from(CONFIG_VERSION)) {
        return Err(SessionError::InvalidConfig(format!(
            "必须显式使用 version = {CONFIG_VERSION}；请在正常运行或配置写入时完成支持的旧格式升级"
        )));
    }
    let config = toml::from_str(content).map_err(config_parse_error)?;
    model_routing::validate_model_routing_structure(&config)?;
    Ok(config)
}

fn config_parse_error(error: toml::de::Error) -> SessionError {
    // TOML Display includes the source document (which may contain secrets).
    let location = error.span().map(|span| span.start).unwrap_or_default();
    SessionError::InvalidConfig(format!(
        "schema {CONFIG_VERSION} 配置字段或格式无效（字节位置 {location}）；请检查字段并重新配置"
    ))
}

pub fn read_file_config(path: &Path) -> Result<FileConfig, SessionError> {
    Ok(read_model_routing_draft(path)?.config)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    CreatePlan {
        plan: TaskPlan,
    },
    UpdatePlan {
        plan: TaskPlan,
    },
    ApprovePlan {
        plan_id: String,
        revision: u64,
        #[serde(default)]
        document_sha256: Option<String>,
    },
    RejectPlan {
        plan_id: String,
        revision: u64,
        reason: Option<String>,
    },
    RequestPlanRevision {
        plan_id: String,
        revision: u64,
        feedback: String,
    },
    StartPlanExecution {
        plan_id: String,
        revision: u64,
        /// Coding turn 的授权上限；省略时使用 workspace_write（Medium）。
        #[serde(default)]
        policy_profile: Option<PolicyProfile>,
    },
    CancelPlan {
        plan_id: String,
    },
    SetGoal {
        objective: String,
        budget: u64,
    },
    GetGoal,
    PauseGoal,
    ResumeGoal,
    ClearGoal,
    SubmitTask {
        text: String,
    },
    AppendMessage {
        text: String,
    },
    /// A user turn with session-scoped media references. The legacy text-only
    /// variants remain for internal callers and older integrations.
    SubmitTaskWithMedia {
        text: String,
        media_refs: Vec<MediaRef>,
    },
    AppendMessageWithMedia {
        text: String,
        media_refs: Vec<MediaRef>,
    },
    Confirm {
        request_id: String,
        allow: bool,
        reason: Option<String>,
        #[serde(default)]
        scope: ApprovalScope,
    },
    RetryStep {
        step_id: String,
    },
    SetTurnOptions {
        options: TurnOptions,
    },
    /// Updates the durable locale used by subsequently-created turns. Unlike
    /// `SetTurnOptions`, this is allowed while a turn or retry is active.
    SetSessionLocale {
        locale: String,
    },
    ApplySessionSettings {
        selection: ModelSelection,
        options: TurnOptions,
    },
    SelectModel {
        selection: ModelSelection,
    },
    RespondInteraction {
        request_id: String,
        value: String,
    },
    ConfirmMemory {
        candidate_id: String,
        body_sha256: String,
    },
    RejectMemory {
        candidate_id: String,
    },
    DeleteMemory {
        memory_id: String,
    },
    ProposeMemory {
        candidate: MemoryCandidate,
    },
    CancelTurn,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    #[default]
    Once,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOptions {
    /// Canonical BCP 47 locale for the next model request.
    #[serde(default = "default_locale")]
    pub locale: String,
    pub capability_profile: CapabilityProfile,
    pub policy_profile: PolicyProfile,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    pub network: bool,
    pub browser: bool,
    #[serde(default)]
    pub computer: bool,
    /// 原始 Shell 仅在 coding turn 中显式启用；默认关闭。
    #[serde(default)]
    pub shell: bool,
    pub mcp_servers: Vec<String>,
    /// 每个 MCP server 在本次 turn 中允许暴露的 named toolsets。
    #[serde(default)]
    pub mcp_toolsets: BTreeMap<String, BTreeSet<String>>,
    /// 仅可列出本机已配对且仍 trusted 的 peer ID。endpoint、证书、token 与
    /// 信任状态绝不由 turn 或前端协议提供。
    #[serde(default)]
    pub a2a_peers: BTreeSet<String>,
    pub memory_recall: bool,
    pub trace: bool,
    #[serde(default = "default_subagents_enabled")]
    pub subagents_enabled: bool,
    #[serde(default = "default_max_subagents")]
    pub max_subagents: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
    BudgetLimited,
    Blocked,
    Cleared,
}

impl GoalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::BudgetLimited => "budget_limited",
            Self::Blocked => "blocked",
            Self::Cleared => "cleared",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub goal_id: String,
    pub session_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub budget: u64,
    pub used_budget: u64,
    pub revision: u64,
    pub last_evidence_summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPlanStatus {
    Draft,
    AwaitingApproval,
    Approved,
    InProgress,
    Blocked,
    Completed,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlanStep {
    pub step_id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub status: TaskPlanStatus,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub failure_reason: Option<String>,
}

/// Plan turn 可选的结构化模型输出。它只描述用户可审批的计划内容；Session
/// 会在接收后补充 session、revision、状态和时间字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDraft {
    #[serde(default)]
    pub plan_id: Option<String>,
    pub objective: String,
    #[serde(default)]
    pub steps: Vec<PlanDraftStep>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDraftStep {
    #[serde(default)]
    pub step_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub plan_id: String,
    pub session_id: String,
    pub objective: String,
    pub revision: u64,
    pub status: TaskPlanStatus,
    #[serde(default)]
    pub steps: Vec<TaskPlanStep>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Default for TurnOptions {
    fn default() -> Self {
        Self {
            locale: default_locale(),
            capability_profile: CapabilityProfile::General,
            policy_profile: PolicyProfile::Medium,
            reasoning_effort: ReasoningEffort::default(),
            network: false,
            browser: false,
            computer: false,
            shell: false,
            mcp_servers: Vec::new(),
            mcp_toolsets: BTreeMap::new(),
            a2a_peers: BTreeSet::new(),
            memory_recall: true,
            trace: false,
            subagents_enabled: default_subagents_enabled(),
            max_subagents: default_max_subagents(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentEvent {
    PlanCreated {
        plan: TaskPlan,
    },
    PlanUpdated {
        plan: TaskPlan,
    },
    PlanApprovalRequired {
        plan: TaskPlan,
        #[serde(default)]
        document_sha256: String,
    },
    PlanApproved {
        plan_id: String,
        revision: u64,
        #[serde(default)]
        document_sha256: String,
        #[serde(default)]
        artifact_path: String,
    },
    PlanRejected {
        plan_id: String,
        revision: u64,
        reason: Option<String>,
    },
    PlanExecutionStarted {
        plan_id: String,
        revision: u64,
    },
    PlanStepStarted {
        plan_id: String,
        revision: u64,
        step_id: String,
    },
    PlanStepCompleted {
        plan_id: String,
        revision: u64,
        step_id: String,
    },
    PlanStepBlocked {
        plan_id: String,
        revision: u64,
        step_id: String,
        reason: String,
    },
    PlanCompleted {
        plan_id: String,
        revision: u64,
    },
    GoalUpdated {
        goal: Goal,
    },
    GoalContinuationScheduled {
        goal_id: String,
        revision: u64,
    },
    GoalAudit {
        goal_id: String,
        evidence: String,
        complete: bool,
    },
    GoalCompleted {
        goal_id: String,
        revision: u64,
    },
    GoalBlocked {
        goal_id: String,
        reason: String,
        revision: u64,
    },
    Started {
        provider: ProviderKind,
        model: String,
        /// 本次 Turn 的冻结选择；Auto 不转换为显式 Provider。
        #[serde(default)]
        selection: Option<ModelSelection>,
        /// 本次 Turn 实际使用的路由；preflight 失败不提供推测路由。
        #[serde(default)]
        route: Option<ResolvedRoute>,
        turn_id: String,
        capability_profile: CapabilityProfile,
        policy_profile: PolicyProfile,
    },
    PromptPrepared {
        revision: String,
        instruction_hashes: Vec<String>,
        instructions_truncated: bool,
    },
    ModelSelected {
        selection: ModelSelection,
        route: ResolvedRoute,
    },
    SessionSettingsApplied {
        selection: ModelSelection,
        route: ResolvedRoute,
        options: TurnOptions,
    },
    /// Durable locale selection. This is deliberately separate from the
    /// broader settings event so a language switch cannot rewrite routing or
    /// policy state.
    SessionLocaleApplied {
        locale: String,
    },
    ModelDelta {
        text: String,
    },
    ToolRequested {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolObservation {
        call_id: String,
        name: String,
        result: serde_json::Value,
    },
    MediaStarted {
        call_id: String,
        operation: MediaOperation,
    },
    MediaReady {
        call_id: String,
        result: MediaResult,
    },
    MediaFailed {
        call_id: String,
        operation: MediaOperation,
        message: String,
    },
    BudgetUpdated {
        used_tokens: u64,
        context_limit_tokens: u64,
        steps: usize,
        max_steps: usize,
    },
    ContextCompressed {
        omitted_start: usize,
        omitted_end: usize,
    },
    SubagentQueued {
        agent_id: AgentId,
        task_id: AgentTaskId,
        /// 根 `delegate_task` 调用；旧 journal 没有此字段。
        #[serde(default)]
        call_id: String,
        label: String,
    },
    SubagentStarted {
        agent_id: AgentId,
        task_id: AgentTaskId,
    },
    /// 子代理内部工具调用的有界投影；不含参数、结果正文或委派内容。
    SubagentActivity {
        agent_id: AgentId,
        task_id: AgentTaskId,
        activity: SubagentToolActivity,
    },
    SubagentCompleted {
        result: SubagentResult,
    },
    /// 子代理请求的高风险操作由根会话统一确认；此事件保留任务来源，
    /// 但不泄露委派正文或子代理完整上下文。
    SubagentConfirmationRequired {
        agent_id: AgentId,
        task_id: AgentTaskId,
        request_id: String,
        call_id: String,
        turn_id: String,
        step_id: String,
        tool_name: String,
        arguments_summary: String,
        targets: Vec<String>,
        program: Option<String>,
        side_effects: Vec<String>,
        capability_profile: CapabilityProfile,
        policy_profile: PolicyProfile,
        constraints: ExecutionConstraints,
        session_scope_available: bool,
        session_scope_category: Option<String>,
    },
    ConfirmationRequired {
        request_id: String,
        call_id: String,
        turn_id: String,
        step_id: String,
        tool_name: String,
        arguments_summary: String,
        targets: Vec<String>,
        program: Option<String>,
        side_effects: Vec<String>,
        capability_profile: CapabilityProfile,
        policy_profile: PolicyProfile,
        constraints: ExecutionConstraints,
        session_scope_available: bool,
        session_scope_category: Option<String>,
    },
    /// 在线确认已答复。允许时随后是同一 `call_id` 的工具结果；拒绝或取消时随后是 `Stopped`。
    ConfirmationResolved {
        request_id: String,
        call_id: String,
        tool_name: String,
        allowed: bool,
    },
    InteractionRequired {
        request: InteractionRequest,
    },
    MemoryCandidate {
        candidate_id: String,
        body_sha256: String,
    },
    MemoryRecalled {
        memory_ids: Vec<String>,
    },
    MemoryInjected {
        memory_ids: Vec<String>,
    },
    MemoryConfirmed {
        memory_id: String,
    },
    MemoryRejected {
        candidate_id: String,
    },
    MemoryDeleted {
        memory_id: String,
    },
    FinalAnswer {
        text: String,
        stop_reason: StopReason,
        usage: Usage,
    },
    Error {
        message: String,
    },
    /// 结构化失败保留完整原因，同时为 TUI/CLI 提供稳定的恢复语义。保留
    /// 原有 Error 变体，确保已有历史和前端能够继续读取。
    Failure {
        message: String,
        details: FailureDetails,
    },
    RetryAvailable {
        step_id: String,
    },
    Stopped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionRequest {
    pub request_id: String,
    pub title: String,
    pub body: String,
    pub options: Vec<InteractionOption>,
    pub allow_freeform: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDetails {
    pub stage: FailureStage,
    pub code: String,
    pub retryable: bool,
    pub recovery: String,
    pub cause: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Submit,
    Stream,
    Tool,
    Persistence,
    Cancel,
    Resume,
    Tui,
    Unknown,
}

impl FailureDetails {
    pub fn unknown(cause: impl Into<String>) -> Self {
        let cause = cause.into();
        Self {
            stage: FailureStage::Unknown,
            code: "unknown".into(),
            retryable: false,
            recovery: "查看诊断详情后重试或新建会话".into(),
            cause,
        }
    }
}

pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
pub type SessionEventStream = Pin<Box<dyn Stream<Item = SessionEvent> + Send>>;
pub type IntentSender = tokio::sync::mpsc::Sender<Intent>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMetadata {
    pub session_id: String,
    pub turn_id: String,
    pub sequence: u64,
    /// Unix epoch milliseconds; the integer is an unambiguous UTC instant.
    pub timestamp_utc_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub metadata: EventMetadata,
    pub event: AgentEvent,
}

pub struct Session {
    config: ResolvedConfig,
    active_llm: Arc<Mutex<Option<ActiveLlm>>>,
    session_id: String,
    storage: Option<Arc<Mutex<rivo_storage::Store>>>,
    #[allow(dead_code)]
    workspace_lease: Option<rivo_storage::WorkspaceLease>,
    /// FEAT-037：宿主为后续 Turn 提供的本机会话消息总线；为空时不暴露消息工具。
    session_messenger: Mutex<Option<Arc<dyn SessionMessenger>>>,
}

#[derive(Clone)]
struct ActiveLlm {
    config: ResolvedConfig,
    client: Arc<dyn LlmClient>,
    media_client: Option<Arc<dyn MediaClient>>,
    agent: Option<Arc<rivo_agent::Agent>>,
    rebuild_agent_per_turn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySummary {
    pub session_id: String,
    pub created_at: String,
    #[serde(default)]
    pub task_summary: Option<String>,
    #[serde(default)]
    pub expiration_reason: Option<String>,
    pub event_count: u64,
    pub message_count: u64,
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub expired: bool,
}

fn history_summary(summary: rivo_storage::SessionSummary) -> HistorySummary {
    HistorySummary {
        session_id: summary.session_id,
        created_at: summary.created_at,
        task_summary: summary.task_summary,
        expiration_reason: summary.expiration_reason,
        event_count: summary.event_count,
        message_count: summary.message_count,
        workspace_root: summary.workspace_root,
        expired: summary.expired,
    }
}

struct PendingTurn {
    messages: Vec<Message>,
    request_id: String,
    turn_id: String,
    session_grant_key: Option<String>,
    options: TurnOptions,
}

type SubagentConfirmationRegistry =
    Arc<tokio::sync::Mutex<HashMap<String, PendingSubagentConfirmation>>>;

type ConfirmationRegistry = Arc<tokio::sync::Mutex<HashMap<String, PendingConfirmation>>>;

struct RetryTurn {
    step_id: String,
    messages: Vec<Message>,
    /// Retry keeps the original turn snapshot. Later UI/session changes must
    /// never alter the prompt or language of a retry.
    options: TurnOptions,
}

/// 会话内所有已送到交互面、等待用户答复的 MCP elicitation 的共享注册表。turn 任务插入待答项，
/// intent 循环在收到 `RespondInteraction` 时取出并回传结果。进程内存，不做持久化：进程重启则
/// 在途 elicitation 由处理器侧超时/断开兜底为 decline。
type InteractionRegistry = Arc<tokio::sync::Mutex<HashMap<String, PendingElicitationState>>>;

/// 一次已送到交互面、等待用户答复的 MCP elicitation 的服务端状态。
struct PendingElicitationState {
    /// 用户答复的回传通道（送回 agent 层的桥接处理器）。
    responder: oneshot::Sender<rivo_agent::ElicitationOutcome>,
    /// server 提供的 requestedSchema，用于把用户字符串答复归一化为结构化 content。
    schema: serde_json::Value,
}

/// `Intent::RespondInteraction.value` 取此哨兵值表示用户拒绝向 server 提供信息。
const ELICIT_DECLINE_ID: &str = "__mcp_elicit_decline__";

/* interaction helpers live in interaction.rs */
/*
    let mut body = if pending.message.trim().is_empty() {
        format!("MCP server {} 请求补充信息", pending.server)
    } else {
        pending.message.clone()
    };
    if let Some(props) = pending
        .requested_schema
        .get("properties")
        .and_then(|value| value.as_object())
    {
        if !props.is_empty() {
            let required = pending
                .requested_schema
                .get("required")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            body.push_str("\n\n请提供以下字段：");
            for (name, spec) in props {
                let ty = spec
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("string");
                let desc = spec
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let flag = if required.contains(name.as_str()) {
                    "必填"
                } else {
                    "可选"
                };
                let separator = if desc.is_empty() { "" } else { "：" };
                body.push_str(&format!("\n- {name}（{ty}，{flag}）{separator}{desc}"));
            }
            if props.len() > 1 {
                body.push_str("\n\n可直接输入 JSON 对象作答。");
            }
        }
    }
    InteractionRequest {
        request_id: pending.request_id.clone(),
        title: format!("MCP server {} 请求输入", pending.server),
        body,
        options: vec![InteractionOption {
            id: ELICIT_DECLINE_ID.into(),
            label: "拒绝".into(),
            description: "不向该 MCP server 提供任何信息".into(),
        }],
        allow_freeform: true,
    }
}

/// 把用户对交互请求的答复映射为 elicitation 处置：哨兵 → decline；空白 → cancel；否则按 schema
/// 归一化为 accept 内容。
fn interaction_outcome(
    value: String,
    schema: &serde_json::Value,
) -> rivo_agent::ElicitationOutcome {
    use rivo_agent::ElicitationOutcome;
    if value == ELICIT_DECLINE_ID {
        return ElicitationOutcome::Decline;
    }
    if value.trim().is_empty() {
        return ElicitationOutcome::Cancel;
    }
    ElicitationOutcome::Accept(elicitation_content(&value, schema))
}

/// 依据 requestedSchema 把用户的字符串答复归一化为结构化 content。用户可直接输入 JSON 对象；
/// 单字段 schema 时按字段名与类型强转；其余情形回退为 `{"value": <原文>}`。
fn elicitation_content(value: &str, schema: &serde_json::Value) -> serde_json::Value {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value) {
        if parsed.is_object() {
            return parsed;
        }
    }
    if let Some(props) = schema.get("properties").and_then(|value| value.as_object()) {
        if props.len() == 1 {
            let (field, spec) = props.iter().next().expect("单字段 schema");
            let mut map = serde_json::Map::new();
            map.insert(field.clone(), coerce_scalar(value, spec));
            return serde_json::Value::Object(map);
        }
    }
    serde_json::json!({ "value": value })
}

/// 依据字段的 JSON Schema 类型对用户输入做最小类型强转；失败则保留字符串。
fn coerce_scalar(value: &str, spec: &serde_json::Value) -> serde_json::Value {
    match spec.get("type").and_then(|value| value.as_str()) {
        Some("integer") => value
            .trim()
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(value.to_owned())),
        Some("number") => value
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(value.to_owned())),
        Some("boolean") => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => serde_json::Value::Bool(true),
            "false" | "no" | "0" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(value.to_owned()),
        },
        _ => serde_json::Value::String(value.to_owned()),
    }
}
*/

impl Session {
    /// Explicitly close the session's durable storage. Callers must first stop
    /// all channel drivers so the Session and Store references are unique.
    pub fn shutdown(self: Arc<Self>) -> Result<(), SessionError> {
        let session = Arc::try_unwrap(self).map_err(|_| {
            SessionError::Storage(rivo_storage::StorageError::Unavailable(
                "会话仍有活动引用，无法安全关闭历史存储".into(),
            ))
        })?;
        let Some(storage) = session.storage else {
            return Ok(());
        };
        let store = Arc::try_unwrap(storage).map_err(|_| {
            SessionError::Storage(rivo_storage::StorageError::Unavailable(
                "历史存储仍有活动引用，无法安全关闭".into(),
            ))
        })?;
        let store = store.into_inner().map_err(|_| {
            SessionError::Storage(rivo_storage::StorageError::Unavailable(
                "历史存储锁已失效，无法安全关闭".into(),
            ))
        })?;
        store.close().map_err(SessionError::Storage)
    }

    /// Wait cooperatively for channel drivers to release their Session clone,
    /// then perform the same explicit storage close. The bounded yield loop
    /// avoids wall-clock thresholds while preventing an accidental hang.
    pub async fn shutdown_async(self: Arc<Self>) -> Result<(), SessionError> {
        for _ in 0..1024 {
            if Arc::strong_count(&self) == 1 {
                return self.shutdown();
            }
            tokio::task::yield_now().await;
        }
        Err(SessionError::Storage(
            rivo_storage::StorageError::Unavailable(
                "会话驱动未在关闭边界内退出，无法安全关闭历史存储".into(),
            ),
        ))
    }

    pub fn connect(config: ResolvedConfig) -> Result<Self, SessionError> {
        Self::connect_with_session_id(config, None)
    }

    pub fn resume(
        config: ResolvedConfig,
        session_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        Self::connect_with_session_id(config, Some(session_id.into()))
    }

    fn connect_with_session_id(
        mut config: ResolvedConfig,
        session_id: Option<String>,
    ) -> Result<Self, SessionError> {
        prune_expired_media_assets();
        config.workspace = rivo_storage::normalize_workspace_path(&config.workspace)?;
        let database_path = default_database_path()?;
        let storage = rivo_storage::Store::open(database_path)?;
        storage.prune_journal(
            rivo_storage::DEFAULT_RETENTION_DAYS,
            rivo_storage::DEFAULT_MAX_JOURNAL_RECORDS,
            rivo_storage::DEFAULT_MAX_JOURNAL_BYTES,
        )?;
        let requested_session_id = session_id;
        let workspace = if requested_session_id.is_some() {
            storage
                .find_workspace_by_path(&config.workspace)?
                .ok_or_else(|| {
                    SessionError::InvalidConfig(
                        "当前目录尚未注册为 Workspace，不能恢复历史 Session".into(),
                    )
                })?
        } else {
            storage.ensure_workspace(&config.workspace, None)?
        };
        let session_id = requested_session_id.clone().unwrap_or_else(new_session_id);
        if requested_session_id.is_none() {
            storage.bind_session_to_workspace(&session_id, &workspace.id)?;
        }
        if let Some(id) = requested_session_id.as_ref() {
            if !storage.has_session(id)? {
                if storage.is_expired_session(id)? {
                    return Err(SessionError::Storage(rivo_storage::StorageError::Expired(
                        id.clone(),
                    )));
                }
                return Err(SessionError::Storage(rivo_storage::StorageError::NotFound(
                    id.clone(),
                )));
            }
            if !config.selection_explicit {
                if let Some(route) = storage.model_route(id)? {
                    config = config.with_model_selection(route.selection)?;
                }
            }
            match storage.session_workspace_id(id)? {
                Some(existing) if existing == workspace.id => {}
                Some(existing) => {
                    return Err(SessionError::InvalidConfig(format!(
                        "Session {id} 属于其他 Workspace ({existing})，不能跨 Workspace 恢复"
                    )));
                }
                None => {
                    return Err(SessionError::InvalidConfig(format!(
                        "Session {id} 尚未归属 Workspace；请先使用 sessions attach 显式归属"
                    )));
                }
            }
        }
        let active_llm = if config.model.is_some() {
            Some(build_active_llm(config.clone())?)
        } else {
            None
        };
        let workspace_lease = storage.acquire_workspace_lease(&workspace.id)?;
        storage.touch_workspace(&workspace.id)?;
        persist_session_identity(&storage, &session_id, &config)?;
        if requested_session_id.is_none() {
            // A freshly-created session must remember the locale it was born
            // with even if the global config is later changed.
            persist_initial_locale(&storage, &session_id, &config.locale)?;
        }
        Ok(Self {
            config,
            active_llm: Arc::new(Mutex::new(active_llm)),
            session_id,
            storage: Some(Arc::new(Mutex::new(storage))),
            workspace_lease: Some(workspace_lease),
            session_messenger: Mutex::new(None),
        })
    }

    pub fn with_client(config: ResolvedConfig, client: Arc<dyn LlmClient>) -> Self {
        let active = ActiveLlm {
            config: config.clone(),
            client,
            media_client: None,
            agent: None,
            rebuild_agent_per_turn: false,
        };
        Self {
            config,
            active_llm: Arc::new(Mutex::new(Some(active))),
            session_id: new_session_id(),
            storage: None,
            workspace_lease: None,
            session_messenger: Mutex::new(None),
        }
    }

    pub fn with_agent(config: ResolvedConfig, agent: Arc<rivo_agent::Agent>) -> Self {
        let client = agent.client();
        let active = ActiveLlm {
            config: config.clone(),
            client,
            media_client: None,
            agent: Some(agent),
            rebuild_agent_per_turn: false,
        };
        Self {
            config,
            active_llm: Arc::new(Mutex::new(Some(active))),
            session_id: new_session_id(),
            storage: None,
            workspace_lease: None,
            session_messenger: Mutex::new(None),
        }
    }

    pub fn with_storage(
        mut self,
        store: rivo_storage::Store,
        session_id: impl Into<String>,
    ) -> Self {
        self.session_id = session_id.into();
        self.storage = Some(Arc::new(Mutex::new(store)));
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 宿主复用本会话已打开的存储连接（例如 FEAT-037 消息 inbox），避免重新
    /// `Store::open` 时把其他运行中会话的子代理任务误标为中断。
    pub fn storage_handle(&self) -> Option<Arc<Mutex<rivo_storage::Store>>> {
        self.storage.clone()
    }

    /// 设置或清除后续 Turn 的会话消息总线。已开始的 Turn 不受影响。
    pub fn set_session_messenger(&self, messenger: Option<Arc<dyn SessionMessenger>>) {
        *self
            .session_messenger
            .lock()
            .expect("session messenger lock poisoned") = messenger;
    }

    fn session_messenger(&self) -> Option<Arc<dyn SessionMessenger>> {
        self.session_messenger
            .lock()
            .expect("session messenger lock poisoned")
            .clone()
    }

    pub fn workspace_path(&self) -> &std::path::Path {
        &self.config.workspace
    }

    pub async fn workspace_async(&self) -> Result<Option<rivo_storage::Workspace>, SessionError> {
        let workspace = self.config.workspace.clone();
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.find_workspace_by_path(&workspace)
        })
        .await?
        .flatten())
    }

    /// Returns the locale that a resumed session will use for its next turn.
    /// This deliberately reads only durable Session events, never UI catalogs.
    pub async fn restored_locale_async(&self) -> String {
        let mut locale = self.config.locale.clone();
        let Ok(events) = self.replay_events_async(&self.session_id).await else {
            return locale;
        };
        for event in events {
            match event {
                AgentEvent::SessionSettingsApplied { options, .. } => locale = options.locale,
                AgentEvent::SessionLocaleApplied { locale: next } => locale = next,
                _ => {}
            }
        }
        locale
    }

    pub async fn workspaces_async(&self) -> Result<Vec<rivo_storage::Workspace>, SessionError> {
        Ok(
            blocking_storage(self.storage.clone(), |store| store.list_workspaces())
                .await?
                .unwrap_or_default(),
        )
    }

    /// 每个新 turn 重新解析一次；确认继续与 retry 复用 active_llm 中的冻结快照。
    fn prepare_next_turn(&self) -> Result<Option<ActiveLlm>, SessionError> {
        let Some(active) = active_llm_snapshot(&self.active_llm)? else {
            return Ok(None);
        };
        if !active.rebuild_agent_per_turn {
            return Ok(Some(active));
        }
        let selection = active
            .config
            .selection
            .clone()
            .ok_or(SessionError::MissingRoute)?;
        let fresh = build_active_llm(active.config.with_model_selection(selection)?)?;
        if let Some(store) = &self.storage {
            storage_lock(store)?
                .set_model_route(&self.session_id, stored_model_route(&fresh.config).as_ref())?;
        }
        replace_active_llm(&self.active_llm, fresh.clone())?;
        Ok(Some(fresh))
    }

    fn select_model(&self, selection: &ModelSelection) -> Result<ActiveLlm, SessionError> {
        let active = active_llm_snapshot(&self.active_llm)?;
        let config = active.as_ref().map(|a| &a.config).unwrap_or(&self.config);
        if let Some(mut injected) = active.clone().filter(|a| !a.rebuild_agent_per_turn) {
            let file = FileConfig {
                vendors: injected.config.routing_vendors.clone(),
                models: injected.config.routing_models.clone(),
                providers: injected.config.routing_providers.clone(),
                ..FileConfig::default()
            };
            injected.config = injected
                .config
                .with_selection_from_file(&file, selection.clone())?;
            return Ok(injected);
        }
        build_active_llm(config.with_model_selection(selection.clone())?)
    }

    /// Validates a transient model selection without mutating the active Session
    /// configuration. Local hosts use this before accepting a new turn so an
    /// invalid requested model cannot silently execute with the previous model.
    pub fn validate_model_selection(&self, model: &str) -> Result<(), SessionError> {
        let active = runtime_lock(&self.active_llm, "LLM")?
            .clone()
            .ok_or(SessionError::MissingModel)?;
        let mut selection = active
            .config
            .selection
            .clone()
            .ok_or(SessionError::MissingRoute)?;
        selection.model = model.into();
        self.validate_route_selection(&selection)
    }

    /// 离线校验完整 Vendor/Model/Auto-or-Explicit 选择，不读取认证值、不联网。
    pub fn validate_route_selection(&self, selection: &ModelSelection) -> Result<(), SessionError> {
        let active = active_llm_snapshot(&self.active_llm)?;
        let config = active.as_ref().map(|a| &a.config).unwrap_or(&self.config);
        let file = if active.as_ref().is_some_and(|a| !a.rebuild_agent_per_turn) {
            FileConfig {
                vendors: config.routing_vendors.clone(),
                models: config.routing_models.clone(),
                providers: config.routing_providers.clone(),
                ..FileConfig::default()
            }
        } else {
            read_file_config(&config.config_path)?
        };
        resolve_available_route(
            &file,
            Some(&selection.vendor),
            Some(&selection.model),
            selection.provider.explicit_id(),
        )
        .map(|_| ())
    }

    /// Revalidate the Session's currently selected route without mutating it.
    /// Hosts use this at an explicit resume boundary so an already-active
    /// in-process Session cannot bypass the same exact-route check used by a
    /// freshly restored Session.
    pub fn validate_current_route(&self) -> Result<(), SessionError> {
        let Some(active) = active_llm_snapshot(&self.active_llm)? else {
            return Ok(());
        };
        let Some(selection) = active.config.selection else {
            return Ok(());
        };
        self.validate_route_selection(&selection)
    }

    /// Validates that a host-selected MCP server and toolsets are already present in the
    /// local allowlist. This intentionally accepts only names, never transport or credential
    /// data supplied by a host protocol.
    pub fn validate_mcp_selection(&self, options: &TurnOptions) -> Result<(), SessionError> {
        for name in &options.mcp_servers {
            let server = self
                .config
                .mcp_servers
                .iter()
                .find(|server| server.name == *name)
                .filter(|server| server.enabled)
                .ok_or_else(|| {
                    SessionError::InvalidConfig(format!("MCP server 未配置或未启用: {name}"))
                })?;
            if let Some(toolsets) = options.mcp_toolsets.get(name) {
                for toolset in toolsets {
                    if !server.toolsets.contains_key(toolset) {
                        return Err(SessionError::InvalidConfig(format!(
                            "MCP server {name} 的 toolset 未配置: {toolset}"
                        )));
                    }
                }
            }
        }
        for server in options.mcp_toolsets.keys() {
            if !options.mcp_servers.contains(server) {
                return Err(SessionError::InvalidConfig(format!(
                    "MCP server {server} 的 toolset 未在当前 turn 启用"
                )));
            }
        }
        Ok(())
    }

    /// A2A peer selection uses only durable local trust records. A host/UI may choose a
    /// peer ID from this set, but cannot inject an endpoint, fingerprint, skill or keychain
    /// reference into a turn.
    pub fn validate_a2a_selection(&self, options: &TurnOptions) -> Result<(), SessionError> {
        if options.a2a_peers.is_empty() {
            return Ok(());
        }
        let store = self.storage.as_ref().ok_or_else(|| {
            SessionError::InvalidConfig("A2A peer 选择需要本机持久化 trust store".into())
        })?;
        let store = storage_lock(store)?;
        for peer_id in &options.a2a_peers {
            let peer = store
                .a2a_peer(&A2aPeerId(peer_id.clone()))?
                .ok_or_else(|| {
                    SessionError::InvalidConfig(format!("A2A peer 未配对或不存在: {peer_id}"))
                })?;
            peer.snapshot.validate_transport().map_err(|error| {
                SessionError::InvalidConfig(format!(
                    "A2A peer {peer_id} 不可用于本次 turn: {error}"
                ))
            })?;
        }
        Ok(())
    }

    pub async fn validate_a2a_selection_async(
        &self,
        options: &TurnOptions,
    ) -> Result<(), SessionError> {
        if options.a2a_peers.is_empty() {
            return Ok(());
        }
        let store = self.storage.clone().ok_or_else(|| {
            SessionError::InvalidConfig("A2A peer 选择需要本机持久化 trust store".into())
        })?;
        let peer_ids = options.a2a_peers.iter().cloned().collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            let store = storage_lock(&store)?;
            for peer_id in peer_ids {
                let peer = store
                    .a2a_peer(&A2aPeerId(peer_id.clone()))?
                    .ok_or_else(|| {
                        SessionError::InvalidConfig(format!("A2A peer 未配对或不存在: {peer_id}"))
                    })?;
                peer.snapshot.validate_transport().map_err(|error| {
                    SessionError::InvalidConfig(format!(
                        "A2A peer {peer_id} 不可用于本次 turn: {error}"
                    ))
                })?;
            }
            Ok(())
        })
        .await
        .map_err(|error| {
            SessionError::Storage(rivo_storage::StorageError::Unavailable(format!(
                "历史存储 worker 已中断: {error}"
            )))
        })?
    }

    pub fn history(&self) -> Result<Vec<HistorySummary>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        Ok(storage_lock(store)?
            .list_sessions()?
            .into_iter()
            .map(|summary| HistorySummary {
                session_id: summary.session_id,
                created_at: summary.created_at,
                task_summary: summary.task_summary,
                expiration_reason: summary.expiration_reason,
                event_count: summary.event_count,
                message_count: summary.message_count,
                workspace_root: summary.workspace_root,
                expired: summary.expired,
            })
            .collect())
    }

    pub fn history_scoped(&self) -> Result<Vec<HistorySummary>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        let workspace = storage_lock(store)?.find_workspace_by_path(&self.config.workspace)?;
        let Some(workspace) = workspace else {
            return Ok(Vec::new());
        };
        Ok(storage_lock(store)?
            .list_sessions_scoped(rivo_storage::SessionScope::WorkspaceId(workspace.id))?
            .into_iter()
            .map(history_summary)
            .collect())
    }

    /// TUI 等异步调用方通过受控 blocking boundary 读取历史；同步 facade 保持兼容。
    pub async fn history_async(&self) -> Result<Vec<HistorySummary>, SessionError> {
        Ok(
            blocking_storage(self.storage.clone(), |store| store.list_sessions())
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(|summary| HistorySummary {
                    session_id: summary.session_id,
                    created_at: summary.created_at,
                    task_summary: summary.task_summary,
                    expiration_reason: summary.expiration_reason,
                    event_count: summary.event_count,
                    message_count: summary.message_count,
                    workspace_root: summary.workspace_root,
                    expired: summary.expired,
                })
                .collect(),
        )
    }

    pub async fn history_scoped_async(&self) -> Result<Vec<HistorySummary>, SessionError> {
        let workspace = self.config.workspace.clone();
        Ok(blocking_storage(self.storage.clone(), move |store| {
            let Some(workspace) = store.find_workspace_by_path(&workspace)? else {
                return Ok(Vec::new());
            };
            store.list_sessions_scoped(rivo_storage::SessionScope::WorkspaceId(workspace.id))
        })
        .await?
        .unwrap_or_default()
        .into_iter()
        .map(history_summary)
        .collect())
    }

    /// 前端通过 Session 查询诊断，不需要也不能直接打开 SQLite。
    pub fn diagnostics(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<rivo_storage::DiagnosticIndexEntry>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        Ok(storage_lock(store)?.list_diagnostics(session_id, limit)?)
    }

    pub async fn diagnostics_async(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<rivo_storage::DiagnosticIndexEntry>, SessionError> {
        let session_id = session_id.map(ToOwned::to_owned);
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.list_diagnostics(session_id.as_deref(), limit)
        })
        .await?
        .unwrap_or_default())
    }

    pub fn replay_events(&self, session_id: &str) -> Result<Vec<AgentEvent>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        let stored = storage_lock(store)?.replay(session_id)?;
        stored
            .into_iter()
            .map(|event| {
                serde_json::from_str(&event.body).map_err(|error| {
                    SessionError::InvalidConfig(format!(
                        "事件回放失败 sequence={}: {error}",
                        event.sequence
                    ))
                })
            })
            .collect()
    }

    pub async fn replay_events_async(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentEvent>, SessionError> {
        let session_id = session_id.to_owned();
        let stored = blocking_storage(self.storage.clone(), move |store| store.replay(&session_id))
            .await?
            .unwrap_or_default();
        stored
            .into_iter()
            .map(|event| {
                serde_json::from_str(&event.body).map_err(|error| {
                    SessionError::InvalidConfig(format!(
                        "事件回放失败 sequence={}: {error}",
                        event.sequence
                    ))
                })
            })
            .collect()
    }

    pub fn replay_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<rivo_storage::StoredMessage>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        Ok(storage_lock(store)?.replay_messages(session_id)?)
    }

    pub async fn replay_messages_async(
        &self,
        session_id: &str,
    ) -> Result<Vec<rivo_storage::StoredMessage>, SessionError> {
        let session_id = session_id.to_owned();
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.replay_messages(&session_id)
        })
        .await?
        .unwrap_or_default())
    }

    pub fn clear_diagnostics(&self) -> Result<(), SessionError> {
        if let Some(store) = &self.storage {
            storage_lock(store)?.clear_diagnostics()?;
        }
        Ok(())
    }

    pub async fn clear_diagnostics_async(&self) -> Result<(), SessionError> {
        blocking_storage(self.storage.clone(), |store| store.clear_diagnostics()).await?;
        Ok(())
    }

    pub fn session_permissions(&self) -> Result<Vec<String>, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(Vec::new());
        };
        Ok(storage_lock(store)?.session_permission_grants(
            &self.session_id,
            &self.config.workspace.to_string_lossy(),
            &PolicyProfile::Medium.to_string(),
        )?)
    }

    pub async fn session_permissions_async(&self) -> Result<Vec<String>, SessionError> {
        let session_id = self.session_id.clone();
        let workspace = self.config.workspace.to_string_lossy().to_string();
        let policy = PolicyProfile::Medium.to_string();
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.session_permission_grants(&session_id, &workspace, &policy)
        })
        .await?
        .unwrap_or_default())
    }

    pub fn clear_session_permissions(&self) -> Result<u64, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(0);
        };
        Ok(storage_lock(store)?.clear_session_permission_grants(&self.session_id)?)
    }

    pub async fn clear_session_permissions_async(&self) -> Result<u64, SessionError> {
        let session_id = self.session_id.clone();
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.clear_session_permission_grants(&session_id)
        })
        .await?
        .unwrap_or(0))
    }

    fn redaction_keys(&self) -> Result<Vec<String>, SessionError> {
        let mut keys = self.config.known_api_keys();
        if let Some(active) = active_llm_snapshot(&self.active_llm)? {
            keys.extend(active.config.known_api_keys());
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    pub fn record_ui_audit(
        &self,
        turn_id: Option<&str>,
        action: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<i64, SessionError> {
        let Some(store) = &self.storage else {
            return Ok(0);
        };
        let keys = self.redaction_keys()?;
        let payload = rivo_telemetry::redact_api_keys(&payload, &keys);
        Ok(
            storage_lock(store)?.append_ui_audit(&rivo_storage::UiAuditEvent {
                session_id: Some(self.session_id.clone()),
                turn_id: turn_id.map(ToOwned::to_owned),
                action: action.into(),
                payload: payload.to_string(),
            })?,
        )
    }

    pub async fn record_ui_audit_async(
        &self,
        turn_id: Option<&str>,
        action: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<i64, SessionError> {
        let keys = self.redaction_keys()?;
        let payload = rivo_telemetry::redact_api_keys(&payload, &keys).to_string();
        let event = rivo_storage::UiAuditEvent {
            session_id: Some(self.session_id.clone()),
            turn_id: turn_id.map(ToOwned::to_owned),
            action: action.into(),
            payload,
        };
        Ok(blocking_storage(self.storage.clone(), move |store| {
            store.append_ui_audit(&event)
        })
        .await?
        .unwrap_or(0))
    }

    /// 创建双向单会话驱动。事件在后台按产生顺序进入 stream；同一时间只接受一个活动 turn。
    pub fn channel(self: Arc<Self>) -> (IntentSender, SessionEventStream) {
        self.channel_with_metadata()
    }

    /// 保留只消费 `AgentEvent` 的兼容流；新前端应使用 [`Session::channel`]。
    pub fn legacy_channel(self: Arc<Self>) -> (IntentSender, AgentEventStream) {
        let (sender, events) = self.channel();
        let events = Box::pin(events.map(|event| event.event));
        (sender, events)
    }

    /// 创建带公共事件元数据的双向单会话驱动。
    pub fn channel_with_metadata(self: Arc<Self>) -> (IntentSender, SessionEventStream) {
        let (intent_tx, mut intent_rx) = tokio::sync::mpsc::channel(32);
        let (auto_tx, mut auto_rx) = tokio::sync::mpsc::channel(4);
        let driver_auto_tx = auto_tx.clone();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(256);
        let active = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio::sync::Mutex::new(
            None::<tokio::sync::watch::Sender<bool>>,
        ));
        let history = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pending = Arc::new(Mutex::new(None::<PendingTurn>));
        let retry = Arc::new(tokio::sync::Mutex::new(None::<RetryTurn>));
        let interactions: InteractionRegistry = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let confirmations: ConfirmationRegistry = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let subagent_confirmations: SubagentConfirmationRegistry =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let (initial_llm, startup_error) = match active_llm_snapshot(&self.active_llm) {
            Ok(active) => (active, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let default_capability = initial_llm
            .as_ref()
            .and_then(|active| active.agent.as_ref())
            .map(|agent| agent.capability_profile())
            .unwrap_or(self.config.capability_profile);
        let default_policy = initial_llm
            .as_ref()
            .and_then(|active| active.agent.as_ref())
            .map(|agent| agent.policy_profile())
            .unwrap_or(self.config.policy_profile);
        let options = Arc::new(tokio::sync::Mutex::new(TurnOptions {
            locale: self.config.locale.clone(),
            capability_profile: default_capability,
            policy_profile: default_policy,
            subagents_enabled: self.config.subagents_enabled,
            max_subagents: self.config.max_subagents,
            ..TurnOptions::default()
        }));
        let driver_session = self.clone();
        let driver_active = active.clone();
        let driver_cancel = cancel.clone();
        let driver_history = history.clone();
        let driver_pending = pending.clone();
        let driver_retry = retry.clone();
        let driver_interactions = interactions.clone();
        let driver_confirmations = confirmations.clone();
        let driver_subagent_confirmations = subagent_confirmations.clone();
        let driver_options = options.clone();
        tokio::spawn(async move {
            if let Some(message) = startup_error {
                let _ = event_tx.send(AgentEvent::Error { message }).await;
                let _ = event_tx
                    .send(AgentEvent::Stopped {
                        reason: "会话运行时状态失效".into(),
                    })
                    .await;
                return;
            }
            let restored_history = restore_history(&driver_session).await;
            *driver_history.lock().await = restored_history;
            let default_options = driver_options.lock().await.clone();
            if let Some((selection, restored_options)) =
                restore_session_settings(&driver_session, default_options).await
            {
                if let Some(selection) = selection {
                    if let Ok(active) = driver_session.select_model(&selection) {
                        let _ = replace_active_llm(&driver_session.active_llm, active);
                    }
                }
                *driver_options.lock().await = restored_options;
            }
            loop {
                let intent = tokio::select! {
                    intent = intent_rx.recv() => match intent {
                        Some(intent) => intent,
                        None => break,
                    },
                    intent = auto_rx.recv() => match intent {
                        Some(intent) => intent,
                        None => break,
                    },
                };
                match intent {
                    intent @ (Intent::SubmitTask { .. }
                    | Intent::AppendMessage { .. }
                    | Intent::SubmitTaskWithMedia { .. }
                    | Intent::AppendMessageWithMedia { .. }) => {
                        let (text, media_refs) = match intent {
                            Intent::SubmitTask { text } | Intent::AppendMessage { text } => {
                                (text, Vec::new())
                            }
                            Intent::SubmitTaskWithMedia { text, media_refs }
                            | Intent::AppendMessageWithMedia { text, media_refs } => {
                                (text, media_refs)
                            }
                            _ => unreachable!("matched user turn intent must carry text"),
                        };
                        let was_active = driver_active.swap(true, Ordering::SeqCst);
                        let waiting_confirmation =
                            match has_pending_turn(&driver_pending) {
                                Ok(pending) => pending,
                                Err(error) => {
                                    driver_active.store(false, Ordering::SeqCst);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                            } || !driver_confirmations.lock().await.is_empty();
                        if was_active || waiting_confirmation {
                            driver_active.store(false, Ordering::SeqCst);
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: if waiting_confirmation {
                                        "会话有等待确认的 turn".into()
                                    } else {
                                        "会话已有活动 turn".into()
                                    },
                                })
                                .await;
                            continue;
                        }
                        if let Some(model) = driver_session.config.model.as_deref() {
                            if let Err(error) = driver_session.validate_model_selection(model) {
                                driver_active.store(false, Ordering::SeqCst);
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: format!("运行前模型路由校验失败: {error}"),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "模型不可用，未发送上游请求".into(),
                                    })
                                    .await;
                                continue;
                            }
                        }
                        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                        *driver_cancel.lock().await = Some(cancel_tx);
                        let mut turn_options = driver_options.lock().await.clone();
                        auto_enable_read_only_mcp(&driver_session.config, &mut turn_options, &text);
                        let turn_id = new_turn_id();
                        let mut messages = driver_history.lock().await.clone();
                        let mut history_messages = messages.clone();
                        let history_before_user = history_messages.clone();
                        let mut memory_ids = Vec::new();
                        if turn_options.memory_recall {
                            if let Some(store) = &driver_session.storage {
                                let memories = match recall_memories(
                                    store,
                                    &driver_session.config.workspace.to_string_lossy(),
                                    &turn_options.capability_profile.to_string(),
                                )
                                .await
                                {
                                    Ok(memories) => memories,
                                    Err(error) => {
                                        emit_preflight_started(
                                            &event_tx,
                                            &driver_session.storage,
                                            &driver_session.session_id,
                                            &turn_id,
                                            &driver_session.config,
                                            &turn_options,
                                        )
                                        .await;
                                        let _ = event_tx
                                            .send(AgentEvent::Error {
                                                message: format!("持久记忆召回失败: {error}"),
                                            })
                                            .await;
                                        let _ = event_tx
                                            .send(AgentEvent::Stopped {
                                                reason: "持久记忆召回失败".into(),
                                            })
                                            .await;
                                        driver_active.store(false, Ordering::SeqCst);
                                        continue;
                                    }
                                };
                                memory_ids =
                                    memories.iter().map(|memory| memory.id.clone()).collect();
                                if !memories.is_empty() {
                                    let context = memories
                                        .iter()
                                        .map(|memory| memory.body.as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    messages.insert(
                                        0,
                                        Message::text(
                                            Role::System,
                                            format!("已确认的持久记忆：\n{context}"),
                                        ),
                                    );
                                }
                            }
                        }
                        if !media_refs.is_empty()
                            && !media_capability_for_route(&driver_session.config).input
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "unsupported_capability: 当前模型不支持图片输入，未发送 Provider 请求".into(),
                                })
                                .await;
                            let _ = event_tx
                                .send(AgentEvent::Stopped {
                                    reason: "当前模型不支持图片输入".into(),
                                })
                                .await;
                            driver_active.store(false, Ordering::SeqCst);
                            continue;
                        }
                        let user_message = match user_message_with_media(
                            &driver_session.session_id,
                            &text,
                            &media_refs,
                        ) {
                            Ok(message) => message,
                            Err(error) => {
                                let _ = event_tx.send(AgentEvent::Error { message: error }).await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "媒体输入无效，未发送上游请求".into(),
                                    })
                                    .await;
                                driver_active.store(false, Ordering::SeqCst);
                                continue;
                            }
                        };
                        messages.push(user_message);
                        history_messages.push(Message::text(Role::User, text.clone()));
                        *driver_history.lock().await = history_messages;
                        let session = driver_session.clone();
                        let sender = event_tx.clone();
                        let active_flag = driver_active.clone();
                        let history = driver_history.clone();
                        let pending_store = driver_pending.clone();
                        let session_id = session.session_id.clone();
                        let storage = session.storage.clone();
                        let retry_store = driver_retry.clone();
                        let turn_interactions = driver_interactions.clone();
                        let active_llm = match session.prepare_next_turn() {
                            Ok(active) => active,
                            Err(error) => {
                                *driver_history.lock().await = history_before_user;
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "模型路由不可用".into(),
                                    })
                                    .await;
                                driver_active.store(false, Ordering::SeqCst);
                                continue;
                            }
                        };
                        let api_keys = active_llm
                            .as_ref()
                            .map(|a| a.config.known_api_keys())
                            .unwrap_or_default();
                        if let Err(error) = append_message(
                            &storage,
                            &session.session_id,
                            &turn_id,
                            "user",
                            &text,
                            &api_keys,
                        )
                        .await
                        {
                            *driver_history.lock().await = history_before_user;
                            emit_preflight_started(
                                &event_tx,
                                &storage,
                                &session.session_id,
                                &turn_id,
                                &session.config,
                                &turn_options,
                            )
                            .await;
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: format!("历史持久化失败: {error}"),
                                })
                                .await;
                            let _ = event_tx
                                .send(AgentEvent::Stopped {
                                    reason: "历史持久化失败".into(),
                                })
                                .await;
                            driver_active.store(false, Ordering::SeqCst);
                            continue;
                        }
                        let sequence =
                            match next_event_sequence(&storage, &session.session_id).await {
                                Ok(sequence) => sequence,
                                Err(error) => {
                                    emit_preflight_started(
                                        &event_tx,
                                        &storage,
                                        &session.session_id,
                                        &turn_id,
                                        &session.config,
                                        &turn_options,
                                    )
                                    .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("历史持久化失败: {error}"),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "历史持久化失败".into(),
                                        })
                                        .await;
                                    driver_active.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            };
                        let (confirmation_tx, confirmation_rx) = mpsc::channel(32);
                        let (subagent_confirmation_tx, subagent_confirmation_rx) =
                            mpsc::channel(32);
                        let turn_confirmations = driver_confirmations.clone();
                        let turn_subagent_confirmations = driver_subagent_confirmations.clone();
                        tokio::spawn(async move {
                            execute_turn_request(
                                session,
                                active_llm,
                                messages,
                                TurnContext {
                                    sender,
                                    cancel_rx,
                                    active: active_flag,
                                    history,
                                    pending: pending_store,
                                    approved: HashSet::new(),
                                    storage,
                                    retry: retry_store,
                                    session_id,
                                    turn_id,
                                    sequence,
                                    turn_options,
                                    memory_ids,
                                    api_keys,
                                    browser_used: false,
                                    interactions: turn_interactions,
                                    confirmation_tx,
                                    confirmation_rx,
                                    confirmations: turn_confirmations,
                                    subagent_confirmation_tx,
                                    subagent_confirmation_rx,
                                    subagent_confirmations: turn_subagent_confirmations,
                                },
                            )
                            .await;
                        });
                    }
                    Intent::CancelTurn => {
                        let active_turn = driver_active.load(Ordering::SeqCst);
                        let pending_for_cancel = if active_turn {
                            None
                        } else {
                            match take_pending_turn(&driver_pending) {
                                Ok(pending_turn) => pending_turn,
                                Err(error) => {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                            }
                        };
                        if active_turn {
                            // 解除所有正在等待根会话答复的子代理，随后取消信号会终止
                            // 根调度器及其余在途任务。
                            let waiting =
                                std::mem::take(&mut *driver_subagent_confirmations.lock().await);
                            for (_, pending_confirmation) in waiting {
                                let _ = pending_confirmation.responder.send(
                                    SubagentConfirmationResponse {
                                        allow: false,
                                        cancelled: true,
                                        session_grant_key: None,
                                    },
                                );
                            }
                            if let Some(sender) = driver_cancel.lock().await.clone() {
                                let _ = sender.send(true);
                            }
                        } else if let Some(pending_turn) = pending_for_cancel {
                            let event = AgentEvent::Stopped {
                                reason: "用户取消等待确认的 turn".into(),
                            };
                            if let Err(error) = persist_standalone_event(
                                &driver_session.storage,
                                &driver_session.session_id,
                                &pending_turn.turn_id,
                                &event,
                            )
                            .await
                            {
                                let restore = restore_pending_turn(&driver_pending, pending_turn);
                                let restore_failed = restore.is_err();
                                let message = match restore {
                                    Ok(()) => format!("历史持久化失败: {error}"),
                                    Err(restore_error) => restore_error.to_string(),
                                };
                                let _ = event_tx.send(AgentEvent::Error { message }).await;
                                if restore_failed {
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                                continue;
                            }
                            let _ = event_tx.send(event).await;
                        } else {
                            let _ = event_tx
                                .send(AgentEvent::Stopped {
                                    reason: "没有活动 turn".into(),
                                })
                                .await;
                        }
                    }
                    intent @ (Intent::SetGoal { .. }
                    | Intent::GetGoal
                    | Intent::PauseGoal
                    | Intent::ResumeGoal
                    | Intent::ClearGoal) => {
                        match handle_management_intent(driver_session.clone(), intent).await {
                            Ok(event) => {
                                let _ = event_tx.send(event).await;
                            }
                            Err(error) => {
                                let _ = event_tx.send(AgentEvent::Error { message: error }).await;
                            }
                        }
                    }
                    Intent::Confirm {
                        request_id,
                        allow,
                        scope,
                        ..
                    } => {
                        let pending_confirmation =
                            { driver_confirmations.lock().await.remove(&request_id) };
                        if let Some(pending_confirmation) = pending_confirmation {
                            let mut granted_session_key = None;
                            if allow && matches!(scope, ApprovalScope::Session) {
                                let Some(grant_key) =
                                    pending_confirmation.session_grant_key.as_ref()
                                else {
                                    driver_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message:
                                                "当前确认不支持本会话授权；请选择允许一次或拒绝"
                                                    .into(),
                                        })
                                        .await;
                                    continue;
                                };
                                if driver_session.storage.is_none() {
                                    driver_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: "当前会话没有持久化存储，不能授予本会话权限"
                                                .into(),
                                        })
                                        .await;
                                    continue;
                                }
                                let grant = SessionPermissionGrant {
                                    session_id: driver_session.session_id.clone(),
                                    workspace: driver_session
                                        .config
                                        .workspace
                                        .to_string_lossy()
                                        .into_owned(),
                                    policy: PolicyProfile::Medium.to_string(),
                                    grant_key: grant_key.clone(),
                                };
                                if let Err(error) =
                                    save_session_permission_grant(&driver_session.storage, grant)
                                        .await
                                {
                                    driver_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("会话授权未保存: {error}"),
                                        })
                                        .await;
                                    continue;
                                }
                                granted_session_key = Some(grant_key.clone());
                            }
                            let _ = pending_confirmation.responder.send(ConfirmationResponse {
                                allow,
                                cancelled: false,
                                session_grant_key: granted_session_key,
                            });
                            continue;
                        }
                        // 子代理确认不会暂停根 Turn。答复经一次性通道送回仍在等待的
                        // 子代理工具循环，从而继续同一原子任务而非重放模型请求。
                        let pending_subagent_confirmation = {
                            driver_subagent_confirmations
                                .lock()
                                .await
                                .remove(&request_id)
                        };
                        if let Some(pending_confirmation) = pending_subagent_confirmation {
                            let mut granted_session_key = None;
                            if allow && matches!(scope, ApprovalScope::Session) {
                                let Some(grant_key) =
                                    pending_confirmation.session_grant_key.as_ref()
                                else {
                                    driver_subagent_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message:
                                                "当前确认不支持本会话授权；请选择允许一次或拒绝"
                                                    .into(),
                                        })
                                        .await;
                                    continue;
                                };
                                if driver_session.storage.is_none() {
                                    driver_subagent_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: "当前会话没有持久化存储，不能授予本会话权限"
                                                .into(),
                                        })
                                        .await;
                                    continue;
                                }
                                let grant = SessionPermissionGrant {
                                    session_id: driver_session.session_id.clone(),
                                    workspace: driver_session
                                        .config
                                        .workspace
                                        .to_string_lossy()
                                        .into_owned(),
                                    policy: PolicyProfile::Medium.to_string(),
                                    grant_key: grant_key.clone(),
                                };
                                if let Err(error) =
                                    save_session_permission_grant(&driver_session.storage, grant)
                                        .await
                                {
                                    driver_subagent_confirmations
                                        .lock()
                                        .await
                                        .insert(request_id, pending_confirmation);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("会话授权未保存: {error}"),
                                        })
                                        .await;
                                    continue;
                                }
                                granted_session_key = Some(grant_key.clone());
                            }
                            let _ =
                                pending_confirmation
                                    .responder
                                    .send(SubagentConfirmationResponse {
                                        allow,
                                        cancelled: false,
                                        session_grant_key: granted_session_key,
                                    });
                            continue;
                        }
                        let pending_turn =
                            match take_matching_pending_turn(&driver_pending, &request_id) {
                                Ok(pending_turn) => pending_turn,
                                Err(error) => {
                                    driver_active.store(false, Ordering::SeqCst);
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                            };
                        let Some(pending_turn) = pending_turn else {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "确认请求已过期或不匹配".into(),
                                })
                                .await;
                            continue;
                        };
                        if !allow {
                            let event = AgentEvent::Stopped {
                                reason: "用户拒绝确认".into(),
                            };
                            if let Err(error) = persist_standalone_event(
                                &driver_session.storage,
                                &driver_session.session_id,
                                &pending_turn.turn_id,
                                &event,
                            )
                            .await
                            {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: format!("历史持久化失败: {error}"),
                                    })
                                    .await;
                            }
                            let _ = event_tx.send(event).await;
                            continue;
                        }
                        if matches!(scope, ApprovalScope::Session) {
                            let Some(grant_key) = pending_turn.session_grant_key.as_ref() else {
                                if let Err(error) =
                                    restore_pending_turn(&driver_pending, pending_turn)
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: "当前确认不支持本会话授权；请选择允许一次或拒绝"
                                            .into(),
                                    })
                                    .await;
                                continue;
                            };
                            if driver_session.storage.is_none() {
                                if let Err(error) =
                                    restore_pending_turn(&driver_pending, pending_turn)
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: "当前会话没有持久化存储，不能授予本会话权限"
                                            .into(),
                                    })
                                    .await;
                                continue;
                            }
                            let grant = SessionPermissionGrant {
                                session_id: driver_session.session_id.clone(),
                                workspace: driver_session
                                    .config
                                    .workspace
                                    .to_string_lossy()
                                    .into_owned(),
                                policy: PolicyProfile::Medium.to_string(),
                                grant_key: grant_key.clone(),
                            };
                            if let Err(error) =
                                save_session_permission_grant(&driver_session.storage, grant).await
                            {
                                if let Err(restore_error) =
                                    restore_pending_turn(&driver_pending, pending_turn)
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: restore_error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: format!("会话授权未保存: {error}"),
                                    })
                                    .await;
                                continue;
                            }
                        }
                        if driver_active.swap(true, Ordering::SeqCst) {
                            if let Err(error) = restore_pending_turn(&driver_pending, pending_turn)
                            {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "会话运行时状态失效".into(),
                                    })
                                    .await;
                                break;
                            }
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "会话已有活动 turn".into(),
                                })
                                .await;
                            continue;
                        }
                        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                        *driver_cancel.lock().await = Some(cancel_tx);
                        let session = driver_session.clone();
                        let sender = event_tx.clone();
                        let active_flag = driver_active.clone();
                        let history = driver_history.clone();
                        let pending_store = driver_pending.clone();
                        let session_id = session.session_id.clone();
                        let turn_id = pending_turn.turn_id.clone();
                        let storage = session.storage.clone();
                        let retry_store = driver_retry.clone();
                        let turn_interactions = driver_interactions.clone();
                        let turn_options = pending_turn.options.clone();
                        let sequence =
                            match last_event_sequence(&storage, &session_id, &turn_id).await {
                                Ok(sequence) => sequence,
                                Err(error) => {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("历史持久化失败: {error}"),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "历史持久化失败".into(),
                                        })
                                        .await;
                                    driver_active.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            };
                        let mut approved = HashSet::new();
                        approved.insert(request_id);
                        if matches!(scope, ApprovalScope::Session) {
                            if let Some(grant_key) = pending_turn.session_grant_key.as_ref() {
                                approved.insert(grant_key.clone());
                            }
                        }
                        let active_llm = match active_llm_snapshot(&session.active_llm) {
                            Ok(active) => active,
                            Err(error) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "会话运行时状态失效".into(),
                                    })
                                    .await;
                                driver_active.store(false, Ordering::SeqCst);
                                break;
                            }
                        };
                        let api_keys = active_llm
                            .as_ref()
                            .map(|a| a.config.known_api_keys())
                            .unwrap_or_default();
                        let (confirmation_tx, confirmation_rx) = mpsc::channel(32);
                        let (subagent_confirmation_tx, subagent_confirmation_rx) =
                            mpsc::channel(32);
                        let turn_confirmations = driver_confirmations.clone();
                        let turn_subagent_confirmations = driver_subagent_confirmations.clone();
                        tokio::spawn(async move {
                            execute_turn_request(
                                session,
                                active_llm,
                                pending_turn.messages,
                                TurnContext {
                                    sender,
                                    cancel_rx,
                                    active: active_flag,
                                    history,
                                    pending: pending_store,
                                    approved,
                                    storage,
                                    retry: retry_store,
                                    session_id,
                                    turn_id,
                                    sequence,
                                    turn_options,
                                    memory_ids: Vec::new(),
                                    api_keys,
                                    browser_used: false,
                                    interactions: turn_interactions,
                                    confirmation_tx,
                                    confirmation_rx,
                                    confirmations: turn_confirmations,
                                    subagent_confirmation_tx,
                                    subagent_confirmation_rx,
                                    subagent_confirmations: turn_subagent_confirmations,
                                },
                            )
                            .await;
                        });
                    }
                    Intent::RetryStep { step_id } => {
                        if driver_active.load(Ordering::SeqCst) {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "会话已有活动 turn".into(),
                                })
                                .await;
                            continue;
                        }
                        let retry_turn = driver_retry.lock().await.take();
                        let Some(retry_turn) = retry_turn.filter(|turn| turn.step_id == step_id)
                        else {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "没有匹配的可重试步骤".into(),
                                })
                                .await;
                            continue;
                        };
                        driver_active.store(true, Ordering::SeqCst);
                        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                        *driver_cancel.lock().await = Some(cancel_tx);
                        let session = driver_session.clone();
                        let sender = event_tx.clone();
                        let active_flag = driver_active.clone();
                        let history = driver_history.clone();
                        let pending_store = driver_pending.clone();
                        let session_id = session.session_id.clone();
                        let turn_id = new_turn_id();
                        let storage = session.storage.clone();
                        let retry_store = driver_retry.clone();
                        let turn_interactions = driver_interactions.clone();
                        let turn_options = retry_turn.options.clone();
                        let sequence = match next_event_sequence(&storage, &session_id).await {
                            Ok(sequence) => sequence,
                            Err(error) => {
                                emit_preflight_started(
                                    &event_tx,
                                    &storage,
                                    &session_id,
                                    &turn_id,
                                    &session.config,
                                    &turn_options,
                                )
                                .await;
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: format!("历史持久化失败: {error}"),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "历史持久化失败".into(),
                                    })
                                    .await;
                                driver_active.store(false, Ordering::SeqCst);
                                continue;
                            }
                        };
                        let active_llm = match active_llm_snapshot(&session.active_llm) {
                            Ok(active) => active,
                            Err(error) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "会话运行时状态失效".into(),
                                    })
                                    .await;
                                driver_active.store(false, Ordering::SeqCst);
                                break;
                            }
                        };
                        let api_keys = active_llm
                            .as_ref()
                            .map(|a| a.config.known_api_keys())
                            .unwrap_or_default();
                        let (confirmation_tx, confirmation_rx) = mpsc::channel(32);
                        let (subagent_confirmation_tx, subagent_confirmation_rx) =
                            mpsc::channel(32);
                        let turn_confirmations = driver_confirmations.clone();
                        let turn_subagent_confirmations = driver_subagent_confirmations.clone();
                        tokio::spawn(async move {
                            execute_turn_request(
                                session,
                                active_llm,
                                retry_turn.messages,
                                TurnContext {
                                    sender,
                                    cancel_rx,
                                    active: active_flag,
                                    history,
                                    pending: pending_store,
                                    approved: HashSet::new(),
                                    storage,
                                    retry: retry_store,
                                    session_id,
                                    turn_id,
                                    sequence,
                                    turn_options,
                                    memory_ids: Vec::new(),
                                    api_keys,
                                    browser_used: false,
                                    interactions: turn_interactions,
                                    confirmation_tx,
                                    confirmation_rx,
                                    confirmations: turn_confirmations,
                                    subagent_confirmation_tx,
                                    subagent_confirmation_rx,
                                    subagent_confirmations: turn_subagent_confirmations,
                                },
                            )
                            .await;
                        });
                    }
                    Intent::SetTurnOptions { options } => {
                        if driver_active.load(Ordering::SeqCst) {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "运行中不能切换 turn options".into(),
                                })
                                .await;
                        } else if rivo_i18n::LocaleId::parse(&options.locale).is_err() {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "locale 必须是 canonical BCP 47 标识".into(),
                                })
                                .await;
                        } else if options.max_subagents == 0 {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "max_subagents 必须是正整数".into(),
                                })
                                .await;
                        } else if options.reasoning_effort != ReasoningEffort::ProviderDefault
                            && !driver_session
                                .config
                                .model
                                .as_ref()
                                .and_then(|model| driver_session.config.routing_models.get(model))
                                .is_some_and(|model| {
                                    model
                                        .supported_reasoning_efforts
                                        .contains(&options.reasoning_effort)
                                })
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: format!(
                                        "当前模型未声明支持 reasoning={}",
                                        options.reasoning_effort.as_str()
                                    ),
                                })
                                .await;
                        } else if let Some(name) = options.mcp_servers.iter().find(|name| {
                            driver_session
                                .config
                                .mcp_servers
                                .iter()
                                .find(|server| &server.name == *name)
                                .map(|server| !server.enabled)
                                .unwrap_or(true)
                        }) {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: format!("MCP server 未配置或未启用: {name}"),
                                })
                                .await;
                        } else if let Some((server, toolset)) =
                            options.mcp_toolsets.iter().find_map(|(server, toolsets)| {
                                let Some(configured) = driver_session
                                    .config
                                    .mcp_servers
                                    .iter()
                                    .find(|configured| configured.name == *server)
                                else {
                                    return Some((server, "server 未配置".to_owned()));
                                };
                                if !options.mcp_servers.contains(server) {
                                    return Some((server, "server 未在当前 turn 启用".to_owned()));
                                }
                                toolsets
                                    .iter()
                                    .find(|toolset| !configured.toolsets.contains_key(*toolset))
                                    .map(|toolset| (server, format!("未知 toolset {toolset}")))
                            })
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: format!("MCP server {server} 的选择无效: {toolset}"),
                                })
                                .await;
                        } else if let Err(error) =
                            driver_session.validate_a2a_selection_async(&options).await
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: error.to_string(),
                                })
                                .await;
                        } else {
                            let mut options = options;
                            options.locale = rivo_i18n::LocaleId::parse(&options.locale)
                                .expect("locale validated")
                                .to_string();
                            *driver_options.lock().await = options;
                        }
                    }
                    Intent::SetSessionLocale { locale } => {
                        let locale = match rivo_i18n::LocaleId::parse(&locale) {
                            Ok(locale) => locale.to_string(),
                            Err(_) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: "locale 必须是 canonical BCP 47 标识".into(),
                                    })
                                    .await;
                                continue;
                            }
                        };
                        let applied = AgentEvent::SessionLocaleApplied {
                            locale: locale.clone(),
                        };
                        if let Err(error) = persist_standalone_event(
                            &driver_session.storage,
                            &driver_session.session_id,
                            "session-locale",
                            &applied,
                        )
                        .await
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: format!("保存 Session locale 失败: {error}"),
                                })
                                .await;
                            continue;
                        }
                        // `driver_options` is only read while a fresh turn is
                        // created. In-flight turns and stored retries already
                        // own a clone, so this cannot change their prompt.
                        driver_options.lock().await.locale = locale;
                        let _ = event_tx.send(applied).await;
                    }
                    Intent::ApplySessionSettings { selection, options } => {
                        let pending = has_pending_turn(&driver_pending).unwrap_or(true);
                        let retry = driver_retry.lock().await.is_some();
                        if driver_active.load(Ordering::SeqCst) || pending || retry {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "运行中、待确认或待重试时不能切换会话设置".into(),
                                })
                                .await;
                            continue;
                        }
                        if options.max_subagents == 0 {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "max_subagents 必须是正整数".into(),
                                })
                                .await;
                            continue;
                        }
                        if rivo_i18n::LocaleId::parse(&options.locale).is_err() {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "locale 必须是 canonical BCP 47 标识".into(),
                                })
                                .await;
                            continue;
                        }
                        if let Err(error) = driver_session
                            .validate_mcp_selection(&options)
                            .and_then(|_| driver_session.validate_a2a_selection(&options))
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: error.to_string(),
                                })
                                .await;
                            continue;
                        }
                        match driver_session.select_model(&selection) {
                            Ok(active) => {
                                let route =
                                    active.config.route.clone().expect("selection resolved");
                                if options.reasoning_effort != ReasoningEffort::ProviderDefault
                                    && !driver_session
                                        .config
                                        .routing_models
                                        .get(&selection.model)
                                        .is_some_and(|model| {
                                            model
                                                .supported_reasoning_efforts
                                                .contains(&options.reasoning_effort)
                                        })
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!(
                                                "模型 {} 未声明支持 reasoning={}",
                                                selection.model,
                                                options.reasoning_effort.as_str()
                                            ),
                                        })
                                        .await;
                                    continue;
                                }
                                if let Err(error) = save_model_selection(
                                    &driver_session.storage,
                                    &driver_session.session_id,
                                    stored_model_route(&active.config),
                                )
                                .await
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("保存模型选择失败: {error}"),
                                        })
                                        .await;
                                    continue;
                                }
                                if let Err(error) =
                                    replace_active_llm(&driver_session.active_llm, active)
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    continue;
                                }
                                *driver_options.lock().await = options.clone();
                                let applied = AgentEvent::SessionSettingsApplied {
                                    selection,
                                    route,
                                    options,
                                };
                                if let Err(error) = persist_standalone_event(
                                    &driver_session.storage,
                                    &driver_session.session_id,
                                    "session-settings",
                                    &applied,
                                )
                                .await
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("保存 Session 设置历史失败: {error}"),
                                        })
                                        .await;
                                }
                                let _ = event_tx.send(applied).await;
                            }
                            Err(error) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Intent::SelectModel { selection } => {
                        let pending = match has_pending_turn(&driver_pending) {
                            Ok(pending) => pending,
                            Err(error) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                                let _ = event_tx
                                    .send(AgentEvent::Stopped {
                                        reason: "会话运行时状态失效".into(),
                                    })
                                    .await;
                                break;
                            }
                        };
                        let retry = driver_retry.lock().await.is_some();
                        if driver_active.load(Ordering::SeqCst) || pending || retry {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "运行中、待确认或待重试时不能切换模型".into(),
                                })
                                .await;
                            continue;
                        }
                        match driver_session.select_model(&selection) {
                            Ok(active) => {
                                if let Err(error) = save_model_selection(
                                    &driver_session.storage,
                                    &driver_session.session_id,
                                    stored_model_route(&active.config),
                                )
                                .await
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!("保存模型选择失败: {error}"),
                                        })
                                        .await;
                                    continue;
                                }
                                let route =
                                    active.config.route.clone().expect("selection resolved");
                                if let Err(error) =
                                    replace_active_llm(&driver_session.active_llm, active)
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: error.to_string(),
                                        })
                                        .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Stopped {
                                            reason: "会话运行时状态失效".into(),
                                        })
                                        .await;
                                    break;
                                }
                                let _ = event_tx
                                    .send(AgentEvent::ModelSelected { selection, route })
                                    .await;
                            }
                            Err(error) => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: error.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Intent::ProposeMemory { candidate } => {
                        let result =
                            save_memory_candidate(&driver_session.storage, candidate.clone()).await;
                        match result {
                            Ok(()) => {
                                let _ = event_tx
                                    .send(AgentEvent::MemoryCandidate {
                                        candidate_id: candidate.id,
                                        body_sha256: candidate.body_sha256,
                                    })
                                    .await;
                            }
                            Err(message) => {
                                let _ = event_tx.send(AgentEvent::Error { message }).await;
                            }
                        }
                    }
                    Intent::RespondInteraction { request_id, value } => {
                        let pending = driver_interactions.lock().await.remove(&request_id);
                        match pending {
                            Some(state) => {
                                let outcome = interaction_outcome(value, &state.schema);
                                if state.responder.send(outcome).is_err() {
                                    // 等待方（工具调用）已结束或被取消，回传通道已关闭。
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: format!(
                                                "Interaction {request_id} 的等待方已结束"
                                            ),
                                        })
                                        .await;
                                }
                            }
                            None => {
                                let _ = event_tx
                                    .send(AgentEvent::Error {
                                        message: format!("未找到待处理的 Interaction {request_id}"),
                                    })
                                    .await;
                            }
                        }
                    }
                    Intent::ConfirmMemory {
                        candidate_id,
                        body_sha256,
                    } => {
                        let result =
                            confirm_memory(&driver_session.storage, candidate_id, body_sha256)
                                .await;
                        match result {
                            Ok(memory) => {
                                let _ = event_tx
                                    .send(AgentEvent::MemoryConfirmed {
                                        memory_id: memory.id,
                                    })
                                    .await;
                            }
                            Err(message) => {
                                let _ = event_tx.send(AgentEvent::Error { message }).await;
                            }
                        }
                    }
                    Intent::RejectMemory { candidate_id } => {
                        let result =
                            reject_memory(&driver_session.storage, candidate_id.clone()).await;
                        match result {
                            Ok(()) => {
                                let _ = event_tx
                                    .send(AgentEvent::MemoryRejected { candidate_id })
                                    .await;
                            }
                            Err(message) => {
                                let _ = event_tx.send(AgentEvent::Error { message }).await;
                            }
                        }
                    }
                    Intent::DeleteMemory { memory_id } => {
                        let result =
                            delete_memory(&driver_session.storage, memory_id.clone()).await;
                        match result {
                            Ok(()) => {
                                let _ =
                                    event_tx.send(AgentEvent::MemoryDeleted { memory_id }).await;
                            }
                            Err(message) => {
                                let _ = event_tx.send(AgentEvent::Error { message }).await;
                            }
                        }
                    }
                    Intent::StartPlanExecution {
                        plan_id,
                        revision,
                        policy_profile,
                    } => {
                        let plan = driver_session
                            .load_task_plan(&plan_id, Some(revision))
                            .ok()
                            .flatten();
                        if matches!(policy_profile, Some(PolicyProfile::Strict)) {
                            let _ = event_tx
                                .send(AgentEvent::Error {
                                    message: "Plan 执行必须使用 workspace_write 或 full_access"
                                        .into(),
                                })
                                .await;
                            continue;
                        }
                        match handle_management_intent(
                            driver_session.clone(),
                            Intent::StartPlanExecution {
                                plan_id: plan_id.clone(),
                                revision,
                                policy_profile,
                            },
                        )
                        .await
                        {
                            Ok(event @ AgentEvent::PlanExecutionStarted { .. }) => {
                                let _ = event_tx.send(event).await;
                                let Some(plan) = plan else {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: "已批准计划无法载入，未启动 coding turn"
                                                .into(),
                                        })
                                        .await;
                                    continue;
                                };
                                let mut coding_options = driver_options.lock().await.clone();
                                coding_options.capability_profile = CapabilityProfile::Coding;
                                coding_options.policy_profile =
                                    policy_profile.unwrap_or(PolicyProfile::Medium);
                                coding_options.network = false;
                                coding_options.browser = false;
                                coding_options.computer = false;
                                coding_options.shell = false;
                                coding_options.mcp_servers.clear();
                                coding_options.mcp_toolsets.clear();
                                coding_options.a2a_peers.clear();
                                *driver_options.lock().await = coding_options;
                                let task = format_plan_execution_request(&plan);
                                if driver_auto_tx
                                    .send(Intent::SubmitTask { text: task })
                                    .await
                                    .is_err()
                                {
                                    let _ = event_tx
                                        .send(AgentEvent::Error {
                                            message: "coding turn 调度失败".into(),
                                        })
                                        .await;
                                }
                            }
                            Ok(event) => {
                                let _ = event_tx.send(event).await;
                            }
                            Err(error) => {
                                let _ = event_tx.send(AgentEvent::Error { message: error }).await;
                            }
                        }
                    }
                    intent @ (Intent::CreatePlan { .. }
                    | Intent::UpdatePlan { .. }
                    | Intent::ApprovePlan { .. }
                    | Intent::RejectPlan { .. }
                    | Intent::RequestPlanRevision { .. }
                    | Intent::CancelPlan { .. }) => {
                        match handle_management_intent(driver_session.clone(), intent).await {
                            Ok(event) => {
                                let _ = event_tx.send(event).await;
                            }
                            Err(error) => {
                                let _ = event_tx.send(AgentEvent::Error { message: error }).await;
                            }
                        }
                    }
                }
            }
        });
        let session_id = self.session_id.clone();
        let events = Box::pin(stream! {
            let mut turn_id = String::new();
            let mut sequence = 0u64;
            while let Some(event) = event_rx.recv().await {
                if let AgentEvent::Started {
                    turn_id: started_turn_id,
                    ..
                } = &event
                {
                    turn_id = started_turn_id.clone();
                    sequence = 0;
                }
                sequence = sequence.saturating_add(1);
                yield SessionEvent {
                    metadata: EventMetadata {
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone(),
                        sequence,
                        timestamp_utc_ms: utc_timestamp_ms(),
                    },
                    event,
                };
            }
        });
        (intent_tx, events)
    }

    pub fn handle(&self, intent: Intent) -> Result<SessionEventStream, SessionError> {
        let events = self.legacy_handle(intent)?;
        Ok(decorate_event_stream(self.session_id.clone(), events))
    }

    pub fn legacy_handle(&self, intent: Intent) -> Result<AgentEventStream, SessionError> {
        match intent {
            Intent::CreatePlan { plan } => self.create_task_plan(plan),
            Intent::UpdatePlan { plan } => self.update_task_plan(plan),
            Intent::ApprovePlan {
                plan_id,
                revision,
                document_sha256,
            } => self.transition_task_plan(
                &plan_id,
                revision,
                TaskPlanStatus::Approved,
                None,
                document_sha256.as_deref(),
            ),
            Intent::RejectPlan {
                plan_id,
                revision,
                reason,
            } => self.transition_task_plan(
                &plan_id,
                revision,
                TaskPlanStatus::Rejected,
                reason,
                None,
            ),
            Intent::RequestPlanRevision {
                plan_id,
                revision,
                feedback,
            } => self.transition_task_plan(
                &plan_id,
                revision,
                TaskPlanStatus::AwaitingApproval,
                Some(feedback),
                None,
            ),
            Intent::StartPlanExecution {
                plan_id,
                revision,
                policy_profile,
            } => self.start_plan_execution(&plan_id, revision, policy_profile),
            Intent::CancelPlan { plan_id } => {
                self.transition_task_plan(&plan_id, 0, TaskPlanStatus::Cancelled, None, None)
            }
            Intent::SetGoal { objective, budget } => self.set_goal(objective, budget),
            Intent::GetGoal => self.goal_event(),
            Intent::PauseGoal => self.transition_goal(GoalStatus::Paused),
            Intent::ResumeGoal => self.transition_goal(GoalStatus::Active),
            Intent::ClearGoal => self.transition_goal(GoalStatus::Cleared),
            Intent::CancelTurn => Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Stopped {
                    reason: "用户取消".into(),
                }
            }))),
            Intent::SubmitTask { text } | Intent::AppendMessage { text } => self.submit(text),
            Intent::SubmitTaskWithMedia { .. } | Intent::AppendMessageWithMedia { .. } => {
                Ok(Box::pin(futures::stream::once(async {
                    AgentEvent::Error {
                        message: "媒体输入需要活动双向 turn 状态".into(),
                    }
                })))
            }
            Intent::Confirm { .. }
            | Intent::RetryStep { .. }
            | Intent::SetTurnOptions { .. }
            | Intent::SetSessionLocale { .. }
            | Intent::ApplySessionSettings { .. }
            | Intent::SelectModel { .. }
            | Intent::RespondInteraction { .. }
            | Intent::ConfirmMemory { .. }
            | Intent::RejectMemory { .. }
            | Intent::DeleteMemory { .. }
            | Intent::ProposeMemory { .. } => Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "该 Intent 需要活动双向 turn 状态，目前仅支持单次流式 turn".into(),
                }
            }))),
        }
    }

    fn goal_event(&self) -> Result<AgentEventStream, SessionError> {
        let goal = self.current_goal()?;
        Ok(Box::pin(futures::stream::once(async move {
            goal.map(|goal| AgentEvent::GoalUpdated { goal })
                .unwrap_or_else(|| AgentEvent::Error {
                    message: "当前 Session 没有 Goal".into(),
                })
        })))
    }

    fn set_goal(&self, objective: String, budget: u64) -> Result<AgentEventStream, SessionError> {
        if objective.trim().is_empty() || budget == 0 {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "Goal objective 不能为空且 budget 必须为正整数".into(),
                }
            })));
        }
        let previous = self.current_goal()?.map(|g| g.revision).unwrap_or(0);
        let goal = Goal {
            goal_id: format!("goal-{}", new_turn_id()),
            session_id: self.session_id.clone(),
            objective,
            status: GoalStatus::Active,
            budget,
            used_budget: 0,
            revision: previous + 1,
            last_evidence_summary: None,
        };
        self.persist_goal(&goal)?;
        Ok(Box::pin(futures::stream::once(async move {
            AgentEvent::GoalUpdated { goal }
        })))
    }

    fn transition_goal(&self, status: GoalStatus) -> Result<AgentEventStream, SessionError> {
        let mut goal = self
            .current_goal()?
            .ok_or_else(|| SessionError::InvalidConfig("当前 Session 没有 Goal".into()))?;
        goal.status = status;
        goal.revision = goal.revision.saturating_add(1);
        self.persist_goal(&goal)?;
        Ok(Box::pin(futures::stream::once(async move {
            AgentEvent::GoalUpdated { goal }
        })))
    }

    fn create_task_plan(&self, mut plan: TaskPlan) -> Result<AgentEventStream, SessionError> {
        if plan.session_id != self.session_id
            || plan.plan_id.trim().is_empty()
            || plan.objective.trim().is_empty()
            || !valid_plan_id(&plan.plan_id)
            || plan.steps.len() > 100
        {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "任务计划标识、session 或目标无效".into(),
                }
            })));
        }
        if self.load_task_plan(&plan.plan_id, None)?.is_some() {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "任务计划标识已存在；请通过新 revision 更新计划".into(),
                }
            })));
        }
        plan.revision = 1;
        plan.status = TaskPlanStatus::AwaitingApproval;
        let document_sha256 = plan_document_sha256_with_keys(&plan, &self.redaction_keys()?)?;
        self.persist_task_plan(&plan)?;
        Ok(Box::pin(futures::stream::iter(vec![
            AgentEvent::PlanCreated { plan: plan.clone() },
            AgentEvent::PlanApprovalRequired {
                plan,
                document_sha256,
            },
        ])))
    }

    fn update_task_plan(&self, mut plan: TaskPlan) -> Result<AgentEventStream, SessionError> {
        let current = self
            .load_task_plan(&plan.plan_id, None)?
            .ok_or_else(|| SessionError::InvalidConfig("任务计划不存在".into()))?;
        if current.status == TaskPlanStatus::Approved
            || plan.revision != current.revision + 1
            || !valid_plan_id(&plan.plan_id)
            || plan.steps.len() > 100
        {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "只能基于最新版本更新计划；已批准版本必须递增 revision".into(),
                }
            })));
        }
        plan.session_id = self.session_id.clone();
        plan.status = TaskPlanStatus::AwaitingApproval;
        let document_sha256 = plan_document_sha256_with_keys(&plan, &self.redaction_keys()?)?;
        self.persist_task_plan(&plan)?;
        Ok(Box::pin(futures::stream::iter(vec![
            AgentEvent::PlanUpdated { plan: plan.clone() },
            AgentEvent::PlanApprovalRequired {
                plan,
                document_sha256,
            },
        ])))
    }

    fn transition_task_plan(
        &self,
        plan_id: &str,
        revision: u64,
        status: TaskPlanStatus,
        reason: Option<String>,
        expected_document_sha256: Option<&str>,
    ) -> Result<AgentEventStream, SessionError> {
        let mut plan = self
            .load_task_plan(plan_id, (revision > 0).then_some(revision))?
            .ok_or_else(|| SessionError::InvalidConfig("任务计划或 revision 不存在".into()))?;
        if matches!(status, TaskPlanStatus::Approved)
            && plan.status != TaskPlanStatus::AwaitingApproval
        {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "只有待审批计划可以批准".into(),
                }
            })));
        }
        let artifact = if matches!(status, TaskPlanStatus::Approved) {
            Some(self.write_plan_artifact(&plan, expected_document_sha256)?)
        } else {
            None
        };
        plan.status = status;
        if let Some(reason) = reason {
            plan.risks.push(reason);
        }
        plan.updated_at = utc_timestamp_ms().to_string();
        self.persist_task_plan(&plan)?;
        let event = match plan.status {
            TaskPlanStatus::Approved => {
                let (document_sha256, artifact_path) = artifact.expect("approved plan artifact");
                AgentEvent::PlanApproved {
                    plan_id: plan.plan_id,
                    revision: plan.revision,
                    document_sha256,
                    artifact_path,
                }
            }
            TaskPlanStatus::Rejected => AgentEvent::PlanRejected {
                plan_id: plan.plan_id,
                revision: plan.revision,
                reason: plan.risks.last().cloned(),
            },
            _ => AgentEvent::PlanUpdated { plan },
        };
        Ok(Box::pin(futures::stream::once(async move { event })))
    }

    fn start_plan_execution(
        &self,
        plan_id: &str,
        revision: u64,
        policy_profile: Option<PolicyProfile>,
    ) -> Result<AgentEventStream, SessionError> {
        if matches!(policy_profile, Some(PolicyProfile::Strict)) {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "Plan 执行必须使用 workspace_write 或 full_access".into(),
                }
            })));
        }
        let mut plan = self
            .load_task_plan(plan_id, Some(revision))?
            .ok_or_else(|| SessionError::InvalidConfig("任务计划或 revision 不存在".into()))?;
        if plan.status != TaskPlanStatus::Approved {
            return Ok(Box::pin(futures::stream::once(async {
                AgentEvent::Error {
                    message: "只有已批准计划可以执行".into(),
                }
            })));
        }
        plan.status = TaskPlanStatus::InProgress;
        plan.updated_at = utc_timestamp_ms().to_string();
        self.persist_task_plan(&plan)?;
        Ok(Box::pin(futures::stream::once(async move {
            AgentEvent::PlanExecutionStarted {
                plan_id: plan.plan_id,
                revision: plan.revision,
            }
        })))
    }

    fn load_task_plan(
        &self,
        plan_id: &str,
        revision: Option<u64>,
    ) -> Result<Option<TaskPlan>, SessionError> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        let store = storage_lock(storage)?;
        let stored = store
            .task_plan(&self.session_id, plan_id, revision)
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))?;
        stored
            .map(|p| {
                serde_json::from_str(&p.payload)
                    .map_err(|e| SessionError::InvalidConfig(e.to_string()))
            })
            .transpose()
    }

    fn persist_task_plan(&self, plan: &TaskPlan) -> Result<(), SessionError> {
        let Some(storage) = &self.storage else {
            return Err(SessionError::InvalidConfig("任务计划需要持久化存储".into()));
        };
        let now = utc_timestamp_ms().to_string();
        let store = storage_lock(storage)?;
        store
            .ensure_session(&self.session_id)
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))?;
        let stored = StoredTaskPlan {
            plan_id: plan.plan_id.clone(),
            session_id: self.session_id.clone(),
            objective: plan.objective.clone(),
            revision: plan.revision,
            status: serde_json::to_string(&plan.status).unwrap_or_else(|_| "\"draft\"".into()),
            payload: serde_json::to_string(plan)
                .map_err(|e| SessionError::InvalidConfig(e.to_string()))?,
            created_at: if plan.created_at.is_empty() {
                now.clone()
            } else {
                plan.created_at.clone()
            },
            updated_at: now,
        };
        if store
            .task_plan(&self.session_id, &plan.plan_id, Some(plan.revision))
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))?
            .is_some()
        {
            store
                .update_task_plan_status(
                    &self.session_id,
                    &plan.plan_id,
                    plan.revision,
                    &stored.status,
                    &stored.payload,
                    &stored.updated_at,
                )
                .map_err(|e| SessionError::InvalidConfig(e.to_string()))?;
            Ok(())
        } else {
            store
                .save_task_plan(&stored)
                .map_err(|e| SessionError::InvalidConfig(e.to_string()))
        }
    }

    fn write_plan_artifact(
        &self,
        plan: &TaskPlan,
        expected_document_sha256: Option<&str>,
    ) -> Result<(String, String), SessionError> {
        let (document, document_sha256) =
            render_plan_document_with_keys(plan, &self.redaction_keys()?)?;
        if expected_document_sha256 != Some(document_sha256.as_str()) {
            return Err(SessionError::InvalidConfig(
                "计划文档 hash 缺失或与当前 revision 不匹配".into(),
            ));
        }
        if !valid_plan_id(&plan.plan_id) || plan.steps.len() > 100 || document.len() > 256 * 1024 {
            return Err(SessionError::InvalidConfig("计划文档范围或标识无效".into()));
        }
        let workspace = fs::canonicalize(&self.config.workspace)
            .map_err(|error| SessionError::InvalidConfig(format!("workspace 不可解析: {error}")))?;
        let root = self.config.workspace.join(".rivo").join("plans");
        fs::create_dir_all(&root)
            .map_err(|error| SessionError::InvalidConfig(format!("创建计划目录失败: {error}")))?;
        let canonical_root = fs::canonicalize(&root)
            .map_err(|error| SessionError::InvalidConfig(format!("计划目录不可解析: {error}")))?;
        if !canonical_root.starts_with(&workspace) {
            return Err(SessionError::InvalidConfig("计划目录越出 workspace".into()));
        }
        let path = canonical_root.join(format!("{}.md", plan.plan_id));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(SessionError::InvalidConfig("计划文件不能是符号链接".into()));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SessionError::InvalidConfig(format!(
                    "读取计划文件失败: {error}"
                )))
            }
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&canonical_root).map_err(|error| {
            SessionError::InvalidConfig(format!("创建计划临时文件失败: {error}"))
        })?;
        temporary
            .write_all(document.as_bytes())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|error| SessionError::InvalidConfig(format!("写入计划文档失败: {error}")))?;
        persist_plan_temporary(temporary, &path)?;
        Ok((document_sha256, path.to_string_lossy().into_owned()))
    }

    fn current_goal(&self) -> Result<Option<Goal>, SessionError> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        let goal = storage_lock(storage)?
            .goal(&self.session_id)
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))?;
        Ok(goal.map(|g| Goal {
            goal_id: g.goal_id,
            session_id: g.session_id,
            objective: g.objective,
            status: match g.status.as_str() {
                "active" => GoalStatus::Active,
                "paused" => GoalStatus::Paused,
                "completed" => GoalStatus::Completed,
                "budget_limited" => GoalStatus::BudgetLimited,
                "blocked" => GoalStatus::Blocked,
                _ => GoalStatus::Cleared,
            },
            budget: g.budget,
            used_budget: g.used_budget,
            revision: g.revision,
            last_evidence_summary: g.last_evidence_summary,
        }))
    }

    pub fn goal(&self) -> Result<Option<Goal>, SessionError> {
        self.current_goal()
    }

    fn persist_goal(&self, goal: &Goal) -> Result<(), SessionError> {
        let Some(storage) = &self.storage else {
            return Err(SessionError::InvalidConfig("Goal 需要配置历史存储".into()));
        };
        let now = utc_timestamp_ms().to_string();
        let store = storage_lock(storage)?;
        store
            .ensure_session(&self.session_id)
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))?;
        store
            .save_goal(&rivo_storage::StoredGoal {
                goal_id: goal.goal_id.clone(),
                session_id: goal.session_id.clone(),
                objective: goal.objective.clone(),
                status: goal.status.as_str().into(),
                budget: goal.budget,
                used_budget: goal.used_budget,
                revision: goal.revision,
                last_evidence_summary: goal.last_evidence_summary.clone(),
                created_at: now.clone(),
                updated_at: now,
            })
            .map_err(|e| SessionError::InvalidConfig(e.to_string()))
    }

    fn submit(&self, text: String) -> Result<AgentEventStream, SessionError> {
        let active = self
            .prepare_next_turn()?
            .ok_or(SessionError::MissingRoute)?;
        let model = active
            .config
            .model
            .clone()
            .ok_or(SessionError::MissingModel)?;
        let started = AgentEvent::Started {
            provider: active.config.provider,
            model: model.clone(),
            selection: active.config.selection.clone(),
            route: active.config.route.clone(),
            turn_id: new_turn_id(),
            capability_profile: active.config.capability_profile,
            policy_profile: active.config.policy_profile,
        };
        let options = TurnOptions {
            capability_profile: active.config.capability_profile,
            policy_profile: active.config.policy_profile,
            ..TurnOptions::default()
        };
        let system = compose_system_prompt(&active.config, &options)?.system;
        let request = ProviderRequest {
            model,
            system: Some(system),
            messages: vec![Message::text(Role::User, text)],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format: None,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: options.reasoning_effort,
            metadata: Default::default(),
        };
        let client = active.client.clone();
        Ok(Box::pin(stream! {
            yield started;
            let mut provider_stream = client.stream(request);
            let mut answer = String::new();
            let mut stop_reason = StopReason::Final;
            let mut usage = Usage::default();
            while let Some(event) = provider_stream.next().await {
                match event {
                    Ok(ProviderStreamEvent::TextDelta { delta }) => {
                        answer.push_str(&delta);
                        yield AgentEvent::ModelDelta { text: delta };
                    }
                    Ok(ProviderStreamEvent::Done { stop_reason: reason, usage: final_usage }) => {
                        stop_reason = reason;
                        usage = final_usage;
                    }
                    Ok(ProviderStreamEvent::ToolArgumentsDelta { .. })
                    | Ok(ProviderStreamEvent::BlockStarted { .. })
                    | Ok(ProviderStreamEvent::BlockFinished { .. }) => {}
                    Err(error) => {
                        yield AgentEvent::Error { message: error.to_string() };
                        yield AgentEvent::Stopped { reason: "提供方失败".into() };
                        return;
                    }
                }
            }
            yield AgentEvent::FinalAnswer { text: answer, stop_reason, usage };
        }))
    }
}

#[cfg(test)]
fn render_plan_document(plan: &TaskPlan) -> Result<(String, String), SessionError> {
    render_plan_document_with_keys(plan, &[])
}

/// Extracts an explicitly marked JSON plan from a model's final answer. Ordinary
/// JSON or prose is intentionally ignored so a normal Plan answer cannot create
/// a durable artifact accidentally.
fn extract_plan_draft(text: &str, turn_id: &str) -> Option<TaskPlan> {
    let mut candidates = Vec::new();
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        candidates.push(trimmed);
    }
    let fenced = text.split("```").collect::<Vec<_>>();
    for (index, block) in fenced.iter().enumerate() {
        if index % 2 == 1 {
            candidates.push(block.strip_prefix("json").unwrap_or(block).trim());
        }
    }
    for candidate in candidates {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
            continue;
        };
        let Some(draft_value) = value.get("rivo_plan") else {
            continue;
        };
        let Ok(draft) = serde_json::from_value::<PlanDraft>(draft_value.clone()) else {
            continue;
        };
        let plan_id = draft
            .plan_id
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("plan-{turn_id}"));
        if !valid_plan_id(&plan_id)
            || draft.objective.trim().is_empty()
            || draft.steps.len() > 100
            || !plan_text_fields_within_bounds(&draft)
        {
            continue;
        }
        let steps = draft
            .steps
            .into_iter()
            .enumerate()
            .map(|(index, step)| TaskPlanStep {
                step_id: step
                    .step_id
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| format!("step-{}", index + 1)),
                title: step.title,
                description: step.description,
                depends_on: step.depends_on,
                acceptance_criteria: step.acceptance_criteria,
                status: TaskPlanStatus::AwaitingApproval,
                evidence_refs: step.evidence_refs,
                failure_reason: None,
            })
            .collect();
        return Some(TaskPlan {
            plan_id,
            session_id: String::new(),
            objective: draft.objective,
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps,
            dependencies: draft.dependencies,
            acceptance_criteria: draft.acceptance_criteria,
            risks: draft.risks,
            evidence_refs: draft.evidence_refs,
            created_at: String::new(),
            updated_at: String::new(),
        });
    }
    None
}

fn plan_text_fields_within_bounds(draft: &PlanDraft) -> bool {
    const MAX_FIELD_BYTES: usize = 16 * 1024;
    let bounded = |value: &str| value.len() <= MAX_FIELD_BYTES;
    bounded(&draft.objective)
        && draft.dependencies.iter().all(|value| bounded(value))
        && draft.acceptance_criteria.iter().all(|value| bounded(value))
        && draft.risks.iter().all(|value| bounded(value))
        && draft.evidence_refs.iter().all(|value| bounded(value))
        && draft.steps.iter().all(|step| {
            step.step_id.as_deref().is_none_or(bounded)
                && bounded(&step.title)
                && bounded(&step.description)
                && step.depends_on.iter().all(|value| bounded(value))
                && step.acceptance_criteria.iter().all(|value| bounded(value))
                && step.evidence_refs.iter().all(|value| bounded(value))
        })
}

fn format_plan_execution_request(plan: &TaskPlan) -> String {
    let mut request = format!(
        "请按照已批准的 Rivo 计划开始 coding turn。plan_id={}，revision={}。\n目标：{}\n",
        plan.plan_id, plan.revision, plan.objective
    );
    for (index, step) in plan.steps.iter().enumerate() {
        request.push_str(&format!(
            "\n{}. {}\n说明：{}\n验收：{}\n",
            index + 1,
            step.title,
            step.description,
            if step.acceptance_criteria.is_empty() {
                "按计划验证".to_owned()
            } else {
                step.acceptance_criteria.join("；")
            }
        ));
    }
    request.push_str(
        "\n请先复核计划范围，再逐步实现并运行必要测试；任何超出计划的高风险动作仍需独立确认。",
    );
    request
}

fn valid_plan_id(plan_id: &str) -> bool {
    !plan_id.is_empty()
        && plan_id.len() <= 128
        && plan_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && plan_id != "."
        && plan_id != ".."
}

fn render_plan_document_with_keys(
    plan: &TaskPlan,
    redaction_keys: &[String],
) -> Result<(String, String), SessionError> {
    let mut document = String::new();
    writeln!(
        document,
        "---\nschema: rivo-plan-v1\nplan_id: {}\nsession_id: {}\nrevision: {}\nstatus: {:?}\n---",
        plan.plan_id, plan.session_id, plan.revision, plan.status
    )
    .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    writeln!(document, "\n# {}\n", plan.objective)
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    document.push_str("## Steps\n");
    for (index, step) in plan.steps.iter().enumerate() {
        writeln!(
            document,
            "{}. **{}**\n   - {}\n   - status: {:?}",
            index + 1,
            step.title,
            step.description,
            step.status
        )
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
        if !step.depends_on.is_empty() {
            writeln!(document, "   - depends_on: {}", step.depends_on.join(", ")).map_err(
                |error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")),
            )?;
        }
        if !step.acceptance_criteria.is_empty() {
            writeln!(
                document,
                "   - acceptance: {}",
                step.acceptance_criteria.join("；")
            )
            .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
        }
        if !step.evidence_refs.is_empty() {
            writeln!(document, "   - evidence: {}", step.evidence_refs.join(", ")).map_err(
                |error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")),
            )?;
        }
    }
    if !plan.acceptance_criteria.is_empty() {
        writeln!(
            document,
            "\n## Acceptance\n{}",
            plan.acceptance_criteria
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    }
    if !plan.risks.is_empty() {
        writeln!(
            document,
            "\n## Risks\n{}",
            plan.risks
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    }
    if !plan.dependencies.is_empty() {
        writeln!(
            document,
            "\n## Dependencies\n{}",
            plan.dependencies
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    }
    if !plan.evidence_refs.is_empty() {
        writeln!(
            document,
            "\n## Evidence\n{}",
            plan.evidence_refs
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .map_err(|error| SessionError::InvalidConfig(format!("计划文档生成失败: {error}")))?;
    }
    let document = redaction_keys.iter().fold(document, |document, key| {
        if key.is_empty() {
            document
        } else {
            document.replace(key, "[REDACTED]")
        }
    });
    let document_sha256 = hex::encode(Sha256::digest(document.as_bytes()));
    Ok((document, document_sha256))
}

#[cfg(test)]
fn plan_document_sha256(plan: &TaskPlan) -> Result<String, SessionError> {
    Ok(render_plan_document(plan)?.1)
}

fn plan_document_sha256_with_keys(
    plan: &TaskPlan,
    redaction_keys: &[String],
) -> Result<String, SessionError> {
    Ok(render_plan_document_with_keys(plan, redaction_keys)?.1)
}

fn persist_plan_temporary(
    temporary: tempfile::NamedTempFile,
    path: &Path,
) -> Result<(), SessionError> {
    #[cfg(not(windows))]
    {
        temporary.persist(path).map_err(|error| {
            SessionError::InvalidConfig(format!("提交计划文档失败: {}", error.error))
        })?;
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
                return Err(SessionError::InvalidConfig(format!(
                    "提交计划文档失败: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
    }
    Ok(())
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

/// Persists a newly allocated public session id before the first turn.  Keeping
/// this separate makes the durability invariant testable without touching the
/// user data directory used by [`Session::connect`].
fn persist_session_identity(
    storage: &rivo_storage::Store,
    session_id: &str,
    config: &ResolvedConfig,
) -> Result<(), SessionError> {
    storage.set_model_route(session_id, stored_model_route(config).as_ref())?;
    storage.set_workspace_root(session_id, &config.workspace.to_string_lossy())?;
    Ok(())
}

fn stored_model_route(config: &ResolvedConfig) -> Option<StoredModelRoute> {
    let route = config.route.as_ref()?;
    Some(StoredModelRoute {
        selection: config.selection.clone()?,
        provider_id: route.provider_id.clone(),
        protocol: route.protocol.to_string(),
        base_url: route.base_url.clone(),
        priority: route.priority,
    })
}

fn build_active_llm(config: ResolvedConfig) -> Result<ActiveLlm, SessionError> {
    if config.route.is_none() {
        return Err(SessionError::MissingRoute);
    }
    let api_key = config.api_key()?;
    let client = create_client(
        config.provider,
        LlmProviderConfig {
            base_url: config.base_url.clone(),
            api_key,
            timeout_seconds: 120,
            max_stream_event_bytes: 1024 * 1024,
        },
    )?;
    let media_client = create_media_client(
        config.provider,
        LlmProviderConfig {
            base_url: config.base_url.clone(),
            api_key: config.api_key()?,
            timeout_seconds: 120,
            max_stream_event_bytes: 1024 * 1024,
        },
    )
    .ok();
    let agent = Arc::new(build_agent_for_turn(
        &config,
        client.clone(),
        config.policy_profile,
        config.capability_profile,
        None,
        "",
    )?);
    Ok(ActiveLlm {
        config,
        client,
        media_client,
        agent: Some(agent),
        rebuild_agent_per_turn: true,
    })
}

static MEDIA_ASSET_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MEDIA_ASSET_TTL_MS: u128 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MediaAssetRecord {
    asset: MediaRef,
    relative_path: String,
    created_at_ms: u128,
    expires_at_ms: u128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MediaAssetIndex {
    assets: BTreeMap<String, MediaAssetRecord>,
}

fn media_capability_for_route(config: &ResolvedConfig) -> MediaCapability {
    let Some(route) = config.route.as_ref() else {
        return MediaCapability::default();
    };
    // This is an exact, reviewable capability table. New model IDs must be
    // added with provider evidence; the runtime never infers support from a
    // vendor prefix or silently switches to another model.
    let responses_models = ["gpt-6-astra"];
    let images_models = [
        "gpt-image-1",
        "gpt-image-2.5-flare",
        "gpt-image-2.5-sunburst",
    ];
    let official_openai_endpoint = matches!(
        route.base_url.trim_end_matches('/'),
        "https://api.openai.com" | "https://api.openai.com/v1"
    );
    let input_supported = (route.vendor == "openai"
        && official_openai_endpoint
        && matches!(
            route.protocol,
            ProviderKind::OpenAiResponses | ProviderKind::OpenAiChatCompletions
        ))
        || (route.vendor == "anthropic"
            && matches!(route.protocol, ProviderKind::ClaudeMessages)
            && matches!(
                route.base_url.trim_end_matches('/'),
                "https://api.anthropic.com" | "https://api.anthropic.com/v1"
            ));
    let supported = (route.vendor == "openai"
        && official_openai_endpoint
        && route.protocol == ProviderKind::OpenAiResponses
        && responses_models.contains(&route.model.as_str()))
        || (route.vendor == "openai"
            && official_openai_endpoint
            && route.protocol == ProviderKind::OpenAiChatCompletions
            && images_models.contains(&route.model.as_str()));
    if supported {
        return MediaCapability {
            input: input_supported,
            generate: true,
            edit: true,
            ..MediaCapability::default()
        };
    }
    MediaCapability {
        input: input_supported,
        ..MediaCapability::default()
    }
}

struct SessionMediaExecutor {
    client: Arc<dyn MediaClient>,
    model: String,
    session_id: String,
    capability: MediaCapability,
}

struct UnsupportedMediaExecutor;

#[async_trait::async_trait]
impl rivo_agent::MediaExecutor for UnsupportedMediaExecutor {
    async fn execute(&self, _request: MediaRequest) -> Result<MediaResult, rivo_agent::AgentError> {
        Err(rivo_agent::AgentError::Tool(
            "unsupported_capability: 当前 Provider 未启用媒体 adapter".into(),
        ))
    }
}

fn media_root(session_id: &str) -> PathBuf {
    media_root_at(
        &data_root_dir().unwrap_or_else(std::env::temp_dir),
        session_id,
    )
}

fn media_root_at(data_root: &Path, session_id: &str) -> PathBuf {
    data_root.join("tmp").join("media").join(session_id)
}

fn media_index_path(session_id: &str) -> PathBuf {
    media_root(session_id).join("index.json")
}

fn valid_media_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 200
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_media_asset_id(asset_id: &str) -> bool {
    !asset_id.is_empty()
        && asset_id.len() <= 200
        && asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn read_media_index(session_id: &str) -> Result<MediaAssetIndex, String> {
    if !valid_media_session_id(session_id) {
        return Err("媒体 Session ID 无效".into());
    }
    let path = media_index_path(session_id);
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| format!("媒体资产索引损坏: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(MediaAssetIndex::default())
        }
        Err(error) => Err(format!("读取媒体资产索引失败: {error}")),
    }
}

fn write_media_index(session_id: &str, index: &MediaAssetIndex) -> Result<(), String> {
    let root = media_root(session_id);
    std::fs::create_dir_all(&root).map_err(|error| format!("创建媒体目录失败: {error}"))?;
    let path = media_index_path(session_id);
    let temporary = root.join(format!(".index-{}.tmp", std::process::id()));
    let bytes =
        serde_json::to_vec(index).map_err(|error| format!("序列化媒体索引失败: {error}"))?;
    std::fs::write(&temporary, bytes).map_err(|error| format!("写入媒体索引失败: {error}"))?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|error| format!("替换媒体索引失败: {error}"))?;
    }
    std::fs::rename(&temporary, &path).map_err(|error| format!("提交媒体索引失败: {error}"))
}

fn prune_media_index(session_id: &str, index: &mut MediaAssetIndex) -> Result<bool, String> {
    prune_media_index_at(&media_root(session_id), session_id, index)
}

fn prune_media_index_at(
    root: &Path,
    session_id: &str,
    index: &mut MediaAssetIndex,
) -> Result<bool, String> {
    let now = utc_timestamp_ms();
    let mut changed = false;
    let expired = index
        .assets
        .iter()
        .filter_map(|(asset_id, record)| {
            let safe_file = Path::new(&record.relative_path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == record.relative_path);
            if !valid_media_asset_id(asset_id)
                || record.asset.asset_id != *asset_id
                || record.asset.session_id != session_id
                || !safe_file
                || record.expires_at_ms <= now
                || !root.join(&record.relative_path).is_file()
            {
                Some((
                    asset_id.clone(),
                    safe_file.then(|| root.join(&record.relative_path)),
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for (asset_id, path) in expired {
        if let Some(path) = path {
            let _ = std::fs::remove_file(path);
        }
        index.assets.remove(&asset_id);
        changed = true;
    }
    Ok(changed)
}

/// Removes all media bytes and references owned by a closed Session.
pub fn remove_media_assets(session_id: &str) {
    if valid_media_session_id(session_id) {
        let root = data_root_dir().unwrap_or_else(std::env::temp_dir);
        remove_media_assets_at(&root, session_id);
    }
}

fn remove_media_assets_at(data_root: &Path, session_id: &str) {
    if valid_media_session_id(session_id) {
        let _ = std::fs::remove_dir_all(media_root_at(data_root, session_id));
    }
}

/// Removes all local media directories. Used together with history deletion;
/// TTL cleanup remains the fallback for assets left by an interrupted process.
pub fn remove_all_media_assets() {
    let Some(root) = data_root_dir().map(|root| root.join("tmp").join("media")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            let session_id = entry.file_name().to_string_lossy().into_owned();
            remove_media_assets(&session_id);
        }
    }
}

/// Performs bounded best-effort TTL cleanup for local media indexes.
pub fn prune_expired_media_assets() {
    let Some(root) = data_root_dir().map(|root| root.join("tmp").join("media")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            let session_id = entry.file_name().to_string_lossy().into_owned();
            if valid_media_session_id(&session_id) {
                if let Ok(mut index) = read_media_index(&session_id) {
                    if prune_media_index(&session_id, &mut index).unwrap_or(false) {
                        let _ = write_media_index(&session_id, &index);
                    }
                }
            }
        }
    }
}

fn media_type_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

pub const MAX_INPUT_MEDIA_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_INPUT_MEDIA_COUNT: usize = 8;
pub const MAX_INPUT_MEDIA_DIMENSION: u32 = 16_384;

/// Imports a user-selected local image into the current Session's private
/// media directory. The source path is transient and is never stored in the
/// index, history, events, or logs.
pub fn import_media_asset(session_id: &str, source_path: &Path) -> Result<MediaRef, String> {
    if !valid_media_session_id(session_id) {
        return Err("invalid_media: session id is invalid".into());
    }
    let metadata = std::fs::symlink_metadata(source_path)
        .map_err(|_| "invalid_media: source file is not readable".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("invalid_media: source must be a regular file".into());
    }
    if metadata.len() > MAX_INPUT_MEDIA_BYTES as u64 {
        return Err("media_too_large: image exceeds 20 MiB".into());
    }
    let data = std::fs::read(source_path)
        .map_err(|_| "invalid_media: source file is not readable".to_owned())?;
    let (media_type, width, height) = detect_image_metadata(&data)
        .ok_or_else(|| "invalid_media: unsupported or corrupt image".to_owned())?;
    if width > MAX_INPUT_MEDIA_DIMENSION || height > MAX_INPUT_MEDIA_DIMENSION {
        return Err("media_too_large: image dimensions exceed 16384 pixels".into());
    }
    let mut digest = Sha256::new();
    digest.update(&data);
    let digest = hex::encode(digest.finalize());
    let sequence = MEDIA_ASSET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let asset_id = format!("input-{sequence}-{}", &digest[..16]);
    let extension = media_type_extension(media_type).expect("detected media type has extension");
    let root = media_root(session_id);
    std::fs::create_dir_all(&root).map_err(|error| format!("media_import_failed: {error}"))?;
    let relative_path = format!("{asset_id}.{extension}");
    let destination = root.join(&relative_path);
    std::fs::write(&destination, &data).map_err(|error| format!("media_import_failed: {error}"))?;
    let now = utc_timestamp_ms();
    let asset = MediaRef {
        asset_id: asset_id.clone(),
        session_id: session_id.to_owned(),
        media_type: media_type.to_owned(),
        width: Some(width),
        height: Some(height),
    };
    let mut index = read_media_index(session_id)?;
    index.assets.insert(
        asset_id,
        MediaAssetRecord {
            asset: asset.clone(),
            relative_path,
            created_at_ms: now,
            expires_at_ms: now + MEDIA_ASSET_TTL_MS,
        },
    );
    if let Err(error) = write_media_index(session_id, &index) {
        let _ = std::fs::remove_file(destination);
        return Err(format!("media_import_failed: {error}"));
    }
    Ok(asset)
}

fn detect_image_metadata(data: &[u8]) -> Option<(&'static str, u32, u32)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") && data.len() >= 24 {
        let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return (width > 0 && height > 0).then_some(("image/png", width, height));
    }
    if data.starts_with(b"RIFF") && data.len() >= 30 && &data[8..12] == b"WEBP" {
        let chunk = &data[12..16];
        if chunk == b"VP8X" && data.len() >= 30 {
            let width = 1 + u32::from_le_bytes([data[24], data[25], data[26], 0]);
            let height = 1 + u32::from_le_bytes([data[27], data[28], data[29], 0]);
            return Some(("image/webp", width, height));
        }
        // VP8/VP8L dimensions are not needed for routing; accept the valid
        // WebP container and leave dimensions unknown to the caller.
        if chunk == b"VP8 " || chunk == b"VP8L" {
            return Some(("image/webp", 1, 1));
        }
    }
    if data.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2usize;
        while offset + 9 < data.len() {
            if data[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = data[offset + 1];
            offset += 2;
            if marker == 0xd9 || marker == 0xda {
                break;
            }
            if offset + 2 > data.len() {
                break;
            }
            let length = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            if length < 2 || offset + length > data.len() {
                break;
            }
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
                && length >= 7
            {
                let height = u16::from_be_bytes([data[offset + 3], data[offset + 4]]) as u32;
                let width = u16::from_be_bytes([data[offset + 5], data[offset + 6]]) as u32;
                if width > 0 && height > 0 {
                    return Some(("image/jpeg", width, height));
                }
            }
            offset += length;
        }
    }
    None
}

fn load_media_asset(session_id: &str, asset_id: &str) -> Result<(String, Vec<u8>), String> {
    if !valid_media_asset_id(asset_id) {
        return Err("输入资产 ID 无效".into());
    }
    let mut index = read_media_index(session_id)?;
    if prune_media_index(session_id, &mut index)? {
        write_media_index(session_id, &index)?;
    }
    let record = index
        .assets
        .get(asset_id)
        .ok_or_else(|| format!("当前 Session 不存在媒体资产: {asset_id}"))?;
    let path = media_root(session_id).join(&record.relative_path);
    let media_type = record.asset.media_type.clone();
    let data = std::fs::read(path).map_err(|error| format!("读取媒体资产失败: {error}"))?;
    Ok((media_type, data))
}

fn user_message_with_media(
    session_id: &str,
    text: &str,
    media_refs: &[MediaRef],
) -> Result<Message, String> {
    if media_refs.len() > MAX_INPUT_MEDIA_COUNT {
        return Err("invalid_media: too many image attachments".into());
    }
    let mut content = vec![ContentPart::Text {
        text: text.to_owned(),
    }];
    use base64::Engine as _;
    for media in media_refs {
        if media.session_id != session_id
            || !matches!(
                media.media_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp"
            )
            || media
                .width
                .is_some_and(|value| value > MAX_INPUT_MEDIA_DIMENSION)
            || media
                .height
                .is_some_and(|value| value > MAX_INPUT_MEDIA_DIMENSION)
        {
            return Err(
                "invalid_media: image reference is invalid or belongs to another session".into(),
            );
        }
        let (media_type, bytes) = load_media_asset(session_id, &media.asset_id)
            .map_err(|error| format!("media_expired: {error}"))?;
        content.push(ContentPart::Image {
            source: ImageSource::Base64 {
                media_type,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        });
    }
    Ok(Message {
        role: Role::User,
        content,
    })
}

#[async_trait::async_trait]
impl rivo_agent::MediaExecutor for SessionMediaExecutor {
    async fn execute(&self, request: MediaRequest) -> Result<MediaResult, rivo_agent::AgentError> {
        let supported = match request.operation {
            MediaOperation::Generate => self.capability.generate,
            MediaOperation::Edit => self.capability.edit,
        };
        if !supported {
            return Err(rivo_agent::AgentError::Tool(format!(
                "当前模型不支持图片{}，未发送 Provider 请求",
                match request.operation {
                    MediaOperation::Generate => "生成",
                    MediaOperation::Edit => "编辑",
                }
            )));
        }
        if request.prompt.trim().is_empty() {
            return Err(rivo_agent::AgentError::Tool("图片提示词不能为空".into()));
        }
        if request.prompt.chars().count() > 16_384 {
            return Err(rivo_agent::AgentError::Tool("图片提示词过长".into()));
        }
        if request
            .size
            .as_deref()
            .is_some_and(|size| !matches!(size, "1024x1024" | "1536x1024" | "1024x1536"))
        {
            return Err(rivo_agent::AgentError::Tool("图片尺寸不受支持".into()));
        }
        if request
            .output_media_type
            .as_deref()
            .is_some_and(|media_type| {
                !matches!(media_type, "image/png" | "image/jpeg" | "image/webp")
            })
        {
            return Err(rivo_agent::AgentError::Tool("图片格式不受支持".into()));
        }
        if matches!(request.operation, MediaOperation::Generate) && !request.input_assets.is_empty()
        {
            return Err(rivo_agent::AgentError::Tool("文生图不接受输入资产".into()));
        }
        if matches!(request.operation, MediaOperation::Edit) && request.input_assets.len() != 1 {
            return Err(rivo_agent::AgentError::Tool(
                "图像编辑必须提供一个当前 Session 资产".into(),
            ));
        }
        let inputs = request
            .input_assets
            .iter()
            .map(|asset| load_media_asset(&self.session_id, &asset.asset_id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(rivo_agent::AgentError::Tool)?;
        let input_bytes = inputs
            .iter()
            .map(|(_, data)| data.len() as u64)
            .sum::<u64>();
        if input_bytes > self.capability.max_input_bytes {
            return Err(rivo_agent::AgentError::Tool("输入媒体超过大小限制".into()));
        }
        let response = self
            .client
            .execute_media(MediaProviderRequest {
                model: self.model.clone(),
                operation: request.operation,
                prompt: request.prompt,
                inputs: inputs
                    .into_iter()
                    .map(|(media_type, data)| MediaInput { media_type, data })
                    .collect(),
                size: request.size,
                output_media_type: request.output_media_type,
            })
            .await
            .map_err(|error| rivo_agent::AgentError::Tool(error.to_string()))?;
        if response.data.is_empty() || response.data.len() as u64 > self.capability.max_output_bytes
        {
            return Err(rivo_agent::AgentError::Tool(
                "Provider 图片输出超过限制或为空".into(),
            ));
        }
        let extension = media_type_extension(&response.media_type).ok_or_else(|| {
            rivo_agent::AgentError::Tool("Provider 返回了不支持的图片格式".into())
        })?;
        let asset_id = format!(
            "asset-{}-{}",
            utc_timestamp_ms(),
            MEDIA_ASSET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let root = media_root(&self.session_id);
        std::fs::create_dir_all(&root)
            .map_err(|error| rivo_agent::AgentError::Tool(format!("创建媒体目录失败: {error}")))?;
        let path = root.join(format!("{asset_id}.{extension}"));
        std::fs::write(&path, &response.data)
            .map_err(|error| rivo_agent::AgentError::Tool(format!("写入媒体结果失败: {error}")))?;
        let relative_path = format!("{asset_id}.{extension}");
        let mut index = read_media_index(&self.session_id).map_err(rivo_agent::AgentError::Tool)?;
        let created_at_ms = utc_timestamp_ms();
        index.assets.insert(
            asset_id.clone(),
            MediaAssetRecord {
                asset: MediaRef {
                    asset_id: asset_id.clone(),
                    session_id: self.session_id.clone(),
                    media_type: response.media_type.clone(),
                    width: None,
                    height: None,
                },
                relative_path: relative_path.clone(),
                created_at_ms,
                expires_at_ms: created_at_ms.saturating_add(MEDIA_ASSET_TTL_MS),
            },
        );
        write_media_index(&self.session_id, &index).map_err(rivo_agent::AgentError::Tool)?;
        Ok(MediaResult {
            asset: MediaRef {
                asset_id,
                session_id: self.session_id.clone(),
                media_type: response.media_type,
                width: None,
                height: None,
            },
            delivery: MediaDelivery::LocalPath {
                path: root.join(relative_path).to_string_lossy().into_owned(),
            },
        })
    }
}

fn build_agent_for_turn(
    config: &ResolvedConfig,
    client: Arc<dyn LlmClient>,
    policy_profile: PolicyProfile,
    capability_profile: CapabilityProfile,
    media_client: Option<Arc<dyn MediaClient>>,
    session_id: &str,
) -> Result<rivo_agent::Agent, SessionError> {
    let policy = rivo_policy::PolicyEvaluator::new(&config.workspace)?;
    let runtime = Arc::new(rivo_tools::ToolRuntime::new(
        &config.workspace,
        policy_profile,
        capability_profile,
    )?);
    let mut registry = rivo_agent::ToolRegistry::default();
    registry
        .register_read_tools(runtime.clone())
        .map_err(|error| SessionError::InvalidConfig(error.to_string()))?;
    if matches!(capability_profile, CapabilityProfile::Coding) {
        registry
            .register_coding_tools(runtime)
            .map_err(|error| SessionError::InvalidConfig(error.to_string()))?;
    }
    if !matches!(capability_profile, CapabilityProfile::Plan) {
        let capability = media_capability_for_route(config);
        let executor: Arc<dyn rivo_agent::MediaExecutor> = match media_client {
            Some(media_client) => Arc::new(SessionMediaExecutor {
                client: media_client,
                model: config.model.clone().unwrap_or_default(),
                session_id: session_id.to_owned(),
                capability,
            }),
            None => Arc::new(UnsupportedMediaExecutor),
        };
        registry
            .register_media_tools(capability, executor)
            .map_err(|error| SessionError::InvalidConfig(error.to_string()))?;
    }
    Ok(
        rivo_agent::Agent::new(client, registry, policy, policy_profile, capability_profile)
            .with_default_model(config.model.clone().unwrap_or_default()),
    )
}

fn decorate_event_stream(session_id: String, mut source: AgentEventStream) -> SessionEventStream {
    Box::pin(stream! {
        let mut turn_id = String::new();
        let mut sequence = 0u64;
        while let Some(event) = source.next().await {
            if let AgentEvent::Started {
                turn_id: started_turn_id,
                ..
            } = &event
            {
                turn_id = started_turn_id.clone();
                sequence = 0;
            }
            sequence = sequence.saturating_add(1);
            yield SessionEvent {
                metadata: EventMetadata {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    sequence,
                    timestamp_utc_ms: utc_timestamp_ms(),
                },
                event,
            };
        }
    })
}

async fn restore_history(session: &Session) -> Vec<Message> {
    let session_id = session.session_id.clone();
    let Ok(Some(messages)) = blocking_storage(session.storage.clone(), move |store| {
        store.replay_messages(&session_id)
    })
    .await
    else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter_map(|message| {
            let role = match message.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                "tool" => Role::Tool,
                _ => return None,
            };
            Some(Message::text(role, message.body))
        })
        .collect()
}

async fn restore_session_settings(
    session: &Session,
    mut options: TurnOptions,
) -> Option<(Option<ModelSelection>, TurnOptions)> {
    let events = session
        .replay_events_async(&session.session_id)
        .await
        .ok()?;
    let mut selection = None;
    let mut restored = false;
    // Replay in journal order. A locale event after a complete settings event
    // overlays only locale; a later settings event intentionally supersedes it.
    for event in events {
        match event {
            AgentEvent::SessionSettingsApplied {
                selection: next_selection,
                options: next_options,
                ..
            } => {
                selection = Some(next_selection);
                options = next_options;
                restored = true;
            }
            AgentEvent::SessionLocaleApplied { locale } => {
                options.locale = locale;
                restored = true;
            }
            _ => {}
        }
    }
    restored.then_some((selection, options))
}

async fn emit_event(context: &mut TurnContext, event: AgentEvent) -> bool {
    context.sequence = context.sequence.saturating_add(1);
    if let AgentEvent::MediaReady { result, .. } = &event {
        // The next turn needs the stable asset identity, not the local path or
        // a potentially signed remote URL. Keep the same bounded reference in
        // memory and in the durable message history.
        let body = format!(
            "Rivo 已生成媒体资产，可在后续图片编辑中引用：{}",
            serde_json::to_string(&result.asset).unwrap_or_else(|_| "{}".into())
        );
        context
            .history
            .lock()
            .await
            .push(Message::text(Role::Assistant, body.clone()));
        if let Err(error) = persist_message(context, "assistant", &body).await {
            let _ = context
                .sender
                .send(AgentEvent::Error {
                    message: format!("媒体资产历史持久化失败: {error}"),
                })
                .await;
            return false;
        }
    }
    if matches!(
        event,
        AgentEvent::SubagentQueued { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentCompleted { .. }
    ) {
        let task_result = persist_agent_task_event(
            &context.storage,
            &context.session_id,
            &context.turn_id,
            &event,
            &context.api_keys,
        )
        .await;
        if let Err(error) = task_result {
            let _ = context
                .sender
                .send(AgentEvent::Error {
                    message: format!("子代理任务持久化失败: {error}"),
                })
                .await;
            return false;
        }
    }
    let persist = !context.browser_used
        || !matches!(
            event,
            AgentEvent::ModelDelta { .. }
                | AgentEvent::ToolRequested { .. }
                | AgentEvent::ToolObservation { .. }
                | AgentEvent::FinalAnswer { .. }
                | AgentEvent::Error { .. }
                | AgentEvent::Failure { .. }
                | AgentEvent::RetryAvailable { .. }
        );
    if persist {
        if let Some(store) = &context.storage {
            let mut value = match serde_json::to_value(&event) {
                Ok(value) => value,
                Err(error) => {
                    let _ = context
                        .sender
                        .send(AgentEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return false;
                }
            };
            if matches!(event, AgentEvent::MediaReady { .. }) {
                if let Some(result) = value
                    .get_mut("result")
                    .and_then(|value| value.as_object_mut())
                {
                    result.remove("delivery");
                }
            }
            let kind = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("event")
                .to_owned();
            let value = rivo_telemetry::redact_api_keys(&value, &context.api_keys);
            let body = value.to_string();
            let store = Arc::clone(store);
            let session_id = context.session_id.clone();
            let turn_id = context.turn_id.clone();
            let sequence = context.sequence;
            let result = match tokio::task::spawn_blocking(move || {
                storage_lock(&store).and_then(|store| {
                    store.append_event(&rivo_storage::StoredEvent {
                        session_id,
                        turn_id,
                        sequence,
                        kind,
                        body,
                    })
                })
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(rivo_storage::StorageError::Unavailable(format!(
                    "历史存储 worker 已中断: {error}"
                ))),
            };
            if let Err(error) = result {
                let _ = context
                    .sender
                    .send(AgentEvent::Error {
                        message: format!("历史持久化失败: {error}"),
                    })
                    .await;
                return false;
            }
        }
    }
    context.sender.send(event).await.is_ok()
}

/// 取消根 Turn 时，调度 future 会被 drop；同时必须立即把尚未取得终态的
/// 子任务写为 cancelled 并通知前端，不能等到下次打开 SQLite 才显示 interrupted。
async fn cancel_subagent_tasks(context: &mut TurnContext) {
    let Some(storage) = context.storage.as_ref() else {
        return;
    };
    let tasks = match active_agent_tasks(storage, &context.session_id, &context.turn_id).await {
        Ok(tasks) => tasks,
        Err(_) => return,
    };
    for task in tasks {
        let _ = emit_event(
            context,
            AgentEvent::SubagentCompleted {
                result: SubagentResult {
                    agent_id: AgentId(task.agent_id),
                    task_id: AgentTaskId(task.task_id),
                    state: AgentTaskState::Cancelled,
                    summary: "根 Turn 已取消".into(),
                    evidence: Vec::new(),
                    truncated: false,
                    usage: Default::default(),
                },
            },
        )
        .await;
    }
}

async fn emit_memory_events(context: &mut TurnContext) {
    if context.memory_ids.is_empty() {
        return;
    }
    let ids = context.memory_ids.clone();
    let _ = emit_event(
        context,
        AgentEvent::MemoryRecalled {
            memory_ids: ids.clone(),
        },
    )
    .await;
    let _ = emit_event(context, AgentEvent::MemoryInjected { memory_ids: ids }).await;
}

async fn persist_message(context: &TurnContext, role: &str, body: &str) -> Result<(), String> {
    append_message(
        &context.storage,
        &context.session_id,
        &context.turn_id,
        role,
        body,
        &context.api_keys,
    )
    .await
}

/// 浏览器 turn 的工具观察和最终回答是当前 turn 的瞬时数据。保留它会让下一次
/// 提交自动把页面派生内容重新发送给模型，并可能在普通 turn 中落盘。
async fn retain_final_answer(
    history: &Arc<tokio::sync::Mutex<Vec<Message>>>,
    browser_used: bool,
    text: &str,
) {
    if browser_used {
        return;
    }
    history
        .lock()
        .await
        .push(Message::text(Role::Assistant, text));
}

/*
fn append_message_legacy(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    role: &str,
    body: &str,
    api_keys: &[String],
) -> Result<(), String> {
    let Some(store) = storage else {
        return Ok(());
    };
    let store = storage_lock(store).map_err(|error| error.to_string())?;
    let sequence = store
        .next_message_sequence(session_id)
        .map_err(|error| error.to_string())?;
    let safe_body =
        rivo_telemetry::redact_api_keys(&serde_json::Value::String(body.to_owned()), api_keys)
            .as_str()
            .unwrap_or(body)
            .to_owned();
        store
            .append_message(&rivo_storage::StoredMessage {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            sequence,
            role: role.into(),
            body: safe_body,
        })
        .map_err(|error| error.to_string())
}

fn recall_memories_legacy(
    store: &Arc<Mutex<rivo_storage::Store>>,
    workspace: &str,
    mode: &str,
) -> Result<Vec<rivo_storage::Memory>, rivo_storage::StorageError> {
    storage_lock(store)?.recall(workspace, mode, 5)
}

fn next_event_sequence_legacy(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
) -> Result<u64, String> {
    let Some(store) = storage else {
        return Ok(0);
    };
    storage_lock(store)
        .map_err(|error| error.to_string())?
        .next_event_sequence(session_id)
        .map_err(|error| error.to_string())
}

fn persist_standalone_event_legacy(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    event: &AgentEvent,
) -> Result<(), String> {
    let Some(store) = storage else {
        return Ok(());
    };
    let value = serde_json::to_value(event).map_err(|error| error.to_string())?;
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("event")
        .to_owned();
    let body = value.to_string();
    let store = storage_lock(store).map_err(|error| error.to_string())?;
    let sequence = store
        .next_event_sequence(session_id)
        .map_err(|error| error.to_string())?;
    store
        .append_event(&rivo_storage::StoredEvent {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            sequence,
            kind,
            body,
        })
        .map_err(|error| error.to_string())
}
*/

async fn emit_preflight_started(
    sender: &tokio::sync::mpsc::Sender<AgentEvent>,
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    config: &ResolvedConfig,
    options: &TurnOptions,
) {
    let event = AgentEvent::Started {
        provider: config.provider,
        model: config.model.clone().unwrap_or_else(|| "未配置".into()),
        selection: None,
        route: None,
        turn_id: turn_id.into(),
        capability_profile: options.capability_profile,
        policy_profile: options.policy_profile,
    };
    let _ = persist_standalone_event(storage, session_id, turn_id, &event).await;
    let _ = sender.send(event).await;
}

async fn execute_turn_request(
    session: Arc<Session>,
    active_llm: Option<ActiveLlm>,
    messages: Vec<Message>,
    mut context: TurnContext,
) {
    let active = context.active.clone();
    let history = context.history.clone();
    let pending = context.pending.clone();
    if let Err(error) = session
        .validate_a2a_selection_async(&context.turn_options)
        .await
    {
        let _ = emit_event(
            &mut context,
            AgentEvent::Error {
                message: error.to_string(),
            },
        )
        .await;
        let _ = emit_event(
            &mut context,
            AgentEvent::Stopped {
                reason: "A2A peer 选择无效".into(),
            },
        )
        .await;
        active.store(false, Ordering::SeqCst);
        return;
    }
    let mut approved = std::mem::take(&mut context.approved);
    if matches!(context.turn_options.policy_profile, PolicyProfile::Medium) {
        if let Some(store) = context.storage.as_ref() {
            if let Ok(store) = store.lock() {
                if let Ok(grants) = store.session_permission_grants(
                    &context.session_id,
                    &session.config.workspace.to_string_lossy(),
                    &PolicyProfile::Medium.to_string(),
                ) {
                    approved.extend(grants);
                }
            }
        }
    }
    let mut cancel_rx = context.cancel_rx.clone();
    let Some(active_llm) = active_llm else {
        let _ = emit_event(
            &mut context,
            AgentEvent::Error {
                message: SessionError::MissingRoute.to_string(),
            },
        )
        .await;
        let _ = emit_event(
            &mut context,
            AgentEvent::Stopped {
                reason: "配置错误".into(),
            },
        )
        .await;
        active.store(false, Ordering::SeqCst);
        return;
    };
    let active_config = active_llm.config.clone();
    let Some(model) = active_config.model.clone() else {
        let _ = emit_event(
            &mut context,
            AgentEvent::Error {
                message: SessionError::MissingModel.to_string(),
            },
        )
        .await;
        let _ = emit_event(
            &mut context,
            AgentEvent::Stopped {
                reason: "配置错误".into(),
            },
        )
        .await;
        active.store(false, Ordering::SeqCst);
        return;
    };
    let prompt = match compose_system_prompt(&active_config, &context.turn_options) {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = emit_event(
                &mut context,
                AgentEvent::Error {
                    message: error.to_string(),
                },
            )
            .await;
            let _ = emit_event(
                &mut context,
                AgentEvent::Stopped {
                    reason: "提示词初始化失败".into(),
                },
            )
            .await;
            active.store(false, Ordering::SeqCst);
            return;
        }
    };
    // MCP elicitation 桥接通道：仅在存在 MCP server 时才注入到 agent。turn 循环通过 elicit_rx
    // 收到服务端 elicitation 后，转成 InteractionRequired 事件并登记到共享注册表等待用户答复。
    let (elicit_tx, mut elicit_rx) =
        tokio::sync::mpsc::channel::<rivo_agent::PendingElicitation>(4);
    let configured_agent = if let Some(base_agent) = active_llm.agent.as_ref() {
        let mut profiled = if active_llm.rebuild_agent_per_turn {
            match build_agent_for_turn(
                &active_config,
                active_llm.client.clone(),
                context.turn_options.policy_profile,
                context.turn_options.capability_profile,
                active_llm.media_client.clone(),
                &context.session_id,
            ) {
                Ok(agent) => agent.with_limits(active_config.turn_limits()),
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "工具初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        } else {
            base_agent
                .with_profiles(
                    context.turn_options.policy_profile,
                    context.turn_options.capability_profile,
                )
                .with_limits(active_config.turn_limits())
        };
        profiled = profiled.with_reasoning_effort(context.turn_options.reasoning_effort);
        if context.turn_options.network {
            match profiled.with_network_tools() {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        if context.turn_options.browser
            && !matches!(
                context.turn_options.capability_profile,
                CapabilityProfile::Plan
            )
        {
            let browser_agent = match (
                session.config.browser.extension_pipe.as_deref(),
                session.config.browser.extension_instance.clone(),
            ) {
                (Some(pipe), Some(instance)) => {
                    rivo_tools::browser_extension::ExtensionBrowserClient::connect_named_pipe(
                        pipe, instance,
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|client| {
                        profiled
                            .with_extension_browser_tools(client)
                            .map_err(|error| error.to_string())
                    })
                }
                _ => profiled
                    .with_browser_tools(&session.config.browser.cdp_url)
                    .map_err(|error| error.to_string()),
            };
            match browser_agent {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(&mut context, AgentEvent::Error { message: error }).await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "浏览器能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        if context.turn_options.computer
            && !matches!(
                context.turn_options.capability_profile,
                CapabilityProfile::Plan
            )
        {
            let backend: Arc<dyn rivo_tools::computer::ComputerBackend> = {
                #[cfg(windows)]
                {
                    Arc::new(rivo_tools::computer::WindowsComputerBackend)
                }
                #[cfg(not(windows))]
                {
                    Arc::new(rivo_tools::computer::UnavailableComputerBackend)
                }
            };
            match profiled.with_computer_tools(backend) {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "Computer Use 能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        if context.turn_options.shell {
            match profiled.with_shell_tools() {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "Shell 能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        if !context.turn_options.mcp_servers.is_empty() {
            let selected = session
                .config
                .mcp_servers
                .iter()
                .filter(|server| context.turn_options.mcp_servers.contains(&server.name))
                .cloned()
                .map(|server| {
                    let toolsets = context
                        .turn_options
                        .mcp_toolsets
                        .get(&server.name)
                        .cloned()
                        .unwrap_or_default();
                    (server, toolsets)
                })
                .collect::<Vec<_>>();
            // 存在 MCP server 时注入交互桥接通道，使服务端 elicitation 可送达交互面。
            profiled = profiled.with_interaction_channel(elicit_tx.clone());
            match profiled.with_mcp_server_toolsets(&selected).await {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "MCP 能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        if !context.turn_options.a2a_peers.is_empty()
            && !matches!(
                context.turn_options.capability_profile,
                CapabilityProfile::Plan
            )
        {
            let selected = match load_a2a_peers(
                &context.storage,
                context.turn_options.a2a_peers.iter().cloned().collect(),
            )
            .await
            {
                Ok(selected) => selected,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "A2A 能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let identity_refs = selected
                .iter()
                .filter_map(|peer| {
                    peer.identity_ref
                        .as_ref()
                        .map(|identity_ref| (peer.snapshot.peer_id.clone(), identity_ref.clone()))
                })
                .collect();
            let peers = selected
                .into_iter()
                .map(|peer| peer.snapshot)
                .collect::<Vec<_>>();
            match profiled.with_a2a_delegate(
                peers,
                Arc::new(KeyringMtlsTransport::new(
                    Arc::new(KeyringA2aIdentityStore),
                    identity_refs,
                )),
            ) {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "A2A 能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        profiled = profiled.with_confirmation_channel(context.confirmation_tx.clone());
        if !matches!(
            context.turn_options.capability_profile,
            CapabilityProfile::Plan
        ) {
            if let Some(messenger) = session.session_messenger() {
                match profiled.with_session_messaging(messenger, session.session_id.clone()) {
                    Ok(agent) => profiled = agent,
                    Err(error) => {
                        let _ = emit_event(
                            &mut context,
                            AgentEvent::Error {
                                message: error.to_string(),
                            },
                        )
                        .await;
                        let _ = emit_event(
                            &mut context,
                            AgentEvent::Stopped {
                                reason: "会话消息能力初始化失败".into(),
                            },
                        )
                        .await;
                        active.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        }
        if context.turn_options.subagents_enabled
            && !matches!(
                context.turn_options.capability_profile,
                CapabilityProfile::Plan
            )
        {
            // 子代理的记忆上下文通过独立 recall 构造；绝不复用根 Agent 已注入
            // 的 System 消息、历史或工具观察。
            if context.turn_options.memory_recall {
                if let Some(store) = context.storage.as_ref() {
                    match recall_memories(
                        store,
                        &session.config.workspace.to_string_lossy(),
                        &context.turn_options.capability_profile.to_string(),
                    )
                    .await
                    {
                        Ok(memories) if !memories.is_empty() => {
                            let body = memories
                                .iter()
                                .map(|memory| memory.body.as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            profiled = profiled.with_subagent_memory(vec![Message::text(
                                Role::System,
                                format!("已确认的持久记忆：\n{body}"),
                            )]);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = emit_event(
                                &mut context,
                                AgentEvent::Error {
                                    message: format!("子代理持久记忆召回失败: {error}"),
                                },
                            )
                            .await;
                            let _ = emit_event(
                                &mut context,
                                AgentEvent::Stopped {
                                    reason: "子代理记忆初始化失败".into(),
                                },
                            )
                            .await;
                            active.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
            profiled = profiled
                .with_subagent_confirmation_channel(context.subagent_confirmation_tx.clone());
            match profiled.with_subagents(context.turn_options.max_subagents) {
                Ok(agent) => profiled = agent,
                Err(error) => {
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = emit_event(
                        &mut context,
                        AgentEvent::Stopped {
                            reason: "子代理能力初始化失败".into(),
                        },
                    )
                    .await;
                    active.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        Some(Arc::new(profiled))
    } else {
        None
    };
    if let Some(agent) = configured_agent {
        let mut agent_stream = agent.stream_messages_approved_with_system(
            model.clone(),
            messages.clone(),
            Some(prompt.system.clone()),
            approved,
        );
        loop {
            tokio::select! {
                // 取消是用户发出的终态命令；当 provider/agent 流也恰好就绪时，
                // 必须先持久化子代理的 cancelled 状态，不能让随后一个失败事件把
                // 根 Turn 错误地收口为 provider failure。
                biased;
                    changed = cancel_rx.changed() => {
                        if changed.is_ok() && *cancel_rx.borrow() {
                            cancel_subagent_tasks(&mut context).await;
                            let waiting_confirmation =
                                !context.confirmations.lock().await.is_empty();
                            context.confirmations.lock().await.clear();
                            active.store(false, Ordering::SeqCst);
                            let _ = emit_event(&mut context, AgentEvent::Stopped {
                                reason: if waiting_confirmation {
                                    "用户取消等待确认的 turn".into()
                                } else {
                                    "用户取消".into()
                                },
                            }).await;
                            break;
                        }
                }
                elicitation = elicit_rx.recv() => {
                    if let Some(elicitation) = elicitation {
                        // MCP server 请求补充信息：登记到共享注册表等待用户答复，并向交互面派发请求。
                        let request = elicitation_to_interaction(&elicitation);
                        context
                            .interactions
                            .lock()
                            .await
                            .insert(
                                elicitation.request_id.clone(),
                                PendingElicitationState {
                                    responder: elicitation.responder,
                                    schema: elicitation.requested_schema,
                                },
                            );
                        let _ = emit_event(&mut context, AgentEvent::InteractionRequired { request }).await;
                    }
                    // None → 桥接通道已关闭（无 MCP server 或 agent 结束），无需处理。
                }
                confirmation = context.confirmation_rx.recv() => {
                    if let Some(confirmation) = confirmation {
                        let session_grant_key = confirmation.session_grant_key.clone();
                        let details = confirmation.confirmation.clone();
                        context.confirmations.lock().await.insert(
                            details.request_id.clone(),
                            confirmation,
                        );
                        let session_scope_available = session_grant_key.is_some()
                            && context.storage.is_some();
                        let session_scope_category = session_scope_available
                            .then(|| session_grant_key.clone())
                            .flatten();
                        let turn_id = context.turn_id.clone();
                        let _ = emit_event(&mut context, AgentEvent::ConfirmationRequired {
                            request_id: details.request_id,
                            call_id: details.call_id.clone(),
                            turn_id,
                            step_id: details.call_id,
                            tool_name: details.tool_name,
                            arguments_summary: details.arguments_summary,
                            targets: details.targets,
                            program: details.program,
                            side_effects: details.side_effects,
                            capability_profile: details.capability_profile,
                            policy_profile: details.policy_profile,
                            constraints: details.constraints,
                            session_scope_available,
                            session_scope_category,
                        }).await;
                    }
                }
                pending_confirmation = context.subagent_confirmation_rx.recv() => {
                    if let Some(pending_confirmation) = pending_confirmation {
                        let session_scope_available = pending_confirmation.session_grant_key.is_some()
                            && context.storage.is_some();
                        let session_scope_category = session_scope_available
                            .then(|| pending_confirmation.session_grant_key.clone())
                            .flatten();
                        let confirmation = pending_confirmation.confirmation.clone();
                        let agent_id = pending_confirmation.agent_id.clone();
                        let task_id = pending_confirmation.task_id.clone();
                        // policy request_id 只由请求内容决定；并行子代理可能提出相同请求，
                        // 因此确认键按任务区分，避免后者覆盖前者的一次性答复通道。
                        let confirmation_key = format!("{}:{}", task_id.0, confirmation.request_id);
                        context
                            .subagent_confirmations
                            .lock()
                            .await
                            .insert(confirmation_key.clone(), pending_confirmation);
                        let turn_id = context.turn_id.clone();
                        let _ = emit_event(&mut context, AgentEvent::SubagentConfirmationRequired {
                            agent_id,
                            task_id,
                            request_id: confirmation_key,
                            call_id: confirmation.call_id.clone(),
                            turn_id,
                            step_id: confirmation.call_id,
                            tool_name: confirmation.tool_name,
                            arguments_summary: confirmation.arguments_summary,
                            targets: confirmation.targets,
                            program: confirmation.program,
                            side_effects: confirmation.side_effects,
                            capability_profile: confirmation.capability_profile,
                            policy_profile: confirmation.policy_profile,
                            constraints: confirmation.constraints,
                            session_scope_available,
                            session_scope_category,
                        }).await;
                    }
                }
                event = agent_stream.next() => match event {
                    Some(rivo_agent::AgentStreamEvent::Started { .. }) => {
                        let capability_profile = context.turn_options.capability_profile;
                        let policy_profile = context.turn_options.policy_profile;
                        let turn_id = context.turn_id.clone();
                        let _ = emit_event(&mut context, AgentEvent::Started {
                            provider: active_config.provider,
                            model: model.clone(),
                            selection: active_config.selection.clone(),
                            route: active_config.route.clone(),
                            turn_id,
                            capability_profile,
                            policy_profile,
                        }).await;
                        let _ = emit_event(&mut context, AgentEvent::PromptPrepared {
                            revision: prompt.revision.clone(),
                            instruction_hashes: prompt.instruction_sources.iter().map(|source| source.sha256.clone()).collect(),
                            instructions_truncated: prompt.instructions_truncated,
                        }).await;
                        emit_memory_events(&mut context).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::ModelDelta { text }) => { let _ = emit_event(&mut context, AgentEvent::ModelDelta { text }).await; }
                    Some(rivo_agent::AgentStreamEvent::ToolRequested { call_id, name, arguments }) => {
                        if name.starts_with("browser_") { context.browser_used = true; }
                        let _ = emit_event(&mut context, AgentEvent::ToolRequested { call_id, name, arguments }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::MediaStarted { call_id, operation }) => {
                        let _ = emit_event(&mut context, AgentEvent::MediaStarted { call_id, operation }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::MediaReady { call_id, result }) => {
                        let _ = emit_event(&mut context, AgentEvent::MediaReady { call_id, result }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::MediaFailed { call_id, operation, message }) => {
                        let _ = emit_event(&mut context, AgentEvent::MediaFailed { call_id, operation, message }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::ToolObservation { call_id, name, result }) => { let _ = emit_event(&mut context, AgentEvent::ToolObservation { call_id, name, result }).await; }
                    Some(rivo_agent::AgentStreamEvent::ConfirmationResolved { request_id, call_id, tool_name, allowed }) => {
                        let _ = emit_event(&mut context, AgentEvent::ConfirmationResolved { request_id, call_id, tool_name, allowed }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::BudgetUpdated { used_tokens, context_limit_tokens, steps, max_steps }) => { let _ = emit_event(&mut context, AgentEvent::BudgetUpdated { used_tokens, context_limit_tokens, steps, max_steps }).await; }
                    Some(rivo_agent::AgentStreamEvent::ContextCompressed { omitted_start, omitted_end }) => { let _ = emit_event(&mut context, AgentEvent::ContextCompressed { omitted_start, omitted_end }).await; }
                    Some(rivo_agent::AgentStreamEvent::SubagentQueued { agent_id, task_id, call_id, label }) => {
                        let _ = emit_event(&mut context, AgentEvent::SubagentQueued { agent_id, task_id, call_id, label }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::SubagentActivity { agent_id, task_id, activity }) => {
                        let _ = emit_event(&mut context, AgentEvent::SubagentActivity { agent_id, task_id, activity }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::SubagentStarted { agent_id, task_id }) => {
                        let _ = emit_event(&mut context, AgentEvent::SubagentStarted { agent_id, task_id }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::SubagentCompleted { result }) => {
                        let _ = emit_event(&mut context, AgentEvent::SubagentCompleted { result }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::ConfirmationRequired { confirmation, session_grant_key }) => {
                        let session_scope_available = session_grant_key.is_some() && context.storage.is_some();
                        let session_scope_category = session_scope_available.then(|| session_grant_key.clone()).flatten();
                        let pending_turn = PendingTurn {
                            messages: messages.clone(),
                            request_id: confirmation.request_id.clone(),
                            turn_id: context.turn_id.clone(),
                            session_grant_key,
                            options: context.turn_options.clone(),
                        };
                        if let Err(error) = runtime_lock(&pending, "等待确认状态")
                            .map(|mut pending| *pending = Some(pending_turn))
                        {
                            active.store(false, Ordering::SeqCst);
                            let _ = emit_event(
                                &mut context,
                                AgentEvent::Error {
                                    message: error.to_string(),
                                },
                            )
                            .await;
                            let _ = emit_event(
                                &mut context,
                                AgentEvent::Stopped {
                                    reason: "会话运行时状态失效".into(),
                                },
                            )
                            .await;
                            break;
                        }
                        active.store(false, Ordering::SeqCst);
                        let turn_id = context.turn_id.clone();
                        let _ = emit_event(&mut context, AgentEvent::ConfirmationRequired {
                            request_id: confirmation.request_id,
                            call_id: confirmation.call_id.clone(),
                            turn_id,
                            step_id: confirmation.call_id,
                            tool_name: confirmation.tool_name,
                            arguments_summary: confirmation.arguments_summary,
                            targets: confirmation.targets,
                            program: confirmation.program,
                            side_effects: confirmation.side_effects,
                            capability_profile: confirmation.capability_profile,
                            policy_profile: confirmation.policy_profile,
                            constraints: confirmation.constraints,
                            session_scope_available,
                            session_scope_category,
                        }).await;
                        break;
                    }
                    Some(rivo_agent::AgentStreamEvent::FinalAnswer { text, stop_reason, usage }) => {
                        if matches!(context.turn_options.capability_profile, CapabilityProfile::Plan) {
                            if let Some(mut plan) = extract_plan_draft(&text, &context.turn_id) {
                                plan.session_id = session.session_id.clone();
                                match session.create_task_plan(plan) {
                                    Ok(mut plan_events) => {
                                        while let Some(plan_event) = plan_events.next().await {
                                            let _ = emit_event(&mut context, plan_event).await;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = emit_event(
                                            &mut context,
                                            AgentEvent::Error {
                                                message: format!("结构化计划未保存: {error}"),
                                            },
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                        if !context.browser_used {
                            if let Err(error) = persist_message(&context, "assistant", &text).await {
                                active.store(false, Ordering::SeqCst);
                                let _ = emit_event(&mut context, AgentEvent::Error { message: format!("历史持久化失败: {error}") }).await;
                                let _ = emit_event(&mut context, AgentEvent::Stopped { reason: "历史持久化失败".into() }).await;
                                break;
                            }
                        }
                        retain_final_answer(&history, context.browser_used, &text).await;
                        active.store(false, Ordering::SeqCst);
                        let _ = emit_event(&mut context, AgentEvent::FinalAnswer { text: text.clone(), stop_reason, usage }).await;
                        break;
                    }
                    Some(rivo_agent::AgentStreamEvent::Error { message }) => {
                        let step_id = format!("{}-retry", context.turn_id);
                        *context.retry.lock().await = Some(RetryTurn { step_id: step_id.clone(), messages: messages.clone(), options: context.turn_options.clone() });
                        let _ = emit_event(&mut context, AgentEvent::Failure {
                            message: message.clone(),
                            details: FailureDetails {
                                stage: FailureStage::Tool,
                                code: "agent_error".into(),
                                retryable: true,
                                recovery: "检查工具或模型错误后重试当前步骤".into(),
                                cause: message,
                            },
                        }).await;
                        let _ = emit_event(&mut context, AgentEvent::RetryAvailable { step_id }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::ProviderFailure { message }) => {
                        let step_id = format!("{}-retry", context.turn_id);
                        *context.retry.lock().await = Some(RetryTurn { step_id: step_id.clone(), messages: messages.clone(), options: context.turn_options.clone() });
                        let _ = emit_event(&mut context, AgentEvent::Failure {
                            message: message.clone(),
                            details: FailureDetails {
                                stage: FailureStage::Stream,
                                code: "provider_stream_failed".into(),
                                retryable: true,
                                recovery: "检查 provider 连接、请求路径和模型配置后重试当前步骤".into(),
                                cause: message,
                            },
                        }).await;
                        let _ = emit_event(&mut context, AgentEvent::RetryAvailable { step_id }).await;
                    }
                    Some(rivo_agent::AgentStreamEvent::Stopped { reason }) => { active.store(false, Ordering::SeqCst); let _ = emit_event(&mut context, AgentEvent::Stopped { reason }).await; break; }
                    None => break,
                }
            }
        }
        active.store(false, Ordering::SeqCst);
        return;
    }
    let capability_profile = context.turn_options.capability_profile;
    let policy_profile = context.turn_options.policy_profile;
    let turn_id = context.turn_id.clone();
    let _ = emit_event(
        &mut context,
        AgentEvent::Started {
            provider: active_config.provider,
            model: model.clone(),
            selection: active_config.selection.clone(),
            route: active_config.route.clone(),
            turn_id,
            capability_profile,
            policy_profile,
        },
    )
    .await;
    let _ = emit_event(
        &mut context,
        AgentEvent::PromptPrepared {
            revision: prompt.revision.clone(),
            instruction_hashes: prompt
                .instruction_sources
                .iter()
                .map(|source| source.sha256.clone())
                .collect(),
            instructions_truncated: prompt.instructions_truncated,
        },
    )
    .await;
    emit_memory_events(&mut context).await;
    let request = ProviderRequest {
        model,
        system: Some(prompt.system),
        messages: messages.clone(),
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: None,
        max_output_tokens: None,
        temperature: None,
        reasoning_effort: context.turn_options.reasoning_effort,
        metadata: Default::default(),
    };
    let mut provider_stream = active_llm.client.stream(request);
    let mut answer = String::new();
    let mut stop_reason = StopReason::Final;
    let mut usage = Usage::default();
    loop {
        tokio::select! {
            changed = cancel_rx.changed() => {
                if changed.is_ok() && *cancel_rx.borrow() {
                    active.store(false, Ordering::SeqCst);
                    let _ = emit_event(&mut context, AgentEvent::Stopped { reason: "用户取消".into() }).await;
                    break;
                }
            }
            event = provider_stream.next() => match event {
                Some(Ok(ProviderStreamEvent::TextDelta { delta })) => {
                    answer.push_str(&delta);
                    let _ = emit_event(&mut context, AgentEvent::ModelDelta { text: delta }).await;
                }
                Some(Ok(ProviderStreamEvent::Done { stop_reason: reason, usage: final_usage })) => {
                    stop_reason = reason;
                    usage = final_usage;
                }
                Some(Ok(ProviderStreamEvent::ToolArgumentsDelta { .. }))
                | Some(Ok(ProviderStreamEvent::BlockStarted { .. }))
                | Some(Ok(ProviderStreamEvent::BlockFinished { .. })) => {}
                Some(Err(error)) => {
                    let step_id = format!("{}-retry", context.turn_id);
                    *context.retry.lock().await = Some(RetryTurn { step_id: step_id.clone(), messages: messages.clone(), options: context.turn_options.clone() });
                    let message = error.to_string();
                    let _ = emit_event(&mut context, AgentEvent::Failure {
                        message: message.clone(),
                        details: FailureDetails {
                            stage: FailureStage::Stream,
                            code: "provider_stream_failed".into(),
                            retryable: true,
                            recovery: "检查 provider 连接后重试当前步骤".into(),
                            cause: message,
                        },
                    }).await;
                    let _ = emit_event(&mut context, AgentEvent::RetryAvailable { step_id }).await;
                    active.store(false, Ordering::SeqCst);
                    let _ = emit_event(&mut context, AgentEvent::Stopped { reason: "提供方失败".into() }).await;
                    break;
                }
                None => {
                    if let Err(error) = persist_message(&context, "assistant", &answer).await {
                        active.store(false, Ordering::SeqCst);
                        let _ = emit_event(&mut context, AgentEvent::Error { message: format!("历史持久化失败: {error}") }).await;
                        let _ = emit_event(&mut context, AgentEvent::Stopped { reason: "历史持久化失败".into() }).await;
                        break;
                    }
                    history
                        .lock()
                        .await
                        .push(Message::text(Role::Assistant, answer.clone()));
                    active.store(false, Ordering::SeqCst);
                    let _ = emit_event(&mut context, AgentEvent::FinalAnswer { text: answer.clone(), stop_reason, usage: usage.clone() }).await;
                    break;
                }
            }
        }
    }
    active.store(false, Ordering::SeqCst);
}

fn compose_system_prompt(
    config: &ResolvedConfig,
    options: &TurnOptions,
) -> Result<PromptEnvelope, SessionError> {
    let user_instruction = config
        .config_path
        .parent()
        .map(|directory| directory.join("AGENTS.md"));
    let current = std::env::current_dir().unwrap_or_else(|_| config.workspace.clone());
    let instruction_cwd = if current.starts_with(&config.workspace) {
        current
    } else {
        config.workspace.clone()
    };
    let instructions = load_persistent_instructions(
        user_instruction.as_deref(),
        &config.workspace,
        &instruction_cwd,
    )
    .map_err(|error| SessionError::InvalidConfig(format!("持久指令加载失败: {error}")))?;
    let mut toolsets = match options.capability_profile {
        CapabilityProfile::Coding => {
            vec!["fs.read".into(), "fs.write".into(), "process.verify".into()]
        }
        _ => vec!["fs.read".into()],
    };
    if options.network {
        toolsets.push("web.read".into());
    }
    toolsets.extend(
        options
            .mcp_servers
            .iter()
            .map(|server| format!("mcp:{server}")),
    );
    let mcp_servers = options.mcp_servers.clone();
    Ok(compose_prompt(PromptInput {
        response_language: &options.locale,
        mode: options.capability_profile,
        policy_profile: &options.policy_profile.to_string(),
        visible_toolsets: &toolsets,
        network_enabled: options.network,
        mcp_servers: &mcp_servers,
        memory_enabled: options.memory_recall,
        instructions: &instructions,
    }))
}

/// 对明确要求查库、核验或纠偏的任务，仅为本次 turn 自动选择已经配置且声明为
/// read_only 的 MCP toolset。它不持久化选择，也不会为一般性不确定问题打开外部数据源。
fn auto_enable_read_only_mcp(config: &ResolvedConfig, options: &mut TurnOptions, task: &str) {
    if !is_explicit_fact_verification_intent(task) {
        return;
    }
    for server in &config.mcp_servers {
        if !server.enabled || !server.allowed_capabilities.contains("tools") {
            continue;
        }
        let read_only_toolsets = server
            .toolsets
            .iter()
            .filter_map(|(name, toolset)| {
                matches!(toolset.risk, rivo_tools::mcp::McpToolRisk::ReadOnly)
                    .then_some(name.clone())
            })
            .collect::<BTreeSet<_>>();
        if read_only_toolsets.is_empty() {
            continue;
        }
        if !options.mcp_servers.contains(&server.name) {
            options.mcp_servers.push(server.name.clone());
        }
        options
            .mcp_toolsets
            .entry(server.name.clone())
            .or_default()
            .extend(read_only_toolsets);
    }
}

fn is_explicit_fact_verification_intent(task: &str) -> bool {
    let text = task.to_lowercase();
    [
        "查库",
        "连库",
        "数据库",
        "查数据源",
        "数据源查询",
        "核验",
        "纠偏",
        "补充事实",
        "query database",
        "verify against data",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn new_session_id() -> String {
    format!("session-{}", unique_suffix())
}

fn default_database_path() -> Result<PathBuf, SessionError> {
    let root = data_root_dir()
        .ok_or_else(|| SessionError::InvalidConfig("无法确定用户数据目录".into()))?;
    let directory = root.join("rivo");
    std::fs::create_dir_all(&directory).map_err(|error| SessionError::ConfigRead {
        path: directory.clone(),
        source: error,
    })?;
    Ok(directory.join("rivo.db"))
}

fn new_turn_id() -> String {
    format!("turn-{}", unique_suffix())
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn utc_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub fn config_path_for(home: &Path) -> PathBuf {
    home.join("rivo").join("config.toml")
}

pub fn redacted_config(config: &ResolvedConfig) -> BTreeMap<String, String> {
    let mut values = BTreeMap::from([
        ("provider".into(), config.provider.to_string()),
        ("model".into(), config.model.clone().unwrap_or_default()),
        ("base_url".into(), config.base_url.clone()),
        (
            "auth.credential".into(),
            config.auth.credential.clone().unwrap_or_default(),
        ),
        (
            "vendor".into(),
            config
                .selection
                .as_ref()
                .map(|s| s.vendor.clone())
                .unwrap_or_default(),
        ),
        (
            "provider_id".into(),
            config
                .route
                .as_ref()
                .map(|r| r.provider_id.clone())
                .unwrap_or_default(),
        ),
        (
            "provider_selection".into(),
            config
                .selection
                .as_ref()
                .map(|s| {
                    if s.provider.explicit_id().is_some() {
                        "explicit"
                    } else {
                        "auto"
                    }
                })
                .unwrap_or("unselected")
                .into(),
        ),
        ("profile".into(), config.capability_profile.to_string()),
        ("policy".into(), config.policy_profile.to_string()),
        ("workspace".into(), config.workspace.display().to_string()),
        ("browser.cdp_url".into(), config.browser.cdp_url.clone()),
    ]);
    values.insert(
        "providers".into(),
        config
            .routing_providers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    );
    values.insert(
        "mcp_servers".into(),
        config
            .mcp_servers
            .iter()
            .map(|server| {
                format!(
                    "{}{}",
                    server.name,
                    if server.enabled {
                        "(enabled)"
                    } else {
                        "(disabled)"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(","),
    );
    values.insert(
        "budget.max_steps".into(),
        config.budget.max_steps.to_string(),
    );
    values.insert(
        "budget.max_retries".into(),
        config.budget.max_retries.to_string(),
    );
    values.insert(
        "budget.max_duration_seconds".into(),
        config.budget.max_duration_seconds.to_string(),
    );
    values.insert(
        "budget.context_limit_tokens".into(),
        config.budget.context_limit_tokens.to_string(),
    );
    values.insert(
        "budget.max_observation_bytes".into(),
        config.budget.max_observation_bytes.to_string(),
    );
    values.insert(
        "logging.required".into(),
        config.logging.required.to_string(),
    );
    values.insert(
        "logging.retention_days".into(),
        config.logging.retention_days.to_string(),
    );
    values
}

#[cfg(test)]
mod tests {
    use super::interaction::elicitation_content;
    use super::*;
    use rivo_core::Message;
    use rivo_llm::{MockLlm, ProviderResponse};

    #[test]
    fn query_format_is_optional_strict_and_round_trips() {
        for source in ["version = 6", "version = 6\n[defaults]"] {
            let file: FileConfig = toml::from_str(source).unwrap();
            assert_eq!(file.defaults.query_format, None);
            assert!(!toml::to_string(&file).unwrap().contains("query_format"));
        }
        for (name, expected) in [
            ("text", QueryFormat::Text),
            ("table", QueryFormat::Table),
            ("json", QueryFormat::Json),
        ] {
            let source = format!("version = 6\n[defaults]\nquery_format = {name:?}");
            let file: FileConfig = toml::from_str(&source).unwrap();
            assert_eq!(file.defaults.query_format, Some(expected));
            let saved = toml::to_string(&file).unwrap();
            let restored: FileConfig = toml::from_str(&saved).unwrap();
            assert_eq!(restored.defaults.query_format, Some(expected));
            let resolved = ResolvedConfig::resolve_at(
                file,
                &BTreeMap::new(),
                ConfigOverrides::default(),
                PathBuf::from("config.toml"),
            )
            .unwrap();
            assert_eq!(resolved.query_format, Some(expected));
        }
        for invalid in ["", "jsonl", "JSON", "unknown"] {
            let source = format!("version = 6\n[defaults]\nquery_format = {invalid:?}");
            assert!(toml::from_str::<FileConfig>(&source).is_err());
        }
    }

    fn base_config() -> ResolvedConfig {
        ResolvedConfig::resolve_at(
            fixture_file(),
            &BTreeMap::new(),
            ConfigOverrides {
                workspace: Some(PathBuf::from(".")),
                capability_profile: Some(CapabilityProfile::Research),
                policy_profile: Some(PolicyProfile::Workspace),
                ..ConfigOverrides::default()
            },
            PathBuf::from("config.toml"),
        )
        .unwrap()
    }

    fn fixture_file() -> FileConfig {
        let mut file = FileConfig::default();
        for model in ["mock", "first-model", "second-model"] {
            file.models.insert(
                model.into(),
                ModelCatalogEntry {
                    vendor: "openai".into(),
                    display_name: model.into(),
                    supported_protocols: Vec::from([
                        ProviderKind::OpenAiChatCompletions,
                        ProviderKind::OpenAiResponses,
                    ]),
                    supported_reasoning_efforts: BTreeSet::new(),
                    enabled: true,
                    priority: 0,
                },
            );
        }
        file.providers.insert(
            "primary".into(),
            ProviderConfig {
                base_url: "https://example.invalid".into(),
                auth: AuthConfig::default(),
                enabled: true,
                priority: 0,
                discovery: ModelDiscoveryCache::default(),
            },
        );
        let fingerprint = provider_config_fingerprint(&file, "primary");
        file.providers.get_mut("primary").unwrap().discovery = ModelDiscoveryCache {
            config_fingerprint: fingerprint,
            supported: Some(true),
            models: vec!["mock".into(), "first-model".into(), "second-model".into()],
            last_checked_at: Some(1),
            last_error: None,
        };
        file.defaults.vendor = Some("openai".into());
        file.defaults.model = Some("mock".into());
        file
    }

    #[test]
    fn media_capability_is_exactly_bound_to_verified_route_identity() {
        let mut config = base_config();
        config.route = Some(ResolvedRoute {
            vendor: "openai".into(),
            model: "gpt-6-astra".into(),
            provider_id: "openai-responses".into(),
            protocol: ProviderKind::OpenAiResponses,
            base_url: "https://api.openai.com/v1".into(),
            priority: 0,
        });
        assert!(media_capability_for_route(&config).generate);
        config.route.as_mut().unwrap().model = "gpt-5.5".into();
        let capability = media_capability_for_route(&config);
        assert!(capability.input);
        assert!(!capability.generate);
        assert!(!capability.edit);
        config.route.as_mut().unwrap().vendor = "openai".into();
        config.route.as_mut().unwrap().base_url = "https://gateway.example/v1".into();
        assert_eq!(
            media_capability_for_route(&config),
            MediaCapability::default()
        );
    }

    #[test]
    fn media_index_ttl_cleanup_is_session_scoped_and_removes_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = media_root_at(directory.path(), "session-1");
        std::fs::create_dir_all(&root).unwrap();
        let valid_path = root.join("asset-valid.png");
        let expired_path = root.join("asset-expired.png");
        std::fs::write(&valid_path, b"valid").unwrap();
        std::fs::write(&expired_path, b"expired").unwrap();
        let now = utc_timestamp_ms();
        let reference = |asset_id: &str| MediaRef {
            asset_id: asset_id.into(),
            session_id: "session-1".into(),
            media_type: "image/png".into(),
            width: None,
            height: None,
        };
        let mut index = MediaAssetIndex {
            assets: BTreeMap::from([
                (
                    "asset-valid".into(),
                    MediaAssetRecord {
                        asset: reference("asset-valid"),
                        relative_path: "asset-valid.png".into(),
                        created_at_ms: now,
                        expires_at_ms: now.saturating_add(60_000),
                    },
                ),
                (
                    "asset-expired".into(),
                    MediaAssetRecord {
                        asset: reference("asset-expired"),
                        relative_path: "asset-expired.png".into(),
                        created_at_ms: now.saturating_sub(60_000),
                        expires_at_ms: now.saturating_sub(1),
                    },
                ),
                (
                    "asset-missing".into(),
                    MediaAssetRecord {
                        asset: reference("asset-missing"),
                        relative_path: "asset-missing.png".into(),
                        created_at_ms: now,
                        expires_at_ms: now.saturating_add(60_000),
                    },
                ),
            ]),
        };
        assert!(prune_media_index_at(&root, "session-1", &mut index).unwrap());
        assert!(index.assets.contains_key("asset-valid"));
        assert!(!index.assets.contains_key("asset-expired"));
        assert!(!index.assets.contains_key("asset-missing"));
        assert!(valid_path.is_file());
        assert!(!expired_path.exists());
    }

    #[test]
    fn image_magic_validation_rejects_text_and_reads_png_dimensions() {
        assert!(detect_image_metadata(b"not an image").is_none());
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&640u32.to_be_bytes());
        png[20..24].copy_from_slice(&480u32.to_be_bytes());
        assert_eq!(detect_image_metadata(&png), Some(("image/png", 640, 480)));
    }

    #[test]
    fn deleting_one_session_media_directory_does_not_delete_another() {
        let directory = tempfile::tempdir().unwrap();
        let first = media_root_at(directory.path(), "session-1");
        let second = media_root_at(directory.path(), "session-2");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("asset.png"), b"first").unwrap();
        std::fs::write(second.join("asset.png"), b"second").unwrap();
        remove_media_assets_at(directory.path(), "session-1");
        assert!(!first.exists());
        assert!(second.join("asset.png").is_file());
    }

    fn selection(model: &str) -> ModelSelection {
        ModelSelection {
            vendor: "openai".into(),
            model: model.into(),
            provider: ProviderSelection::Auto,
        }
    }

    #[test]
    fn newly_allocated_session_identity_preserves_auto_selection_and_route() {
        let mut config = base_config();
        config.selection = Some(selection("mock"));
        let store = rivo_storage::Store::open_memory().unwrap();
        persist_session_identity(&store, "app-server-start", &config).unwrap();
        assert!(store.has_session("app-server-start").unwrap());
        assert_eq!(
            store
                .model_route("app-server-start")
                .unwrap()
                .unwrap()
                .selection
                .provider,
            ProviderSelection::Auto
        );
        assert_eq!(
            store.list_sessions().unwrap()[0].workspace_root.as_deref(),
            Some(".")
        );
    }

    #[test]
    fn active_catalog_model_is_valid_but_unknown_model_is_rejected() {
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()));
        session.validate_model_selection("mock").unwrap();
        assert!(matches!(
            session.validate_model_selection("different-model"),
            Err(SessionError::InvalidConfig(_))
        ));
    }

    #[test]
    fn selecting_catalog_model_does_not_mutate_source() {
        let config = base_config();
        let selected = config
            .with_selection_from_file(&fixture_file(), selection("second-model"))
            .unwrap();
        assert_eq!(selected.model.as_deref(), Some("second-model"));
        assert_eq!(config.model.as_deref(), Some("mock"));
        let bad = ModelSelection {
            provider: ProviderSelection::Explicit {
                provider_id: "missing".into(),
            },
            ..selection("mock")
        };
        assert!(config
            .with_selection_from_file(&fixture_file(), bad)
            .is_err());
    }

    #[test]
    fn one_shot_turn_rejects_missing_active_provider_or_model_before_streaming() {
        let no_provider = Session::with_client(base_config(), Arc::new(MockLlm::default()));
        *no_provider
            .active_llm
            .lock()
            .expect("session LLM lock poisoned") = None;
        assert!(matches!(
            no_provider.legacy_handle(Intent::SubmitTask {
                text: "fixture".into()
            }),
            Err(SessionError::MissingRoute)
        ));

        let mut config = base_config();
        config.model = None;
        let no_model = Session::with_client(config, Arc::new(MockLlm::default()));
        assert!(matches!(
            no_model.legacy_handle(Intent::SubmitTask {
                text: "fixture".into()
            }),
            Err(SessionError::MissingModel)
        ));
    }

    #[test]
    fn mode_registry_hides_coding_tools_outside_coding_mode() {
        let config = base_config();
        let plan = build_agent_for_turn(
            &config,
            Arc::new(MockLlm::default()),
            PolicyProfile::Workspace,
            CapabilityProfile::Plan,
            None,
            "",
        )
        .unwrap();
        let coding = build_agent_for_turn(
            &config,
            Arc::new(MockLlm::default()),
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
            None,
            "",
        )
        .unwrap();
        assert_eq!(plan.capability_profile(), CapabilityProfile::Plan);
        assert!(!plan.tool_catalog().descriptors.contains_key("apply_patch"));
        assert!(!plan.tool_catalog().descriptors.contains_key("run_process"));
        assert!(!plan
            .tool_catalog()
            .descriptors
            .contains_key("generate_image"));
        assert!(!plan.tool_catalog().descriptors.contains_key("edit_image"));
        assert!(!plan
            .tool_catalog()
            .descriptors
            .contains_key("delegate_task"));
        assert!(!plan
            .tool_catalog()
            .descriptors
            .keys()
            .any(|name| name.starts_with("browser_") || name == "computer"));
        assert!(coding
            .tool_catalog()
            .descriptors
            .contains_key("apply_patch"));
        assert!(coding
            .tool_catalog()
            .descriptors
            .contains_key("run_process"));
    }

    #[test]
    fn plan_capability_defaults_to_strict_policy_without_an_explicit_ceiling() {
        let mut file = fixture_file();
        file.defaults.capability = Some(CapabilityProfile::Plan);
        file.defaults.policy = None;
        let resolved =
            ResolvedConfig::resolve(file, &BTreeMap::new(), ConfigOverrides::default()).unwrap();
        assert_eq!(resolved.capability_profile, CapabilityProfile::Plan);
        assert_eq!(resolved.policy_profile, PolicyProfile::Strict);
    }

    #[test]
    fn cli_model_selection_overrides_environment() {
        let file = fixture_file();
        let environment = BTreeMap::from([("RIVO_MODEL".into(), "env-model".into())]);
        let resolved = ResolvedConfig::resolve(
            file,
            &environment,
            ConfigOverrides {
                model: Some("second-model".into()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.provider, ProviderKind::OpenAiChatCompletions);
        assert_eq!(resolved.model.as_deref(), Some("second-model"));
    }

    #[test]
    fn schema4_rejects_unsafe_providers_and_invalid_runtime_limits() {
        fn configured_file() -> FileConfig {
            let mut file = fixture_file();
            file.defaults.vendor = None;
            file.defaults.model = None;
            file
        }
        let resolve = |file| {
            ResolvedConfig::resolve_at(
                file,
                &BTreeMap::new(),
                ConfigOverrides::default(),
                PathBuf::from("fixture.toml"),
            )
        };
        assert!(resolve(configured_file()).is_ok());

        let mut userinfo = configured_file();
        userinfo.providers.get_mut("primary").unwrap().base_url =
            "https://user:secret@gateway.example/v1".into();
        assert!(
            matches!(resolve(userinfo), Err(SessionError::InvalidConfig(message)) if message.contains("base_url"))
        );

        let invalid_environment = ResolvedConfig::resolve_at(
            configured_file(),
            &BTreeMap::new(),
            ConfigOverrides {
                api_key_env: Some("BAD-NAME".into()),
                ..ConfigOverrides::default()
            },
            PathBuf::from("fixture.toml"),
        );
        assert!(
            matches!(invalid_environment, Err(SessionError::InvalidConfig(message)) if message.contains("--api-key-env"))
        );

        let mut invalid_credential = configured_file();
        invalid_credential
            .providers
            .get_mut("primary")
            .unwrap()
            .auth
            .credential = Some("invalid credential".into());
        assert!(
            matches!(resolve(invalid_credential), Err(SessionError::InvalidConfig(message)) if message.contains("credential_name"))
        );

        let mut invalid_browser = configured_file();
        invalid_browser.browser.cdp_url = "http://example.com:9222".into();
        assert!(
            matches!(resolve(invalid_browser), Err(SessionError::InvalidConfig(message)) if message.contains("CDP"))
        );

        let mut invalid_budget = configured_file();
        invalid_budget.defaults.budget.max_steps = 0;
        assert!(
            matches!(resolve(invalid_budget), Err(SessionError::InvalidConfig(message)) if message.contains("budget"))
        );

        let mut invalid_retention = configured_file();
        invalid_retention.defaults.logging.retention_days = 0;
        assert!(matches!(
            resolve(invalid_retention),
            Err(SessionError::InvalidConfig(_))
        ));

        let mut unnamed_mcp = configured_file();
        unnamed_mcp.mcp.servers.push(McpServerConfig {
            name: " ".into(),
            enabled: true,
            transport: rivo_tools::mcp::McpTransport::Stdio {
                program: "fixture-mcp".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: BTreeMap::new(),
            allow_sampling: false,
        });
        assert!(
            matches!(resolve(unnamed_mcp), Err(SessionError::InvalidConfig(message)) if message.contains("name"))
        );

        let mut unbounded_mcp = configured_file();
        unbounded_mcp.mcp.servers.push(McpServerConfig {
            name: "fixture".into(),
            enabled: true,
            transport: rivo_tools::mcp::McpTransport::Stdio {
                program: "fixture-mcp".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            allowed_capabilities: BTreeSet::new(),
            toolsets: BTreeMap::new(),
            allow_sampling: false,
        });
        assert!(
            matches!(resolve(unbounded_mcp), Err(SessionError::InvalidConfig(message)) if message.contains("allowed_capabilities"))
        );
    }

    #[test]
    fn current_schema_keeps_non_model_turn_option_defaults() {
        assert!(FileConfig::default().defaults.memory_recall);
        assert!(FileConfig::default().defaults.subagents_enabled);
        assert_eq!(FileConfig::default().defaults.max_subagents, 5);
        let file: FileConfig = toml::from_str(
            r#"
version = 6

[defaults]
vendor = "openai"
"#,
        )
        .unwrap();
        assert!(!file.defaults.network);
        assert!(!file.defaults.browser);
        assert!(file.defaults.mcp_servers.is_empty());
        assert!(file.defaults.memory_recall);
        assert!(!file.defaults.trace);
        assert!(file.defaults.subagents_enabled);
        assert_eq!(file.defaults.max_subagents, 5);
    }

    #[test]
    fn provider_auth_reference_and_default_selection_are_preserved() {
        let mut file = fixture_file();
        file.providers.get_mut("primary").unwrap().auth = AuthConfig {
            credential: Some("fixture-reference".into()),
        };
        let fingerprint = provider_config_fingerprint(&file, "primary");
        file.providers
            .get_mut("primary")
            .unwrap()
            .discovery
            .config_fingerprint = fingerprint;
        file.defaults.provider_id = Some("primary".into());
        let resolved =
            ResolvedConfig::resolve(file.clone(), &BTreeMap::new(), ConfigOverrides::default())
                .unwrap();
        assert_eq!(
            resolved.auth.credential.as_deref(),
            Some("fixture-reference")
        );
        assert_eq!(
            resolved.selection.unwrap().provider.explicit_id(),
            Some("primary")
        );
        let auto = ResolvedConfig::resolve(
            file,
            &BTreeMap::new(),
            ConfigOverrides {
                auto_provider: true,
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(auto.selection.unwrap().provider, ProviderSelection::Auto);
    }

    #[test]
    fn config_file_lifecycle_rejects_old_documents_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.toml");
        let missing_config = read_file_config(&missing).unwrap();
        assert_eq!(missing_config.version, CONFIG_VERSION);
        assert!(missing_config.providers.is_empty());
        assert!(!missing.exists());
        for content in [
            "",
            "version = 2",
            "version = 99",
            "version = 3\n[profiles]",
            "version = 3\n[endpoints]",
            "version = 3\n[bindings]",
            "version = 3\n[defaults]\nprofile = 'old'",
            "version = 3\nsecret = 'do-not-print-this'",
        ] {
            std::fs::write(&missing, content).unwrap();
            let error = read_file_config(&missing).unwrap_err();
            assert!(!error.to_string().contains("do-not-print-this"));
            assert_eq!(std::fs::read_to_string(&missing).unwrap(), content);
        }
    }

    #[test]
    fn provider_fingerprint_changes_with_identity_auth_or_address() {
        let config = fixture_file();
        let p = config.providers["primary"].clone();
        let first = provider_connection_fingerprint("primary", &p);
        assert_ne!(first, provider_connection_fingerprint("other", &p));
        for changed in [
            ProviderConfig {
                auth: AuthConfig {
                    credential: Some("KEY_B".into()),
                },
                ..p.clone()
            },
            ProviderConfig {
                base_url: "https://other.example/v1".into(),
                ..p
            },
        ] {
            assert_ne!(first, provider_connection_fingerprint("primary", &changed));
        }
    }

    #[test]
    fn known_api_keys_collects_active_and_all_providers() {
        std::env::set_var("RIVO_TEST_BASE_KEY", "base-secret-123456");
        std::env::set_var("RIVO_TEST_ALT_KEY", "alt-secret-abcdef");
        let mut config = base_config();
        config.api_key_env = Some("RIVO_TEST_BASE_KEY".into());
        let keys = config.known_api_keys();
        assert!(keys.iter().any(|key| key == "base-secret-123456"));
        std::env::remove_var("RIVO_TEST_BASE_KEY");
        std::env::remove_var("RIVO_TEST_ALT_KEY");
    }

    #[test]
    fn workspace_policy_is_the_default() {
        let resolved = ResolvedConfig::resolve(
            FileConfig::default(),
            &BTreeMap::new(),
            ConfigOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.policy_profile, PolicyProfile::Workspace);
    }

    #[test]
    fn invalid_environment_values_fail_explicitly() {
        let environment = BTreeMap::from([("RIVO_PROFILE".into(), "not-a-mode".into())]);
        let error = ResolvedConfig::resolve(
            FileConfig::default(),
            &environment,
            ConfigOverrides::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, SessionError::InvalidEnvironment { ref key, .. } if key == "RIVO_PROFILE")
        );
    }

    #[test]
    fn empty_model_and_invalid_api_key_environment_are_rejected() {
        let model_error = ResolvedConfig::resolve(
            fixture_file(),
            &BTreeMap::new(),
            ConfigOverrides {
                model: Some("  ".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(model_error, SessionError::InvalidConfig(_)));
        let env_error = ResolvedConfig::resolve(
            fixture_file(),
            &BTreeMap::new(),
            ConfigOverrides {
                api_key_env: Some("bad-name".into()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap_err();
        assert!(env_error.to_string().contains("--api-key-env"));
    }

    #[test]
    fn browser_cdp_endpoint_must_be_loopback_and_can_be_overridden() {
        let invalid = ResolvedConfig::resolve(
            FileConfig {
                browser: BrowserConfig {
                    cdp_url: "http://example.com:9222".into(),
                    ..BrowserConfig::default()
                },
                ..FileConfig::default()
            },
            &BTreeMap::new(),
            ConfigOverrides::default(),
        )
        .unwrap_err();
        assert!(invalid.to_string().contains("loopback"));

        let resolved = ResolvedConfig::resolve(
            FileConfig::default(),
            &BTreeMap::new(),
            ConfigOverrides {
                browser_cdp_url: Some("http://localhost:9333".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.browser.cdp_url, "http://localhost:9333");
    }

    #[test]
    fn browser_extension_requires_instance_and_accepts_environment_identity() {
        let mut file = FileConfig::default();
        file.browser.extension_pipe = Some(r"\\.\pipe\rivo-browser-extension-edge".into());
        let missing =
            ResolvedConfig::resolve(file.clone(), &BTreeMap::new(), ConfigOverrides::default())
                .unwrap_err();
        assert!(missing.to_string().contains("extension_instance"));

        let instance = rivo_tools::browser_extension::BrowserInstanceRef {
            browser_family: "edge".into(),
            extension_id: "edge-extension".into(),
            installation_id: "profile-install".into(),
            profile_key: "work".into(),
            boot_id: "boot-1".into(),
        };
        let mut environment = BTreeMap::new();
        environment.insert(
            "RIVO_BROWSER_EXTENSION_INSTANCE".into(),
            serde_json::to_string(&instance).unwrap(),
        );
        let resolved =
            ResolvedConfig::resolve(file, &environment, ConfigOverrides::default()).unwrap();
        assert_eq!(resolved.browser.extension_instance, Some(instance));
    }

    #[tokio::test]
    async fn browser_final_answer_is_not_retained_for_a_later_turn() {
        let history = Arc::new(tokio::sync::Mutex::new(vec![Message::text(
            Role::User,
            "open page",
        )]));
        retain_final_answer(&history, true, "private page detail").await;
        assert_eq!(history.lock().await.len(), 1);

        retain_final_answer(&history, false, "ordinary response").await;
        let history = history.lock().await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].text_content(), "ordinary response");
    }

    #[test]
    fn unknown_config_fields_fail_validation() {
        for content in [
            "version = 6\nunknown = true",
            "version = 6\n[supervision]\nenabled = false",
            "version = 6\n[supervision]\nenabled = true",
        ] {
            let error = toml::from_str::<FileConfig>(content).unwrap_err();
            assert!(error.to_string().contains("unknown field"));
            assert!(matches!(
                parse_file_config(content),
                Err(SessionError::InvalidConfig(message)) if message.contains("schema 6")
            ));
        }
    }

    #[test]
    fn mcp_servers_are_resolved_from_the_allowlist() {
        let file = FileConfig {
            mcp: McpFileConfig {
                servers: vec![McpServerConfig {
                    name: "docs".into(),
                    enabled: true,
                    transport: rivo_tools::mcp::McpTransport::Stdio {
                        program: "mcp-docs".into(),
                        args: vec!["--stdio".into()],
                        cwd: None,
                        env: Default::default(),
                    },
                    allowed_capabilities: ["tools".into(), "resources".into()]
                        .into_iter()
                        .collect(),
                    toolsets: Default::default(),
                    allow_sampling: false,
                }],
            },
            ..FileConfig::default()
        };
        let resolved =
            ResolvedConfig::resolve(file, &BTreeMap::new(), ConfigOverrides::default()).unwrap();
        assert_eq!(resolved.mcp_servers[0].name, "docs");
        assert!(resolved.mcp_servers[0]
            .allowed_capabilities
            .contains("resources"));

        let invalid = FileConfig {
            mcp: McpFileConfig {
                servers: vec![McpServerConfig {
                    name: "bad".into(),
                    enabled: true,
                    transport: rivo_tools::mcp::McpTransport::Stdio {
                        program: "mcp-bad".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                    },
                    allowed_capabilities: ["secrets".into()].into_iter().collect(),
                    toolsets: Default::default(),
                    allow_sampling: false,
                }],
            },
            ..FileConfig::default()
        };
        assert!(
            ResolvedConfig::resolve(invalid, &BTreeMap::new(), ConfigOverrides::default()).is_err()
        );
    }

    #[test]
    fn explicit_fact_verification_only_auto_selects_read_only_mcp_toolsets() {
        let mut config = base_config();
        config.mcp_servers = vec![rivo_tools::mcp::McpServerConfig {
            name: "data".into(),
            enabled: true,
            transport: rivo_tools::mcp::McpTransport::Stdio {
                program: "fixture".into(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: [
                (
                    "query".into(),
                    rivo_tools::mcp::McpToolsetConfig {
                        tools: ["query_data".into()].into_iter().collect(),
                        risk: rivo_tools::mcp::McpToolRisk::ReadOnly,
                    },
                ),
                (
                    "mutate".into(),
                    rivo_tools::mcp::McpToolsetConfig {
                        tools: ["write_data".into()].into_iter().collect(),
                        risk: rivo_tools::mcp::McpToolRisk::Action,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            allow_sampling: false,
        }];

        let mut options = TurnOptions::default();
        auto_enable_read_only_mcp(&config, &mut options, "帮我连库核验这条记录");
        assert_eq!(options.mcp_servers, vec!["data"]);
        assert_eq!(
            options.mcp_toolsets["data"],
            BTreeSet::from(["query".into()])
        );

        let mut ordinary_options = TurnOptions::default();
        auto_enable_read_only_mcp(&config, &mut ordinary_options, "解释这段代码");
        assert!(ordinary_options.mcp_servers.is_empty());
    }

    #[test]
    fn base_url_userinfo_is_rejected() {
        let mut file = fixture_file();
        file.providers.get_mut("primary").unwrap().base_url =
            "https://user:password@example.invalid/v1".into();
        let error = ResolvedConfig::resolve(file, &BTreeMap::new(), ConfigOverrides::default())
            .unwrap_err();
        assert!(error.to_string().contains("base_url"));
    }

    #[test]
    fn prompt_composition_and_mcp_selection_keep_runtime_choices_bounded() {
        let mut config = base_config();
        config.mcp_servers = vec![rivo_tools::mcp::McpServerConfig {
            name: "docs".into(),
            enabled: true,
            transport: rivo_tools::mcp::McpTransport::Stdio {
                program: "fixture".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: [(
                "read".into(),
                rivo_tools::mcp::McpToolsetConfig {
                    tools: ["search".into()].into_iter().collect(),
                    risk: rivo_tools::mcp::McpToolRisk::ReadOnly,
                },
            )]
            .into_iter()
            .collect(),
            allow_sampling: false,
        }];
        let session = Session::with_client(config.clone(), Arc::new(MockLlm::default()));
        let options = TurnOptions {
            capability_profile: CapabilityProfile::Coding,
            policy_profile: PolicyProfile::Workspace,
            network: true,
            mcp_servers: vec!["docs".into()],
            mcp_toolsets: [("docs".into(), BTreeSet::from(["read".into()]))]
                .into_iter()
                .collect(),
            ..TurnOptions::default()
        };
        let prompt = compose_system_prompt(&config, &options).unwrap();
        assert!(prompt.system.contains("fs.write"));
        assert!(prompt.system.contains("web.read"));
        assert!(prompt.system.contains("mcp:docs"));
        session.validate_mcp_selection(&options).unwrap();

        let mut unknown_toolset = options.clone();
        unknown_toolset
            .mcp_toolsets
            .insert("docs".into(), BTreeSet::from(["unknown".into()]));
        assert!(session.validate_mcp_selection(&unknown_toolset).is_err());
        let detached_toolset = TurnOptions {
            mcp_toolsets: [("docs".into(), BTreeSet::from(["read".into()]))]
                .into_iter()
                .collect(),
            ..TurnOptions::default()
        };
        assert!(session.validate_mcp_selection(&detached_toolset).is_err());
    }

    #[test]
    fn turn_options_only_accept_locally_trusted_a2a_peers() {
        let store = rivo_storage::Store::open_memory().unwrap();
        store
            .save_a2a_peer(&rivo_storage::StoredA2aPeer {
                snapshot: rivo_a2a::A2aTrustSnapshot {
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
                    status: rivo_a2a::A2aTrustState::Trusted,
                    approved_at: "2026-07-31T00:00:00Z".into(),
                },
                identity_ref: Some("a2a-peer-1".into()),
            })
            .unwrap();
        store
            .save_session_permission_grant(&SessionPermissionGrant {
                session_id: "async-frontend-session".into(),
                workspace: ".".into(),
                policy: PolicyProfile::Medium.to_string(),
                grant_key: "workspace_file_mutation".into(),
            })
            .unwrap();
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()))
            .with_storage(store, "a2a-selection");
        let options = TurnOptions {
            a2a_peers: BTreeSet::from(["peer-1".into()]),
            ..TurnOptions::default()
        };
        session.validate_a2a_selection(&options).unwrap();
        let unknown = TurnOptions {
            a2a_peers: BTreeSet::from(["missing".into()]),
            ..TurnOptions::default()
        };
        assert!(session.validate_a2a_selection(&unknown).is_err());
        session
            .storage
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .revoke_a2a_peer(&A2aPeerId("peer-1".into()))
            .unwrap();
        assert!(session.validate_a2a_selection(&options).is_err());
    }

    #[tokio::test]
    async fn selected_a2a_peer_registers_mtls_transport_before_model_execution() {
        let store = rivo_storage::Store::open_memory().unwrap();
        store
            .save_a2a_peer(&rivo_storage::StoredA2aPeer {
                snapshot: rivo_a2a::A2aTrustSnapshot {
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
                    status: rivo_a2a::A2aTrustState::Trusted,
                    approved_at: "2026-07-31T00:00:00Z".into(),
                },
                identity_ref: Some("a2a-peer-1".into()),
            })
            .unwrap();
        let session = Arc::new(
            Session::with_client(base_config(), Arc::new(MockLlm::default()))
                .with_storage(store, "a2a-transport"),
        );
        let (sender, mut events) = session.channel();
        sender
            .send(Intent::SetTurnOptions {
                options: TurnOptions {
                    a2a_peers: BTreeSet::from(["peer-1".into()]),
                    ..TurnOptions::default()
                },
            })
            .await
            .unwrap();
        sender
            .send(Intent::SubmitTask {
                text: "delegate research".into(),
            })
            .await
            .unwrap();
        let mut saw_legacy_transport_error = false;
        let mut saw_provider_attempt = false;
        while let Some(event) = events.next().await {
            match event.event {
                AgentEvent::Error { message } => {
                    saw_legacy_transport_error |= message.contains("A2A transport 尚未初始化");
                }
                AgentEvent::Stopped { reason } => {
                    saw_provider_attempt = reason == "提供方失败";
                    break;
                }
                _ => {}
            }
        }
        assert!(!saw_legacy_transport_error);
        assert!(saw_provider_attempt);
    }

    #[test]
    fn model_selection_validates_catalog_and_does_not_write_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let file = fixture_file();
        let original = toml::to_string(&file).unwrap();
        std::fs::write(&config_path, &original).unwrap();
        let config = ResolvedConfig::resolve_at(
            file,
            &BTreeMap::new(),
            ConfigOverrides::default(),
            config_path.clone(),
        )
        .unwrap();
        let session = Session::with_client(config, Arc::new(MockLlm::default()));
        session.validate_model_selection("first-model").unwrap();
        session.validate_model_selection("second-model").unwrap();
        assert!(matches!(
            session.validate_model_selection("unknown-model"),
            Err(SessionError::InvalidConfig(_))
        ));
        let selected = session.select_model(&selection("second-model")).unwrap();
        assert_eq!(selected.config.model.as_deref(), Some("second-model"));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn bidirectional_channel_switches_model_and_rejects_unknown_mcp() {
        let config = base_config();
        let session = Arc::new(Session::with_client(config, Arc::new(MockLlm::default())));
        let (sender, mut events) = session.channel();
        sender
            .send(Intent::SelectModel {
                selection: ModelSelection {
                    provider: ProviderSelection::Explicit {
                        provider_id: "primary".into(),
                    },
                    ..selection("first-model")
                },
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await.map(|event| event.event),
            Some(AgentEvent::ModelSelected { ref selection, ref route })
                if selection.provider.explicit_id() == Some("primary") && route.model == "first-model"
        ));
        sender
            .send(Intent::SelectModel {
                selection: selection("second-model"),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await.map(|event| event.event),
            Some(AgentEvent::ModelSelected { ref selection, ref route })
                if selection.provider == ProviderSelection::Auto && route.model == "second-model"
        ));
        sender
            .send(Intent::SetTurnOptions {
                options: TurnOptions {
                    mcp_servers: vec!["unknown".into()],
                    ..TurnOptions::default()
                },
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await.map(|event| event.event),
            Some(AgentEvent::Error { ref message }) if message.contains("未配置或未启用")
        ));
        drop(sender);
    }

    #[tokio::test]
    async fn strict_shell_run_stops_before_provider_or_tool_execution() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        let agent = rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([])),
            rivo_agent::ToolRegistry::default(),
            rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap(),
            PolicyProfile::Medium,
            CapabilityProfile::General,
        );
        let session = Arc::new(Session::with_agent(config, Arc::new(agent)));
        let (sender, mut events) = session.legacy_channel();
        sender
            .send(Intent::SetTurnOptions {
                options: TurnOptions {
                    capability_profile: CapabilityProfile::Coding,
                    policy_profile: PolicyProfile::Strict,
                    shell: true,
                    ..TurnOptions::default()
                },
            })
            .await
            .unwrap();
        sender
            .send(Intent::SubmitTask {
                text: "this must not reach the provider".into(),
            })
            .await
            .unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let terminal = matches!(event, AgentEvent::Stopped { .. });
                observed.push(event);
                if terminal {
                    break;
                }
            }
            observed
        })
        .await
        .expect("strict shell turn did not terminate");
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AgentEvent::Error { message } if message.contains("strict")
            )),
            "unexpected events: {observed:?}"
        );
        assert!(observed.iter().any(|event| matches!(
            event,
            AgentEvent::Stopped { reason } if reason == "Shell 能力初始化失败"
        )));
        assert!(!observed.iter().any(|event| matches!(
            event,
            AgentEvent::Started { .. } | AgentEvent::ModelDelta { .. }
        )));
    }

    #[tokio::test]
    async fn legacy_session_api_rejects_bidirectional_intents_and_preserves_stream_failure_order() {
        struct FailingStream;
        #[async_trait::async_trait]
        impl LlmClient for FailingStream {
            async fn complete(
                &self,
                _request: ProviderRequest,
            ) -> Result<rivo_llm::ProviderResponse, rivo_llm::ProviderError> {
                unreachable!("legacy streaming test never calls complete")
            }

            fn stream(&self, _request: ProviderRequest) -> rivo_llm::ProviderStream {
                Box::pin(async_stream::stream! {
                    yield Ok(ProviderStreamEvent::BlockStarted { index: 0 });
                    yield Ok(ProviderStreamEvent::TextDelta { delta: "partial".into() });
                    yield Ok(ProviderStreamEvent::ToolArgumentsDelta {
                        call_id: "fixture-tool".into(),
                        name: Some("fixture".into()),
                        delta: "{}".into(),
                    });
                    yield Ok(ProviderStreamEvent::BlockFinished { index: 0 });
                    yield Err(rivo_llm::ProviderError::invalid_response("fixture failure"));
                })
            }
        }

        let session = Session::with_client(base_config(), Arc::new(FailingStream));
        let unsupported = session
            .legacy_handle(Intent::SetTurnOptions {
                options: TurnOptions::default(),
            })
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(unsupported.as_slice(), [AgentEvent::Error { message }] if message.contains("活动双向 turn"))
        );
        let cancelled = session
            .legacy_handle(Intent::CancelTurn)
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(cancelled.as_slice(), [AgentEvent::Stopped { reason }] if reason == "用户取消")
        );
        let events = session
            .legacy_handle(Intent::SubmitTask {
                text: "fixture".into(),
            })
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.first(), Some(AgentEvent::Started { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelDelta { text } if text == "partial")));
        assert!(matches!(events[events.len() - 2], AgentEvent::Error { .. }));
        assert!(
            matches!(events.last(), Some(AgentEvent::Stopped { reason }) if reason == "提供方失败")
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, AgentEvent::FinalAnswer { .. })));
    }

    #[tokio::test]
    async fn cli_facing_session_only_consumes_intents_and_emits_events() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "answer"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let session = Session::with_client(
            base_config(),
            Arc::new(MockLlm::from_responses([Ok(response)])),
        );
        let events = session
            .handle(Intent::SubmitTask {
                text: "question".into(),
            })
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first().map(|event| &event.event),
            Some(AgentEvent::Started { .. })
        ));
        assert!(matches!(
            events.last().map(|event| &event.event),
            Some(AgentEvent::FinalAnswer { text, .. }) if text == "answer"
        ));
        assert!(events
            .iter()
            .all(|event| event.metadata.session_id == session.session_id));
    }

    #[tokio::test]
    async fn channel_forwards_a_complete_turn_without_waiting_for_stream_close() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "answer"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let config = base_config();
        let session = Arc::new(Session::with_client(
            config.clone(),
            Arc::new(MockLlm::from_responses([Ok(response)])),
        ));
        let (sender, events) = session.legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "question".into(),
            })
            .await
            .unwrap();
        let events = events.take(4).collect::<Vec<_>>().await;
        assert!(matches!(
            events.first(),
            Some(AgentEvent::Started { selection, route, .. })
                if selection == &config.selection && route == &config.route
        ));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::FinalAnswer { .. })
        ));
    }

    #[tokio::test]
    async fn metadata_channel_assigns_ordered_turn_metadata_and_one_terminal_event() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "answer"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let session = Arc::new(Session::with_client(
            base_config(),
            Arc::new(MockLlm::from_responses([Ok(response)])),
        ));
        let session_id = session.session_id().to_owned();
        let (sender, mut events) = session.channel();
        sender
            .send(Intent::SubmitTask {
                text: "question".into(),
            })
            .await
            .unwrap();
        let mut collected = Vec::new();
        while let Some(event) = events.next().await {
            let terminal = matches!(
                &event.event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
            collected.push(event);
            if terminal {
                break;
            }
        }
        assert!(!collected.is_empty());
        assert!(collected
            .iter()
            .all(|event| event.metadata.session_id == session_id));
        assert!(collected
            .iter()
            .all(|event| !event.metadata.turn_id.is_empty()));
        assert_eq!(
            collected
                .iter()
                .map(|event| event.metadata.sequence)
                .collect::<Vec<_>>(),
            (1..=collected.len() as u64).collect::<Vec<_>>()
        );
        assert!(collected.windows(2).all(
            |events| events[0].metadata.timestamp_utc_ms <= events[1].metadata.timestamp_utc_ms
        ));
        assert_eq!(
            collected
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn preflight_failure_path_starts_with_identified_turn() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let options = TurnOptions::default();
        let turn_id = "turn-preflight";
        emit_preflight_started(
            &sender,
            &None,
            "session-preflight",
            turn_id,
            &base_config(),
            &options,
        )
        .await;
        let event = receiver.recv().await.expect("started event");
        assert!(matches!(
            event,
            AgentEvent::Started {
                turn_id: started_turn_id,
                selection: None,
                route: None,
                ..
            } if started_turn_id == turn_id
        ));
    }

    #[tokio::test]
    async fn cancel_stops_a_live_provider_stream_before_final_answer() {
        struct SlowStreamClient {
            release: Arc<tokio::sync::Notify>,
        }

        #[async_trait::async_trait]
        impl LlmClient for SlowStreamClient {
            async fn complete(
                &self,
                _request: ProviderRequest,
            ) -> Result<rivo_llm::ProviderResponse, rivo_llm::ProviderError> {
                Err(rivo_llm::ProviderError::invalid("complete not used"))
            }

            fn stream(&self, _request: ProviderRequest) -> rivo_llm::ProviderStream {
                let release = self.release.clone();
                Box::pin(async_stream::stream! {
                    yield Ok(ProviderStreamEvent::TextDelta { delta: "partial".into() });
                    release.notified().await;
                    yield Ok(ProviderStreamEvent::Done { stop_reason: StopReason::Final, usage: Usage::default() });
                })
            }
        }

        let release = Arc::new(tokio::sync::Notify::new());
        let session = Arc::new(
            Session::with_client(
                base_config(),
                Arc::new(SlowStreamClient {
                    release: release.clone(),
                }),
            )
            .with_storage(
                rivo_storage::Store::open_memory().unwrap(),
                "cancel-history",
            ),
        );
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "cancel me".into(),
            })
            .await
            .unwrap();
        let mut saw_delta = false;
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
                .await
                .unwrap()
        {
            if matches!(event, AgentEvent::ModelDelta { .. }) {
                saw_delta = true;
                break;
            }
        }
        assert!(saw_delta);
        sender.send(Intent::CancelTurn).await.unwrap();
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = events.next().await {
                if matches!(event, AgentEvent::Stopped { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(stopped);
        let persisted = session.replay_events(session.session_id()).unwrap();
        assert!(persisted
            .iter()
            .any(|event| matches!(event, AgentEvent::Stopped { .. })));
        release.notify_one();
    }

    #[tokio::test]
    async fn configured_storage_persists_ordered_turn_events() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "stored"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("rivo.db");
        let session = Arc::new(
            Session::with_client(
                base_config(),
                Arc::new(MockLlm::from_responses([Ok(response)])),
            )
            .with_storage(rivo_storage::Store::open(&database).unwrap(), "history"),
        );
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "persist me".into(),
            })
            .await
            .unwrap();
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let terminal = matches!(
                event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
            observed.push(event);
            if terminal {
                break;
            }
        }
        assert!(observed.len() >= 2);
        drop(events);
        drop(sender);
        drop(session);
        let replay = rivo_storage::Store::open(&database)
            .unwrap()
            .replay("history")
            .unwrap();
        assert_eq!(replay.len(), observed.len());
        assert!(replay
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert_eq!(replay.last().unwrap().kind, "final_answer");
    }

    #[tokio::test]
    async fn multi_turn_storage_assigns_session_wide_message_sequences() {
        let responses = ["first stored", "second stored"].into_iter().map(|text| {
            Ok(ProviderResponse {
                message: Message::text(Role::Assistant, text),
                structured_output: None,
                stop_reason: StopReason::Final,
                usage: Usage::default(),
                provider_raw: None,
                request_id: None,
                model: "mock".into(),
            })
        });
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("multi-turn.db");
        let session = Arc::new(
            Session::with_client(base_config(), Arc::new(MockLlm::from_responses(responses)))
                .with_storage(rivo_storage::Store::open(&database).unwrap(), "history"),
        );
        let (sender, mut events) = session.clone().legacy_channel();
        for (intent, expected) in [
            (Intent::SubmitTask { text: "one".into() }, "first stored"),
            (
                Intent::AppendMessage { text: "two".into() },
                "second stored",
            ),
        ] {
            sender.send(intent).await.unwrap();
            let mut answer = None;
            while let Some(event) =
                tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
                    .await
                    .expect("session did not produce the next event")
            {
                if let AgentEvent::FinalAnswer { text, .. } = event {
                    answer = Some(text);
                    break;
                }
            }
            assert_eq!(answer.as_deref(), Some(expected));
        }
        let messages = rivo_storage::Store::open(&database)
            .unwrap()
            .replay_messages("history")
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.body.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "first stored", "two", "second stored"]
        );
    }

    #[test]
    fn session_history_is_exposed_without_database_access_in_the_tui() {
        let store = rivo_storage::Store::open_memory().unwrap();
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id: "history-session".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        let session = Session::with_client(base_config(), Arc::new(MockLlm::from_responses([])))
            .with_storage(store, "current");
        let history = session.history().unwrap();
        assert_eq!(history[0].session_id, "history-session");
        assert_eq!(history[0].event_count, 1);
        assert_eq!(history[0].workspace_root, None);
    }

    #[tokio::test]
    async fn retry_step_reuses_session_context_after_provider_failure() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "retried"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let session = Arc::new(Session::with_client(
            base_config(),
            Arc::new(MockLlm::from_responses([
                Err(rivo_llm::ProviderError::invalid_response("temporary")),
                Ok(response),
            ])),
        ));
        let (sender, mut events) = session.legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "retry".into(),
            })
            .await
            .unwrap();
        let mut retry_id = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::RetryAvailable { step_id } = event {
                retry_id = Some(step_id);
                break;
            }
        }
        let retry_id = retry_id.expect("retry event");
        sender
            .send(Intent::RetryStep { step_id: retry_id })
            .await
            .unwrap();
        let mut final_seen = false;
        while let Some(event) = events.next().await {
            if matches!(event, AgentEvent::FinalAnswer { ref text, .. } if text == "retried") {
                final_seen = true;
                break;
            }
        }
        assert!(final_seen);
    }

    #[tokio::test]
    async fn memory_intents_confirm_recall_and_delete_with_disclosure_events() {
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "memory answer"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(
            Session::with_client(
                base_config(),
                Arc::new(MockLlm::from_responses([Ok(response)])),
            )
            .with_storage(
                rivo_storage::Store::open(directory.path().join("memory.db")).unwrap(),
                directory.path().to_string_lossy().to_string(),
            ),
        );
        let candidate = MemoryCandidate {
            id: "memory-1".into(),
            workspace: ".".into(),
            mode: "research".into(),
            body: "stable fact".into(),
            source: "session/turn/event".into(),
            body_sha256: rivo_storage::sha256_text("stable fact"),
        };
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::ProposeMemory {
                candidate: candidate.clone(),
            })
            .await
            .unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::MemoryCandidate { candidate_id, .. }) if candidate_id == "memory-1")
        );
        sender
            .send(Intent::ConfirmMemory {
                candidate_id: candidate.id.clone(),
                body_sha256: candidate.body_sha256.clone(),
            })
            .await
            .unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::MemoryConfirmed { memory_id }) if memory_id == "memory-1")
        );
        sender
            .send(Intent::SubmitTask {
                text: "use memory".into(),
            })
            .await
            .unwrap();
        let mut turn_events = Vec::new();
        while let Some(event) = events.next().await {
            let terminal = matches!(
                event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
            turn_events.push(event);
            if terminal {
                break;
            }
        }
        assert!(turn_events.iter().any(|event| matches!(event, AgentEvent::MemoryRecalled { memory_ids } if memory_ids == &["memory-1"])));
        assert!(turn_events.iter().any(|event| matches!(event, AgentEvent::MemoryInjected { memory_ids } if memory_ids == &["memory-1"])),);
        sender
            .send(Intent::DeleteMemory {
                memory_id: candidate.id,
            })
            .await
            .unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::MemoryDeleted { memory_id }) if memory_id == "memory-1")
        );
    }

    #[tokio::test]
    async fn matching_confirmation_resumes_a_pending_agent_request() {
        use async_trait::async_trait;
        use rivo_core::{ContentPart, ToolDefinition};
        use rivo_policy::{Operation, ToolRequest};
        use serde_json::json;

        struct WriteProbe;
        #[async_trait]
        impl rivo_agent::RegisteredTool for WriteProbe {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "write_probe".into(),
                    description: "test".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            fn request(&self, _arguments: &serde_json::Value) -> rivo_policy::ToolRequest {
                ToolRequest {
                    tool_name: "write_probe".into(),
                    operation: Operation::Write,
                    targets: vec![PathBuf::from("note.txt")],
                    program: None,
                    args: vec![],
                    arguments_summary: "write".into(),
                    binary: false,
                    item_count: 1,
                    network: false,
                }
            }
            async fn execute(
                &self,
                _arguments: serde_json::Value,
            ) -> Result<serde_json::Value, rivo_agent::AgentError> {
                Ok(json!({"ok":true}))
            }
        }
        let tool_message = ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "write-1".into(),
                    name: "write_probe".into(),
                    arguments: json!({"x":1}),
                }],
            },
            structured_output: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let final_message = ProviderResponse {
            message: Message::text(Role::Assistant, "approved"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let mut registry = rivo_agent::ToolRegistry::default();
        registry.register(Arc::new(WriteProbe)).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let policy = rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap();
        let agent = Arc::new(rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([
                Ok(tool_message.clone()),
                Ok(tool_message.clone()),
                Ok(final_message),
                Ok(tool_message),
            ])),
            registry,
            policy,
            PolicyProfile::Workspace,
            CapabilityProfile::Coding,
        ));
        let session = Arc::new(Session::with_agent(base_config(), agent).with_storage(
            rivo_storage::Store::open(workspace.path().join("session.db")).unwrap(),
            "confirm-session",
        ));
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "write".into(),
            })
            .await
            .unwrap();
        let mut first = Vec::new();
        for _ in 0..8 {
            if let Some(event) = events.next().await {
                let confirmation = matches!(event, AgentEvent::ConfirmationRequired { .. });
                first.push(event);
                if confirmation {
                    break;
                }
            }
        }
        let request_id = first
            .iter()
            .find_map(|event| {
                if let AgentEvent::ConfirmationRequired { request_id, .. } = event {
                    Some(request_id.clone())
                } else {
                    None
                }
            })
            .expect("confirmation event");
        assert!(first.iter().any(|event| matches!(
            event,
            AgentEvent::ConfirmationRequired {
                session_scope_available: true,
                session_scope_category: Some(category),
                ..
            } if category == "workspace_file_mutation"
        )));
        sender
            .send(Intent::AppendMessage {
                text: "must not start while confirmation is pending".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Error { message }) if message.contains("等待确认")
        ));
        sender
            .send(Intent::Confirm {
                request_id,
                allow: true,
                reason: None,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        let mut second = Vec::new();
        for _ in 0..8 {
            if let Some(event) = events.next().await {
                let terminal = matches!(
                    event,
                    AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
                );
                second.push(event);
                if terminal {
                    break;
                }
            }
        }
        assert!(second
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolObservation { .. })));
        assert!(!second
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { .. })));
        assert!(second.iter().any(
            |event| matches!(event, AgentEvent::FinalAnswer { text, .. } if text == "approved")
        ));
        sender
            .send(Intent::SubmitTask {
                text: "write then cancel".into(),
            })
            .await
            .unwrap();
        let confirmation = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(event) = events.next().await {
                    if matches!(event, AgentEvent::ConfirmationRequired { .. }) {
                        return true;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(confirmation);
        sender.send(Intent::CancelTurn).await.unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Stopped { reason }) if reason == "用户取消等待确认的 turn"
        ));
        let replay = session.replay_events("confirm-session").unwrap();
        assert_eq!(
            replay
                .iter()
                .filter(|event| matches!(event, AgentEvent::Stopped { reason } if reason == "用户取消等待确认的 turn"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn subagent_lifecycle_is_persisted_without_task_or_credential_leaks() {
        let workspace = tempfile::tempdir().unwrap();
        let secret = "subagent-session-secret-987654";
        let key_name = "RIVO_SUBAGENT_SESSION_TEST_KEY";
        std::env::set_var(key_name, secret);
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        config.api_key_env = Some(key_name.into());

        let delegate = ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![rivo_core::ContentPart::ToolUse {
                    id: "delegate-1".into(),
                    name: "delegate_task".into(),
                    arguments: serde_json::json!({
                        "task": "这是完整的子任务正文，不能成为持久化标签",
                        "label": format!("label-{secret}"),
                    }),
                }],
            },
            structured_output: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let child_final = ProviderResponse {
            message: Message::text(Role::Assistant, format!("child summary {secret}")),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let root_final = ProviderResponse {
            message: Message::text(Role::Assistant, "root complete"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let agent = Arc::new(rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([
                Ok(delegate),
                Ok(child_final),
                Ok(root_final),
            ])),
            rivo_agent::ToolRegistry::default(),
            rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap(),
            PolicyProfile::Medium,
            CapabilityProfile::General,
        ));
        let session = Arc::new(Session::with_agent(config, agent).with_storage(
            rivo_storage::Store::open_memory().unwrap(),
            "subagent-session",
        ));
        let (sender, mut receiver) = session.clone().channel();
        sender
            .send(Intent::SubmitTask {
                text: "root task".into(),
            })
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = receiver.next().await {
            let terminal = matches!(
                &event.event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
            events.push(event);
            if terminal {
                break;
            }
        }

        assert!(events
            .iter()
            .any(|event| matches!(&event.event, AgentEvent::SubagentQueued { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(&event.event, AgentEvent::SubagentStarted { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(&event.event, AgentEvent::SubagentCompleted { .. })));
        let tasks = session
            .storage
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .agent_tasks("subagent-session")
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, "completed");
        assert_eq!(tasks[0].terminal_reason.as_deref(), Some("completed"));
        assert!(tasks[0].usage_json.is_some());
        assert!(!tasks[0].label.contains(secret));
        assert!(!tasks[0]
            .summary
            .as_deref()
            .unwrap_or_default()
            .contains(secret));
        assert!(!tasks[0].label.contains("完整的子任务正文"));
        std::env::remove_var(key_name);
    }

    #[tokio::test]
    async fn root_confirmation_resumes_the_waiting_subagent_task() {
        use async_trait::async_trait;
        use rivo_core::{ContentPart, ToolDefinition};
        use rivo_policy::{Operation, ToolRequest};
        use serde_json::json;

        struct WriteProbe;
        #[async_trait]
        impl rivo_agent::RegisteredTool for WriteProbe {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "write_probe".into(),
                    description: "test-only write".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            fn request(&self, _arguments: &serde_json::Value) -> ToolRequest {
                ToolRequest {
                    tool_name: "write_probe".into(),
                    operation: Operation::Write,
                    targets: vec![PathBuf::from("child-note.txt")],
                    program: None,
                    args: vec![],
                    arguments_summary: "child write".into(),
                    binary: false,
                    item_count: 1,
                    network: false,
                }
            }
            async fn execute(
                &self,
                _arguments: serde_json::Value,
            ) -> Result<serde_json::Value, rivo_agent::AgentError> {
                Ok(json!({"ok":true}))
            }
        }

        let tool = |id: &str, name: &str, arguments| ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                }],
            },
            structured_output: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let workspace = tempfile::tempdir().unwrap();
        let delegate = tool(
            "delegate-1",
            "delegate_task",
            json!({"task":"write a child note","label":"child write"}),
        );
        let child_write = tool("child-write-1", "write_probe", json!({}));
        let child_write_after_session_grant = tool("child-write-2", "write_probe", json!({}));
        let child_final = ProviderResponse {
            message: Message::text(Role::Assistant, "child wrote the note"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let root_final = ProviderResponse {
            message: Message::text(Role::Assistant, "root complete"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let mut registry = rivo_agent::ToolRegistry::default();
        registry.register(Arc::new(WriteProbe)).unwrap();
        let agent = Arc::new(rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([
                Ok(delegate),
                Ok(child_write),
                Ok(child_write_after_session_grant),
                Ok(child_final),
                Ok(root_final),
            ])),
            registry,
            rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap(),
            PolicyProfile::Medium,
            CapabilityProfile::Coding,
        ));
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        config.capability_profile = CapabilityProfile::Coding;
        config.policy_profile = PolicyProfile::Medium;
        let session = Arc::new(Session::with_agent(config, agent).with_storage(
            rivo_storage::Store::open_memory().unwrap(),
            "child-confirm-session",
        ));
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "delegate a write".into(),
            })
            .await
            .unwrap();

        let request_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.next().await {
                    Some(AgentEvent::SubagentConfirmationRequired {
                        request_id,
                        agent_id,
                        task_id,
                        ..
                    }) => {
                        assert!(agent_id.0.starts_with("agent-"));
                        assert!(task_id.0.starts_with("task-"));
                        return request_id;
                    }
                    Some(_) => {}
                    None => panic!("session event stream ended before child confirmation"),
                }
            }
        })
        .await
        .expect("child confirmation timed out");
        sender
            .send(Intent::Confirm {
                request_id,
                allow: true,
                reason: None,
                scope: ApprovalScope::Session,
            })
            .await
            .unwrap();

        let mut completed = false;
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::SubagentCompleted { result } => {
                    assert_eq!(result.state, rivo_core::AgentTaskState::Completed);
                    completed = true;
                }
                AgentEvent::SubagentConfirmationRequired { .. } => {
                    panic!("session-scoped approval must be reused by the running subagent")
                }
                AgentEvent::FinalAnswer { text, .. } => {
                    assert_eq!(text, "root complete");
                    break;
                }
                AgentEvent::Stopped { reason } => panic!("unexpected stop: {reason}"),
                _ => {}
            }
        }
        assert!(completed);
        assert_eq!(session.session_permissions().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn parallel_subagents_with_identical_requests_get_independent_confirmations() {
        use async_trait::async_trait;
        use rivo_core::{ContentPart, ToolDefinition};
        use rivo_policy::{Operation, ToolRequest};
        use serde_json::json;

        struct WriteProbe;
        #[async_trait]
        impl rivo_agent::RegisteredTool for WriteProbe {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "write_probe".into(),
                    description: "test-only write".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            fn request(&self, _arguments: &serde_json::Value) -> ToolRequest {
                ToolRequest {
                    tool_name: "write_probe".into(),
                    operation: Operation::Write,
                    targets: vec![PathBuf::from("shared-note.txt")],
                    program: None,
                    args: vec![],
                    arguments_summary: "shared write".into(),
                    binary: false,
                    item_count: 1,
                    network: false,
                }
            }
            async fn execute(
                &self,
                _arguments: serde_json::Value,
            ) -> Result<serde_json::Value, rivo_agent::AgentError> {
                Ok(json!({"ok":true}))
            }
        }

        let response = |content: Vec<ContentPart>, stop_reason| ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            structured_output: None,
            stop_reason,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let delegate = |id: &str| ContentPart::ToolUse {
            id: id.into(),
            name: "delegate_task".into(),
            arguments: json!({"task":"write the shared note","label":id}),
        };
        let child_write = || {
            response(
                vec![ContentPart::ToolUse {
                    id: "call_0".into(),
                    name: "write_probe".into(),
                    arguments: json!({}),
                }],
                StopReason::ToolUse,
            )
        };
        let text = |value: &str| {
            response(
                vec![ContentPart::Text { text: value.into() }],
                StopReason::Final,
            )
        };
        let workspace = tempfile::tempdir().unwrap();
        let mut registry = rivo_agent::ToolRegistry::default();
        registry.register(Arc::new(WriteProbe)).unwrap();
        let agent = Arc::new(rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([
                Ok(response(
                    vec![delegate("first"), delegate("second")],
                    StopReason::ToolUse,
                )),
                Ok(child_write()),
                Ok(child_write()),
                Ok(text("child wrote the note")),
                Ok(text("root complete")),
            ])),
            registry,
            rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap(),
            PolicyProfile::Medium,
            CapabilityProfile::Coding,
        ));
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        config.capability_profile = CapabilityProfile::Coding;
        config.policy_profile = PolicyProfile::Medium;
        let session = Arc::new(Session::with_agent(config, agent).with_storage(
            rivo_storage::Store::open_memory().unwrap(),
            "parallel-confirm-session",
        ));
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "delegate two writes".into(),
            })
            .await
            .unwrap();

        let mut confirmations = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while confirmations.len() < 2 {
                match events.next().await {
                    Some(AgentEvent::SubagentConfirmationRequired {
                        request_id,
                        task_id,
                        ..
                    }) => confirmations.push((request_id, task_id)),
                    Some(_) => {}
                    None => panic!("session event stream ended before child confirmations"),
                }
            }
        })
        .await
        .expect("child confirmations timed out");
        assert_ne!(confirmations[0].0, confirmations[1].0);
        for (request_id, task_id) in &confirmations {
            assert!(request_id.starts_with(&format!("{}:", task_id.0)));
        }
        let (approved_id, approved_task) = confirmations[0].clone();
        let (denied_id, denied_task) = confirmations[1].clone();
        for (request_id, allow) in [(approved_id, true), (denied_id, false)] {
            sender
                .send(Intent::Confirm {
                    request_id,
                    allow,
                    reason: None,
                    scope: ApprovalScope::Once,
                })
                .await
                .unwrap();
        }

        let mut states = HashMap::new();
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::SubagentCompleted { result } => {
                    states.insert(result.task_id.0, result.state);
                }
                AgentEvent::FinalAnswer { text, .. } => {
                    assert_eq!(text, "root complete");
                    break;
                }
                AgentEvent::Stopped { reason } => panic!("unexpected stop: {reason}"),
                _ => {}
            }
        }
        assert_eq!(
            states.get(&approved_task.0),
            Some(&rivo_core::AgentTaskState::Completed)
        );
        assert_eq!(
            states.get(&denied_task.0),
            Some(&rivo_core::AgentTaskState::Failed),
            "a denied task must not be reported as cancelled"
        );
    }

    #[tokio::test]
    async fn cancelling_a_waiting_subagent_marks_its_task_cancelled_immediately() {
        use async_trait::async_trait;
        use rivo_core::{ContentPart, ToolDefinition};
        use rivo_policy::{Operation, ToolRequest};
        use serde_json::json;

        struct WriteProbe;
        #[async_trait]
        impl rivo_agent::RegisteredTool for WriteProbe {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "write_probe_cancel".into(),
                    description: "test-only write".into(),
                    parameters: json!({"type":"object"}),
                }
            }

            fn request(&self, _arguments: &serde_json::Value) -> ToolRequest {
                ToolRequest {
                    tool_name: "write_probe_cancel".into(),
                    operation: Operation::Write,
                    targets: vec![PathBuf::from("child-note.txt")],
                    program: None,
                    args: vec![],
                    arguments_summary: "child write".into(),
                    binary: false,
                    item_count: 1,
                    network: false,
                }
            }

            async fn execute(
                &self,
                _arguments: serde_json::Value,
            ) -> Result<serde_json::Value, rivo_agent::AgentError> {
                Ok(json!({"ok":true}))
            }
        }

        let tool = |id: &str, name: &str, arguments| ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                }],
            },
            structured_output: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let workspace = tempfile::tempdir().unwrap();
        let mut registry = rivo_agent::ToolRegistry::default();
        registry.register(Arc::new(WriteProbe)).unwrap();
        let agent = Arc::new(rivo_agent::Agent::new(
            Arc::new(MockLlm::from_responses([
                Ok(tool(
                    "delegate-1",
                    "delegate_task",
                    json!({"task":"write a child note","label":"child write"}),
                )),
                Ok(tool("child-write-1", "write_probe_cancel", json!({}))),
            ])),
            registry,
            rivo_policy::PolicyEvaluator::new(workspace.path()).unwrap(),
            PolicyProfile::Medium,
            CapabilityProfile::Coding,
        ));
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        config.capability_profile = CapabilityProfile::Coding;
        config.policy_profile = PolicyProfile::Medium;
        let session = Arc::new(Session::with_agent(config, agent).with_storage(
            rivo_storage::Store::open_memory().unwrap(),
            "cancel-waiting-subagent",
        ));
        let (sender, mut events) = session.clone().legacy_channel();
        sender
            .send(Intent::SubmitTask {
                text: "delegate a write".into(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.next().await {
                    Some(AgentEvent::SubagentConfirmationRequired { .. }) => return,
                    Some(_) => {}
                    None => panic!("session event stream ended before child confirmation"),
                }
            }
        })
        .await
        .expect("child confirmation timed out");
        sender.send(Intent::CancelTurn).await.unwrap();

        let mut saw_cancelled_task = false;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                match event {
                    AgentEvent::SubagentCompleted { result } => {
                        assert_eq!(result.state, AgentTaskState::Cancelled);
                        saw_cancelled_task = true;
                    }
                    AgentEvent::Stopped { reason } => {
                        assert_eq!(reason, "用户取消");
                        return;
                    }
                    _ => {}
                }
            }
            panic!("session event stream ended before cancellation terminal event");
        })
        .await
        .expect("cancellation did not complete");
        assert!(saw_cancelled_task);
        let tasks = session
            .storage
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .agent_tasks("cancel-waiting-subagent")
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, "cancelled");
        assert_eq!(tasks[0].terminal_reason.as_deref(), Some("cancelled"));
    }

    #[test]
    fn storage_backed_session_management_and_diagnostics_are_safe_for_frontends() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()))
            .with_storage(store, "frontend-session");

        assert!(session.history().unwrap().is_empty());
        assert!(session.diagnostics(None, 10).unwrap().is_empty());
        assert!(session
            .replay_events("frontend-session")
            .unwrap()
            .is_empty());
        assert!(session
            .replay_messages("frontend-session")
            .unwrap()
            .is_empty());
        assert!(session.session_permissions().unwrap().is_empty());
        assert_eq!(session.clear_session_permissions().unwrap(), 0);

        let event = AgentEvent::Failure {
            message: "provider request failed".into(),
            details: FailureDetails {
                stage: FailureStage::Stream,
                code: "provider_failure".into(),
                retryable: true,
                recovery: "retry".into(),
                cause: "fixture-cause".into(),
            },
        };
        let store = session.storage.as_ref().unwrap().lock().unwrap();
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id: "frontend-session".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                kind: "failure".into(),
                body: serde_json::to_string(&event).unwrap(),
            })
            .unwrap();
        store
            .save_session_permission_grant(&SessionPermissionGrant {
                session_id: "frontend-session".into(),
                workspace: ".".into(),
                policy: PolicyProfile::Medium.to_string(),
                grant_key: "workspace_file_mutation".into(),
            })
            .unwrap();
        drop(store);

        assert_eq!(
            session.replay_events("frontend-session").unwrap(),
            vec![event]
        );
        assert_eq!(
            session.session_permissions().unwrap(),
            vec!["workspace_file_mutation"]
        );
        assert_eq!(session.clear_session_permissions().unwrap(), 1);
        assert!(session.session_permissions().unwrap().is_empty());

        let audit_id = session
            .record_ui_audit(
                Some("turn-1"),
                "failure",
                serde_json::json!({
                    "cause": "fixture-cause",
                    "api_key": "secret-value-that-must-not-be-persisted"
                }),
            )
            .unwrap();
        assert!(audit_id > 0);
        assert_eq!(
            session
                .diagnostics(Some("frontend-session"), 10)
                .unwrap()
                .len(),
            2
        );
        session.clear_diagnostics().unwrap();
        assert!(session.diagnostics(None, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn async_frontend_storage_facades_preserve_history_replay_and_audit_behavior() {
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()))
            .with_storage(
                rivo_storage::Store::open_memory().unwrap(),
                "async-frontend-session",
            );
        let event = AgentEvent::Failure {
            message: "provider request failed".into(),
            details: FailureDetails {
                stage: FailureStage::Stream,
                code: "provider_failure".into(),
                retryable: true,
                recovery: "retry".into(),
                cause: "fixture-cause".into(),
            },
        };
        {
            let store = session.storage.as_ref().unwrap().lock().unwrap();
            store
                .append_event(&rivo_storage::StoredEvent {
                    session_id: "async-frontend-session".into(),
                    turn_id: "turn-1".into(),
                    sequence: 1,
                    kind: "failure".into(),
                    body: serde_json::to_string(&event).unwrap(),
                })
                .unwrap();
            store
                .append_message(&rivo_storage::StoredMessage {
                    session_id: "async-frontend-session".into(),
                    turn_id: "turn-1".into(),
                    sequence: 1,
                    role: "user".into(),
                    body: "history fixture".into(),
                })
                .unwrap();
            store
                .save_session_permission_grant(&SessionPermissionGrant {
                    session_id: "async-frontend-session".into(),
                    workspace: session.config.workspace.to_string_lossy().to_string(),
                    policy: PolicyProfile::Medium.to_string(),
                    grant_key: "workspace_file_mutation".into(),
                })
                .unwrap();
        }

        assert_eq!(session.history_async().await.unwrap().len(), 1);
        assert_eq!(
            session.session_permissions_async().await.unwrap(),
            vec!["workspace_file_mutation"]
        );
        assert_eq!(session.clear_session_permissions_async().await.unwrap(), 1);
        assert!(session
            .session_permissions_async()
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            session
                .replay_events_async("async-frontend-session")
                .await
                .unwrap(),
            vec![event]
        );
        assert_eq!(
            session
                .replay_messages_async("async-frontend-session")
                .await
                .unwrap()[0]
                .body,
            "history fixture"
        );
        let audit_id = session
            .record_ui_audit_async(
                Some("turn-1"),
                "command",
                serde_json::json!({"api_key": "secret-value-that-must-not-be-persisted"}),
            )
            .await
            .unwrap();
        assert!(audit_id > 0);
        assert!(!session
            .diagnostics_async(Some("async-frontend-session"), 10)
            .await
            .unwrap()
            .is_empty());
        session.clear_diagnostics_async().await.unwrap();
        assert!(session
            .diagnostics_async(None, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn poisoned_storage_lock_is_reported_as_a_structured_storage_error() {
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()))
            .with_storage(
                rivo_storage::Store::open_memory().unwrap(),
                "poisoned-storage",
            );
        let store = session.storage.as_ref().unwrap().clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.lock().unwrap();
            panic!("fixture poisons the storage mutex");
        }));

        assert!(matches!(
            session.history(),
            Err(SessionError::Storage(rivo_storage::StorageError::Unavailable(message)))
                if message.contains("历史存储锁")
        ));
    }

    #[test]
    fn poisoned_active_llm_lock_is_reported_as_a_structured_session_error() {
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()));
        let active_llm = Arc::clone(&session.active_llm);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = active_llm.lock().unwrap();
            panic!("fixture poisons the active LLM mutex");
        }));

        assert!(matches!(
            session.validate_model_selection("fixture-model"),
            Err(SessionError::RuntimeUnavailable("LLM"))
        ));
    }

    #[tokio::test]
    async fn poisoned_active_llm_lock_stops_the_channel_without_panicking() {
        let session = Arc::new(Session::with_client(
            base_config(),
            Arc::new(MockLlm::default()),
        ));
        let active_llm = Arc::clone(&session.active_llm);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = active_llm.lock().unwrap();
            panic!("fixture poisons the active LLM mutex");
        }));

        let (_sender, mut events) = session.legacy_channel();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Error { message }) if message.contains("会话运行时锁已失效: LLM")
        ));
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Stopped { reason }) if reason == "会话运行时状态失效"
        ));
        assert!(events.next().await.is_none());
    }

    #[test]
    fn explicit_shutdown_closes_unique_storage() {
        let session = Arc::new(
            Session::with_client(base_config(), Arc::new(MockLlm::default()))
                .with_storage(rivo_storage::Store::open_memory().unwrap(), "shutdown"),
        );
        assert!(session.shutdown().is_ok());
    }

    #[tokio::test]
    async fn explicit_shutdown_rejects_active_channel_reference() {
        let session = Arc::new(
            Session::with_client(base_config(), Arc::new(MockLlm::default())).with_storage(
                rivo_storage::Store::open_memory().unwrap(),
                "shutdown-active",
            ),
        );
        let (_sender, events) = session.clone().legacy_channel();
        let error = session.shutdown().unwrap_err();
        assert!(matches!(
            error,
            SessionError::Storage(rivo_storage::StorageError::Unavailable(message))
                if message.contains("活动引用")
        ));
        drop(events);
    }

    #[test]
    fn storage_lock_contention_is_serialized_until_the_guard_is_released() {
        let store = Arc::new(Mutex::new(rivo_storage::Store::open_memory().unwrap()));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_store = Arc::clone(&store);
        let worker_barrier = Arc::clone(&barrier);
        let worker_acquired = Arc::clone(&acquired);
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            let _guard = worker_store.lock().unwrap();
            worker_acquired.store(true, Ordering::SeqCst);
        });

        let guard = store.lock().unwrap();
        barrier.wait();
        assert!(!acquired.load(Ordering::SeqCst));
        drop(guard);
        worker.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_scheduler_observes_sync_storage_lock_blocking_without_wall_clock_thresholds() {
        let store = Arc::new(Mutex::new(rivo_storage::Store::open_memory().unwrap()));
        let guard = store.lock().unwrap();
        let blocked_store = Arc::clone(&store);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let blocked = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            let _guard = blocked_store.lock().unwrap();
        });

        started_rx.await.unwrap();
        let (marker_tx, marker_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            marker_tx.send(()).unwrap();
        });
        // The marker must run on the other Tokio worker while the first worker
        // is synchronously blocked on SQLite's mutex.
        marker_rx.await.unwrap();
        drop(guard);
        blocked.await.unwrap();
    }

    #[test]
    fn redacted_config_and_atomic_write_keep_configuration_references_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("config.toml");
        let mut config = base_config();
        config.config_path = path.clone();
        config.auth.credential = Some("fixture-key".into());
        config.mcp_servers.push(McpServerConfig {
            name: "docs".into(),
            enabled: true,
            transport: rivo_tools::mcp::McpTransport::Stdio {
                program: "mcp-docs".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            allowed_capabilities: BTreeSet::from(["tools".into()]),
            toolsets: BTreeMap::new(),
            allow_sampling: false,
        });
        let values = redacted_config(&config);
        assert_eq!(values["provider_id"], "primary");
        assert_eq!(values["providers"], "primary");
        assert_eq!(values["mcp_servers"], "docs(enabled)");
        assert!(!values.values().any(|value| value.contains("secret-value")));

        let file = FileConfig::default();
        let revision = read_model_routing_draft(&path).unwrap().revision;
        commit_model_routing_draft(&path, &revision, &file).unwrap();
        assert_eq!(read_file_config(&path).unwrap().version, CONFIG_VERSION);
    }

    #[test]
    fn no_storage_management_queries_are_safe_noops() {
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()));
        assert!(session.history().unwrap().is_empty());
        assert!(session.diagnostics(None, 10).unwrap().is_empty());
        assert!(session.replay_events("missing").unwrap().is_empty());
        assert!(session.replay_messages("missing").unwrap().is_empty());
        session.clear_diagnostics().unwrap();
        assert!(session.session_permissions().unwrap().is_empty());
        assert_eq!(session.clear_session_permissions().unwrap(), 0);
        assert_eq!(
            session
                .record_ui_audit(Some("turn"), "test", serde_json::json!({"ok": true}))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn channel_reports_invalid_management_intents_without_storage() {
        let session = Arc::new(Session::with_client(
            base_config(),
            Arc::new(MockLlm::default()),
        ));
        let (sender, mut events) = session.channel();
        macro_rules! expect_error {
            ($text:expr) => {{
                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
                        .await
                        .expect("channel should emit an error")
                        .expect("channel should remain open");
                assert!(matches!(event.event, AgentEvent::Error { message } if message.contains($text)));
            }};
        }

        sender.send(Intent::CancelTurn).await.unwrap();
        let event = events.next().await.unwrap();
        assert!(matches!(event.event, AgentEvent::Stopped { reason } if reason == "没有活动 turn"));

        sender
            .send(Intent::Confirm {
                request_id: "stale".into(),
                allow: true,
                reason: None,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        expect_error!("过期或不匹配");

        sender
            .send(Intent::SetTurnOptions {
                options: TurnOptions {
                    mcp_servers: vec!["unknown".into()],
                    ..TurnOptions::default()
                },
            })
            .await
            .unwrap();
        expect_error!("MCP server 未配置或未启用: unknown");

        sender
            .send(Intent::SelectModel {
                selection: ModelSelection {
                    provider: ProviderSelection::Explicit {
                        provider_id: "missing".into(),
                    },
                    ..selection("mock")
                },
            })
            .await
            .unwrap();
        expect_error!("显式 Provider missing 不可用");

        sender
            .send(Intent::SelectModel {
                selection: selection("other"),
            })
            .await
            .unwrap();
        expect_error!("Model 不属于所选 Vendor");

        let candidate = MemoryCandidate {
            id: "candidate".into(),
            workspace: ".".into(),
            mode: "general".into(),
            body: "fixture".into(),
            source: "test".into(),
            body_sha256: rivo_storage::sha256_text("fixture"),
        };
        sender
            .send(Intent::ProposeMemory { candidate })
            .await
            .unwrap();
        expect_error!("未配置历史存储");
        sender
            .send(Intent::ConfirmMemory {
                candidate_id: "missing".into(),
                body_sha256: "hash".into(),
            })
            .await
            .unwrap();
        expect_error!("未配置历史存储");
        sender
            .send(Intent::RejectMemory {
                candidate_id: "missing".into(),
            })
            .await
            .unwrap();
        expect_error!("未配置历史存储");
        sender
            .send(Intent::DeleteMemory {
                memory_id: "missing".into(),
            })
            .await
            .unwrap();
        expect_error!("未配置历史存储");
        sender
            .send(Intent::RespondInteraction {
                request_id: "missing".into(),
                value: "answer".into(),
            })
            .await
            .unwrap();
        expect_error!("未找到待处理的 Interaction missing");
    }

    #[tokio::test]
    async fn rejected_confirmation_terminal_event_is_persisted() {
        let store = Arc::new(Mutex::new(rivo_storage::Store::open_memory().unwrap()));
        let event = AgentEvent::Stopped {
            reason: "用户拒绝确认".into(),
        };
        persist_standalone_event(&Some(store.clone()), "session", "turn", &event)
            .await
            .unwrap();
        let events = store.lock().unwrap().replay("session").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].turn_id, "turn");
        assert_eq!(events[0].kind, "stopped");
    }

    #[test]
    fn elicitation_content_passes_through_json_object() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } }
        });
        let content = elicitation_content("{\"name\":\"Ada\",\"age\":36}", &schema);
        assert_eq!(content, serde_json::json!({ "name": "Ada", "age": 36 }));
    }

    #[test]
    fn elicitation_content_coerces_single_field_by_type() {
        let integer_schema = serde_json::json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } }
        });
        assert_eq!(
            elicitation_content("42", &integer_schema),
            serde_json::json!({ "count": 42 })
        );
        let bool_schema = serde_json::json!({
            "type": "object",
            "properties": { "confirmed": { "type": "boolean" } }
        });
        assert_eq!(
            elicitation_content("yes", &bool_schema),
            serde_json::json!({ "confirmed": true })
        );
        let string_schema = serde_json::json!({
            "type": "object",
            "properties": { "note": { "type": "string" } }
        });
        assert_eq!(
            elicitation_content("hello world", &string_schema),
            serde_json::json!({ "note": "hello world" })
        );
    }

    #[test]
    fn elicitation_content_falls_back_to_value_wrapper() {
        // 多字段 schema 且用户未输入 JSON 对象 → 回退到 {"value": <原文>}。
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "first": { "type": "string" },
                "second": { "type": "string" }
            }
        });
        assert_eq!(
            elicitation_content("freeform", &schema),
            serde_json::json!({ "value": "freeform" })
        );
    }

    #[test]
    fn interaction_outcome_maps_decline_cancel_and_accept() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "note": { "type": "string" } }
        });
        assert!(matches!(
            interaction_outcome(ELICIT_DECLINE_ID.to_owned(), &schema),
            rivo_agent::ElicitationOutcome::Decline
        ));
        assert!(matches!(
            interaction_outcome("   ".to_owned(), &schema),
            rivo_agent::ElicitationOutcome::Cancel
        ));
        match interaction_outcome("hi".to_owned(), &schema) {
            rivo_agent::ElicitationOutcome::Accept(content) => {
                assert_eq!(content, serde_json::json!({ "note": "hi" }));
            }
            other => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn elicitation_to_interaction_lists_required_fields() {
        let (responder, _receiver) = oneshot::channel();
        let pending = rivo_agent::PendingElicitation {
            server: "docs".into(),
            request_id: "elicit-docs-1".into(),
            message: "需要补充查询参数".into(),
            requested_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "keyword": { "type": "string", "description": "搜索关键词" },
                    "limit": { "type": "integer" }
                },
                "required": ["keyword"]
            }),
            responder,
        };
        let request = elicitation_to_interaction(&pending);
        assert_eq!(request.request_id, "elicit-docs-1");
        assert!(request.allow_freeform);
        assert_eq!(request.options.len(), 1);
        assert_eq!(request.options[0].id, ELICIT_DECLINE_ID);
        assert!(request.body.contains("需要补充查询参数"));
        assert!(request.body.contains("keyword"));
        assert!(request.body.contains("必填"));
        assert!(request.body.contains("limit"));
        assert!(request.body.contains("可选"));
    }

    #[tokio::test]
    async fn goal_lifecycle_persists_and_increments_revision() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let session = Session::with_client(base_config(), Arc::new(MockLlm::default()))
            .with_storage(store, "goal-session");
        let mut events = session
            .legacy_handle(Intent::SetGoal {
                objective: "完成基准测试".into(),
                budget: 3,
            })
            .unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::GoalUpdated { goal }) if goal.revision == 1 && goal.status == GoalStatus::Active)
        );
        let mut events = session.legacy_handle(Intent::PauseGoal).unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::GoalUpdated { goal }) if goal.revision == 2 && goal.status == GoalStatus::Paused)
        );
        let goal = session.goal().unwrap().unwrap();
        assert_eq!(goal.objective, "完成基准测试");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.revision, 2);
    }

    #[test]
    fn routing_keeps_explicit_model_and_uses_provider_priority_with_stable_ties() {
        let mut file = FileConfig::default();
        for (name, address) in [
            ("provider-a", "https://first.example/v1"),
            ("provider-b", "https://second.example/v1"),
        ] {
            file.providers.insert(
                name.into(),
                ProviderConfig {
                    base_url: address.into(),
                    auth: AuthConfig::default(),
                    enabled: true,
                    priority: 10,
                    discovery: ModelDiscoveryCache::default(),
                },
            );
            let fingerprint = provider_config_fingerprint(&file, name);
            file.providers.get_mut(name).unwrap().discovery = ModelDiscoveryCache {
                config_fingerprint: fingerprint,
                supported: Some(true),
                models: vec!["gpt-5".into()],
                last_checked_at: Some(1),
                last_error: None,
            };
        }
        let route = resolve_available_route(&file, Some("openai"), Some("gpt-5"), None).unwrap();
        assert_eq!(route.provider_id, "provider-a");
        assert_eq!(route.model, "gpt-5");
        file.providers.get_mut("provider-b").unwrap().priority = 11;
        assert_eq!(
            resolve_available_route(&file, Some("openai"), Some("gpt-5"), None)
                .unwrap()
                .provider_id,
            "provider-b"
        );
        assert_eq!(route.provider_id, "provider-a");
    }

    #[test]
    fn routing_uses_model_priority_only_when_model_is_omitted() {
        let mut file = FileConfig::default();
        file.providers.insert(
            "gateway".into(),
            ProviderConfig {
                base_url: "https://gateway.example/v1".into(),
                auth: AuthConfig::default(),
                enabled: true,
                priority: 9,
                discovery: ModelDiscoveryCache::default(),
            },
        );
        file.models.insert(
            "local-fast".into(),
            ModelCatalogEntry {
                vendor: "openai".into(),
                display_name: "Local Fast".into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority: 2,
            },
        );
        file.models.insert(
            "local-best".into(),
            ModelCatalogEntry {
                vendor: "openai".into(),
                display_name: "Local Best".into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority: 3,
            },
        );
        let fingerprint = provider_config_fingerprint(&file, "gateway");
        file.providers.get_mut("gateway").unwrap().discovery = ModelDiscoveryCache {
            config_fingerprint: fingerprint,
            supported: Some(true),
            models: vec!["local-fast".into(), "local-best".into()],
            last_checked_at: Some(1),
            last_error: None,
        };
        let route = resolve_available_route(&file, Some("openai"), None, None).unwrap();
        assert_eq!(route.model, "local-best");
        assert_eq!(route.priority, 9);
        assert!(resolve_available_route(&file, Some("openai"), Some("missing"), None).is_err());
        file.providers.get_mut("gateway").unwrap().discovery.models = vec!["local-fast".into()];
        assert!(resolve_available_route(&file, Some("openai"), None, None).is_err());
    }

    #[test]
    fn available_route_requires_exact_fresh_provider_discovery_and_stable_priority() {
        let mut file = FileConfig::default();
        file.models.insert(
            "canonical-model".into(),
            ModelCatalogEntry {
                vendor: "openai".into(),
                display_name: "Canonical".into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority: 0,
            },
        );
        for (id, priority) in [("z-provider", 10), ("a-provider", 10)] {
            file.providers.insert(
                id.into(),
                ProviderConfig {
                    base_url: "https://gateway.example/v1".into(),
                    auth: AuthConfig::default(),
                    enabled: true,
                    priority,
                    discovery: ModelDiscoveryCache::default(),
                },
            );
            let fingerprint = provider_config_fingerprint(&file, id);
            file.providers.get_mut(id).unwrap().discovery = ModelDiscoveryCache {
                config_fingerprint: fingerprint,
                supported: Some(true),
                models: vec!["canonical-model".into(), "canonical-model".into()],
                last_checked_at: Some(1),
                last_error: None,
            };
        }
        let route =
            resolve_available_route(&file, Some("openai"), Some("canonical-model"), None).unwrap();
        assert_eq!(route.provider_id, "a-provider");
        assert_eq!(route.model, "canonical-model");
        assert!(resolve_available_route(
            &file,
            Some("openai"),
            Some("canonical-model-alias"),
            None
        )
        .is_err());
        file.providers.get_mut("a-provider").unwrap().base_url =
            "https://changed.example/v1".into();
        assert_eq!(
            resolve_available_route(&file, Some("openai"), Some("canonical-model"), None)
                .unwrap()
                .provider_id,
            "z-provider"
        );
        assert!(resolve_available_route(
            &file,
            Some("openai"),
            Some("canonical-model"),
            Some("a-provider")
        )
        .is_err());
        file.providers
            .get_mut("z-provider")
            .unwrap()
            .discovery
            .last_error = Some("最近刷新失败".into());
        assert!(
            resolve_available_route(&file, Some("openai"), Some("canonical-model"), None).is_err()
        );
    }

    #[test]
    fn structured_plan_parser_requires_rivo_marker_and_supports_fenced_json() {
        let prose_json = r#"{"objective":"普通 JSON，不应创建计划"}"#;
        assert!(extract_plan_draft(prose_json, "turn-1").is_none());

        let fenced = r#"调查完成：
```json
{"rivo_plan":{"objective":"实现只读计划","steps":[{"title":"检查代码","description":"记录证据","acceptance_criteria":["测试通过"]}]}}
```"#;
        let plan = extract_plan_draft(fenced, "turn-1").expect("marked plan");
        assert_eq!(plan.plan_id, "plan-turn-1");
        assert_eq!(plan.status, TaskPlanStatus::Draft);
        assert_eq!(plan.steps[0].step_id, "step-1");
        assert_eq!(plan.steps[0].status, TaskPlanStatus::AwaitingApproval);
    }

    #[test]
    fn structured_plan_parser_rejects_unsafe_or_oversized_drafts() {
        let invalid_id = r#"{"rivo_plan":{"plan_id":"../escape","objective":"目标"}}"#;
        assert!(extract_plan_draft(invalid_id, "turn-2").is_none());

        let oversized = "x".repeat(16 * 1024 + 1);
        let value = serde_json::json!({
            "rivo_plan": {"objective": oversized, "steps": []}
        });
        assert!(extract_plan_draft(&value.to_string(), "turn-3").is_none());

        let steps = (0..=100)
            .map(|index| serde_json::json!({"title": format!("step-{index}")}))
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "rivo_plan": {"objective": "目标", "steps": steps}
        });
        assert!(extract_plan_draft(&value.to_string(), "turn-4").is_none());
    }

    #[tokio::test]
    async fn task_plan_requires_approval_and_exact_revision() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        let session = Session::with_client(config, Arc::new(MockLlm::default()))
            .with_storage(store, "plan-session");
        let plan = TaskPlan {
            plan_id: "plan-1".into(),
            session_id: "plan-session".into(),
            objective: "实现计划工作流".into(),
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps: vec![],
            dependencies: vec![],
            acceptance_criteria: vec!["测试通过".into()],
            risks: vec![],
            evidence_refs: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut events = session.legacy_handle(Intent::CreatePlan { plan }).unwrap();
        assert!(
            matches!(events.next().await, Some(AgentEvent::PlanCreated { plan }) if plan.revision == 1 && plan.status == TaskPlanStatus::AwaitingApproval)
        );
        let mut events = session
            .legacy_handle(Intent::StartPlanExecution {
                plan_id: "plan-1".into(),
                revision: 1,
                policy_profile: None,
            })
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Error { .. })
        ));
        let document_sha256 =
            plan_document_sha256(&session.load_task_plan("plan-1", Some(1)).unwrap().unwrap())
                .unwrap();
        let mut events = session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "plan-1".into(),
                revision: 1,
                document_sha256: Some(document_sha256),
            })
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::PlanApproved { revision: 1, .. })
        ));
        let mut events = session
            .legacy_handle(Intent::StartPlanExecution {
                plan_id: "plan-1".into(),
                revision: 1,
                policy_profile: None,
            })
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::PlanExecutionStarted { revision: 1, .. })
        ));
    }

    #[tokio::test]
    async fn approved_plan_starts_a_separate_coding_turn() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        let response = ProviderResponse {
            message: Message::text(Role::Assistant, "coding complete"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        };
        let session = Arc::new(
            Session::with_client(config, Arc::new(MockLlm::from_responses([Ok(response)])))
                .with_storage(store, "handoff-session"),
        );
        let plan = TaskPlan {
            plan_id: "handoff-plan".into(),
            session_id: "handoff-session".into(),
            objective: "实现 coding 交接".into(),
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps: vec![TaskPlanStep {
                step_id: "step-1".into(),
                title: "实现变更".into(),
                description: "修改代码并验证".into(),
                depends_on: vec![],
                acceptance_criteria: vec!["测试通过".into()],
                status: TaskPlanStatus::AwaitingApproval,
                evidence_refs: vec![],
                failure_reason: None,
            }],
            dependencies: vec![],
            acceptance_criteria: vec!["测试通过".into()],
            risks: vec![],
            evidence_refs: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut events = session.legacy_handle(Intent::CreatePlan { plan }).unwrap();
        let _ = events.next().await;
        let hash = match events.next().await.unwrap() {
            AgentEvent::PlanApprovalRequired {
                document_sha256, ..
            } => document_sha256,
            other => panic!("unexpected event: {other:?}"),
        };
        let _ = session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "handoff-plan".into(),
                revision: 1,
                document_sha256: Some(hash),
            })
            .unwrap()
            .next()
            .await;

        let (sender, mut event_stream) = session.legacy_channel();
        sender
            .send(Intent::StartPlanExecution {
                plan_id: "handoff-plan".into(),
                revision: 1,
                policy_profile: None,
            })
            .await
            .unwrap();
        let mut observed = Vec::new();
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(2), event_stream.next())
                .await
                .expect("handoff timed out")
        {
            let terminal = matches!(event, AgentEvent::FinalAnswer { .. });
            observed.push(event);
            if terminal {
                break;
            }
        }
        assert!(matches!(
            observed.first(),
            Some(AgentEvent::PlanExecutionStarted { revision: 1, .. })
        ));
        assert!(observed.iter().any(|event| matches!(
            event,
            AgentEvent::Started {
                capability_profile: CapabilityProfile::Coding,
                policy_profile: PolicyProfile::Medium,
                ..
            }
        )));
        assert!(observed.iter().any(|event| matches!(
            event,
            AgentEvent::FinalAnswer { text, .. } if text == "coding complete"
        )));
    }

    #[tokio::test]
    async fn approved_plan_writes_redacted_structured_artifact_and_binds_hash() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        config.api_key_env = Some("RIVO_PLAN_TEST_KEY".into());
        std::env::set_var("RIVO_PLAN_TEST_KEY", "plan-secret-value");
        let session = Session::with_client(config, Arc::new(MockLlm::default()))
            .with_storage(store, "plan-artifact-session");
        let plan = TaskPlan {
            plan_id: "artifact-plan".into(),
            session_id: "plan-artifact-session".into(),
            objective: "调查并实现 plan-secret-value 的替换".into(),
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps: vec![TaskPlanStep {
                step_id: "inspect".into(),
                title: "调查代码".into(),
                description: "读取相关模块并记录证据".into(),
                depends_on: vec![],
                acceptance_criteria: vec!["EvidenceRef 已记录".into()],
                status: TaskPlanStatus::AwaitingApproval,
                evidence_refs: vec!["file:runtime/crates/session/src/lib.rs".into()],
                failure_reason: None,
            }],
            dependencies: vec!["只读工作区".into()],
            acceptance_criteria: vec!["测试通过".into()],
            risks: vec!["需要用户确认".into()],
            evidence_refs: vec!["local:session".into()],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut events = session.legacy_handle(Intent::CreatePlan { plan }).unwrap();
        let _ = events.next().await;
        let approval = events.next().await.unwrap();
        let document_sha256 = match approval {
            AgentEvent::PlanApprovalRequired {
                document_sha256, ..
            } => document_sha256,
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(!workspace.path().join(".rivo").exists());

        assert!(session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "artifact-plan".into(),
                revision: 1,
                document_sha256: None,
            })
            .is_err());
        assert!(!workspace.path().join(".rivo").exists());

        assert!(session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "artifact-plan".into(),
                revision: 1,
                document_sha256: Some("0".repeat(64)),
            })
            .is_err());
        assert!(!workspace.path().join(".rivo").exists());

        let mut events = session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "artifact-plan".into(),
                revision: 1,
                document_sha256: Some(document_sha256.clone()),
            })
            .unwrap();
        let approved = events.next().await.unwrap();
        let artifact_path = match approved {
            AgentEvent::PlanApproved {
                document_sha256: actual,
                artifact_path,
                ..
            } => {
                assert_eq!(actual, document_sha256);
                artifact_path
            }
            other => panic!("unexpected event: {other:?}"),
        };
        let artifact = std::fs::read_to_string(&artifact_path).unwrap();
        assert!(artifact.contains("schema: rivo-plan-v1"));
        assert!(artifact.contains("## Steps"));
        assert!(artifact.contains("## Evidence"));
        assert!(!artifact.contains("plan-secret-value"));
        assert_eq!(
            hex::encode(Sha256::digest(artifact.as_bytes())),
            document_sha256
        );
        assert!(workspace
            .path()
            .join(".rivo/plans/artifact-plan.md")
            .is_file());
        std::env::remove_var("RIVO_PLAN_TEST_KEY");
    }

    #[tokio::test]
    async fn invalid_plan_id_is_rejected_before_artifact_write() {
        let store = rivo_storage::Store::open_memory().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        let session = Session::with_client(config, Arc::new(MockLlm::default()))
            .with_storage(store, "invalid-plan-session");
        let plan = TaskPlan {
            plan_id: "../escape".into(),
            session_id: "invalid-plan-session".into(),
            objective: "非法计划标识".into(),
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps: vec![],
            dependencies: vec![],
            acceptance_criteria: vec![],
            risks: vec![],
            evidence_refs: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut events = session.legacy_handle(Intent::CreatePlan { plan }).unwrap();
        assert!(matches!(
            events.next().await,
            Some(AgentEvent::Error { .. })
        ));
        assert!(!workspace.path().join(".rivo").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plan_artifact_rejects_symlink_targets() {
        use std::os::unix::fs::symlink;

        let store = rivo_storage::Store::open_memory().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut config = base_config();
        config.workspace = workspace.path().to_path_buf();
        let session = Session::with_client(config, Arc::new(MockLlm::default()))
            .with_storage(store, "symlink-plan-session");
        let plan = TaskPlan {
            plan_id: "symlink-plan".into(),
            session_id: "symlink-plan-session".into(),
            objective: "拒绝符号链接目标".into(),
            revision: 0,
            status: TaskPlanStatus::Draft,
            steps: vec![],
            dependencies: vec![],
            acceptance_criteria: vec![],
            risks: vec![],
            evidence_refs: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut events = session.legacy_handle(Intent::CreatePlan { plan }).unwrap();
        let _ = events.next().await;
        let hash = match events.next().await.unwrap() {
            AgentEvent::PlanApprovalRequired {
                document_sha256, ..
            } => document_sha256,
            other => panic!("unexpected event: {other:?}"),
        };
        let plans = workspace.path().join(".rivo/plans");
        std::fs::create_dir_all(&plans).unwrap();
        let outside_file = outside.path().join("outside.md");
        std::fs::write(&outside_file, "outside").unwrap();
        symlink(&outside_file, plans.join("symlink-plan.md")).unwrap();
        assert!(session
            .legacy_handle(Intent::ApprovePlan {
                plan_id: "symlink-plan".into(),
                revision: 1,
                document_sha256: Some(hash),
            })
            .is_err());
        assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "outside");
    }

    #[test]
    fn unsupported_serialized_intents_are_rejected() {
        assert!(matches!(
            serde_json::from_str::<Intent>(r#"{"type":"cancel_turn"}"#),
            Ok(Intent::CancelTurn)
        ));
        for serialized in [
            r#"{"type":"cancel"}"#,
            r#"{"type":"set_options","options":{}}"#,
        ] {
            assert!(
                serde_json::from_str::<Intent>(serialized).is_err(),
                "old serialized intent must be rejected: {serialized}"
            );
        }
    }

    #[test]
    fn locale_is_canonicalized_and_rejects_invalid_schema_values() {
        let mut file = FileConfig::default();
        file.defaults.locale = "ZH-cn".into();
        let resolved =
            ResolvedConfig::resolve(file.clone(), &BTreeMap::new(), ConfigOverrides::default())
                .unwrap();
        assert_eq!(resolved.locale, "zh-CN");
        file.defaults.locale = "not_a_locale".into();
        assert!(
            ResolvedConfig::resolve(file, &BTreeMap::new(), ConfigOverrides::default()).is_err()
        );
    }

    #[test]
    fn turn_options_locale_is_serialized_for_session_journal_replay() {
        let options = TurnOptions {
            locale: "zh-CN".into(),
            ..TurnOptions::default()
        };
        let value = serde_json::to_value(&options).unwrap();
        assert_eq!(value["locale"], "zh-CN");
        let replayed: TurnOptions = serde_json::from_value(value).unwrap();
        assert_eq!(replayed.locale, "zh-CN");
    }

    #[tokio::test]
    async fn durable_locale_is_replayed_without_rewriting_other_turn_options() {
        let session = Arc::new(
            Session::with_client(base_config(), Arc::new(MockLlm::default())).with_storage(
                rivo_storage::Store::open_memory().unwrap(),
                "locale-session",
            ),
        );
        let (sender, mut events) = session.clone().channel();
        sender
            .send(Intent::SetSessionLocale {
                locale: "ZH-cn".into(),
            })
            .await
            .unwrap();
        let event = events.next().await.unwrap();
        assert!(
            matches!(event.event, AgentEvent::SessionLocaleApplied { ref locale } if locale == "zh-CN")
        );
        let defaults = TurnOptions {
            network: true,
            ..TurnOptions::default()
        };
        let (selection, restored) = restore_session_settings(&session, defaults).await.unwrap();
        assert!(selection.is_none());
        assert_eq!(restored.locale, "zh-CN");
        assert!(restored.network);
    }
}

//! Local stdio JSON-RPC bridge for a supervising host such as AgentRemoter.
//!
//! This module deliberately has no listener, authentication, or provider configuration RPCs.
//! It is a thin, bounded projection of `rivo_session::Session` rather than another agent loop.

use anyhow::Context;
use base64::Engine;
use futures::StreamExt;
use rivo_core::MediaRef;
use rivo_core::{
    AgentId, AgentTaskId, CapabilitySummary, SidecarDeploymentType, SidecarNodeContext,
};
use rivo_instance_control::{
    host_capabilities, instance_id_for, load_or_create_node_id, local_endpoint, process_is_running,
    registry_path, sidecar_node_context, validate_control, validate_workspace_path,
    validate_workspace_root, ControlRequest, ControlScope, InstanceInfo, InstanceRegistry,
    InstanceState, PROTOCOL_VERSION as INSTANCE_PROTOCOL_VERSION,
};
use rivo_policy::{CapabilityProfile, PolicyProfile};
use rivo_session::import_media_asset;
use rivo_session::{
    catalog_warnings, commit_model_routing_change, config_file_path, credential_exists,
    credential_names, default_catalog_revision, delete_model_routing_config, merged_model_catalog,
    merged_vendor_catalog, model_catalog_sources, model_lifecycle, plan_model_routing_change,
    plan_model_routing_delete, read_default_catalog, read_file_config, read_model_routing_draft,
    refresh_provider_models, resolve_available_route, set_credential, unset_credential,
    validate_model_routing_config, vendor_catalog_sources, AgentEvent, ApprovalScope,
    ConfigOverrides, CredentialStatus, FileConfig, Intent, ModelRoutingDeletePlan, ModelSelection,
    ProviderKind, ProviderSelection, ResolvedConfig, Session, SessionEvent, SubagentToolActivity,
    TaskPlan, TurnOptions,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

#[path = "app_server_messaging.rs"]
mod messaging;

const PROTOCOL_VERSION: &str = "rivo-app-server-v2";
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_STRING_BYTES: usize = 32 * 1024;
const MAX_EVENT_TEXT_BYTES: usize = 8 * 1024;
const MAX_CACHE_ENTRIES: usize = 256;
/// Durable history is replayed in bounded pages so a large session cannot
/// exceed the JSON-RPC line limit.  The cursor is the projected notification
/// sequence (rather than the storage event sequence), because one AgentEvent
/// can produce more than one host notification.
const MAX_REPLAY_EVENTS: usize = 256;
const MAX_REPLAY_BYTES: usize = MAX_LINE_BYTES - 4 * 1024;
/// Upper bound for host-selected concurrent subagents (FEAT-023 bridge contract).
const MAX_SUBAGENTS: usize = 32;

fn history_store() -> Result<Option<rivo_storage::Store>, RpcError> {
    let root = rivo_session::data_root_dir().ok_or_else(|| {
        RpcError::new(
            "storage_unavailable",
            "local session storage is unavailable",
            true,
        )
    })?;
    let path = root.join("rivo").join("rivo.db");
    if !path.exists() {
        return Ok(None);
    }
    rivo_storage::Store::open_runtime(path)
        .map(Some)
        .map_err(|_| {
            RpcError::new(
                "storage_unavailable",
                "local session storage is unavailable",
                false,
            )
        })
}

fn sidecar_scope(state: &Arc<Mutex<ServerState>>) -> Option<(String, String)> {
    let locked = state.lock().expect("server state lock poisoned");
    let context = locked.sidecar_context.as_ref()?;
    Some((context.node_id.clone(), context.workspace_id.clone()))
}

fn sidecar_lease_expiry() -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::minutes(30))
        .format(&Rfc3339)
        .unwrap_or_else(|_| "2099-01-01T00:00:00Z".to_owned())
}

fn expired_sidecar_turn_senders(state: &Arc<Mutex<ServerState>>) -> Vec<mpsc::Sender<Intent>> {
    let Ok(Some(store)) = history_store() else {
        return Vec::new();
    };
    if store.expire_sidecar_leases().is_err() {
        return Vec::new();
    }
    let mut locked = state.lock().expect("server state lock poisoned");
    let mut senders = Vec::new();
    for (session_ref, session) in &mut locked.sessions {
        let Some(turn) = session.active.as_mut() else {
            continue;
        };
        if turn.interrupted {
            continue;
        }
        if store
            .sidecar_task_by_turn(session_ref, &turn.id)
            .ok()
            .flatten()
            .is_some_and(|task| task.state == "expired")
        {
            turn.interrupted = true;
            senders.push(session.sender.clone());
        }
    }
    senders
}

async fn watch_sidecar_leases(state: std::sync::Weak<Mutex<ServerState>>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        for sender in expired_sidecar_turn_senders(&state) {
            let _ = sender.send(Intent::CancelTurn).await;
        }
    }
}

fn create_sidecar_task(
    state: &Arc<Mutex<ServerState>>,
    session_ref: &str,
    turn_id: &str,
    intent_id: &str,
    intent_fingerprint: &str,
) -> Result<(), RpcError> {
    let Some((_node_id, workspace_id)) = sidecar_scope(state) else {
        return Ok(());
    };
    let Some(store) = history_store()? else {
        return Err(RpcError::new(
            "storage_unavailable",
            "sidecar task storage is unavailable",
            true,
        ));
    };
    let expires_at = sidecar_lease_expiry();
    let task_id = format!("sidecar-{session_ref}-{intent_id}-{turn_id}-{}", utc_now());
    store
        .accept_sidecar_task(
            &rivo_storage::StoredSidecarTask {
                task_id: task_id.clone(),
                intent_id: intent_id.to_owned(),
                workspace_id,
                session_id: session_ref.to_owned(),
                turn_id: turn_id.to_owned(),
                state: "accepted".to_owned(),
                lease_epoch: 1,
                expires_at: expires_at.clone(),
                last_event_sequence: 0,
                tool_call_status: "{}".to_owned(),
                terminal_reason: None,
            },
            &rivo_storage::StoredSidecarLease {
                task_id,
                lease_epoch: 1,
                expires_at,
                state: "accepted".to_owned(),
            },
            intent_fingerprint,
        )
        .map_err(|_| {
            RpcError::new(
                "storage_unavailable",
                "sidecar task and lease could not be persisted",
                true,
            )
        })?;
    Ok(())
}

fn update_sidecar_task(
    session_ref: &str,
    turn_id: &str,
    task_state: &str,
    sequence: u64,
    reason: Option<&str>,
) {
    if turn_id.is_empty() {
        return;
    }
    let Ok(Some(store)) = history_store() else {
        return;
    };
    let Ok(Some(task)) = store.sidecar_task_by_turn(session_ref, turn_id) else {
        return;
    };
    let _ = store.update_sidecar_task(
        &task.task_id,
        task_state,
        task.lease_epoch,
        sequence,
        reason,
    );
}

fn update_sidecar_tool_status(session_ref: &str, turn_id: &str, notifications: &[(String, Value)]) {
    if turn_id.is_empty() {
        return;
    }
    let Ok(Some(store)) = history_store() else {
        return;
    };
    let Ok(Some(task)) = store.sidecar_task_by_turn(session_ref, turn_id) else {
        return;
    };
    let mut statuses =
        serde_json::from_str::<Map<String, Value>>(&task.tool_call_status).unwrap_or_default();
    for (kind, payload) in notifications {
        if kind != "tool_call" {
            continue;
        }
        let Some(tool_call_id) = payload["tool_call_id"].as_str() else {
            continue;
        };
        let Some(status) = payload["status"].as_str() else {
            continue;
        };
        // Persist lifecycle labels only. Tool arguments, observations and
        // provider content must remain outside the recovery index.
        if matches!(
            status,
            "requested" | "running" | "waiting_interaction" | "completed" | "failed"
        ) {
            statuses.insert(tool_call_id.to_owned(), json!(status));
        }
    }
    if statuses.is_empty() {
        return;
    }
    let mut encoded = serde_json::to_string(&statuses).unwrap_or_else(|_| "{}".to_owned());
    while encoded.len() > 16 * 1024 && statuses.len() > 1 {
        let Some(oldest) = statuses.keys().next().cloned() else {
            break;
        };
        statuses.remove(&oldest);
        encoded = serde_json::to_string(&statuses).unwrap_or_else(|_| "{}".to_owned());
    }
    let _ = store.update_sidecar_tool_status(&task.task_id, &encoded);
}

fn update_sidecar_event_sequence(session_ref: &str, turn_id: &str, sequence: u64) {
    if turn_id.is_empty() {
        return;
    }
    let Ok(Some(store)) = history_store() else {
        return;
    };
    let Ok(Some(task)) = store.sidecar_task_by_turn(session_ref, turn_id) else {
        return;
    };
    let _ = store.update_sidecar_task(
        &task.task_id,
        &task.state,
        task.lease_epoch,
        sequence,
        task.terminal_reason.as_deref(),
    );
}

fn cleanup_artifact_set(
    store: &rivo_storage::Store,
    artifact_store: &rivo_storage::ArtifactStore,
    artifacts: Vec<rivo_storage::StoredArtifact>,
) {
    for artifact in artifacts {
        let state = if artifact_store.cleanup(&artifact).is_ok() {
            "deleted"
        } else {
            // Keep the metadata auditable and never broaden cleanup to the
            // user's Workspace when a runtime-owned path cannot be removed.
            "cleanup_failed"
        };
        let _ = store.update_artifact_state(
            &artifact.artifact_id,
            &artifact.node_id,
            &artifact.workspace_id,
            &artifact.session_id,
            state,
        );
    }
}

fn cleanup_sidecar_artifacts_for_session(state: &Arc<Mutex<ServerState>>, session_ref: &str) {
    let Some((node_id, workspace_id)) = sidecar_scope(state) else {
        return;
    };
    let (Some(root), Ok(Some(store))) = (
        state
            .lock()
            .expect("server state lock poisoned")
            .workspace_root
            .clone(),
        history_store(),
    ) else {
        return;
    };
    let Ok(artifacts) = store.artifacts_for_scope(&node_id, &workspace_id, session_ref) else {
        return;
    };
    cleanup_artifact_set(&store, &rivo_storage::ArtifactStore::new(root), artifacts);
}

fn cleanup_expired_sidecar_artifacts(
    database: &Path,
    workspace_root: &Path,
    node_id: &str,
    workspace_id: &str,
) {
    let Ok(store) = rivo_storage::Store::open_runtime(database) else {
        return;
    };
    let artifact_store = rivo_storage::ArtifactStore::new(workspace_root);
    if let Ok(artifacts) = store.expired_artifacts(node_id, workspace_id) {
        cleanup_artifact_set(&store, &artifact_store, artifacts);
    }
    let Ok(sessions) = store.sidecar_sessions_needing_artifact_cleanup() else {
        return;
    };
    for session_id in sessions {
        let Ok(artifacts) = store.artifacts_for_scope(node_id, workspace_id, &session_id) else {
            continue;
        };
        cleanup_artifact_set(&store, &artifact_store, artifacts);
    }
}

/// Build the same host event envelope used by live notifications from the
/// redacted AgentEvents persisted in SQLite.  Projection is deliberately done
/// in one pass so tool-call closure and message-start state match a live turn.
fn durable_history_notifications(session_ref: &str) -> Result<Vec<Value>, RpcError> {
    let store = history_store()?
        .ok_or_else(|| RpcError::new("session_not_found", "session was not found", false))?;
    durable_history_notifications_from_store(session_ref, &store)
}

fn durable_history_notifications_from_store(
    session_ref: &str,
    store: &rivo_storage::Store,
) -> Result<Vec<Value>, RpcError> {
    let stored = store.replay(session_ref).map_err(|_| {
        RpcError::new(
            "storage_unavailable",
            "local session history is unavailable",
            false,
        )
    })?;
    if stored.is_empty()
        && !store
            .list_sessions()
            .map_err(|_| {
                RpcError::new(
                    "storage_unavailable",
                    "local session history is unavailable",
                    false,
                )
            })?
            .iter()
            .any(|session| session.session_id == session_ref)
    {
        return Err(RpcError::new(
            "session_not_found",
            "session was not found",
            false,
        ));
    }

    let messages = store.replay_messages(session_ref).map_err(|_| {
        RpcError::new(
            "storage_unavailable",
            "local session history is unavailable",
            false,
        )
    })?;
    project_durable_history(session_ref, &stored, &messages)
}

fn project_durable_history(
    session_ref: &str,
    stored: &[rivo_storage::StoredEvent],
    messages: &[rivo_storage::StoredMessage],
) -> Result<Vec<Value>, RpcError> {
    let (sender, _intents) = mpsc::channel(1);
    let mut projection = replay_session_state(sender);
    let mut notifications = Vec::new();
    let mut messages_by_turn = messages
        .iter()
        .filter(|message| message.role == "user")
        .fold(
            HashMap::<&str, Vec<&rivo_storage::StoredMessage>>::new(),
            |mut grouped, message| {
                grouped
                    .entry(message.turn_id.as_str())
                    .or_default()
                    .push(message);
                grouped
            },
        );
    for event in stored {
        if let Some(turn_messages) = messages_by_turn.remove(event.turn_id.as_str()) {
            for message in turn_messages {
                let (text, truncated, original_bytes) =
                    bounded_text_with_keys(&message.body, MAX_EVENT_TEXT_BYTES, &[]);
                let seq = notifications.len() as u64 + 1;
                notifications.push(event_notification(
                    session_ref,
                    &event.turn_id,
                    seq,
                    "user_message",
                    json!({"text":text,"truncated":truncated,"original_bytes":original_bytes}),
                ));
            }
        }
        let agent_event: AgentEvent = serde_json::from_str(&event.body).map_err(|_| {
            RpcError::new(
                "storage_unavailable",
                "local session history is unavailable",
                false,
            )
        })?;
        // A live session marks the turn active before the first event arrives.
        // Reconstruct that state so ModelDelta/FinalAnswer and open tool calls
        // receive the same projection as they did during execution.
        let turn_changed = projection
            .active
            .as_ref()
            .is_none_or(|turn| turn.id != event.turn_id);
        if turn_changed && !event.turn_id.is_empty() {
            projection.active = Some(TurnState {
                id: event.turn_id.clone(),
                interrupted: false,
                message_started: false,
            });
            projection.interactions.clear();
            projection.subagent_tasks.clear();
            projection.open_tool_calls.clear();
        }
        let items = event_notifications(&agent_event, &mut projection, &event.turn_id);
        for (kind, payload) in items {
            let seq = notifications.len() as u64 + 1;
            notifications.push(event_notification(
                session_ref,
                &event.turn_id,
                seq,
                &kind,
                payload,
            ));
        }
        if !event.turn_id.is_empty()
            && matches!(
                agent_event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            )
        {
            projection.active = None;
            projection.interactions.clear();
            projection.subagent_tasks.clear();
        }
    }
    // A persisted user message can outlive a journal that failed before its
    // first AgentEvent. Preserve it rather than silently dropping the prompt.
    let orphan_turns = messages_by_turn.keys().copied().collect::<HashSet<_>>();
    for message in messages
        .iter()
        .filter(|message| message.role == "user" && orphan_turns.contains(message.turn_id.as_str()))
    {
        let (text, truncated, original_bytes) =
            bounded_text_with_keys(&message.body, MAX_EVENT_TEXT_BYTES, &[]);
        let seq = notifications.len() as u64 + 1;
        notifications.push(event_notification(
            session_ref,
            &message.turn_id,
            seq,
            "user_message",
            json!({"text":text,"truncated":truncated,"original_bytes":original_bytes}),
        ));
    }
    Ok(notifications)
}

fn durable_replay_page(notifications: Vec<Value>, from_seq: u64) -> Vec<Value> {
    let mut page = Vec::new();
    for event in notifications.into_iter().filter(|event| {
        event["params"]["seq"]
            .as_u64()
            .is_some_and(|seq| seq > from_seq)
    }) {
        if page.len() >= MAX_REPLAY_EVENTS {
            break;
        }
        let mut candidate = page.clone();
        candidate.push(event.clone());
        let response = json!({
            "session_ref": event["params"]["session_ref"].clone(),
            "from_seq": from_seq,
            "next_seq": event["params"]["seq"].clone(),
            "events": candidate,
        });
        if !page.is_empty()
            && serde_json::to_vec(&response).is_ok_and(|bytes| bytes.len() > MAX_REPLAY_BYTES)
        {
            break;
        }
        page.push(event);
    }
    page
}

fn replay_session_state(sender: mpsc::Sender<Intent>) -> SessionState {
    SessionState {
        session: None,
        sender,
        seq: 0,
        active: None,
        interactions: HashMap::new(),
        subagent_tasks: HashMap::new(),
        open_tool_calls: BTreeMap::new(),
        default_settings: RuntimeSettings::default(),
        closing: false,
        redaction_keys: Vec::new(),
        inbound: messaging::InboundMode::default(),
    }
}

#[derive(Debug, Clone)]
struct RpcError {
    code: &'static str,
    message: &'static str,
    retryable: bool,
    /// Human-readable cause for configuration errors. Never carries secrets:
    /// config.toml holds credential names only.
    detail: Option<String>,
}

impl RpcError {
    fn new(code: &'static str, message: &'static str, retryable: bool) -> Self {
        Self {
            code,
            message,
            retryable,
            detail: None,
        }
    }
    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail: String = detail.into();
        self.detail = Some(detail.chars().take(1024).collect());
        self
    }
    fn invalid() -> Self {
        Self::new("invalid_request", "request is invalid", false)
    }
    fn method() -> Self {
        Self::new("unsupported_method", "method is not supported", false)
    }
}

#[derive(Clone)]
struct CachedResponse {
    semantic: Value,
    result: Result<Value, RpcError>,
}

struct TurnState {
    id: String,
    interrupted: bool,
    message_started: bool,
}

#[derive(Clone)]
enum InteractionKind {
    Confirmation,
    Input,
}

/// Bridge-side view of a subagent task: the root `delegate_task` call it belongs
/// to and child tool calls that have not reached `completed|failed` yet.
#[derive(Default)]
struct SubagentTaskState {
    parent_call_id: String,
}

#[derive(Clone)]
struct InteractionState {
    turn_id: String,
    kind: InteractionKind,
    option_ids: HashSet<String>,
    allow_freeform: bool,
}

struct SessionState {
    session: Option<Arc<Session>>,
    sender: mpsc::Sender<Intent>,
    seq: u64,
    active: Option<TurnState>,
    interactions: HashMap<String, InteractionState>,
    /// Active subagent tasks of the current turn, keyed by task id.
    subagent_tasks: HashMap<String, SubagentTaskState>,
    /// Projected tool calls of the current turn that have not reached
    /// `completed|failed`, keyed by tool_call_id; the value is the payload
    /// identity (name and subagent origin) used to close them.
    open_tool_calls: BTreeMap<String, Value>,
    default_settings: RuntimeSettings,
    closing: bool,
    redaction_keys: Vec<String>,
    /// FEAT-037 inbound mode for messages from other hosted sessions.
    inbound: messaging::InboundMode,
}

#[derive(Default)]
struct ServerState {
    initialized: bool,
    sessions: HashMap<String, SessionState>,
    idempotency: HashMap<String, CachedResponse>,
    idempotency_order: VecDeque<String>,
    instance_registry_path: Option<PathBuf>,
    child_parents: HashMap<String, String>,
    instance_id: Option<String>,
    shutdown_requested: bool,
    event_broadcast: Option<broadcast::Sender<Value>>,
    event_history: HashMap<String, VecDeque<Value>>,
    credential_upsert_allowed: bool,
    sidecar_context: Option<SidecarNodeContext>,
    workspace_root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeParams {
    protocol_version: String,
    client: ClientInfo,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientInfo {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartParams {
    working_directory: String,
    #[serde(default)]
    runtime_settings: RuntimeSettings,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeParams {
    session_ref: String,
    working_directory: String,
    #[serde(default)]
    runtime_settings: RuntimeSettings,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnStartParams {
    session_ref: String,
    prompt: String,
    #[serde(default)]
    media_refs: Vec<MediaRef>,
    #[serde(default)]
    runtime_settings: RuntimeSettings,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptParams {
    session_ref: String,
    turn_id: String,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadSendParams {
    session_ref: String,
    #[serde(alias = "message")]
    text: String,
    #[serde(default)]
    media_refs: Vec<MediaRef>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaImportParams {
    session_ref: String,
    source_path: String,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactAccessParams {
    session_ref: String,
    artifact_id: String,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadForkParams {
    parent_session_ref: String,
    working_directory: String,
    #[serde(default)]
    runtime_settings: RuntimeSettings,
    #[serde(default)]
    initial_prompt: Option<String>,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InteractionParams {
    session_ref: String,
    interaction_id: String,
    decision: String,
    #[serde(default)]
    selected_option_id: Option<String>,
    #[serde(default)]
    freeform_text: Option<String>,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseParams {
    session_ref: String,
    idempotency_key: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleParams {
    #[serde(default)]
    instance_id: Option<String>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRoutingPreviewParams {
    vendor: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRoutingReadParams {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRoutingCommitParams {
    revision: String,
    config: FileConfig,
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRoutingDeleteParams {
    target_kind: String,
    target_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRoutingDeleteCommitParams {
    plan: ModelRoutingDeletePlan,
    confirmation_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialUpsertParams {
    name: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialListParams {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialDeleteParams {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderDiscoveryRefreshParams {
    revision: String,
    provider_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogReadParams {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryListParams {
    session_ref: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryConfirmParams {
    session_ref: String,
    candidate_id: String,
    body_sha256: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryMutationParams {
    session_ref: String,
    memory_id: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryRejectParams {
    session_ref: String,
    candidate_id: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanListParams {
    session_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanReadParams {
    session_ref: String,
    plan_id: String,
    #[serde(default)]
    revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanApproveParams {
    session_ref: String,
    plan_id: String,
    revision: u64,
    #[serde(default)]
    document_sha256: Option<String>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRejectParams {
    session_ref: String,
    plan_id: String,
    revision: u64,
    #[serde(default)]
    reason: Option<String>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanReviseParams {
    session_ref: String,
    #[serde(default)]
    plan: Option<TaskPlan>,
    feedback: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanStartParams {
    session_ref: String,
    plan_id: String,
    revision: u64,
    #[serde(default)]
    permission_preset: Option<String>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanCancelParams {
    session_ref: String,
    plan_id: String,
    idempotency_key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeSettings {
    /// Canonical BCP 47 locale for UI-hosted default model replies.
    #[serde(default)]
    response_language: Option<String>,
    #[serde(default)]
    vendor: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    auto_provider: bool,
    #[serde(default)]
    collaboration_mode: Option<String>,
    #[serde(default)]
    permission_preset: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Explicitly enables bounded HTTPS read tools for this turn.
    #[serde(default)]
    network: bool,
    /// Names of preconfigured MCP servers to expose for this run. Hosts cannot provide URLs
    /// or credentials through the app-server protocol.
    #[serde(default)]
    mcp_servers: Vec<String>,
    /// Exact preconfigured toolsets to expose for each selected MCP server.
    #[serde(default)]
    mcp_toolsets: BTreeMap<String, BTreeSet<String>>,
    /// FEAT-023 internal subagents. Omitted values follow the local config defaults.
    #[serde(default)]
    subagents_enabled: Option<bool>,
    #[serde(default)]
    max_subagents: Option<usize>,
    /// FEAT-037 session messaging tools for the turn. Off unless the host enables it.
    #[serde(default)]
    session_messaging: Option<bool>,
}

fn sidecar_workspace_root() -> anyhow::Result<PathBuf> {
    if let Some(value) = std::env::var_os("RIVO_WORKSPACE") {
        return validate_workspace_root(&PathBuf::from(value))
            .map_err(|error| anyhow::anyhow!("invalid RIVO_WORKSPACE binding: {error}"));
    }
    // Test and embedded hosts provide an isolated config root. If that root
    // has the conventional sibling workspace, bind it at startup as well;
    // request paths are still checked against this one canonical root.
    if let Some(config) = std::env::var_os("RIVO_CONFIG") {
        if let Some(root) = PathBuf::from(config)
            .parent()
            .and_then(std::path::Path::parent)
            .map(|parent| parent.join("workspace"))
            .filter(|candidate| candidate.is_dir())
        {
            return validate_workspace_root(&root)
                .map_err(|error| anyhow::anyhow!("invalid workspace binding: {error}"));
        }
    }
    validate_workspace_root(&std::env::current_dir()?)
        .map_err(|error| anyhow::anyhow!("invalid current workspace binding: {error}"))
}

pub async fn run(args: Vec<String>) -> anyhow::Result<()> {
    if matches!(args.as_slice(), [help] if matches!(help.as_str(), "--help" | "-h")) {
        println!(
            "{}",
            crate::ui::text(
                "cli-app-server-help",
                "Start the local JSON-RPC bridge.\n\nUsage: rivo app-server <--stdio|--pipe>\n\nOptions:\n      --stdio  Transport JSON-RPC over stdin/stdout\n      --pipe   Serve locally over a Windows named pipe\n  -h, --help   Show help",
            )
        );
        return Ok(());
    }
    let pipe_mode = args.as_slice() == ["--pipe"];
    if args.as_slice() != ["--stdio"] && !pipe_mode {
        anyhow::bail!("usage: rivo app-server --stdio|--pipe");
    }
    let data_root = std::env::var_os("RIVO_DATA_DIR")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .context("无法确定实例数据目录")?;
    let workspace_root = sidecar_workspace_root()?;
    let _runtime_lease = rivo_storage::SidecarRuntimeLease::acquire(&data_root)?;
    // Perform restart recovery exactly once for this Sidecar process. Request
    // handlers use `open_runtime`, which deliberately does not reinterpret
    // every short-lived read connection as another restart.
    let history_path = data_root.join("rivo").join("rivo.db");
    if history_path.exists() {
        let store = rivo_storage::Store::open_runtime(&history_path)?;
        store.recover_sidecar_tasks()?;
    }
    let endpoint = local_endpoint(&data_root);
    let registry = InstanceRegistry::new(registry_path(&data_root));
    let started_at_unix_ms =
        time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u128 / 1_000_000;
    let instance_id = instance_id_for(
        &endpoint.to_string_lossy(),
        std::process::id(),
        started_at_unix_ms,
    );
    let node_id = load_or_create_node_id(&data_root)
        .map_err(|error| anyhow::anyhow!("cannot load Sidecar node identity: {error}"))?;
    let workspace_id = format!(
        "workspace-{}",
        hex::encode(Sha256::digest(workspace_root.to_string_lossy().as_bytes()))[..24].to_owned()
    );
    let mut sidecar_context = sidecar_node_context(
        &node_id,
        if pipe_mode {
            SidecarDeploymentType::Desktop
        } else {
            SidecarDeploymentType::LocalWorker
        },
        &workspace_id,
        &workspace_root,
        "workspace_write",
        CapabilitySummary {
            capabilities: vec!["text".into(), "streaming".into()],
        },
        CapabilitySummary {
            capabilities: host_capabilities().capabilities,
        },
    )?;
    sidecar_context.resource_limits.max_concurrent_tasks = 32;
    cleanup_expired_sidecar_artifacts(&history_path, &workspace_root, &node_id, &workspace_id);
    registry.register(InstanceInfo {
        instance_id: instance_id.clone(),
        pid: std::process::id(),
        protocol_version: INSTANCE_PROTOCOL_VERSION.into(),
        endpoint: endpoint.to_string_lossy().into_owned(),
        workspace: workspace_root.to_string_lossy().into_owned(),
        started_at_unix_ms,
        state: InstanceState::Running,
        capabilities: vec![
            "thread_read".into(),
            "thread_subscribe".into(),
            "process_control".into(),
        ],
    })?;
    let state = Arc::new(Mutex::new(ServerState {
        instance_registry_path: Some(registry_path(&data_root)),
        instance_id: Some(instance_id.clone()),
        credential_upsert_allowed: !pipe_mode,
        sidecar_context: Some(sidecar_context),
        workspace_root: Some(workspace_root),
        ..ServerState::default()
    }));
    let (event_tx, _) = broadcast::channel::<Value>(512);
    state
        .lock()
        .expect("server state lock poisoned")
        .event_broadcast = Some(event_tx);
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(512);
    tokio::spawn(watch_sidecar_leases(Arc::downgrade(&state)));
    if pipe_mode {
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let pipe_name = endpoint.to_string_lossy().into_owned();
            // Pipe clients receive events from the broadcast bus; the stdio
            // queue must still be drained or event emission blocks once full.
            tokio::spawn(async move { while out_rx.recv().await.is_some() {} });
            let mut first = true;
            'pipe: loop {
                let server = ServerOptions::new()
                    .first_pipe_instance(first)
                    .create(&pipe_name)
                    .map_err(std::io::Error::other)?;
                first = false;
                server.connect().await.map_err(std::io::Error::other)?;
                let receiver = state
                    .lock()
                    .expect("server state lock poisoned")
                    .event_broadcast
                    .as_ref()
                    .expect("pipe event bus initialized")
                    .subscribe();
                tokio::spawn(serve_pipe_connection(
                    server,
                    state.clone(),
                    out_tx.clone(),
                    receiver,
                ));
                if state
                    .lock()
                    .expect("server state lock poisoned")
                    .shutdown_requested
                {
                    break 'pipe;
                }
            }
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("Named Pipe transport is only available on Windows");
        }
        #[cfg(windows)]
        {
            close_all(&state).await;
            let _ = registry.unregister(&instance_id);
            return Ok(());
        }
    }
    let (drain_tx, mut drain_rx) = tokio::sync::oneshot::channel::<()>();
    let mut writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut draining = false;
        loop {
            let message = if draining {
                out_rx.recv().await
            } else {
                tokio::select! {
                    biased;
                    _ = &mut drain_rx => {
                        // Stop new sends but retain every response already
                        // queued before stdin EOF, including initialize.
                        out_rx.close();
                        draining = true;
                        out_rx.recv().await
                    }
                    message = out_rx.recv() => message,
                }
            };
            let Some(message) = message else { break };
            let Ok(mut bytes) = serde_json::to_vec(&message) else {
                continue;
            };
            bytes.push(b'\n');
            if stdout.write_all(&bytes).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("read app-server stdin")? {
        let response = handle_line(&line, state.clone(), out_tx.clone()).await;
        if let Some(message) = response {
            if out_tx.send(message).await.is_err() {
                break;
            }
        }
    }
    // EOF is a transport disconnect, not an explicit cancellation. Do not
    // open another SQLite writer or send CancelTurn here: the process is
    // exiting, and the next Sidecar startup classifies persisted accepted or
    // running tasks as interrupted/unknown_side_effect for explicit handling.
    // Session runtimes intentionally retain output senders so a client
    // disconnect does not cancel accepted work. Those senders would otherwise
    // keep the writer channel alive forever and prevent the transport process
    // from exiting. Close the receiver and drain queued replies first; bound
    // the wait in case the disconnected peer also stopped reading stdout.
    let _ = drain_tx.send(());
    drop(out_tx);
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
    let _ = registry.unregister(&instance_id);
    Ok(())
}

#[cfg(windows)]
async fn serve_pipe_connection(
    server: tokio::net::windows::named_pipe::NamedPipeServer,
    state: Arc<Mutex<ServerState>>,
    out: mpsc::Sender<Value>,
    mut events: broadcast::Receiver<Value>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(server);
    let mut lines = BufReader::new(reader).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                if let Some(message) = handle_line(&line, state.clone(), out.clone()).await {
                    let mut bytes = serde_json::to_vec(&message)?;
                    bytes.push(b'\n');
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
                if state.lock().expect("server state lock poisoned").shutdown_requested { break; }
            }
            event = events.recv() => {
                let Ok(event) = event else { continue; };
                let mut bytes = serde_json::to_vec(&event)?;
                bytes.push(b'\n');
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

async fn handle_line(
    line: &str,
    state: Arc<Mutex<ServerState>>,
    out: mpsc::Sender<Value>,
) -> Option<Value> {
    if line.len() > MAX_LINE_BYTES {
        return Some(error_response(Value::Null, RpcError::invalid()));
    }
    let raw: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return Some(error_response(Value::Null, RpcError::invalid())),
    };
    let object = match raw.as_object() {
        Some(object) => object,
        None => return Some(error_response(Value::Null, RpcError::invalid())),
    };
    if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
        return Some(error_response(
            object.get("id").cloned().unwrap_or(Value::Null),
            RpcError::invalid(),
        ));
    }
    let id = match object.get("id") {
        Some(Value::String(value)) if bounded(value) => Value::String(value.clone()),
        Some(Value::Number(value)) => Value::Number(value.clone()),
        _ => return Some(error_response(Value::Null, RpcError::invalid())),
    };
    let method = match object.get("method").and_then(Value::as_str) {
        Some(value) if bounded(value) => value,
        _ => return Some(error_response(id, RpcError::invalid())),
    };
    let params = object
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    if !params.is_object()
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params"))
    {
        return Some(error_response(id, RpcError::invalid()));
    }
    let result = dispatch(method, params, state, out).await;
    Some(match result {
        Ok(value) => success_response(id, value),
        Err(error) => error_response(id, error),
    })
}

async fn dispatch(
    method: &str,
    params: Value,
    state: Arc<Mutex<ServerState>>,
    out: mpsc::Sender<Value>,
) -> Result<Value, RpcError> {
    let method_alias = match method {
        "health/read" => "health",
        _ => method,
    };
    if matches!(
        method,
        "instance/stop"
            | "instance/restart"
            | "session/send"
            | "session/fork"
            | "session/message/respond"
            | "session/message/configure"
    ) {
        let confirmation = params
            .get("confirmation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let idempotency_key = params
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        validate_control(&ControlRequest {
            scope: if method.starts_with("session/") {
                ControlScope::Thread
            } else {
                ControlScope::Lifecycle
            },
            confirmation,
            idempotency_key,
        })
        .map_err(|_| {
            RpcError::new(
                "confirmation_required",
                "confirmation and idempotency key are required",
                false,
            )
        })?;
    }
    if method_alias != "initialize"
        && !state
            .lock()
            .expect("server state lock poisoned")
            .initialized
    {
        return Err(RpcError::new(
            "invalid_request",
            "initialize must be called first",
            false,
        ));
    }
    match method_alias {
        "initialize" => {
            let p: InitializeParams = parse(params)?;
            if !bounded(&p.protocol_version)
                || !bounded(&p.client.name)
                || !bounded(&p.client.version)
            {
                return Err(RpcError::invalid());
            }
            if p.protocol_version != PROTOCOL_VERSION {
                return Err(RpcError::new(
                    "unsupported_protocol_version",
                    "protocol version is not supported",
                    false,
                ));
            }
            state
                .lock()
                .expect("server state lock poisoned")
                .initialized = true;
            let (credential_upsert_allowed, sidecar_context) = {
                let locked = state.lock().expect("server state lock poisoned");
                (
                    locked.credential_upsert_allowed,
                    locked.sidecar_context.clone(),
                )
            };
            Ok(
                json!({"server":{"name":"rivo","version":env!("RIVO_RELEASE_VERSION"),"build":env!("RIVO_BUILD_COMMIT")},"capabilities":{"supports_resume":true,"supports_interrupt":true,"supports_history_replay":true,"emits_tool_events":true,"emits_media_events":true,"supports_media_refs":true,"supports_media_import":true,"supports_artifacts":true,"artifact":{"read":true,"delete":true,"logical_refs_only":true},"supports_image_input":true,"supports_approval":true,"supports_plan_workflow":true,"plan":{"list":true,"read":true,"approve":true,"reject":true,"revise":true,"start":true,"cancel":true,"artifact_runtime_owned":true},"supports_subagents":true,"sidecar":sidecar_context,"model_routing_config":{"read":true,"preview":true,"write":true,"delete":true,"raw_toml":true,"catalog":true,"discovery_refresh":true,"credential_list":true,"credential_delete":true,"error_detail":true,"credential_upsert":credential_upsert_allowed},"memory":{"list":true,"confirm":true,"reject":true,"delete":true}},"limits":{"max_request_bytes":MAX_LINE_BYTES,"max_string_bytes":MAX_STRING_BYTES,"idempotency_cache_entries":MAX_CACHE_ENTRIES,"max_media_bytes":rivo_session::MAX_INPUT_MEDIA_BYTES,"max_media_count":rivo_session::MAX_INPUT_MEDIA_COUNT},"supported_runtime_settings":{"vendor":true,"model":true,"provider_id":true,"auto_provider":true,"collaboration_mode":["plan","code"],"permission_preset":["read_only","workspace_write","full_access"],"network":true,"mcp_servers":true,"mcp_toolsets":true,"reasoning_effort":["default","low","medium","high"],"subagents_enabled":true,"max_subagents":{"min":1,"max":MAX_SUBAGENTS}}}),
            )
        }
        "model_routing/read" => {
            let _: ModelRoutingReadParams = parse(params)?;
            let path = local_config_path()?;
            let draft = read_model_routing_draft(&path).map_err(map_config_error)?;
            // config.toml only names credentials; secrets live in credentials.toml.
            let raw_toml = rivo_session::injected_config_content()
                .or_else(|| std::fs::read_to_string(&path).ok());
            Ok(
                json!({"revision": draft.revision, "config": draft.config, "secrets_redacted": true, "raw_toml": raw_toml}),
            )
        }
        "model_routing/preview" => {
            let p: ModelRoutingPreviewParams = parse(params)?;
            if !bounded(&p.vendor)
                || p.model.as_deref().is_some_and(|model| !bounded(model))
                || p.provider_id
                    .as_deref()
                    .is_some_and(|provider| !bounded(provider))
            {
                return Err(RpcError::invalid());
            }
            let route = resolve_available_route(
                &read_file_config(&local_config_path()?).map_err(map_config_error)?,
                Some(&p.vendor),
                p.model.as_deref(),
                p.provider_id.as_deref(),
            )
            .map_err(map_config_error)?;
            Ok(json!(route))
        }
        "model_routing/validate" => {
            let p: ModelRoutingCommitParams = parse(params)?;
            validate_model_routing_config(&p.config).map_err(map_config_error)?;
            let plan = plan_model_routing_change(&local_config_path()?, &p.revision, &p.config)
                .map_err(map_config_error)?;
            Ok(json!({"valid":true,"revision":p.revision,"change_plan":plan}))
        }
        "model_routing/commit" => {
            let p: ModelRoutingCommitParams = parse(params)?;
            let draft = commit_model_routing_change(
                &local_config_path()?,
                &p.revision,
                &p.config,
                p.confirmation_token.as_deref(),
            )
            .map_err(map_config_error)?;
            Ok(json!({"revision":draft.revision,"config":draft.config,"secrets_redacted":true}))
        }
        "model_routing/delete_plan" => {
            let p: ModelRoutingDeleteParams = parse(params)?;
            if !bounded(&p.target_kind) || !bounded(&p.target_id) {
                return Err(RpcError::invalid());
            }
            let plan =
                plan_model_routing_delete(&local_config_path()?, &p.target_kind, &p.target_id)
                    .map_err(map_config_error)?;
            Ok(json!(plan))
        }
        "model_routing/delete_commit" => {
            let p: ModelRoutingDeleteCommitParams = parse(params)?;
            let draft =
                delete_model_routing_config(&local_config_path()?, &p.plan, &p.confirmation_token)
                    .map_err(map_config_error)?;
            Ok(json!({"revision":draft.revision,"config":draft.config,"secrets_redacted":true}))
        }
        "credential/upsert" => {
            let p: CredentialUpsertParams = parse(params)?;
            if !state
                .lock()
                .expect("server state lock poisoned")
                .credential_upsert_allowed
            {
                return Err(RpcError::new(
                    "credential_transport_forbidden",
                    "credential upsert requires desktop stdio",
                    false,
                ));
            }
            if !bounded(&p.name) || !bounded(&p.api_key) || p.api_key.trim().is_empty() {
                return Err(RpcError::invalid());
            }
            set_credential(&local_config_path()?, &p.name, &p.api_key).map_err(map_config_error)?;
            Ok(json!({"name":p.name,"stored":true}))
        }
        "credential/list" => {
            let _: CredentialListParams = parse(params)?;
            let path = local_config_path()?;
            let draft = read_model_routing_draft(&path).map_err(map_config_error)?;
            let stored = credential_names(&path).map_err(map_config_error)?;
            let mut references: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (id, provider) in &draft.config.providers {
                if let Some(name) = provider.auth.credential.as_deref() {
                    references
                        .entry(name.to_owned())
                        .or_default()
                        .push(id.clone());
                }
            }
            let stored_names = stored
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<BTreeSet<_>>();
            let credentials = stored
                .iter()
                .map(|(name, status)| {
                    json!({
                        "name": name,
                        "encrypted": *status == CredentialStatus::Encrypted,
                        "referenced_by": references.get(name).cloned().unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>();
            let missing = references
                .iter()
                .filter(|(name, _)| !stored_names.contains(name.as_str()))
                .map(|(name, providers)| json!({"name": name, "referenced_by": providers}))
                .collect::<Vec<_>>();
            Ok(json!({"revision": draft.revision, "credentials": credentials, "missing": missing}))
        }
        "credential/delete" => {
            let p: CredentialDeleteParams = parse(params)?;
            if !bounded(&p.name) || p.name.trim().is_empty() {
                return Err(RpcError::invalid());
            }
            let path = local_config_path()?;
            let draft = read_model_routing_draft(&path).map_err(map_config_error)?;
            let referenced_by = draft
                .config
                .providers
                .iter()
                .filter(|(_, provider)| {
                    provider.auth.credential.as_deref() == Some(p.name.as_str())
                })
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>();
            if !referenced_by.is_empty() {
                return Err(RpcError::new(
                    "credential_in_use",
                    "credential is still referenced by providers",
                    false,
                )
                .with_detail(referenced_by.join(", ")));
            }
            if !credential_exists(&path, &p.name).map_err(map_config_error)? {
                return Err(RpcError::new(
                    "credential_not_found",
                    "credential was not found",
                    false,
                ));
            }
            unset_credential(&path, &p.name).map_err(map_config_error)?;
            Ok(json!({"name": p.name, "deleted": true}))
        }
        "provider/discovery_refresh" => {
            let p: ProviderDiscoveryRefreshParams = parse(params)?;
            if !bounded(&p.revision) || !bounded(&p.provider_id) {
                return Err(RpcError::invalid());
            }
            let draft = refresh_provider_models(&local_config_path()?, &p.revision, &p.provider_id)
                .await
                .map_err(map_config_error)?;
            let discovery = draft
                .config
                .providers
                .get(&p.provider_id)
                .map(|provider| json!(provider.discovery))
                .unwrap_or(Value::Null);
            Ok(
                json!({"revision": draft.revision, "config": draft.config, "discovery": discovery, "secrets_redacted": true}),
            )
        }
        "catalog/read" => {
            let _: CatalogReadParams = parse(params)?;
            let file = read_file_config(&local_config_path()?).map_err(map_config_error)?;
            let builtin = read_default_catalog().map_err(map_config_error)?;
            let vendor_sources = vendor_catalog_sources(&file).map_err(map_config_error)?;
            let model_sources = model_catalog_sources(&file).map_err(map_config_error)?;
            let vendors = merged_vendor_catalog(&file)
                .map_err(map_config_error)?
                .into_iter()
                .map(|(id, entry)| {
                    json!({"id": id, "display_name": entry.display_name, "source": vendor_sources.get(&id)})
                })
                .collect::<Vec<_>>();
            let models = merged_model_catalog(&file)
                .map_err(map_config_error)?
                .into_iter()
                .map(|(id, entry)| {
                    let (status, replaced_by) = model_lifecycle(&builtin, &id);
                    json!({
                        "id": id,
                        "source": model_sources.get(&id),
                        "builtin": builtin.models.get(&id),
                        "entry": entry,
                        "status": status,
                        "replaced_by": replaced_by,
                    })
                })
                .collect::<Vec<_>>();
            let warnings = catalog_warnings(&file).map_err(map_config_error)?;
            let protocols = [
                ProviderKind::OpenAiChatCompletions,
                ProviderKind::OpenAiResponses,
                ProviderKind::ClaudeMessages,
            ]
            .into_iter()
            .map(|kind| json!({"id": kind, "default_base_url": kind.default_base_url()}))
            .collect::<Vec<_>>();
            Ok(json!({
                "catalog_revision": default_catalog_revision(),
                "vendors": vendors,
                "models": models,
                "protocols": protocols,
                "reasoning_efforts": ["low", "medium", "high"],
                "warnings": warnings,
            }))
        }
        "memory/list" => {
            let p: MemoryListParams = parse(params)?;
            valid_ref(&p.session_ref)?;
            if p.mode.as_deref().is_some_and(|mode| !bounded(mode)) {
                return Err(RpcError::invalid());
            }
            let limit = p.limit.unwrap_or(20);
            if limit == 0 || limit > 50 {
                return Err(RpcError::invalid());
            }
            let workspace = session_workspace(&state, &p.session_ref)?;
            let store = history_store()?.ok_or_else(|| {
                RpcError::new(
                    "storage_unavailable",
                    "local memory storage is unavailable",
                    true,
                )
            })?;
            let memories = store
                .list_memories(&workspace, p.mode.as_deref(), limit)
                .map_err(map_memory_storage_error)?;
            let candidates = store
                .list_memory_candidates(&workspace, limit)
                .map_err(map_memory_storage_error)?;
            Ok(json!({
                "session_ref": p.session_ref,
                "workspace": workspace,
                "memories": memories.iter().map(memory_metadata).collect::<Vec<_>>(),
                "candidates": candidates.iter().map(memory_candidate_metadata).collect::<Vec<_>>(),
            }))
        }
        "memory/confirm" => {
            let p: MemoryConfirmParams = parse(params.clone())?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.candidate_id)?;
            valid_ref(&p.body_sha256)?;
            valid_key(&p.idempotency_key)?;
            cached_or(
                "memory/confirm",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let workspace = session_workspace(&state, &p.session_ref)?;
                    let store = history_store()?.ok_or_else(|| {
                        RpcError::new(
                            "storage_unavailable",
                            "local memory storage is unavailable",
                            true,
                        )
                    })?;
                    let candidate = store
                        .get_memory_candidate(&p.candidate_id)
                        .map_err(map_memory_storage_error)?
                        .ok_or_else(|| {
                            RpcError::new(
                                "memory_not_found",
                                "memory candidate was not found",
                                false,
                            )
                        })?;
                    if candidate.workspace != workspace {
                        return Err(RpcError::new(
                            "ownership_mismatch",
                            "memory candidate belongs to another workspace",
                            false,
                        ));
                    }
                    let memory = store
                        .confirm_memory(&candidate, &p.body_sha256)
                        .map_err(map_memory_storage_error)?;
                    Ok(json!({"memory": memory_metadata(&memory), "confirmed": true}))
                },
            )
            .await
        }
        "memory/reject" => {
            let p: MemoryRejectParams = parse(params.clone())?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.candidate_id)?;
            valid_key(&p.idempotency_key)?;
            cached_or(
                "memory/reject",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let workspace = session_workspace(&state, &p.session_ref)?;
                    let store = history_store()?.ok_or_else(|| {
                        RpcError::new(
                            "storage_unavailable",
                            "local memory storage is unavailable",
                            true,
                        )
                    })?;
                    let candidate = store
                        .get_memory_candidate(&p.candidate_id)
                        .map_err(map_memory_storage_error)?
                        .ok_or_else(|| {
                            RpcError::new(
                                "memory_not_found",
                                "memory candidate was not found",
                                false,
                            )
                        })?;
                    if candidate.workspace != workspace {
                        return Err(RpcError::new(
                            "ownership_mismatch",
                            "memory candidate belongs to another workspace",
                            false,
                        ));
                    }
                    store
                        .reject_memory(&p.candidate_id)
                        .map_err(map_memory_storage_error)?;
                    Ok(json!({"candidate_id": p.candidate_id, "rejected": true}))
                },
            )
            .await
        }
        "memory/delete" => {
            let p: MemoryMutationParams = parse(params.clone())?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.memory_id)?;
            valid_key(&p.idempotency_key)?;
            cached_or(
                "memory/delete",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let workspace = session_workspace(&state, &p.session_ref)?;
                    let store = history_store()?.ok_or_else(|| {
                        RpcError::new(
                            "storage_unavailable",
                            "local memory storage is unavailable",
                            true,
                        )
                    })?;
                    store
                        .delete_memory_scoped(&workspace, &p.memory_id)
                        .map_err(map_memory_storage_error)?;
                    Ok(json!({"memory_id": p.memory_id, "deleted": true}))
                },
            )
            .await
        }
        "health" => Ok(health(&state)),
        "instance/list" => {
            let path = state
                .lock()
                .expect("server state lock poisoned")
                .instance_registry_path
                .clone();
            let instances = path
                .map(|path| {
                    InstanceRegistry::new(path)
                        .prune(process_is_running)
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            Ok(json!({"instances": instances}))
        }
        "instance/read" => {
            let target = params.get("instance_id").and_then(Value::as_str);
            let path = state
                .lock()
                .expect("server state lock poisoned")
                .instance_registry_path
                .clone();
            let instances = path
                .map(|path| InstanceRegistry::new(path).load().unwrap_or_default())
                .unwrap_or_default();
            instances
                .into_iter()
                .find(|instance| target.is_none_or(|id| id == instance.instance_id))
                .map(|instance| json!(instance))
                .ok_or_else(|| RpcError::new("instance_not_found", "instance was not found", false))
        }
        "instance/stop" => {
            let p: LifecycleParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            cached_or(
                "instance/stop",
                "__instance__",
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let current = state
                        .lock()
                        .expect("server state lock poisoned")
                        .instance_id
                        .clone();
                    if p.instance_id
                        .as_ref()
                        .is_some_and(|target| Some(target) != current.as_ref())
                    {
                        return Err(RpcError::new(
                            "instance_not_found",
                            "instance was not found",
                            false,
                        ));
                    }
                    close_all(&state).await;
                    state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .clear();
                    state
                        .lock()
                        .expect("server state lock poisoned")
                        .shutdown_requested = true;
                    Ok(json!({"accepted":true,"stopping":true}))
                },
            )
            .await
        }
        "instance/restart" => {
            let p: LifecycleParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            cached_or(
                "instance/restart",
                "__instance__",
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let current = state
                        .lock()
                        .expect("server state lock poisoned")
                        .instance_id
                        .clone();
                    if p.instance_id
                        .as_ref()
                        .is_some_and(|target| Some(target) != current.as_ref())
                    {
                        return Err(RpcError::new(
                            "instance_not_found",
                            "instance was not found",
                            false,
                        ));
                    }
                    #[cfg(windows)]
                    {
                        let executable = std::env::current_exe().map_err(|_| {
                            RpcError::new(
                                "runtime_unavailable",
                                "current executable is unavailable",
                                true,
                            )
                        })?;
                        std::process::Command::new(executable)
                            .args(["app-server", "--pipe"])
                            .spawn()
                            .map_err(|_| {
                                RpcError::new(
                                    "runtime_unavailable",
                                    "failed to start replacement instance",
                                    true,
                                )
                            })?;
                    }
                    #[cfg(not(windows))]
                    {
                        Err(RpcError::new(
                            "unsupported_method",
                            "instance restart is only available on Windows",
                            false,
                        ))
                    }
                    #[cfg(windows)]
                    {
                        close_all(&state).await;
                        state
                            .lock()
                            .expect("server state lock poisoned")
                            .shutdown_requested = true;
                        Ok(json!({"accepted":true,"restarting":true}))
                    }
                },
            )
            .await
        }
        "workspace/list" => {
            only_keys(&params, &[])?;
            let workspaces = history_store()?
                .map(|store| store.list_workspaces())
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .unwrap_or_default();
            Ok(
                json!({"workspaces": workspaces.iter().map(workspace_metadata).collect::<Vec<_>>() }),
            )
        }
        "workspace/read" => {
            only_keys(&params, &["workspace_id"])?;
            let workspace_id = params
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            valid_ref(workspace_id)?;
            let workspace = history_store()?
                .map(|store| store.get_workspace(workspace_id))
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .ok_or_else(|| {
                    RpcError::new("workspace_not_found", "workspace was not found", false)
                })?;
            Ok(json!({"workspace": workspace_metadata(&workspace)}))
        }
        "project/list" => {
            let projects = history_store()?
                .map(|store| store.list_projects())
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .unwrap_or_default();
            Ok(json!({"projects": projects.iter().map(project_metadata).collect::<Vec<_>>() }))
        }
        "project/read" => {
            let project_id = params
                .get("project_id")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            let project = history_store()?
                .map(|store| store.list_projects())
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .and_then(|projects| {
                    projects
                        .into_iter()
                        .find(|project| project.id == project_id)
                })
                .ok_or_else(|| {
                    RpcError::new("project_not_found", "project was not found", false)
                })?;
            Ok(json!({"project": project_metadata(&project)}))
        }
        "session/list" => {
            only_keys(&params, &["workspace_id"])?;
            let workspace_filter = params
                .get("workspace_id")
                .map(|value| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty() && bounded(value))
                        .map(str::to_owned)
                        .ok_or_else(RpcError::invalid)
                })
                .transpose()?;
            let store = history_store()?;
            let locked = state.lock().expect("server state lock poisoned");
            let active_sessions = locked
                .sessions
                .iter()
                .filter(|(_, session)| match workspace_filter.as_deref() {
                    Some(workspace_id) => store.as_ref().is_some_and(|store| {
                        session.session.as_ref().is_some_and(|session| {
                            store
                                .find_workspace_by_path(session.workspace_path())
                                .ok()
                                .flatten()
                                .is_some_and(|workspace| workspace.id == workspace_id)
                        })
                    }),
                    None => true,
                })
                .map(|(session_ref, session)| {
                    let workspace_id = store.as_ref().and_then(|store| {
                        session.session.as_ref().and_then(|session| {
                            store
                                .find_workspace_by_path(session.workspace_path())
                                .ok()
                                .flatten()
                                .map(|workspace| workspace.id)
                        })
                    });
                    json!({
                        "session_ref": session_ref,
                        "workspace_id": workspace_id,
                        "active": session.active.as_ref().map(|turn| turn.id.clone()),
                        "closing": session.closing,
                        "parent_session_ref": locked.child_parents.get(session_ref),
                    })
                })
                .collect::<Vec<_>>();
            drop(locked);
            let persisted = store
                .map(|store| match workspace_filter {
                    Some(id) => {
                        store.list_sessions_scoped(rivo_storage::SessionScope::WorkspaceId(id))
                    }
                    None => store.list_sessions_scoped(rivo_storage::SessionScope::AllWorkspaces),
                })
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .unwrap_or_default();
            Ok(
                json!({"sessions": persisted.iter().map(session_metadata).collect::<Vec<_>>(), "active_sessions": active_sessions}),
            )
        }
        "session/read" => {
            only_keys(&params, &["session_ref"])?;
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            valid_ref(session_ref)?;
            let locked = state.lock().expect("server state lock poisoned");
            if let Some(session) = locked.sessions.get(session_ref) {
                return Ok(
                    json!({"session_ref": session_ref, "active": session.active.as_ref().map(|turn| turn.id.clone()), "closing": session.closing, "parent_session_ref": locked.child_parents.get(session_ref)}),
                );
            }
            drop(locked);
            let session = history_store()?
                .map(|store| store.list_sessions())
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .and_then(|sessions| {
                    sessions
                        .into_iter()
                        .find(|session| session.session_id == session_ref)
                })
                .ok_or_else(|| {
                    RpcError::new("session_not_found", "session was not found", false)
                })?;
            Ok(json!({"session": session_metadata(&session)}))
        }
        "turn/list" => {
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            let turns = history_store()?
                .map(|store| store.list_turns(session_ref))
                .transpose()
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .unwrap_or_default();
            Ok(
                json!({"session_ref": session_ref, "turns": turns.iter().map(turn_metadata).collect::<Vec<_>>() }),
            )
        }
        "turn/read" => {
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            let turn_id = params
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            let store = history_store()?.ok_or_else(|| {
                RpcError::new("session_not_found", "session was not found", false)
            })?;
            let turn = store
                .list_turns(session_ref)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .into_iter()
                .find(|turn| turn.turn_id == turn_id)
                .ok_or_else(|| RpcError::new("turn_not_found", "turn was not found", false))?;
            let events = store
                .replay(session_ref)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .into_iter()
                .filter(|event| event.turn_id == turn_id)
                .map(event_metadata)
                .collect::<Vec<_>>();
            let messages = store
                .replay_messages(session_ref)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local session storage is unavailable",
                        false,
                    )
                })?
                .into_iter()
                .filter(|message| message.turn_id == turn_id)
                .map(message_metadata)
                .collect::<Vec<_>>();
            let sidecar_task = store
                .sidecar_task_by_turn(session_ref, turn_id)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "sidecar task state is unavailable",
                        false,
                    )
                })?
                .map(|task| {
                    json!({
                        "task_id": task.task_id,
                        "intent_id": task.intent_id,
                        "state": task.state,
                        "lease_epoch": task.lease_epoch,
                        "expires_at": task.expires_at,
                        "last_event_sequence": task.last_event_sequence,
                        "tool_call_status": task.tool_call_status,
                        "terminal_reason": task.terminal_reason,
                    })
                });
            Ok(
                json!({"turn": turn_metadata(&turn), "sidecar_task": sidecar_task, "events": events, "messages": messages}),
            )
        }
        "sidecar/task/read" => {
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            let turn_id = params
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            valid_ref(session_ref)?;
            valid_ref(turn_id)?;
            let store = history_store()?.ok_or_else(|| {
                RpcError::new("session_not_found", "session was not found", false)
            })?;
            let task = store
                .sidecar_task_by_turn(session_ref, turn_id)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "sidecar task state is unavailable",
                        false,
                    )
                })?
                .ok_or_else(|| {
                    RpcError::new("task_not_found", "sidecar task was not found", false)
                })?;
            if sidecar_scope(&state).is_some_and(|(_, workspace)| workspace != task.workspace_id) {
                return Err(RpcError::new(
                    "task_not_found",
                    "sidecar task was not found",
                    false,
                ));
            }
            Ok(json!({
                "task": {
                    "task_id": task.task_id,
                    "intent_id": task.intent_id,
                    "session_id": task.session_id,
                    "turn_id": task.turn_id,
                    "state": task.state,
                    "lease_epoch": task.lease_epoch,
                    "expires_at": task.expires_at,
                    "last_event_sequence": task.last_event_sequence,
                    "tool_call_status": task.tool_call_status,
                    "terminal_reason": task.terminal_reason,
                }
            }))
        }
        "session/subscribe" => {
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            valid_ref(session_ref)?;
            let from_seq = params.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            let locked = state.lock().expect("server state lock poisoned");
            if !locked.sessions.contains_key(session_ref)
                && !locked.event_history.contains_key(session_ref)
            {
                return Err(RpcError::new(
                    "session_not_found",
                    "session was not found",
                    false,
                ));
            }
            let events = locked
                .event_history
                .get(session_ref)
                .into_iter()
                .flat_map(|history| history.iter())
                .filter(|event| {
                    event["params"]["seq"]
                        .as_u64()
                        .is_some_and(|seq| seq > from_seq)
                })
                .cloned()
                .collect::<Vec<_>>();
            let next_seq = locked
                .sessions
                .get(session_ref)
                .map(|session| session.seq)
                .or_else(|| {
                    events
                        .last()
                        .and_then(|event| event["params"]["seq"].as_u64())
                })
                .unwrap_or(from_seq);
            drop(locked);
            if let Ok(Some(store)) = history_store() {
                store
                    .save_event_cursor(&rivo_storage::StoredEventCursor {
                        session_id: session_ref.to_owned(),
                        next_sequence: next_seq,
                    })
                    .map_err(|_| {
                        RpcError::new(
                            "storage_unavailable",
                            "event cursor could not be persisted",
                            true,
                        )
                    })?;
            }
            Ok(
                json!({"session_ref":session_ref,"from_seq":from_seq,"next_seq":next_seq,"events":events}),
            )
        }
        "session/replay" => {
            let session_ref = params
                .get("session_ref")
                .and_then(Value::as_str)
                .ok_or_else(RpcError::invalid)?;
            valid_ref(session_ref)?;
            let from_seq = params.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
            let page = durable_replay_page(durable_history_notifications(session_ref)?, from_seq);
            let next_seq = page
                .last()
                .and_then(|event| event["params"]["seq"].as_u64())
                .unwrap_or(from_seq);
            if let Some(store) = history_store()? {
                store
                    .save_event_cursor(&rivo_storage::StoredEventCursor {
                        session_id: session_ref.to_owned(),
                        next_sequence: next_seq,
                    })
                    .map_err(|_| {
                        RpcError::new(
                            "storage_unavailable",
                            "event cursor could not be persisted",
                            true,
                        )
                    })?;
            }
            Ok(
                json!({"session_ref":session_ref,"from_seq":from_seq,"next_seq":next_seq,"events":page}),
            )
        }
        "plan/list" => {
            let p: PlanListParams = parse(params)?;
            valid_ref(&p.session_ref)?;
            let store = history_store()?.ok_or_else(|| {
                RpcError::new("session_not_found", "session was not found", false)
            })?;
            let plans = store.task_plans(&p.session_ref).map_err(|_| {
                RpcError::new(
                    "storage_unavailable",
                    "local plan history is unavailable",
                    true,
                )
            })?;
            let plans = plans
                .into_iter()
                .filter_map(|stored| serde_json::from_str::<TaskPlan>(&stored.payload).ok())
                .collect::<Vec<_>>();
            Ok(json!({"session_ref": p.session_ref, "plans": plans}))
        }
        "plan/read" => {
            let p: PlanReadParams = parse(params)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.plan_id)?;
            let store = history_store()?.ok_or_else(|| {
                RpcError::new("session_not_found", "session was not found", false)
            })?;
            let stored = store
                .task_plan(&p.session_ref, &p.plan_id, p.revision)
                .map_err(|_| {
                    RpcError::new(
                        "storage_unavailable",
                        "local plan history is unavailable",
                        true,
                    )
                })?;
            let Some(stored) = stored else {
                return Err(RpcError::new("plan_not_found", "plan was not found", false));
            };
            let plan: TaskPlan = serde_json::from_str(&stored.payload).map_err(|_| {
                RpcError::new("storage_unavailable", "stored plan is invalid", false)
            })?;
            Ok(json!({"session_ref": p.session_ref, "plan": plan}))
        }
        "plan/approve" => {
            let p: PlanApproveParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.plan_id)?;
            cached_or("plan/approve", &p.session_ref, &p.idempotency_key, params, &state, || async {
                send_plan_intent(&state, &p.session_ref, Intent::ApprovePlan { plan_id: p.plan_id.clone(), revision: p.revision, document_sha256: p.document_sha256.clone() }).await?;
                Ok(json!({"accepted":true,"session_ref":p.session_ref,"plan_id":p.plan_id,"revision":p.revision}))
            }).await
        }
        "plan/reject" => {
            let p: PlanRejectParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.plan_id)?;
            cached_or("plan/reject", &p.session_ref, &p.idempotency_key, params, &state, || async {
                send_plan_intent(&state, &p.session_ref, Intent::RejectPlan { plan_id: p.plan_id.clone(), revision: p.revision, reason: p.reason.clone() }).await?;
                Ok(json!({"accepted":true,"session_ref":p.session_ref,"plan_id":p.plan_id,"revision":p.revision}))
            }).await
        }
        "plan/revise" => {
            let p: PlanReviseParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            if p.feedback.trim().is_empty() || !bounded(&p.feedback) {
                return Err(RpcError::invalid());
            }
            if let Some(plan) = &p.plan {
                valid_ref(&plan.plan_id)?;
                if plan.session_id != p.session_ref {
                    return Err(RpcError::invalid());
                }
            }
            cached_or("plan/revise", &p.session_ref, &p.idempotency_key, params.clone(), &state, || async {
                let (plan_id, revision) = if let Some(plan) = p.plan.clone() {
                    let id = plan.plan_id.clone(); let revision = plan.revision;
                    send_plan_intent(&state, &p.session_ref, Intent::UpdatePlan { plan }).await?;
                    (id, revision)
                } else {
                    let plan_id = params.get("plan_id").and_then(Value::as_str).ok_or_else(RpcError::invalid)?.to_owned();
                    let revision = params.get("revision").and_then(Value::as_u64).ok_or_else(RpcError::invalid)?;
                    send_plan_intent(&state, &p.session_ref, Intent::RequestPlanRevision { plan_id: plan_id.clone(), revision, feedback: p.feedback.clone() }).await?;
                    (plan_id, revision)
                };
                Ok(json!({"accepted":true,"session_ref":p.session_ref,"plan_id":plan_id,"revision":revision,"feedback":p.feedback}))
            }).await
        }
        "plan/start" => {
            let p: PlanStartParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.plan_id)?;
            let policy_profile = match p.permission_preset.as_deref().unwrap_or("workspace_write") {
                "workspace_write" => PolicyProfile::Medium,
                "full_access" => PolicyProfile::FullControl,
                "read_only" => PolicyProfile::Strict,
                _ => {
                    return Err(RpcError::new(
                        "unsupported_setting",
                        "permission_preset is not supported",
                        false,
                    ))
                }
            };
            if policy_profile == PolicyProfile::Strict {
                return Err(RpcError::new(
                    "unsupported_setting",
                    "Plan execution requires workspace_write or full_access",
                    false,
                ));
            }
            cached_or("plan/start", &p.session_ref, &p.idempotency_key, params, &state, || async {
                send_plan_intent(&state, &p.session_ref, Intent::StartPlanExecution { plan_id: p.plan_id.clone(), revision: p.revision, policy_profile: Some(policy_profile) }).await?;
                Ok(json!({"accepted":true,"session_ref":p.session_ref,"plan_id":p.plan_id,"revision":p.revision,"permission_preset":p.permission_preset.unwrap_or_else(|| "workspace_write".into())}))
            }).await
        }
        "plan/cancel" => {
            let p: PlanCancelParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.plan_id)?;
            cached_or(
                "plan/cancel",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    send_plan_intent(
                        &state,
                        &p.session_ref,
                        Intent::CancelPlan {
                            plan_id: p.plan_id.clone(),
                        },
                    )
                    .await?;
                    Ok(json!({"accepted":true,"session_ref":p.session_ref,"plan_id":p.plan_id}))
                },
            )
            .await
        }
        "session/start" => {
            let p: StartParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            cached_or("session/start", "__new__", &p.idempotency_key, params, &state, || async {
                turn_options(&p.runtime_settings)?;
                validate_bound_workspace(&state, &p.working_directory)?;
                let config = build_config(&p.working_directory, &p.runtime_settings)?;
                let redaction_keys = config.known_api_keys();
                let session = Arc::new(Session::connect(config.clone()).map_err(map_session_error)?);
                let session_ref = session.session_id().to_owned();
                let mut settings = p.runtime_settings.clone();
                apply_config_defaults(&mut settings, &config);
                if settings.response_language.is_none() {
                    settings.response_language = Some(config.locale.clone());
                }
                install_session(session, session_ref.clone(), settings.clone(), redaction_keys, 0, state.clone(), out.clone()).await;
                Ok(json!({"session_ref":session_ref,"accepted_runtime_settings":settings_summary(&settings)}))
            }).await
        }
        "session/resume" => {
            let p: ResumeParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            cached_or("session/resume", &p.session_ref, &p.idempotency_key, params, &state, || async {
                turn_options(&p.runtime_settings)?;
                validate_bound_workspace(&state, &p.working_directory)?;
                validate_resume_host_binding(&state)?;
                let config = build_config(&p.working_directory, &p.runtime_settings)?;
                if let Some(active_session) = state
                    .lock()
                    .expect("server state lock poisoned")
                    .sessions
                    .get(&p.session_ref)
                {
                    let matches_workspace = active_session
                        .session
                        .as_ref()
                        .is_some_and(|session| session.workspace_path() == config.workspace);
                    if !matches_workspace {
                        return Err(RpcError::new(
                            "workspace_mismatch",
                            "session does not belong to the requested workspace",
                            false,
                        ));
                    }
                    validate_resume_runtime(active_session.session.as_ref(), &p.runtime_settings)?;
                    return Ok(json!({"session_ref":p.session_ref,"accepted_runtime_settings":settings_summary(&p.runtime_settings)}));
                }
                let redaction_keys = config.known_api_keys();
                let session = Arc::new(Session::resume(config.clone(), p.session_ref.clone()).map_err(map_resume_error)?);
                let mut settings = p.runtime_settings.clone();
                apply_config_defaults(&mut settings, &config);
                if settings.response_language.is_none() {
                    settings.response_language = Some(session.restored_locale_async().await);
                }
                validate_resume_runtime(Some(&session), &settings)?;
                let durable_seq = durable_history_notifications(&p.session_ref)?.len() as u64;
                let sender = install_session(session, p.session_ref.clone(), settings.clone(), redaction_keys, durable_seq, state.clone(), out.clone()).await;
                // An explicit session/resume value is a durable session choice;
                // omission leaves the replayed journal locale intact.
                if let Some(locale) = p.runtime_settings.response_language.as_ref() {
                    sender
                        .send(Intent::SetSessionLocale { locale: locale.clone() })
                        .await
                        .map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))?;
                }
                Ok(json!({"session_ref":p.session_ref,"accepted_runtime_settings":settings_summary(&settings)}))
            }).await
        }
        "media/import" => {
            let p: MediaImportParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            if p.source_path.trim().is_empty() || !bounded(&p.source_path) {
                return Err(RpcError::invalid());
            }
            cached_or(
                "media/import",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let exists = state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .get(&p.session_ref)
                        .is_some_and(|session| !session.closing);
                    if !exists {
                        return Err(RpcError::new(
                            "session_not_found",
                            "session was not found",
                            false,
                        ));
                    }
                    let asset =
                        import_media_asset(&p.session_ref, std::path::Path::new(&p.source_path))
                            .map_err(|message| {
                                let code = match message.split(':').next() {
                                    Some("media_too_large") => "media_too_large",
                                    Some("media_import_failed") => "media_import_failed",
                                    _ => "invalid_media",
                                };
                                RpcError::new(code, "image import failed", false)
                            })?;
                    Ok(json!({"asset": asset}))
                },
            )
            .await
        }
        "artifact/read" => {
            let p: ArtifactAccessParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.artifact_id)?;
            cached_or(
                "artifact/read",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let Some((node_id, workspace_id)) = sidecar_scope(&state) else {
                        return Err(RpcError::new(
                            "unsupported_capability",
                            "Sidecar context is unavailable",
                            false,
                        ));
                    };
                    let root = state
                        .lock()
                        .expect("server state lock poisoned")
                        .workspace_root
                        .clone()
                        .ok_or_else(|| {
                            RpcError::new(
                                "workspace_binding_mismatch",
                                "workspace is not bound",
                                false,
                            )
                        })?;
                    let store = history_store()?.ok_or_else(|| {
                        RpcError::new(
                            "storage_unavailable",
                            "artifact storage is unavailable",
                            true,
                        )
                    })?;
                    let artifact = store
                        .artifact_for_scope(&p.artifact_id, &node_id, &workspace_id, &p.session_ref)
                        .map_err(|_| {
                            RpcError::new(
                                "storage_unavailable",
                                "artifact metadata is unavailable",
                                true,
                            )
                        })?
                        .ok_or_else(|| {
                            RpcError::new(
                                "artifact_not_found",
                                "artifact was not found in the authorized scope",
                                false,
                            )
                        })?;
                    if artifact.download_scope != "session" {
                        return Err(RpcError::new(
                            "artifact_scope_denied",
                            "artifact download scope is not authorized for this session",
                            false,
                        ));
                    }
                    let expiry = time::OffsetDateTime::parse(&artifact.expires_at, &Rfc3339)
                        .map_err(|_| {
                            RpcError::new("artifact_invalid", "artifact expiry is invalid", false)
                        })?;
                    if artifact.state == "cleanup_failed" {
                        return Err(RpcError::new(
                            "artifact_cleanup_failed",
                            "runtime-owned artifact cleanup requires audit",
                            false,
                        ));
                    }
                    if expiry <= time::OffsetDateTime::now_utc() || artifact.state != "ready" {
                        return Err(RpcError::new(
                            "artifact_expired",
                            "artifact is no longer available",
                            false,
                        ));
                    }
                    if artifact.size > 44 * 1024 {
                        return Err(RpcError::new(
                            "artifact_too_large",
                            "artifact download exceeds the app-server response limit",
                            false,
                        ));
                    }
                    let bytes = rivo_storage::ArtifactStore::new(root)
                        .read(&artifact)
                        .map_err(|_| {
                            RpcError::new(
                                "artifact_unavailable",
                                "artifact content is unavailable",
                                false,
                            )
                        })?;
                    Ok(json!({
                        "artifact": {
                            "artifact_id": artifact.artifact_id,
                            "kind": artifact.kind,
                            "size": artifact.size,
                            "hash": artifact.hash,
                            "expires_at": artifact.expires_at,
                            "download_scope": artifact.download_scope,
                        },
                        "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                    }))
                },
            )
            .await
        }
        "artifact/delete" => {
            let p: ArtifactAccessParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.artifact_id)?;
            cached_or(
                "artifact/delete",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let Some((node_id, workspace_id)) = sidecar_scope(&state) else {
                        return Err(RpcError::new(
                            "unsupported_capability",
                            "Sidecar context is unavailable",
                            false,
                        ));
                    };
                    let root = state
                        .lock()
                        .expect("server state lock poisoned")
                        .workspace_root
                        .clone()
                        .ok_or_else(|| {
                            RpcError::new(
                                "workspace_binding_mismatch",
                                "workspace is not bound",
                                false,
                            )
                        })?;
                    let store = history_store()?.ok_or_else(|| {
                        RpcError::new(
                            "storage_unavailable",
                            "artifact storage is unavailable",
                            true,
                        )
                    })?;
                    let artifact = store
                        .artifact_for_scope(&p.artifact_id, &node_id, &workspace_id, &p.session_ref)
                        .map_err(|_| {
                            RpcError::new(
                                "storage_unavailable",
                                "artifact metadata is unavailable",
                                true,
                            )
                        })?
                        .ok_or_else(|| {
                            RpcError::new(
                                "artifact_not_found",
                                "artifact was not found in the authorized scope",
                                false,
                            )
                        })?;
                    if artifact.download_scope != "session" {
                        return Err(RpcError::new(
                            "artifact_scope_denied",
                            "artifact download scope is not authorized for this session",
                            false,
                        ));
                    }
                    if rivo_storage::ArtifactStore::new(root)
                        .cleanup(&artifact)
                        .is_err()
                    {
                        store
                            .update_artifact_state(
                                &p.artifact_id,
                                &node_id,
                                &workspace_id,
                                &p.session_ref,
                                "cleanup_failed",
                            )
                            .map_err(|_| {
                                RpcError::new(
                                    "storage_unavailable",
                                    "artifact cleanup failure audit could not be persisted",
                                    true,
                                )
                            })?;
                        return Err(RpcError::new(
                            "artifact_cleanup_failed",
                            "artifact cleanup failed",
                            true,
                        ));
                    }
                    store
                        .update_artifact_state(
                            &p.artifact_id,
                            &node_id,
                            &workspace_id,
                            &p.session_ref,
                            "deleted",
                        )
                        .map_err(|_| {
                            RpcError::new(
                                "storage_unavailable",
                                "artifact metadata could not be updated",
                                true,
                            )
                        })?;
                    Ok(json!({"artifact_id": p.artifact_id, "deleted": true}))
                },
            )
            .await
        }
        "turn/start" => {
            let p: TurnStartParams = parse(params.clone())?;
            let intent_fingerprint = hex::encode(Sha256::digest(params.to_string().as_bytes()));
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            if !bounded(&p.prompt) || p.prompt.trim().is_empty() {
                return Err(RpcError::invalid());
            }
            cached_or("turn/start", &p.session_ref, &p.idempotency_key, params, &state, || async {
                append_media_refs("", &p.media_refs, &p.session_ref)?;
                if let Some(store) = history_store()? {
                    if let Some(existing) = store
                        .sidecar_task_by_intent(&p.idempotency_key)
                        .map_err(|_| RpcError::new("storage_unavailable", "sidecar task storage is unavailable", true))?
                    {
                        if existing.session_id != p.session_ref
                            || sidecar_scope(&state).is_none_or(|(_, workspace)| workspace != existing.workspace_id)
                            || store.sidecar_intent_fingerprint(&existing.task_id)
                                .map_err(|_| RpcError::new("storage_unavailable", "sidecar intent metadata is unavailable", true))?
                                .as_deref() != Some(intent_fingerprint.as_str())
                        {
                            return Err(RpcError::new("idempotency_conflict", "intent key scope or payload does not match the original request", false));
                        }
                        return Ok(json!({
                            "turn_id": existing.turn_id,
                            "task_state": existing.state,
                            "accepted_runtime_settings": Value::Null,
                            "replayed": true,
                        }));
                    }
                }
                let settings = {
                    let locked = state.lock().expect("server state lock poisoned");
                    let Some(session) = locked.sessions.get(&p.session_ref) else { return Err(RpcError::new("session_not_found", "session was not found", false)); };
                    merge_settings(&session.default_settings, &p.runtime_settings)
                };
                let options = turn_options(&settings)?;
                let selection = selection_from_settings(&settings)?;
                let turn_id = next_id("turn");
                let messenger = settings
                    .session_messaging
                    .unwrap_or(false)
                    .then(|| messaging::messenger_for(&state, &out));
                let sender = {
                    let mut locked = state.lock().expect("server state lock poisoned");
                    if let Some(context) = locked.sidecar_context.as_ref() {
                        if locked.sessions.values().filter(|session| session.active.is_some()).count()
                            >= context.resource_limits.max_concurrent_tasks as usize {
                            return Err(RpcError::new("resource_limit", "Sidecar concurrent task limit reached", true));
                        }
                    }
                    let Some(session) = locked.sessions.get_mut(&p.session_ref) else { return Err(RpcError::new("session_not_found", "session was not found", false)); };
                    if session.closing { return Err(RpcError::new("session_not_found", "session was not found", false)); }
                    if session.active.is_some() { return Err(RpcError::new("turn_in_progress", "session already has an active turn", false)); }
                    if let Some(selection) = selection.as_ref() {
                        session
                            .session
                            .as_ref()
                            .ok_or_else(|| RpcError::new("internal_error", "local runtime state is unavailable", false))?
                            .validate_route_selection(selection)
                            .map_err(|_| RpcError::new("unsupported_setting", "model is not available for this local session", false))?;
                    }
                    session
                        .session
                        .as_ref()
                        .ok_or_else(|| RpcError::new("internal_error", "local runtime state is unavailable", false))?
                        .validate_mcp_selection(&options)
                        .map_err(|_| RpcError::new("unsupported_capability", "requested MCP selection is not configured for this local session", false))?;
                    session.active = Some(TurnState { id: turn_id.clone(), interrupted: false, message_started: false });
                    if let Some(runtime) = session.session.as_ref() {
                        runtime.set_session_messenger(messenger);
                    }
                    session.sender.clone()
                };
                if let Err(error) = create_sidecar_task(&state, &p.session_ref, &turn_id, &p.idempotency_key, &intent_fingerprint) {
                    clear_active_turn(&state, &p.session_ref, &turn_id);
                    return Err(error);
                }
                let accepted = async {
                    if let Some(selection) = selection {
                        sender.send(Intent::SelectModel { selection }).await.map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))?;
                    }
                    sender.send(Intent::SetTurnOptions { options }).await.map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))?;
                    let prompt = p.prompt.clone();
                    let intent = if p.media_refs.is_empty() {
                        Intent::SubmitTask { text: prompt }
                    } else {
                        Intent::SubmitTaskWithMedia {
                            text: prompt,
                            media_refs: p.media_refs.clone(),
                        }
                    };
                    sender.send(intent).await.map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))
                }
                .await;
                if accepted.is_err() {
                    update_sidecar_task(&p.session_ref, &turn_id, "failed", 0, Some("intent_rejected"));
                    clear_active_turn(&state, &p.session_ref, &turn_id);
                } else if let Ok(Some(store)) = history_store() {
                    if let Ok(Some(task)) = store.sidecar_task_by_turn(&p.session_ref, &turn_id) {
                        let _ = store.update_sidecar_task(&task.task_id, "running", task.lease_epoch, 0, None);
                    }
                }
                accepted?;
                Ok(json!({"turn_id":turn_id,"accepted_runtime_settings":settings_summary(&settings)}))
            }).await
        }
        "turn/interrupt" => {
            let p: InterruptParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.turn_id)?;
            cached_or(
                "turn/interrupt",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let sender = {
                        let mut locked = state.lock().expect("server state lock poisoned");
                        let Some(session) = locked.sessions.get_mut(&p.session_ref) else {
                            return Err(RpcError::new(
                                "session_not_found",
                                "session was not found",
                                false,
                            ));
                        };
                        let Some(turn) = session.active.as_mut() else {
                            return Err(RpcError::new(
                                "no_active_turn",
                                "session has no active turn",
                                false,
                            ));
                        };
                        if turn.id != p.turn_id {
                            return Err(RpcError::new(
                                "no_active_turn",
                                "turn is not active",
                                false,
                            ));
                        }
                        turn.interrupted = true;
                        session.sender.clone()
                    };
                    if sender.send(Intent::CancelTurn).await.is_err() {
                        update_sidecar_task(
                            &p.session_ref,
                            &p.turn_id,
                            "interrupted",
                            0,
                            Some("runtime_unavailable"),
                        );
                        clear_active_turn(&state, &p.session_ref, &p.turn_id);
                        return Err(RpcError::new(
                            "runtime_unavailable",
                            "local runtime is unavailable",
                            true,
                        ));
                    }
                    cancel_descendants(&state, &p.session_ref).await;
                    Ok(json!({"accepted":true,"turn_id":p.turn_id}))
                },
            )
            .await
        }
        "session/peers" => messaging::host_peers(params, &state),
        "session/message/list" => messaging::host_list(params, &state),
        "session/message/respond" => messaging::host_respond(params, &state, &out).await,
        "session/message/configure" => messaging::host_configure(params, &state, &out).await,
        "session/send" => {
            let p: ThreadSendParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            if p.text.trim().is_empty() || !bounded(&p.text) {
                return Err(RpcError::invalid());
            }
            cached_or(
                "session/send",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    append_media_refs("", &p.media_refs, &p.session_ref)?;
                    let sender = state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .get(&p.session_ref)
                        .filter(|session| !session.closing)
                        .map(|session| session.sender.clone())
                        .ok_or_else(|| {
                            RpcError::new("session_not_found", "session was not found", false)
                        })?;
                    let intent = if p.media_refs.is_empty() {
                        Intent::AppendMessage {
                            text: p.text.clone(),
                        }
                    } else {
                        Intent::AppendMessageWithMedia {
                            text: p.text.clone(),
                            media_refs: p.media_refs.clone(),
                        }
                    };
                    sender.send(intent).await.map_err(|_| {
                        RpcError::new("runtime_unavailable", "local runtime is unavailable", true)
                    })?;
                    Ok(json!({"accepted":true,"session_ref":p.session_ref}))
                },
            )
            .await
        }
        "session/fork" => {
            let p: ThreadForkParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.parent_session_ref)?;
            if let Some(prompt) = p.initial_prompt.as_deref() {
                if prompt.trim().is_empty() || !bounded(prompt) {
                    return Err(RpcError::invalid());
                }
            }
            cached_or(
                "session/fork",
                &p.parent_session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let parent_exists = state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .get(&p.parent_session_ref)
                        .is_some_and(|session| !session.closing);
                    if !parent_exists {
                        return Err(RpcError::new("session_not_found", "parent session was not found", false));
                    }
                    turn_options(&p.runtime_settings)?;
                    validate_bound_workspace(&state, &p.working_directory)?;
                    let config = build_config(&p.working_directory, &p.runtime_settings)?;
                    let redaction_keys = config.known_api_keys();
                    let mut settings = p.runtime_settings.clone();
                    apply_config_defaults(&mut settings, &config);
                    let session = Arc::new(Session::connect(config).map_err(map_session_error)?);
                    let child_ref = session.session_id().to_owned();
                    let sender = install_session(
                        session,
                        child_ref.clone(),
                        settings,
                        redaction_keys,
                        0,
                        state.clone(),
                        out.clone(),
                    )
                    .await;
                    state
                        .lock()
                        .expect("server state lock poisoned")
                        .child_parents
                        .insert(child_ref.clone(), p.parent_session_ref.clone());
                    if let Some(prompt) = p.initial_prompt.clone() {
                        sender
                            .send(Intent::SubmitTask { text: prompt })
                            .await
                            .map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))?;
                    }
                    Ok(json!({"accepted":true,"session_ref":child_ref,"parent_session_ref":p.parent_session_ref}))
                },
            )
            .await
        }
        "interaction/respond" => {
            let p: InteractionParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            valid_ref(&p.interaction_id)?;
            if !bounded(&p.decision)
                || p.selected_option_id
                    .as_deref()
                    .is_some_and(|value| !bounded(value))
                || p.freeform_text
                    .as_deref()
                    .is_some_and(|value| !bounded(value))
            {
                return Err(RpcError::invalid());
            }
            cached_or(
                "interaction/respond",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let (sender, intent) = {
                        let mut locked = state.lock().expect("server state lock poisoned");
                        let Some(session) = locked.sessions.get_mut(&p.session_ref) else {
                            return Err(RpcError::new(
                                "session_not_found",
                                "session was not found",
                                false,
                            ));
                        };
                        let Some(interaction) =
                            session.interactions.get(&p.interaction_id).cloned()
                        else {
                            return Err(RpcError::new(
                                "interaction_not_found",
                                "interaction was not found",
                                false,
                            ));
                        };
                        if session.active.as_ref().map(|turn| &turn.id)
                            != Some(&interaction.turn_id)
                        {
                            return Err(RpcError::new(
                                "interaction_stale",
                                "interaction is stale",
                                false,
                            ));
                        }
                        let intent = match interaction.kind {
                            InteractionKind::Confirmation
                                if matches!(p.decision.as_str(), "allow" | "deny") =>
                            {
                                Intent::Confirm {
                                    request_id: p.interaction_id.clone(),
                                    allow: p.decision == "allow",
                                    reason: None,
                                    scope: ApprovalScope::Once,
                                }
                            }
                            InteractionKind::Input if p.decision == "respond" => {
                                if p.selected_option_id.is_some() && p.freeform_text.is_some() {
                                    return Err(RpcError::invalid());
                                }
                                let value = match (&p.selected_option_id, &p.freeform_text) {
                                    (Some(option), None)
                                        if bounded(option)
                                            && interaction.option_ids.contains(option) =>
                                    {
                                        option.clone()
                                    }
                                    (None, Some(text))
                                        if interaction.allow_freeform && bounded(text) =>
                                    {
                                        text.clone()
                                    }
                                    _ => return Err(RpcError::invalid()),
                                };
                                Intent::RespondInteraction {
                                    request_id: p.interaction_id.clone(),
                                    value,
                                }
                            }
                            InteractionKind::Input => {
                                return Err(RpcError::new(
                                    "unsupported_capability",
                                    "interaction decision is not supported",
                                    false,
                                ));
                            }
                            InteractionKind::Confirmation => return Err(RpcError::invalid()),
                        };
                        (session.sender.clone(), intent)
                    };
                    sender.send(intent).await.map_err(|_| {
                        RpcError::new("runtime_unavailable", "local runtime is unavailable", true)
                    })?;
                    if let Some(session) = state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .get_mut(&p.session_ref)
                    {
                        session.interactions.remove(&p.interaction_id);
                    }
                    Ok(json!({"accepted":true}))
                },
            )
            .await
        }
        "session/close" => {
            let p: CloseParams = parse(params.clone())?;
            valid_key(&p.idempotency_key)?;
            valid_ref(&p.session_ref)?;
            cached_or(
                "session/close",
                &p.session_ref,
                &p.idempotency_key,
                params,
                &state,
                || async {
                    let sender = state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .get_mut(&p.session_ref)
                        .map(|session| {
                            session.closing = true;
                            session.sender.clone()
                        });
                    if let Some(sender) = sender {
                        if sender.send(Intent::CancelTurn).await.is_err() {
                            rivo_session::remove_media_assets(&p.session_ref);
                            state
                                .lock()
                                .expect("server state lock poisoned")
                                .sessions
                                .remove(&p.session_ref);
                        }
                        cancel_descendants(&state, &p.session_ref).await;
                    }
                    if !state
                        .lock()
                        .expect("server state lock poisoned")
                        .sessions
                        .contains_key(&p.session_ref)
                    {
                        rivo_session::remove_media_assets(&p.session_ref);
                    }
                    Ok(json!({"closed":true}))
                },
            )
            .await
        }
        _ => Err(RpcError::method()),
    }
}

fn clear_active_turn(state: &Arc<Mutex<ServerState>>, session_ref: &str, turn_id: &str) {
    if let Some(session) = state
        .lock()
        .expect("server state lock poisoned")
        .sessions
        .get_mut(session_ref)
    {
        if session
            .active
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id)
        {
            session.active = None;
        }
    }
}

async fn cancel_descendants(state: &Arc<Mutex<ServerState>>, parent_ref: &str) {
    let senders = {
        let locked = state.lock().expect("server state lock poisoned");
        let mut queue = vec![parent_ref.to_owned()];
        let mut descendants = Vec::new();
        while let Some(parent) = queue.pop() {
            for (child, registered_parent) in &locked.child_parents {
                if registered_parent == &parent {
                    queue.push(child.clone());
                    if let Some(session) = locked.sessions.get(child) {
                        descendants.push(session.sender.clone());
                    }
                }
            }
        }
        descendants
    };
    for sender in senders {
        let _ = sender.send(Intent::CancelTurn).await;
    }
}

async fn cached_or<F, Fut>(
    method: &str,
    session_ref: &str,
    key: &str,
    semantic: Value,
    state: &Arc<Mutex<ServerState>>,
    work: F,
) -> Result<Value, RpcError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    let cache_key = format!("{session_ref}\u{1f}{method}\u{1f}{key}");
    if let Some(cached) = state
        .lock()
        .expect("server state lock poisoned")
        .idempotency
        .get(&cache_key)
        .cloned()
    {
        return if cached.semantic == semantic {
            cached.result
        } else {
            Err(RpcError::new(
                "invalid_request",
                "idempotency key conflicts with a different request",
                false,
            ))
        };
    }
    let result = work().await;
    // A durable collision describes an existing accepted intent. Caching the
    // conflicting payload here would poison the original key after reconnect.
    if result
        .as_ref()
        .is_err_and(|error| error.code == "idempotency_conflict")
    {
        return result;
    }
    let mut locked = state.lock().expect("server state lock poisoned");
    if locked.idempotency_order.len() >= MAX_CACHE_ENTRIES {
        if let Some(old) = locked.idempotency_order.pop_front() {
            locked.idempotency.remove(&old);
        }
    }
    locked.idempotency_order.push_back(cache_key.clone());
    locked.idempotency.insert(
        cache_key,
        CachedResponse {
            semantic,
            result: result.clone(),
        },
    );
    result
}

async fn install_session(
    session: Arc<Session>,
    session_ref: String,
    default_settings: RuntimeSettings,
    redaction_keys: Vec<String>,
    durable_seq: u64,
    state: Arc<Mutex<ServerState>>,
    out: mpsc::Sender<Value>,
) -> rivo_session::IntentSender {
    let (sender, mut events) = session.clone().channel();
    state
        .lock()
        .expect("server state lock poisoned")
        .sessions
        .insert(
            session_ref.clone(),
            SessionState {
                session: Some(session),
                sender,
                seq: durable_seq,
                active: None,
                interactions: HashMap::new(),
                subagent_tasks: HashMap::new(),
                open_tool_calls: BTreeMap::new(),
                default_settings,
                closing: false,
                redaction_keys,
                inbound: messaging::InboundMode::default(),
            },
        );
    let event_session_ref = session_ref.clone();
    let event_state = state.clone();
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            emit_session_event(&event_session_ref, event, &event_state, &out).await;
        }
    });
    let sender = state
        .lock()
        .expect("server state lock poisoned")
        .sessions
        .get(&session_ref)
        .expect("session was installed")
        .sender
        .clone();
    sender
}

async fn send_plan_intent(
    state: &Arc<Mutex<ServerState>>,
    session_ref: &str,
    intent: Intent,
) -> Result<(), RpcError> {
    let sender = state
        .lock()
        .expect("server state lock poisoned")
        .sessions
        .get(session_ref)
        .filter(|session| !session.closing)
        .map(|session| session.sender.clone())
        .ok_or_else(|| RpcError::new("session_not_found", "session was not found", false))?;
    sender
        .send(intent)
        .await
        .map_err(|_| RpcError::new("runtime_unavailable", "local runtime is unavailable", true))
}

async fn emit_session_event(
    session_ref: &str,
    event: SessionEvent,
    state: &Arc<Mutex<ServerState>>,
    out: &mpsc::Sender<Value>,
) {
    let terminal_state = match &event.event {
        AgentEvent::FinalAnswer { .. } => Some(("completed", None::<String>)),
        AgentEvent::Stopped { reason } => {
            let state = if reason.contains("取消") {
                "cancelled"
            } else {
                "failed"
            };
            Some((state, Some(reason.clone())))
        }
        _ => None,
    };
    let turn_ended = matches!(
        event.event,
        AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
    );
    let (turn_id, notifications, close_after_events, cancel_unanswerable) = {
        let mut locked = state.lock().expect("server state lock poisoned");
        let Some(session) = locked.sessions.get_mut(session_ref) else {
            return;
        };
        let turn_id = session
            .active
            .as_ref()
            .map(|turn| turn.id.clone())
            .unwrap_or_default();
        let mut items = event_notifications(&event.event, session, &turn_id);
        if matches!(
            event.event,
            AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
        ) {
            session.active = None;
            session.interactions.clear();
            session.subagent_tasks.clear();
        }
        let close_after_events = session.closing
            && matches!(
                event.event,
                AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
            );
        let cancel_unanswerable = matches!(
            &event.event,
            AgentEvent::InteractionRequired { request }
                if request.options.is_empty() && !request.allow_freeform
        )
        .then(|| session.sender.clone());
        (
            turn_id,
            std::mem::take(&mut items),
            close_after_events,
            cancel_unanswerable,
        )
    };
    update_sidecar_tool_status(session_ref, &turn_id, &notifications);
    for (kind, payload) in notifications {
        if !publish_notification(state, out, session_ref, &turn_id, &kind, payload).await {
            return;
        }
    }
    if let Some((task_state, reason)) = terminal_state {
        let sequence = state
            .lock()
            .expect("server state lock poisoned")
            .sessions
            .get(session_ref)
            .map(|session| session.seq)
            .unwrap_or_default();
        update_sidecar_task(
            session_ref,
            &turn_id,
            task_state,
            sequence,
            reason.as_deref(),
        );
        if task_state != "completed" {
            cleanup_sidecar_artifacts_for_session(state, session_ref);
        }
    }
    if let Some(sender) = cancel_unanswerable {
        let _ = sender.send(Intent::CancelTurn).await;
    }
    if turn_ended && !close_after_events {
        // FEAT-037: queued messages are delivered only once the receiver is idle.
        let state = state.clone();
        let out = out.clone();
        let session_ref = session_ref.to_owned();
        tokio::spawn(async move {
            messaging::try_deliver(&state, &out, &session_ref).await;
        });
    }
    if close_after_events {
        state
            .lock()
            .expect("server state lock poisoned")
            .sessions
            .remove(session_ref);
        rivo_session::remove_media_assets(session_ref);
    }
}

fn append_media_refs(
    text: &str,
    media_refs: &[MediaRef],
    session_ref: &str,
) -> Result<String, RpcError> {
    if media_refs.len() > 8 {
        return Err(RpcError::new(
            "invalid_request",
            "too many media references",
            false,
        ));
    }
    for media in media_refs {
        if media.session_id != session_ref
            || !valid_media_asset_id(&media.asset_id)
            || !matches!(
                media.media_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp"
            )
            || media.width.is_some_and(|width| width > 16_384)
            || media.height.is_some_and(|height| height > 16_384)
        {
            return Err(RpcError::new(
                "invalid_request",
                "media reference is invalid or belongs to another session",
                false,
            ));
        }
    }
    let serialized = serde_json::to_string(media_refs).map_err(|_| RpcError::invalid())?;
    if serialized.len() > 8 * 1024 {
        return Err(RpcError::new(
            "invalid_request",
            "media references are too large",
            false,
        ));
    }
    if media_refs.is_empty() {
        return Ok(text.to_owned());
    }
    Ok(format!("{text}\n\n[当前会话媒体资产引用]\n{serialized}"))
}

fn valid_media_asset_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn event_notifications(
    event: &AgentEvent,
    session: &mut SessionState,
    turn_id: &str,
) -> Vec<(String, Value)> {
    let terminal = !turn_id.is_empty()
        && matches!(
            event,
            AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
        );
    let mut items = project_event(event, session, turn_id);
    for (kind, payload) in &mut items {
        if kind == "tool_call" {
            track_tool_call(session, payload);
        }
    }
    if terminal {
        // The turn can end (cancel, provider failure) before the runtime reports
        // results for in-flight calls, e.g. a parent `delegate_task`; close them so
        // every projected tool_call reaches a terminal status before turn_complete.
        let mut closed = close_open_tool_calls(session, |_| true);
        closed.append(&mut items);
        items = closed;
        session.open_tool_calls.clear();
    }
    items
}

/// Records open tool calls and fills a missing name from the call's earlier events.
fn track_tool_call(session: &mut SessionState, payload: &mut Value) {
    let Some(tool_call_id) = payload["tool_call_id"].as_str().map(str::to_owned) else {
        return;
    };
    let known = session.open_tool_calls.get(&tool_call_id).cloned();
    if payload["name"].as_str().is_none_or(str::is_empty) {
        if let Some(name) = known.as_ref().and_then(|identity| identity.get("name")) {
            payload["name"] = name.clone();
        }
    }
    match payload["status"].as_str() {
        Some("completed" | "failed") => {
            session.open_tool_calls.remove(&tool_call_id);
        }
        Some(_) => {
            let mut identity =
                json!({"tool_call_id": tool_call_id, "name": payload["name"].clone()});
            for key in ["agent_id", "task_id"] {
                if let Some(value) = payload.get(key) {
                    identity[key] = value.clone();
                }
            }
            session.open_tool_calls.insert(tool_call_id, identity);
        }
        None => {}
    }
}

fn close_open_tool_calls(
    session: &mut SessionState,
    select: impl Fn(&Value) -> bool,
) -> Vec<(String, Value)> {
    let ids = session
        .open_tool_calls
        .iter()
        .filter(|(_, identity)| select(identity))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| session.open_tool_calls.remove(&id))
        .map(|mut identity| {
            identity["status"] = json!("failed");
            identity["error_code"] = json!("cancelled");
            identity["summary"] = json!({"truncated":false,"original_bytes":0});
            ("tool_call".into(), identity)
        })
        .collect()
}

/// Turn 用量只投影计数，不含提供方或路由细节；可选计数缺失时省略对应字段。
fn turn_usage_payload(usage: &rivo_llm::Usage) -> Value {
    let mut payload = json!({
        "input_tokens":usage.input_tokens,
        "output_tokens":usage.output_tokens,
        "total_tokens":usage.total_tokens,
    });
    if let Some(reasoning) = usage.reasoning_tokens {
        payload["reasoning_tokens"] = json!(reasoning);
    }
    if let Some(cached) = usage.cache_read_input_tokens {
        payload["cache_read_input_tokens"] = json!(cached);
    }
    payload
}

fn plan_projection(plan: &TaskPlan, redaction_keys: &[String]) -> Value {
    let steps = plan.steps.iter().take(100).map(|step| json!({
        "step_id": safe_id(&step.step_id),
        "title": bounded_text_with_keys(&step.title, 256, redaction_keys).0,
        "description": bounded_text_with_keys(&step.description, 2048, redaction_keys).0,
        "depends_on": step.depends_on.iter().take(32).map(|value| safe_id(value)).collect::<Vec<_>>(),
        "acceptance_criteria": step.acceptance_criteria.iter().take(32).map(|value| bounded_text_with_keys(value, 512, redaction_keys).0).collect::<Vec<_>>(),
        "status": step.status,
        "evidence_refs": step.evidence_refs.iter().take(32).map(|value| bounded_text_with_keys(value, 256, redaction_keys).0).collect::<Vec<_>>(),
        "failure_reason": step.failure_reason.as_deref().map(|value| bounded_text_with_keys(value, 512, redaction_keys).0),
    })).collect::<Vec<_>>();
    json!({
        "plan_id": safe_id(&plan.plan_id),
        "session_id": safe_id(&plan.session_id),
        "objective": bounded_text_with_keys(&plan.objective, 2048, redaction_keys).0,
        "revision": plan.revision,
        "status": plan.status,
        "steps": steps,
        "dependencies": plan.dependencies.iter().take(64).map(|value| bounded_text_with_keys(value, 512, redaction_keys).0).collect::<Vec<_>>(),
        "acceptance_criteria": plan.acceptance_criteria.iter().take(64).map(|value| bounded_text_with_keys(value, 512, redaction_keys).0).collect::<Vec<_>>(),
        "risks": plan.risks.iter().take(64).map(|value| bounded_text_with_keys(value, 512, redaction_keys).0).collect::<Vec<_>>(),
        "evidence_refs": plan.evidence_refs.iter().take(64).map(|value| bounded_text_with_keys(value, 256, redaction_keys).0).collect::<Vec<_>>(),
        "created_at": plan.created_at,
        "updated_at": plan.updated_at,
    })
}

fn project_event(
    event: &AgentEvent,
    session: &mut SessionState,
    turn_id: &str,
) -> Vec<(String, Value)> {
    match event {
        AgentEvent::PlanCreated { plan } => vec![
            (
                "plan_created".into(),
                plan_projection(plan, &session.redaction_keys),
            ),
            ("status".into(), json!({"status":"awaiting_plan_approval"})),
        ],
        AgentEvent::PlanUpdated { plan } => vec![
            (
                "plan_updated".into(),
                plan_projection(plan, &session.redaction_keys),
            ),
            ("status".into(), json!({"status":"awaiting_plan_approval"})),
        ],
        AgentEvent::PlanApprovalRequired {
            plan,
            document_sha256,
        } => vec![
            (
                "plan_approval_required".into(),
                json!({"plan":plan_projection(plan, &session.redaction_keys),"document_sha256":document_sha256}),
            ),
            ("status".into(), json!({"status":"awaiting_plan_approval"})),
        ],
        AgentEvent::PlanApproved {
            plan_id,
            revision,
            document_sha256,
            artifact_path,
        } => vec![
            (
                "plan_approved".into(),
                json!({"plan_id":safe_id(plan_id),"revision":revision,"document_sha256":document_sha256,"artifact_path":artifact_path}),
            ),
            ("status".into(), json!({"status":"plan_approved"})),
        ],
        AgentEvent::PlanRejected {
            plan_id,
            revision,
            reason,
        } => vec![
            (
                "plan_rejected".into(),
                json!({"plan_id":safe_id(plan_id),"revision":revision,"reason":reason}),
            ),
            ("status".into(), json!({"status":"plan_rejected"})),
        ],
        AgentEvent::PlanExecutionStarted { plan_id, revision } => vec![
            (
                "plan_execution_started".into(),
                json!({"plan_id":safe_id(plan_id),"revision":revision}),
            ),
            ("status".into(), json!({"status":"plan_executing"})),
        ],
        AgentEvent::PlanStepStarted {
            plan_id,
            revision,
            step_id,
        } => vec![(
            "plan_step_started".into(),
            json!({"plan_id":safe_id(plan_id),"revision":revision,"step_id":safe_id(step_id)}),
        )],
        AgentEvent::PlanStepCompleted {
            plan_id,
            revision,
            step_id,
        } => vec![(
            "plan_step_completed".into(),
            json!({"plan_id":safe_id(plan_id),"revision":revision,"step_id":safe_id(step_id)}),
        )],
        AgentEvent::PlanStepBlocked {
            plan_id,
            revision,
            step_id,
            reason,
        } => vec![(
            "plan_step_blocked".into(),
            json!({"plan_id":safe_id(plan_id),"revision":revision,"step_id":safe_id(step_id),"reason":bounded_text_with_keys(reason,512,&session.redaction_keys).0}),
        )],
        AgentEvent::PlanCompleted { plan_id, revision } => vec![
            (
                "plan_completed".into(),
                json!({"plan_id":safe_id(plan_id),"revision":revision}),
            ),
            ("status".into(), json!({"status":"completed"})),
        ],
        AgentEvent::Started {
            selection, route, ..
        } => {
            let mut status = json!({"status":"running"});
            if let Some(selection) = selection {
                status["selection"] = json!(selection);
            }
            if let Some(route) = route {
                status["route"] = json!(route);
            }
            vec![(
                "status".into(),
                rivo_telemetry::redact_api_keys(&status, &session.redaction_keys),
            )]
        }
        AgentEvent::ModelDelta { text } => {
            let mut values = Vec::new();
            if let Some(turn) = session.active.as_mut() {
                if !turn.message_started {
                    turn.message_started = true;
                    values.push((
                        "message_start".into(),
                        json!({"message_id":format!("{turn_id}-message-1")}),
                    ));
                }
            }
            values.push((
                "message_delta".into(),
                json!({"message_id":format!("{turn_id}-message-1"),"delta":text_payload_with_keys(text, &session.redaction_keys)}),
            ));
            values
        }
        AgentEvent::FinalAnswer { text, usage, .. } => {
            let message_id = format!("{turn_id}-message-1");
            let mut values = Vec::new();
            if session
                .active
                .as_ref()
                .is_some_and(|turn| !turn.message_started)
            {
                values.push(("message_start".into(), json!({"message_id":message_id})));
            }
            values.push((
                "message_end".into(),
                json!({"message_id":message_id,"content":text_payload_with_keys(text, &session.redaction_keys)}),
            ));
            values.push((
                "turn_complete".into(),
                json!({"outcome":"completed","usage":turn_usage_payload(usage)}),
            ));
            values.push(("status".into(), json!({"status":"idle"})));
            values
        }
        AgentEvent::ToolRequested { call_id, name, .. } => {
            let payload = json!({"tool_call_id":safe_id(call_id),"name":bounded_text_with_keys(name, 128, &session.redaction_keys).0,"summary":{"truncated":false,"original_bytes":0}});
            vec![
                ("tool_call".into(), with_tool_status(&payload, "requested")),
                ("tool_call".into(), with_tool_status(&payload, "running")),
            ]
        }
        AgentEvent::ToolObservation {
            call_id,
            name,
            result,
        } => {
            let failed = result.as_object().is_some_and(|value| {
                value.contains_key("error")
                    || value.get("error_code").is_some_and(|code| !code.is_null())
            });
            let error_code = failed.then(|| tool_error_code(result));
            let mut payload = json!({
                "tool_call_id":safe_id(call_id),
                "name":bounded_text_with_keys(name, 128, &session.redaction_keys).0,
                "status":if failed {"failed"} else {"completed"},
                "summary":{"truncated":false,"original_bytes":0}
            });
            if let Some(code) = error_code.flatten() {
                payload["error_code"] = json!(code);
            }
            vec![("tool_call".into(), payload)]
        }
        AgentEvent::MediaStarted { call_id, operation } => vec![(
            "media_started".into(),
            json!({"tool_call_id":safe_id(call_id),"operation":operation}),
        )],
        AgentEvent::MediaReady { call_id, result } => vec![(
            "media_ready".into(),
            json!({"tool_call_id":safe_id(call_id),"asset":result.asset,"delivery":result.delivery}),
        )],
        AgentEvent::MediaFailed {
            call_id,
            operation,
            message,
        } => vec![(
            "media_failed".into(),
            json!({"tool_call_id":safe_id(call_id),"operation":operation,"message":message}),
        )],
        AgentEvent::ConfirmationRequired {
            request_id,
            call_id,
            tool_name,
            arguments_summary,
            ..
        } => {
            session.interactions.insert(
                request_id.clone(),
                InteractionState {
                    turn_id: turn_id.into(),
                    kind: InteractionKind::Confirmation,
                    option_ids: HashSet::new(),
                    allow_freeform: false,
                },
            );
            let (summary, truncated, bytes) =
                bounded_text_with_keys(arguments_summary, 512, &session.redaction_keys);
            vec![
                (
                    "tool_call".into(),
                    json!({"tool_call_id":safe_id(call_id),"name":bounded_text_with_keys(tool_name,128, &session.redaction_keys).0,"status":"waiting_interaction","summary":{"text":summary,"truncated":truncated,"original_bytes":bytes}}),
                ),
                (
                    "interaction_request".into(),
                    json!({"interaction_id":safe_id(request_id),"tool_call_id":safe_id(call_id),"type":interaction_type(tool_name),"response":{"decisions":["allow","deny"],"approval_scope":"once"},"summary":{"truncated":truncated,"original_bytes":bytes}}),
                ),
                ("status".into(), json!({"status":"waiting_interaction"})),
            ]
        }
        AgentEvent::ConfirmationResolved {
            call_id,
            tool_name,
            allowed,
            ..
        } => {
            let name = bounded_text_with_keys(tool_name, 128, &session.redaction_keys).0;
            if *allowed {
                vec![
                    (
                        "tool_call".into(),
                        json!({"tool_call_id":safe_id(call_id),"name":name,"status":"running","summary":{"truncated":false,"original_bytes":0}}),
                    ),
                    ("status".into(), json!({"status":"running"})),
                ]
            } else {
                // A denial is handed back to the model: the turn keeps running and the
                // following tool observation closes this call as failed/policy_denied.
                vec![("status".into(), json!({"status":"running"}))]
            }
        }
        AgentEvent::SubagentQueued {
            agent_id,
            task_id,
            call_id,
            label,
        } => {
            session.subagent_tasks.insert(
                task_id.0.clone(),
                SubagentTaskState {
                    parent_call_id: call_id.clone(),
                },
            );
            let mut payload = subagent_payload(session, agent_id, task_id, "queued");
            payload["label"] = json!(bounded_text_with_keys(label, 120, &session.redaction_keys).0);
            vec![("subagent".into(), payload)]
        }
        AgentEvent::SubagentStarted { agent_id, task_id } => vec![(
            "subagent".into(),
            subagent_payload(session, agent_id, task_id, "running"),
        )],
        AgentEvent::SubagentActivity {
            agent_id,
            task_id,
            activity,
        } => {
            let (call_id, name, status, error_code) = match activity {
                SubagentToolActivity::Requested { call_id, name } => {
                    (call_id, name, "requested", None)
                }
                SubagentToolActivity::Observed {
                    call_id,
                    name,
                    failed,
                    error,
                } => (
                    call_id,
                    name,
                    if *failed { "failed" } else { "completed" },
                    failed.then(|| {
                        tool_error_code(&json!({"error": error.as_deref().unwrap_or_default()}))
                            .unwrap_or("tool_error")
                    }),
                ),
                SubagentToolActivity::ConfirmationResolved {
                    call_id,
                    tool_name,
                    allowed,
                    ..
                } => (
                    call_id,
                    tool_name,
                    if *allowed { "running" } else { "failed" },
                    (!allowed).then_some("policy_denied"),
                ),
            };
            let tool_call_id = subagent_call_id(task_id, call_id);
            if status == "requested" && session.open_tool_calls.contains_key(&tool_call_id) {
                // The confirmation channel can overtake the child's request
                // activity; never move a waiting call back to running.
                return Vec::new();
            }
            let mut payload =
                subagent_tool_payload(session, agent_id, task_id, &tool_call_id, name, status);
            if let Some(code) = error_code {
                payload["error_code"] = json!(code);
            }
            if status == "requested" {
                vec![
                    ("tool_call".into(), payload.clone()),
                    ("tool_call".into(), with_tool_status(&payload, "running")),
                ]
            } else {
                vec![("tool_call".into(), payload)]
            }
        }
        AgentEvent::SubagentConfirmationRequired {
            agent_id,
            task_id,
            request_id,
            call_id,
            tool_name,
            arguments_summary,
            ..
        } => {
            session.interactions.insert(
                request_id.clone(),
                InteractionState {
                    turn_id: turn_id.into(),
                    kind: InteractionKind::Confirmation,
                    option_ids: HashSet::new(),
                    allow_freeform: false,
                },
            );
            let tool_call_id = subagent_call_id(task_id, call_id);
            let (summary, truncated, bytes) =
                bounded_text_with_keys(arguments_summary, 512, &session.redaction_keys);
            let mut tool_call = subagent_tool_payload(
                session,
                agent_id,
                task_id,
                &tool_call_id,
                tool_name,
                "waiting_interaction",
            );
            tool_call["summary"] =
                json!({"text":summary,"truncated":truncated,"original_bytes":bytes});
            // The root turn keeps running while a subagent waits, so the turn
            // status is deliberately left unchanged.
            vec![
                ("tool_call".into(), tool_call),
                (
                    "interaction_request".into(),
                    json!({"interaction_id":safe_id(request_id),"tool_call_id":tool_call_id,"agent_id":safe_id(&agent_id.0),"task_id":safe_id(&task_id.0),"type":interaction_type(tool_name),"response":{"decisions":["allow","deny"],"approval_scope":"once"},"summary":{"truncated":truncated,"original_bytes":bytes}}),
                ),
            ]
        }
        AgentEvent::SubagentCompleted { result } => {
            let task = session.subagent_tasks.remove(&result.task_id.0);
            // A task can end (cancel, budget, denial) while child calls are still
            // open; every tool_call must still reach a terminal status.
            let task_id = safe_id(&result.task_id.0);
            let mut values = close_open_tool_calls(session, |identity| {
                identity["task_id"].as_str() == Some(task_id.as_str())
            });
            let status = serde_json::to_value(result.state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "failed".into());
            let mut payload = json!({
                "agent_id":safe_id(&result.agent_id.0),
                "task_id":safe_id(&result.task_id.0),
                "status":status,
                "truncated":result.truncated,
                "usage":result.usage,
                "evidence_count":result.evidence.len(),
            });
            if let Some(task) = &task {
                payload["tool_call_id"] = json!(safe_id(&task.parent_call_id));
            }
            values.push(("subagent".into(), payload));
            values
        }
        AgentEvent::InteractionRequired { request } => {
            if request.options.is_empty() && !request.allow_freeform {
                return vec![(
                    "error".into(),
                    json!({"code":"unsupported_capability","retryable":false,"message":"runtime requested an interaction without a supported response"}),
                )];
            }
            session.interactions.insert(
                request.request_id.clone(),
                InteractionState {
                    turn_id: turn_id.into(),
                    kind: InteractionKind::Input,
                    option_ids: request
                        .options
                        .iter()
                        .map(|option| option.id.clone())
                        .collect(),
                    allow_freeform: request.allow_freeform,
                },
            );
            let options = request.options.iter().take(20).map(|option| json!({"id":safe_id(&option.id),"label":bounded_text_with_keys(&option.label,128,&session.redaction_keys).0})).collect::<Vec<_>>();
            let (body, truncated, bytes) =
                bounded_text_with_keys(&request.body, 512, &session.redaction_keys);
            vec![
                (
                    "interaction_request".into(),
                    json!({"interaction_id":safe_id(&request.request_id),"type":"user_input","response":{"decisions":["respond"],"options":options,"allow_freeform":request.allow_freeform},"summary":{"text":body,"truncated":truncated,"original_bytes":bytes}}),
                ),
                ("status".into(), json!({"status":"waiting_interaction"})),
            ]
        }
        AgentEvent::MemoryCandidate {
            candidate_id,
            body_sha256,
        } => {
            // Candidate events are deliberately metadata-only. The host must
            // fetch the scoped candidate through memory/list before displaying
            // or mutating it; never put the candidate body on the event bus.
            if !valid_memory_candidate_id(candidate_id) || !valid_sha256(body_sha256) {
                return Vec::new();
            }
            vec![(
                "memory_candidate".into(),
                json!({
                    "candidate_id": candidate_id,
                    "body_sha256": body_sha256,
                }),
            )]
        }
        AgentEvent::Error { .. } => vec![(
            "error".into(),
            json!({"code":"runtime_unavailable","retryable":true,"message":"local runtime reported an error"}),
        )],
        AgentEvent::Failure { details, .. } => {
            // Only failures of the local runtime itself are `runtime_unavailable`; model
            // provider and tool failures must not tell the host the runtime is down.
            let code = match details.stage {
                rivo_session::FailureStage::Persistence => "storage_unavailable",
                _ if details.code.contains("timeout") => "runtime_timeout",
                rivo_session::FailureStage::Stream => "provider_failed",
                rivo_session::FailureStage::Tool => "tool_failed",
                _ => "runtime_unavailable",
            };
            let response_source =
                provider_response_body(&details.cause).unwrap_or_else(|| details.cause.clone());
            let (response_body, truncated, original_bytes) = bounded_text_with_keys(
                &response_source,
                MAX_EVENT_TEXT_BYTES,
                &session.redaction_keys,
            );
            let message_source = if code == "provider_failed" {
                "Provider request failed"
            } else if details.cause.is_empty() {
                "local runtime reported a failure"
            } else {
                &details.cause
            };
            let (message, _, _) = bounded_text_with_keys(
                message_source,
                MAX_EVENT_TEXT_BYTES,
                &session.redaction_keys,
            );
            let mut response = serde_json::Map::new();
            response.insert("body".into(), json!(response_body));
            response.insert("body_truncated".into(), json!(truncated));
            response.insert("body_original_bytes".into(), json!(original_bytes));
            if let Some(status) = provider_http_status(&details.cause) {
                response.insert("status".into(), json!(status));
            }
            if let Some(provider_code) = provider_error_code(&details.cause) {
                response.insert("provider_code".into(), json!(provider_code));
            }
            let recovery_source = if details.recovery.trim().is_empty() && code == "provider_failed"
            {
                "Retry later or switch provider/model"
            } else {
                &details.recovery
            };
            vec![(
                "error".into(),
                json!({
                    "code":code,
                    "retryable":details.retryable,
                    "message":message,
                    "response":response,
                    "recovery":bounded_text_with_keys(recovery_source, 512, &session.redaction_keys).0,
                }),
            )]
        }
        AgentEvent::Stopped { reason } => {
            if turn_id.is_empty() {
                return vec![("status".into(), json!({"status":"stopped"}))];
            }
            let interrupted = session.active.as_ref().is_some_and(|turn| turn.interrupted)
                || reason.contains("取消");
            vec![
                (
                    "turn_complete".into(),
                    json!({"outcome":if interrupted {"interrupted"} else {"failed"}}),
                ),
                (
                    "status".into(),
                    json!({"status":if interrupted {"stopped"} else {"error"}}),
                ),
            ]
        }
        _ => Vec::new(),
    }
}

/// Records and fans out one session notification. Returns `false` once the
/// session is no longer hosted.
async fn publish_notification(
    state: &Arc<Mutex<ServerState>>,
    out: &mpsc::Sender<Value>,
    session_ref: &str,
    turn_id: &str,
    kind: &str,
    payload: Value,
) -> bool {
    let seq = {
        let mut locked = state.lock().expect("server state lock poisoned");
        let Some(session) = locked.sessions.get_mut(session_ref) else {
            return false;
        };
        session.seq = session.seq.saturating_add(1);
        session.seq
    };
    update_sidecar_event_sequence(session_ref, turn_id, seq);
    let notification = event_notification(session_ref, turn_id, seq, kind, payload);
    {
        let mut locked = state.lock().expect("server state lock poisoned");
        let history = locked
            .event_history
            .entry(session_ref.to_owned())
            .or_default();
        history.push_back(notification.clone());
        while history.len() > 512 {
            history.pop_front();
        }
    }
    let _ = out.send(notification.clone()).await;
    let bus = state
        .lock()
        .expect("server state lock poisoned")
        .event_broadcast
        .clone();
    if let Some(bus) = bus {
        let _ = bus.send(notification);
    }
    true
}

fn event_notification(
    session_ref: &str,
    turn_id: &str,
    seq: u64,
    kind: &str,
    payload: Value,
) -> Value {
    json!({"jsonrpc":"2.0","method":"event","params":{"session_ref":session_ref,"turn_id":turn_id,"seq":seq,"timestamp_utc_ms":utc_now(),"type":kind,"payload":payload}})
}

fn health(state: &Arc<Mutex<ServerState>>) -> Value {
    if let Ok(Some(store)) = history_store() {
        let _ = store.expire_sidecar_leases();
    }
    let locked = state.lock().expect("server state lock poisoned");
    let active_turn_count = locked
        .sessions
        .values()
        .filter(|session| session.active.is_some())
        .count();
    let sidecar_context = locked.sidecar_context.clone();
    drop(locked);
    let session_storage = rivo_session::data_root_dir()
        .map(|root| root.join("rivo").join("rivo.db"))
        .and_then(|path| rivo_storage::Store::open_runtime(path).ok())
        .map(|_| "ok")
        .unwrap_or("unavailable");
    let provider_configuration = match ResolvedConfig::load(ConfigOverrides::default()) {
        Ok(config) if config.route.is_some() => "ok",
        Ok(_) => "degraded",
        Err(_) => "unavailable",
    };
    json!({"app_server":"ok","session_storage":session_storage,"provider_configuration":provider_configuration,"active_turns":"ok","active_turn_count":active_turn_count,"sidecar":sidecar_context})
}

fn validate_bound_workspace(
    state: &Arc<Mutex<ServerState>>,
    working_directory: &str,
) -> Result<(), RpcError> {
    let root = state
        .lock()
        .expect("server state lock poisoned")
        .workspace_root
        .clone();
    let Some(root) = root else {
        return Ok(());
    };
    validate_workspace_path(&root, &PathBuf::from(working_directory)).map_err(|_| {
        RpcError::new(
            "workspace_binding_mismatch",
            "working_directory is outside the bound Sidecar Workspace",
            false,
        )
    })?;
    Ok(())
}

fn validate_resume_host_binding(state: &Arc<Mutex<ServerState>>) -> Result<(), RpcError> {
    let context = state
        .lock()
        .expect("server state lock poisoned")
        .sidecar_context
        .clone()
        .ok_or_else(|| {
            RpcError::new(
                "unsupported_capability",
                "Sidecar host capability context is unavailable",
                false,
            )
        })?;
    context.validate_public().map_err(|_| {
        RpcError::new(
            "unsupported_capability",
            "Sidecar host capability context is invalid",
            false,
        )
    })?;
    Ok(())
}

fn validate_resume_runtime(
    session: Option<&Arc<Session>>,
    settings: &RuntimeSettings,
) -> Result<(), RpcError> {
    let session = session.ok_or_else(|| {
        RpcError::new(
            "runtime_unavailable",
            "local runtime state is unavailable",
            true,
        )
    })?;
    let options = turn_options(settings)?;
    session.validate_current_route().map_err(|_| {
        RpcError::new(
            "unsupported_setting",
            "current model route is no longer available for this local session",
            false,
        )
    })?;
    if let Some(selection) = selection_from_settings(settings)? {
        session.validate_route_selection(&selection).map_err(|_| {
            RpcError::new(
                "unsupported_setting",
                "model route is no longer available for this local session",
                false,
            )
        })?;
    }
    session.validate_mcp_selection(&options).map_err(|_| {
        RpcError::new(
            "unsupported_capability",
            "requested MCP selection is not configured for this local session",
            false,
        )
    })?;
    Ok(())
}

fn build_config(
    working_directory: &str,
    settings: &RuntimeSettings,
) -> Result<rivo_session::ResolvedConfig, RpcError> {
    let workspace = PathBuf::from(working_directory);
    if !bounded(working_directory) || !workspace.is_absolute() || !workspace.is_dir() {
        return Err(RpcError::new(
            "invalid_request",
            "working_directory must be an existing absolute local directory",
            false,
        ));
    }
    let configured = ResolvedConfig::load(ConfigOverrides::default())
        .map_err(map_session_error)?
        .workspace;
    let requested = workspace.canonicalize().map_err(|_| RpcError::invalid())?;
    let configured = configured.canonicalize().map_err(|_| {
        RpcError::new(
            "runtime_unavailable",
            "local workspace configuration is unavailable",
            false,
        )
    })?;
    if requested != configured {
        return Err(RpcError::new(
            "unsupported_capability",
            "working_directory is not the locally configured workspace",
            false,
        ));
    }
    let mut config = ConfigOverrides {
        workspace: Some(requested),
        provider_id: settings.provider_id.clone(),
        auto_provider: settings.auto_provider,
        ..ConfigOverrides::default()
    };
    if let Some(locale) = &settings.response_language {
        if !bounded(locale) {
            return Err(RpcError::invalid());
        }
        config.locale = Some(locale.clone());
    }
    if settings
        .provider_id
        .as_deref()
        .is_some_and(|id| !bounded(id))
        || (settings.auto_provider && settings.provider_id.is_some())
    {
        return Err(RpcError::invalid());
    }
    if let Some(vendor) = &settings.vendor {
        if !bounded(vendor) {
            return Err(RpcError::invalid());
        }
        config.vendor = Some(vendor.clone());
    }
    if let Some(model) = &settings.model {
        if !bounded(model) {
            return Err(RpcError::invalid());
        }
        config.model = Some(model.clone());
    }
    ResolvedConfig::load(config).map_err(map_session_error)
}

fn local_file_config() -> Result<rivo_session::FileConfig, RpcError> {
    read_file_config(&local_config_path()?).map_err(map_session_error)
}

fn local_config_path() -> Result<PathBuf, RpcError> {
    std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(config_file_path)
        .ok_or_else(|| {
            RpcError::new(
                "configuration_error",
                "configuration path unavailable",
                false,
            )
        })
}

fn turn_options(settings: &RuntimeSettings) -> Result<TurnOptions, RpcError> {
    let max_subagents = settings.max_subagents.unwrap_or(5);
    if !(1..=MAX_SUBAGENTS).contains(&max_subagents) {
        return Err(RpcError::new(
            "unsupported_setting",
            "max_subagents is outside the supported range",
            false,
        ));
    }
    let reasoning_effort = match settings.reasoning_effort.as_deref().unwrap_or("default") {
        "default" | "provider_default" => rivo_llm::ReasoningEffort::ProviderDefault,
        "low" => rivo_llm::ReasoningEffort::Low,
        "medium" => rivo_llm::ReasoningEffort::Medium,
        "high" => rivo_llm::ReasoningEffort::High,
        _ => {
            return Err(RpcError::new(
                "invalid_setting",
                "reasoning_effort must be default, low, medium, or high",
                false,
            ))
        }
    };
    let capability_profile = match settings.collaboration_mode.as_deref().unwrap_or("code") {
        "plan" => CapabilityProfile::Plan,
        "code" => CapabilityProfile::Coding,
        _ => {
            return Err(RpcError::new(
                "unsupported_setting",
                "collaboration_mode is not supported",
                false,
            ));
        }
    };
    let policy_profile = match effective_permission_preset(settings) {
        "read_only" => PolicyProfile::Strict,
        "workspace_write" => PolicyProfile::Medium,
        "full_access" => PolicyProfile::FullControl,
        _ => {
            return Err(RpcError::new(
                "unsupported_setting",
                "permission_preset is not supported",
                false,
            ));
        }
    };
    if settings.mcp_servers.iter().any(|server| !bounded(server))
        || settings.mcp_servers.iter().collect::<HashSet<_>>().len() != settings.mcp_servers.len()
        || settings.mcp_toolsets.iter().any(|(server, toolsets)| {
            !bounded(server)
                || !settings.mcp_servers.contains(server)
                || toolsets.is_empty()
                || toolsets.iter().any(|toolset| !bounded(toolset))
        })
    {
        return Err(RpcError::invalid());
    }
    let locale = settings.response_language.as_deref().unwrap_or("en-US");
    let locale = rivo_i18n::LocaleId::parse(locale)
        .map_err(|_| {
            RpcError::new(
                "unsupported_setting",
                "response_language is not a canonical BCP 47 locale",
                false,
            )
        })?
        .to_string();
    Ok(TurnOptions {
        locale,
        capability_profile,
        policy_profile,
        reasoning_effort,
        network: settings.network,
        browser: false,
        computer: false,
        shell: false,
        mcp_servers: settings.mcp_servers.clone(),
        mcp_toolsets: settings.mcp_toolsets.clone(),
        memory_recall: true,
        trace: false,
        subagents_enabled: settings.subagents_enabled.unwrap_or(true),
        max_subagents,
        a2a_peers: Default::default(),
    })
}

/// Session defaults follow the local config for subagents unless the host chose explicitly.
fn apply_config_defaults(settings: &mut RuntimeSettings, config: &ResolvedConfig) {
    settings
        .subagents_enabled
        .get_or_insert(config.subagents_enabled);
    settings
        .max_subagents
        .get_or_insert(config.max_subagents.min(MAX_SUBAGENTS));
}

fn settings_summary(settings: &RuntimeSettings) -> Value {
    let mut value = json!({"vendor":settings.vendor,"model":settings.model,"provider_id":settings.provider_id,"auto_provider":settings.auto_provider,"collaboration_mode":settings.collaboration_mode.as_deref().unwrap_or("code"),"permission_preset":effective_permission_preset(settings),"network":settings.network,"reasoning_effort":settings.reasoning_effort.as_deref().unwrap_or("default"),"mcp_servers":settings.mcp_servers,"mcp_toolsets":settings.mcp_toolsets,"subagents_enabled":settings.subagents_enabled.unwrap_or(true),"max_subagents":settings.max_subagents.unwrap_or(5),"session_messaging":settings.session_messaging.unwrap_or(false)});
    if let Some(locale) = &settings.response_language {
        value["response_language"] = Value::String(locale.clone());
    }
    value
}

fn effective_permission_preset(settings: &RuntimeSettings) -> &str {
    settings.permission_preset.as_deref().unwrap_or_else(|| {
        if settings.collaboration_mode.as_deref() == Some("plan") {
            "read_only"
        } else {
            "workspace_write"
        }
    })
}

fn selection_from_settings(settings: &RuntimeSettings) -> Result<Option<ModelSelection>, RpcError> {
    if settings.auto_provider && settings.provider_id.is_some() {
        return Err(RpcError::invalid());
    }
    if settings.vendor.is_none()
        && settings.model.is_none()
        && settings.provider_id.is_none()
        && !settings.auto_provider
    {
        return Ok(None);
    }
    let file = local_file_config()?;
    let provider_id = if settings.auto_provider {
        None
    } else {
        settings
            .provider_id
            .as_deref()
            .or(file.defaults.provider_id.as_deref())
    };
    let route = resolve_available_route(
        &file,
        settings.vendor.as_deref(),
        settings.model.as_deref(),
        provider_id,
    )
    .map_err(map_session_error)?;
    Ok(Some(ModelSelection {
        vendor: route.vendor,
        model: route.model,
        provider: provider_id.map_or(ProviderSelection::Auto, |id| ProviderSelection::Explicit {
            provider_id: id.into(),
        }),
    }))
}

fn merge_settings(defaults: &RuntimeSettings, requested: &RuntimeSettings) -> RuntimeSettings {
    RuntimeSettings {
        response_language: requested
            .response_language
            .clone()
            .or_else(|| defaults.response_language.clone()),
        vendor: requested.vendor.clone().or_else(|| defaults.vendor.clone()),
        model: requested.model.clone().or_else(|| defaults.model.clone()),
        provider_id: if requested.auto_provider {
            None
        } else {
            requested
                .provider_id
                .clone()
                .or_else(|| defaults.provider_id.clone())
        },
        auto_provider: requested.auto_provider
            || (requested.provider_id.is_none() && defaults.auto_provider),
        collaboration_mode: requested
            .collaboration_mode
            .clone()
            .or_else(|| defaults.collaboration_mode.clone()),
        permission_preset: requested
            .permission_preset
            .clone()
            .or_else(|| defaults.permission_preset.clone()),
        reasoning_effort: requested
            .reasoning_effort
            .clone()
            .or_else(|| defaults.reasoning_effort.clone()),
        network: requested.network || defaults.network,
        mcp_servers: if requested.mcp_servers.is_empty() {
            defaults.mcp_servers.clone()
        } else {
            requested.mcp_servers.clone()
        },
        mcp_toolsets: if requested.mcp_toolsets.is_empty() {
            defaults.mcp_toolsets.clone()
        } else {
            requested.mcp_toolsets.clone()
        },
        subagents_enabled: requested.subagents_enabled.or(defaults.subagents_enabled),
        max_subagents: requested.max_subagents.or(defaults.max_subagents),
        session_messaging: requested.session_messaging.or(defaults.session_messaging),
    }
}
fn map_session_error(error: rivo_session::SessionError) -> RpcError {
    match error {
        rivo_session::SessionError::Storage(_) => RpcError::new(
            "storage_unavailable",
            "local session storage is unavailable",
            true,
        ),
        _ => RpcError::new(
            "runtime_unavailable",
            "local runtime configuration is unavailable",
            true,
        ),
    }
}
/// Configuration-management methods surface the concrete cause so a desktop
/// editor can point at the offending field instead of a generic outage.
fn map_config_error(error: rivo_session::SessionError) -> RpcError {
    use rivo_session::SessionError;
    let detail = error.to_string();
    match error {
        SessionError::RevisionConflict => RpcError::new(
            "revision_conflict",
            "configuration changed since it was read",
            false,
        ),
        SessionError::InvalidConfig(_)
        | SessionError::ConfigParse(_)
        | SessionError::ConfigSerialize(_)
        | SessionError::InvalidEnvironment { .. }
        | SessionError::MissingModel
        | SessionError::MissingRoute => {
            RpcError::new("configuration_invalid", "configuration is invalid", false)
                .with_detail(detail)
        }
        SessionError::MissingApiKey(_) | SessionError::Credential(_) => {
            RpcError::new("credential_error", "credential operation failed", false)
                .with_detail(detail)
        }
        SessionError::ConfigRead { .. } => RpcError::new(
            "configuration_unavailable",
            "configuration file is unavailable",
            true,
        )
        .with_detail(detail),
        SessionError::Provider(_) => {
            RpcError::new("provider_error", "provider request failed", true).with_detail(detail)
        }
        other => map_session_error(other),
    }
}
fn map_resume_error(error: rivo_session::SessionError) -> RpcError {
    match error {
        rivo_session::SessionError::Storage(rivo_storage::StorageError::NotFound(_)) => {
            RpcError::new("session_not_found", "session was not found", false)
        }
        rivo_session::SessionError::Storage(rivo_storage::StorageError::Expired(_)) => {
            RpcError::new(
                "session_resume_unavailable",
                "session is no longer resumable",
                false,
            )
        }
        _ => map_session_error(error),
    }
}
fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|_| RpcError::invalid())
}
fn only_keys(params: &Value, allowed: &[&str]) -> Result<(), RpcError> {
    let Some(object) = params.as_object() else {
        return Err(RpcError::invalid());
    };
    if object
        .keys()
        .any(|key| !allowed.iter().any(|allowed| *allowed == key))
    {
        return Err(RpcError::invalid());
    }
    Ok(())
}
fn valid_key(key: &str) -> Result<(), RpcError> {
    if bounded(key) && !key.is_empty() {
        Ok(())
    } else {
        Err(RpcError::invalid())
    }
}
fn valid_ref(value: &str) -> Result<(), RpcError> {
    if bounded(value) && !value.is_empty() {
        Ok(())
    } else {
        Err(RpcError::invalid())
    }
}
fn valid_memory_candidate_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn bounded(value: &str) -> bool {
    value.len() <= MAX_STRING_BYTES && !value.contains('\0')
}
fn session_workspace(
    state: &Arc<Mutex<ServerState>>,
    session_ref: &str,
) -> Result<String, RpcError> {
    let locked = state.lock().expect("server state lock poisoned");
    let session = locked
        .sessions
        .get(session_ref)
        .and_then(|session| session.session.as_ref())
        .ok_or_else(|| RpcError::new("session_not_found", "session was not found", false))?;
    Ok(session.workspace_path().to_string_lossy().into_owned())
}
fn map_memory_storage_error(error: rivo_storage::StorageError) -> RpcError {
    match error {
        rivo_storage::StorageError::NotFound(_) => RpcError::new(
            "memory_not_found",
            "memory was not found or is stale",
            false,
        ),
        _ => RpcError::new(
            "storage_unavailable",
            "local memory storage is unavailable",
            true,
        ),
    }
}
fn project_metadata(project: &rivo_storage::Project) -> Value {
    json!({
        "project_id": project.id,
        "display_name": project.display_name,
        "is_default": project.is_default,
        "last_opened_at": project.last_opened_at,
    })
}
fn workspace_metadata(workspace: &rivo_storage::Workspace) -> Value {
    json!({
        "workspace_id": workspace.id,
        "display_name": workspace.display_name,
        "is_default": workspace.is_default,
        "last_opened_at": workspace.last_opened_at,
    })
}
fn session_metadata(session: &rivo_storage::SessionSummary) -> Value {
    json!({
        "session_id": session.session_id,
        "workspace_id": session.project_id,
        "created_at": session.created_at,
        "expiration_reason": session.expiration_reason,
        "event_count": session.event_count,
        "message_count": session.message_count,
        "expired": session.expired,
    })
}
fn turn_metadata(turn: &rivo_storage::TurnSummary) -> Value {
    json!({
        "session_id": turn.session_id,
        "turn_id": turn.turn_id,
        "created_at": turn.created_at,
        "event_count": turn.event_count,
        "message_count": turn.message_count,
    })
}
fn event_metadata(event: rivo_storage::StoredEvent) -> Value {
    json!({
        "sequence": event.sequence,
        "kind": event.kind,
    })
}
fn message_metadata(message: rivo_storage::StoredMessage) -> Value {
    json!({
        "sequence": message.sequence,
        "role": message.role,
    })
}
fn memory_metadata(memory: &rivo_storage::Memory) -> Value {
    let (body, truncated, original_bytes) = bounded_text(&memory.body, MAX_EVENT_TEXT_BYTES);
    json!({
        "memory_id": memory.id,
        "workspace": memory.workspace,
        "mode": memory.mode,
        "body": body,
        "body_truncated": truncated,
        "body_original_bytes": original_bytes,
        "source": memory.source,
    })
}
fn memory_candidate_metadata(candidate: &rivo_storage::MemoryCandidate) -> Value {
    let (body, truncated, original_bytes) = bounded_text(&candidate.body, MAX_EVENT_TEXT_BYTES);
    json!({
        "candidate_id": candidate.id,
        "workspace": candidate.workspace,
        "mode": candidate.mode,
        "body": body,
        "body_truncated": truncated,
        "body_original_bytes": original_bytes,
        "body_sha256": candidate.body_sha256,
        "source": candidate.source,
    })
}
fn bounded_text(value: &str, limit: usize) -> (String, bool, usize) {
    bounded_text_with_keys(value, limit, &[])
}
fn provider_http_status(value: &str) -> Option<u16> {
    let marker = "HTTP ";
    let start = value.find(marker)? + marker.len();
    let digits: String = value[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    let status = digits.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}
fn provider_response_body(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let without_metadata = value
        .split_once(" (")
        .map(|(body, _)| body)
        .unwrap_or(value)
        .trim();
    let body = without_metadata
        .strip_prefix("Responses stream error: ")
        .or_else(|| without_metadata.strip_prefix("Provider request failed: "))
        .or_else(|| without_metadata.strip_prefix("No available accounts: "))
        .unwrap_or(without_metadata)
        .trim();
    (!body.is_empty()).then_some(body.to_owned())
}
fn provider_error_code(value: &str) -> Option<String> {
    let marker = "provider code ";
    let start = value.find(marker)? + marker.len();
    let code: String = value[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .collect();
    (!code.is_empty()).then_some(code)
}
fn bounded_text_with_keys(value: &str, limit: usize, keys: &[String]) -> (String, bool, usize) {
    let bytes = value.len();
    if bytes <= limit {
        return (
            redact_text(value, keys).as_str().unwrap_or_default().into(),
            false,
            bytes,
        );
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (
        redact_text(&value[..end], keys)
            .as_str()
            .unwrap_or_default()
            .into(),
        true,
        bytes,
    )
}
fn text_payload_with_keys(value: &str, keys: &[String]) -> Value {
    let (text, truncated, original_bytes) =
        bounded_text_with_keys(value, MAX_EVENT_TEXT_BYTES, keys);
    json!({"text":text,"truncated":truncated,"original_bytes":original_bytes})
}
fn redact_text(value: &str, keys: &[String]) -> Value {
    let generic = rivo_telemetry::redact(&Value::String(value.into()));
    rivo_telemetry::redact_api_keys(&generic, keys)
}
fn with_tool_status(payload: &Value, status: &str) -> Value {
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("status".into(), Value::String(status.into()));
    }
    payload
}

/// Child call ids are only unique within one task; prefix them so they cannot
/// collide with root calls or calls of a parallel subagent.
fn subagent_call_id(task_id: &AgentTaskId, call_id: &str) -> String {
    safe_id(&format!("{}:{call_id}", task_id.0))
}

fn subagent_payload(
    session: &SessionState,
    agent_id: &AgentId,
    task_id: &AgentTaskId,
    status: &str,
) -> Value {
    let mut payload =
        json!({"agent_id":safe_id(&agent_id.0),"task_id":safe_id(&task_id.0),"status":status});
    if let Some(task) = session.subagent_tasks.get(&task_id.0) {
        payload["tool_call_id"] = json!(safe_id(&task.parent_call_id));
    }
    payload
}

fn subagent_tool_payload(
    session: &SessionState,
    agent_id: &AgentId,
    task_id: &AgentTaskId,
    tool_call_id: &str,
    name: &str,
    status: &str,
) -> Value {
    json!({
        "tool_call_id":tool_call_id,
        "name":bounded_text_with_keys(name, 128, &session.redaction_keys).0,
        "agent_id":safe_id(&agent_id.0),
        "task_id":safe_id(&task_id.0),
        "status":status,
        "summary":{"truncated":false,"original_bytes":0}
    })
}

fn tool_error_code(result: &Value) -> Option<&'static str> {
    if let Some(code) = result.get("error_code").and_then(Value::as_str) {
        return match code {
            "invalid_arguments" => Some("invalid_arguments"),
            "policy_denied" => Some("policy_denied"),
            "io_error" => Some("io_error"),
            "timeout" => Some("timeout"),
            "version_conflict" => Some("version_conflict"),
            "protected_path" => Some("protected_path"),
            "confirmation_mismatch" => Some("confirmation_mismatch"),
            "tool_error" => Some("tool_error"),
            "unsupported_capability" => Some("unsupported_capability"),
            _ => None,
        };
    }
    let message = result.get("error").and_then(Value::as_str)?;
    if message.contains("参数错误") {
        Some("invalid_arguments")
    } else if message.contains("策略拒绝") {
        Some("policy_denied")
    } else if message.contains("I/O 错误") {
        Some("io_error")
    } else if message.contains("命令执行超时") {
        Some("timeout")
    } else if message.contains("版本已变化") {
        Some("version_conflict")
    } else if message.contains("受保护路径") {
        Some("protected_path")
    } else if message.contains("确认信息") {
        Some("confirmation_mismatch")
    } else {
        Some("tool_error")
    }
}
fn interaction_type(tool: &str) -> &'static str {
    let tool = tool.to_ascii_lowercase();
    if tool.contains("write") || tool.contains("file") {
        "file_change_approval"
    } else if tool.contains("permission") {
        "permission_request"
    } else {
        "command_approval"
    }
}
fn safe_id(value: &str) -> String {
    bounded_text(value, 128).0
}
fn utc_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn next_id(prefix: &str) -> String {
    static IDS: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}", IDS.fetch_add(1, Ordering::Relaxed))
}
fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn error_response(id: Value, error: RpcError) -> Value {
    let mut body = json!({"code":error.code,"message":error.message,"retryable":error.retryable});
    if let Some(detail) = error.detail {
        body["detail"] = json!(detail);
    }
    json!({"jsonrpc":"2.0","id":id,"error":body})
}
async fn close_all(state: &Arc<Mutex<ServerState>>) {
    let senders = state
        .lock()
        .expect("server state lock poisoned")
        .sessions
        .drain()
        .map(|(_, s)| s.sender)
        .collect::<Vec<_>>();
    for sender in senders {
        let _ = sender.send(Intent::CancelTurn).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialized_state() -> Arc<Mutex<ServerState>> {
        Arc::new(Mutex::new(ServerState {
            initialized: true,
            ..ServerState::default()
        }))
    }

    #[test]
    fn media_refs_are_session_bound_and_injected_without_delivery_data() {
        let media = MediaRef {
            asset_id: "asset-1".into(),
            session_id: "session-1".into(),
            media_type: "image/png".into(),
            width: Some(1024),
            height: Some(1024),
        };
        let prompt =
            append_media_refs("把它改成油画", std::slice::from_ref(&media), "session-1").unwrap();
        assert!(prompt.contains("asset-1"));
        assert!(!prompt.contains("delivery"));
        assert!(append_media_refs("x", &[media], "other-session").is_err());
        assert!(append_media_refs(
            "x",
            &[MediaRef {
                asset_id: "../escape".into(),
                session_id: "session-1".into(),
                media_type: "image/png".into(),
                width: None,
                height: None,
            }],
            "session-1"
        )
        .is_err());
    }

    fn session_state(
        sender: mpsc::Sender<Intent>,
        active: Option<&str>,
        interactions: HashMap<String, InteractionState>,
    ) -> SessionState {
        SessionState {
            session: None,
            sender,
            seq: 0,
            active: active.map(|id| TurnState {
                id: id.into(),
                interrupted: false,
                message_started: false,
            }),
            interactions,
            subagent_tasks: HashMap::new(),
            open_tool_calls: BTreeMap::new(),
            default_settings: RuntimeSettings::default(),
            closing: false,
            redaction_keys: Vec::new(),
            inbound: messaging::InboundMode::default(),
        }
    }

    #[tokio::test]
    async fn initialize_validates_protocol_and_unlocks_health() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let (out, _) = mpsc::channel(1);
        let before = dispatch("health", json!({}), state.clone(), out.clone())
            .await
            .unwrap_err();
        assert_eq!(before.code, "invalid_request");
        let incompatible = dispatch(
            "initialize",
            json!({"protocol_version":"other","client":{"name":"test","version":"1"}}),
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(incompatible.code, "unsupported_protocol_version");
        let initialized = dispatch(
            "initialize",
            json!({"protocol_version":PROTOCOL_VERSION,"client":{"name":"test","version":"1"}}),
            state.clone(),
            out,
        )
        .await
        .unwrap();
        assert_eq!(initialized["capabilities"]["supports_resume"], true);
        assert_eq!(initialized["capabilities"]["supports_approval"], true);
        assert_eq!(initialized["capabilities"]["supports_subagents"], true);
        assert_eq!(initialized["capabilities"]["supports_plan_workflow"], true);
        assert_eq!(
            initialized["capabilities"]["plan"]["artifact_runtime_owned"],
            true
        );
        assert_eq!(initialized["supported_runtime_settings"]["network"], true);
        assert_eq!(
            initialized["supported_runtime_settings"]["max_subagents"],
            json!({"min":1,"max":MAX_SUBAGENTS})
        );
        assert_eq!(initialized["capabilities"]["memory"]["list"], true);
        assert_eq!(initialized["capabilities"]["memory"]["confirm"], true);
        assert!(state.lock().unwrap().initialized);
    }

    #[tokio::test]
    async fn line_protocol_enforces_json_rpc_shape_and_can_report_health() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let (out, _) = mpsc::channel(4);
        for line in [
            "not-json",
            "[]",
            r#"{"jsonrpc":"1.0","id":1,"method":"health"}"#,
            r#"{"jsonrpc":"2.0","id":null,"method":"health"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"health","extra":true}"#,
        ] {
            let response = handle_line(line, state.clone(), out.clone()).await.unwrap();
            assert_eq!(response["error"]["code"], "invalid_request");
        }
        let initialized = handle_line(
            r#"{"jsonrpc":"2.0","id":"initialize","method":"initialize","params":{"protocol_version":"rivo-app-server-v2","client":{"name":"fixture","version":"1"}}}"#,
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        assert_eq!(initialized["result"]["server"]["name"], "rivo");
        let health = handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"health"}"#, state, out)
            .await
            .unwrap();
        assert_eq!(health["result"]["app_server"], "ok");
    }

    #[tokio::test]
    async fn rejects_legacy_protocol_and_thread_methods() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let (out, _) = mpsc::channel(4);
        let legacy = handle_line(
            r#"{"jsonrpc":"2.0","id":"initialize","method":"initialize","params":{"protocol_version":"rivo-app-server-v1","client":{"name":"fixture","version":"1"}}}"#,
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        assert_eq!(legacy["error"]["code"], "unsupported_protocol_version");
        let initialized = dispatch(
            "initialize",
            json!({"protocol_version": PROTOCOL_VERSION, "client":{"name":"test","version":"1"}}),
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        assert_eq!(initialized["server"]["name"], "rivo");
        let error = dispatch("thread/list", json!({}), state, out)
            .await
            .unwrap_err();
        assert_eq!(error.code, "unsupported_method");
    }

    #[tokio::test]
    async fn instance_control_session_methods_and_mutation_gate_are_enforced() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let (out, _) = mpsc::channel(1);
        dispatch(
            "initialize",
            json!({"protocol_version":PROTOCOL_VERSION,"client":{"name":"test","version":"1"}}),
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        let health = dispatch("health/read", json!({}), state.clone(), out.clone())
            .await
            .unwrap();
        assert_eq!(health["app_server"], "ok");
        let denied = dispatch(
            "session/send",
            json!({"session_ref":"missing","text":"x","idempotency_key":"1234567890123456"}),
            state,
            out,
        )
        .await
        .unwrap_err();
        assert_eq!(denied.code, "confirmation_required");
    }

    #[tokio::test]
    async fn close_all_cancels_each_active_runtime_sender() {
        let state = initialized_state();
        let (first_sender, mut first_receiver) = mpsc::channel(1);
        let (second_sender, mut second_receiver) = mpsc::channel(1);
        {
            let mut locked = state.lock().unwrap();
            locked.sessions.insert(
                "first".into(),
                session_state(first_sender, Some("turn-1"), HashMap::new()),
            );
            locked.sessions.insert(
                "second".into(),
                session_state(second_sender, None, HashMap::new()),
            );
        }
        close_all(&state).await;
        assert!(matches!(
            first_receiver.recv().await,
            Some(Intent::CancelTurn)
        ));
        assert!(matches!(
            second_receiver.recv().await,
            Some(Intent::CancelTurn)
        ));
        assert!(state.lock().unwrap().sessions.is_empty());
    }

    #[tokio::test]
    async fn interrupt_rejects_missing_or_non_active_turns() {
        let state = initialized_state();
        let (sender, _receiver) = mpsc::channel(1);
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            session_state(sender, None, HashMap::new()),
        );
        let (out, _) = mpsc::channel(1);
        let no_active = dispatch(
            "turn/interrupt",
            json!({"session_ref":"session-1","turn_id":"turn-1","idempotency_key":"key-1"}),
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(no_active.code, "no_active_turn");
        state
            .lock()
            .unwrap()
            .sessions
            .get_mut("session-1")
            .unwrap()
            .active = Some(TurnState {
            id: "turn-2".into(),
            interrupted: false,
            message_started: false,
        });
        let wrong_turn = dispatch(
            "turn/interrupt",
            json!({"session_ref":"session-1","turn_id":"turn-1","idempotency_key":"key-2"}),
            state,
            out,
        )
        .await
        .unwrap_err();
        assert_eq!(wrong_turn.code, "no_active_turn");
    }

    #[tokio::test]
    async fn interrupt_clears_turn_when_runtime_sender_is_closed() {
        let state = initialized_state();
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            session_state(sender, Some("turn-1"), HashMap::new()),
        );
        let (out, _) = mpsc::channel(1);
        let result = dispatch(
            "turn/interrupt",
            json!({"session_ref":"session-1","turn_id":"turn-1","idempotency_key":"key-1"}),
            state.clone(),
            out,
        )
        .await
        .unwrap_err();
        assert_eq!(result.code, "runtime_unavailable");
        assert!(state.lock().unwrap().sessions["session-1"].active.is_none());
    }

    #[tokio::test]
    async fn input_interaction_accepts_option_or_freeform_and_removes_it() {
        for (index, (request, expected)) in [
            (
                json!({"decision":"respond","selected_option_id":"option-a"}),
                Intent::RespondInteraction {
                    request_id: "interaction-1".into(),
                    value: "option-a".into(),
                },
            ),
            (
                json!({"decision":"respond","freeform_text":"custom"}),
                Intent::RespondInteraction {
                    request_id: "interaction-1".into(),
                    value: "custom".into(),
                },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let state = initialized_state();
            let (sender, mut receiver) = mpsc::channel(1);
            state.lock().unwrap().sessions.insert(
                "session-1".into(),
                session_state(
                    sender,
                    Some("turn-1"),
                    HashMap::from([(
                        "interaction-1".into(),
                        InteractionState {
                            turn_id: "turn-1".into(),
                            kind: InteractionKind::Input,
                            option_ids: HashSet::from(["option-a".into()]),
                            allow_freeform: true,
                        },
                    )]),
                ),
            );
            let mut request = request;
            request["session_ref"] = json!("session-1");
            request["interaction_id"] = json!("interaction-1");
            request["idempotency_key"] = json!(format!("key-{index}"));
            let (out, _) = mpsc::channel(1);
            assert_eq!(
                dispatch("interaction/respond", request, state.clone(), out)
                    .await
                    .unwrap()["accepted"],
                true
            );
            assert!(matches!(receiver.recv().await, Some(actual) if actual == expected));
            assert!(state.lock().unwrap().sessions["session-1"]
                .interactions
                .is_empty());
        }
    }

    #[tokio::test]
    async fn input_interaction_rejects_invalid_response_without_consuming_it() {
        let state = initialized_state();
        let (sender, _receiver) = mpsc::channel(1);
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            session_state(
                sender,
                Some("turn-1"),
                HashMap::from([(
                    "interaction-1".into(),
                    InteractionState {
                        turn_id: "turn-1".into(),
                        kind: InteractionKind::Input,
                        option_ids: HashSet::from(["option-a".into()]),
                        allow_freeform: false,
                    },
                )]),
            ),
        );
        let (out, _) = mpsc::channel(1);
        let result = dispatch(
            "interaction/respond",
            json!({"session_ref":"session-1","interaction_id":"interaction-1","decision":"respond","selected_option_id":"option-a","freeform_text":"also","idempotency_key":"key-1"}),
            state.clone(),
            out,
        )
        .await
        .unwrap_err();
        assert_eq!(result.code, "invalid_request");
        assert!(state.lock().unwrap().sessions["session-1"]
            .interactions
            .contains_key("interaction-1"));
    }
    #[test]
    fn event_text_is_bounded_and_redacted() {
        std::env::set_var("RIVO_APP_SERVER_API_KEY", "secret-value");
        let payload = text_payload_with_keys(&"x".repeat(MAX_EVENT_TEXT_BYTES + 1), &[]);
        assert!(payload["truncated"].as_bool().unwrap());
        assert_eq!(payload["original_bytes"], MAX_EVENT_TEXT_BYTES + 1);
        std::env::remove_var("RIVO_APP_SERVER_API_KEY");
    }
    #[test]
    fn event_text_masks_configured_key_values_with_nonstandard_environment_names() {
        let payload = text_payload_with_keys(
            "provider echoed bridge-secret-value",
            &["bridge-secret-value".into()],
        );
        assert!(!payload.to_string().contains("bridge-secret-value"));
    }
    #[test]
    fn persisted_history_projections_exclude_paths_and_content() {
        let secret = "bridge-secret-value";
        let workspace = "C:/private/workspace";
        let project = project_metadata(&rivo_storage::Project {
            id: "project-1".into(),
            display_name: "Workspace".into(),
            workspace_root: workspace.into(),
            is_default: true,
            last_opened_at: "2026-09-01T00:00:00Z".into(),
        });
        let session = session_metadata(&rivo_storage::SessionSummary {
            session_id: "session-1".into(),
            project_id: Some("project-1".into()),
            created_at: "2026-09-01T00:00:00Z".into(),
            task_summary: Some(format!("prompt {secret}")),
            expiration_reason: None,
            event_count: 1,
            message_count: 1,
            workspace_root: Some(workspace.into()),
            expired: false,
        });
        let event = event_metadata(rivo_storage::StoredEvent {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            sequence: 1,
            kind: "tool_call".into(),
            body: format!("{workspace} {secret}"),
        });
        let message = message_metadata(rivo_storage::StoredMessage {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            sequence: 2,
            role: "user".into(),
            body: format!("prompt {secret}"),
        });
        let output = json!({"project":project,"session":session,"event":event,"message":message});
        assert!(!output.to_string().contains(workspace));
        assert!(!output.to_string().contains(secret));
        assert_eq!(output["session"]["session_id"], "session-1");
        assert_eq!(output["session"]["workspace_id"], "project-1");
        assert!(output["session"].get("project_id").is_none());
        assert_eq!(output["event"]["kind"], "tool_call");
        assert_eq!(output["message"]["role"], "user");
    }
    #[test]
    fn interaction_type_classification_is_case_insensitive() {
        assert_eq!(interaction_type("WriteFile"), "file_change_approval");
        assert_eq!(interaction_type("RequestPermission"), "permission_request");
    }
    #[test]
    fn rejects_unknown_fields_and_malformed_settings() {
        let bad = serde_json::json!({"unknown":true});
        assert!(parse::<RuntimeSettings>(bad).is_err());
        let options = turn_options(&RuntimeSettings {
            reasoning_effort: Some("high".into()),
            ..RuntimeSettings::default()
        })
        .unwrap();
        assert_eq!(options.reasoning_effort, rivo_llm::ReasoningEffort::High);
    }

    #[test]
    fn routing_payloads_reject_old_objects_and_connection_overrides() {
        for field in [
            "provider_profile",
            "provider",
            "base_url",
            "api_key_env",
            "credential_name",
        ] {
            let payload = json!({field: "old"});
            assert!(parse::<RuntimeSettings>(payload.clone()).is_err());
            assert!(parse::<ModelRoutingReadParams>(payload).is_err());
        }
        for field in [
            "binding_id",
            "request_model_id",
            "endpoint",
            "provider_profile",
        ] {
            assert!(
                parse::<ModelRoutingPreviewParams>(json!({"vendor":"fixture",field:"old"}))
                    .is_err()
            );
        }
        for field in ["profiles", "endpoints", "bindings", "provider_profiles"] {
            assert!(parse::<ModelRoutingCommitParams>(
                json!({"revision":"missing","config":{"version":3,field:{}}})
            )
            .is_err());
        }
        let merged = merge_settings(
            &RuntimeSettings {
                provider_id: Some("default".into()),
                ..RuntimeSettings::default()
            },
            &RuntimeSettings {
                auto_provider: true,
                ..RuntimeSettings::default()
            },
        );
        assert!(merged.auto_provider);
        assert!(merged.provider_id.is_none());
        assert!(selection_from_settings(&RuntimeSettings {
            auto_provider: true,
            provider_id: Some("bad".into()),
            ..RuntimeSettings::default()
        })
        .is_err());
    }

    #[test]
    fn runtime_settings_map_only_supported_profiles_and_preserve_explicit_overrides() {
        let plan_default = RuntimeSettings {
            collaboration_mode: Some("plan".into()),
            ..RuntimeSettings::default()
        };
        assert_eq!(
            turn_options(&plan_default).unwrap().policy_profile,
            PolicyProfile::Strict
        );
        assert_eq!(
            settings_summary(&plan_default)["permission_preset"],
            "read_only"
        );
        let plan_read_only = RuntimeSettings {
            model: Some("fixture-model".into()),
            collaboration_mode: Some("plan".into()),
            permission_preset: Some("read_only".into()),
            reasoning_effort: None,
            ..RuntimeSettings::default()
        };
        let options = turn_options(&plan_read_only).unwrap();
        assert_eq!(options.capability_profile, CapabilityProfile::Plan);
        assert_eq!(options.policy_profile, PolicyProfile::Strict);
        assert!(!options.network && !options.browser && options.memory_recall);
        assert_eq!(
            settings_summary(&plan_read_only),
            json!({"vendor":null,"model":"fixture-model","provider_id":null,"auto_provider":false,"collaboration_mode":"plan","permission_preset":"read_only","network":false,"reasoning_effort":"default","mcp_servers":[],"mcp_toolsets":{},"subagents_enabled":true,"max_subagents":5,"session_messaging":false})
        );
        assert!(options.subagents_enabled);
        assert_eq!(options.max_subagents, 5);
        let plan_with_network = RuntimeSettings {
            collaboration_mode: Some("plan".into()),
            permission_preset: Some("read_only".into()),
            network: true,
            ..RuntimeSettings::default()
        };
        let network_options = turn_options(&plan_with_network).unwrap();
        assert_eq!(network_options.capability_profile, CapabilityProfile::Plan);
        assert_eq!(network_options.policy_profile, PolicyProfile::Strict);
        assert!(network_options.network);
        assert_eq!(settings_summary(&plan_with_network)["network"], true);
        let merged_network = merge_settings(
            &RuntimeSettings {
                network: true,
                ..RuntimeSettings::default()
            },
            &RuntimeSettings::default(),
        );
        assert!(merged_network.network);
        let disabled = turn_options(&RuntimeSettings {
            subagents_enabled: Some(false),
            max_subagents: Some(2),
            ..RuntimeSettings::default()
        })
        .unwrap();
        assert!(!disabled.subagents_enabled);
        assert_eq!(disabled.max_subagents, 2);
        for max_subagents in [0, MAX_SUBAGENTS + 1] {
            let error = turn_options(&RuntimeSettings {
                max_subagents: Some(max_subagents),
                ..RuntimeSettings::default()
            })
            .unwrap_err();
            assert_eq!(error.code, "unsupported_setting");
        }
        let merged = merge_settings(
            &RuntimeSettings {
                subagents_enabled: Some(false),
                max_subagents: Some(3),
                ..RuntimeSettings::default()
            },
            &RuntimeSettings {
                max_subagents: Some(4),
                ..RuntimeSettings::default()
            },
        );
        assert_eq!(
            (merged.subagents_enabled, merged.max_subagents),
            (Some(false), Some(4))
        );

        let full = RuntimeSettings {
            collaboration_mode: Some("code".into()),
            permission_preset: Some("full_access".into()),
            ..RuntimeSettings::default()
        };
        let options = turn_options(&full).unwrap();
        assert_eq!(options.capability_profile, CapabilityProfile::Coding);
        assert_eq!(options.policy_profile, PolicyProfile::FullControl);

        let mcp = RuntimeSettings {
            mcp_servers: vec!["data".into()],
            mcp_toolsets: BTreeMap::from([("data".into(), BTreeSet::from(["query".into()]))]),
            ..RuntimeSettings::default()
        };
        let options = turn_options(&mcp).unwrap();
        assert_eq!(options.mcp_servers, vec!["data"]);
        assert!(options.mcp_toolsets["data"].contains("query"));
        assert!(turn_options(&RuntimeSettings {
            mcp_toolsets: BTreeMap::from([("data".into(), BTreeSet::from(["query".into()]),)]),
            ..RuntimeSettings::default()
        })
        .is_err());

        for (settings, expected_code) in [
            (
                RuntimeSettings {
                    collaboration_mode: Some("pair".into()),
                    ..RuntimeSettings::default()
                },
                "unsupported_setting",
            ),
            (
                RuntimeSettings {
                    permission_preset: Some("unsafe".into()),
                    ..RuntimeSettings::default()
                },
                "unsupported_setting",
            ),
            (
                RuntimeSettings {
                    reasoning_effort: Some("invalid".into()),
                    ..RuntimeSettings::default()
                },
                "invalid_setting",
            ),
        ] {
            assert_eq!(turn_options(&settings).unwrap_err().code, expected_code);
        }

        let merged = merge_settings(
            &RuntimeSettings {
                vendor: None,
                model: Some("base".into()),
                collaboration_mode: Some("plan".into()),
                permission_preset: Some("read_only".into()),
                reasoning_effort: None,
                mcp_servers: vec!["readonly-data".into()],
                mcp_toolsets: BTreeMap::from([(
                    "readonly-data".into(),
                    BTreeSet::from(["query".into()]),
                )]),
                ..RuntimeSettings::default()
            },
            &RuntimeSettings {
                model: None,
                collaboration_mode: Some("code".into()),
                permission_preset: None,
                reasoning_effort: Some("ignored".into()),
                ..RuntimeSettings::default()
            },
        );
        assert_eq!(merged.model.as_deref(), Some("base"));
        assert_eq!(merged.collaboration_mode.as_deref(), Some("code"));
        assert_eq!(merged.permission_preset.as_deref(), Some("read_only"));
        assert_eq!(merged.reasoning_effort.as_deref(), Some("ignored"));
        assert_eq!(merged.mcp_servers, vec!["readonly-data"]);
        assert!(merged.mcp_toolsets["readonly-data"].contains("query"));
    }

    #[test]
    fn protocol_strings_are_bounded_redacted_and_unicode_safe() {
        assert!(valid_key("key-1").is_ok());
        assert!(valid_ref("session-1").is_ok());
        assert!(valid_key("").is_err());
        assert!(valid_ref("\0").is_err());
        assert!(!bounded(&"x".repeat(MAX_STRING_BYTES + 1)));

        let (text, truncated, bytes) = bounded_text("你好🙂x", 10);
        assert_eq!(text, "你好🙂");
        assert!(truncated);
        assert_eq!(bytes, "你好🙂x".len());

        let redacted =
            bounded_text_with_keys("prefix secret-value suffix", 128, &["secret-value".into()]);
        assert!(!redacted.0.contains("secret-value"));
        assert_eq!(safe_id(&"z".repeat(256)).len(), 128);
        assert_eq!(
            with_tool_status(&json!({"name":"read_file"}), "running")["status"],
            "running"
        );
    }

    #[test]
    fn memory_candidate_projection_is_metadata_only_and_strictly_validated() {
        let (sender, _) = mpsc::channel(8);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        let hash = "a".repeat(64);
        let notifications = event_notifications(
            &AgentEvent::MemoryCandidate {
                candidate_id: "candidate-1".into(),
                body_sha256: hash.clone(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "memory_candidate");
        assert_eq!(notifications[0].1["candidate_id"], "candidate-1");
        assert_eq!(notifications[0].1["body_sha256"], hash);
        let fields = notifications[0]
            .1
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(fields, vec!["body_sha256", "candidate_id"]);

        for (candidate_id, body_sha256) in [
            ("", "a".repeat(64)),
            ("candidate with spaces", "a".repeat(64)),
            ("candidate-1", "not-a-hash".to_string()),
            ("candidate-1", "g".repeat(64)),
        ] {
            assert!(event_notifications(
                &AgentEvent::MemoryCandidate {
                    candidate_id: candidate_id.into(),
                    body_sha256,
                },
                &mut session,
                "turn-1",
            )
            .is_empty());
        }
    }

    #[test]
    fn emits_single_line_json_rpc_notification() {
        let message = event_notification("s", "t", 1, "status", json!({"status":"running"}));
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(!encoded.contains('\n'));
        assert_eq!(message["jsonrpc"], "2.0");
    }

    #[tokio::test]
    async fn emitted_terminal_events_are_sequenced_and_reclaim_closing_sessions() {
        let (sender, _intents) = mpsc::channel(4);
        let state = initialized_state();
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        session.closing = true;
        state
            .lock()
            .unwrap()
            .sessions
            .insert("session-1".into(), session);
        let (out, mut received) = mpsc::channel(8);
        let event = SessionEvent {
            metadata: rivo_session::EventMetadata {
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                timestamp_utc_ms: 1,
            },
            event: AgentEvent::FinalAnswer {
                text: "done".into(),
                stop_reason: rivo_llm::StopReason::Final,
                usage: rivo_llm::Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    total_tokens: 150,
                    reasoning_tokens: Some(8),
                    ..Default::default()
                },
            },
        };
        emit_session_event("session-1", event, &state, &out).await;
        let messages = (0..4)
            .map(|_| received.try_recv().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[0]["params"]["type"], "message_start");
        assert_eq!(messages[1]["params"]["type"], "message_end");
        assert_eq!(messages[2]["params"]["type"], "turn_complete");
        assert_eq!(
            messages[2]["params"]["payload"]["usage"],
            json!({"input_tokens":120,"output_tokens":30,"total_tokens":150,"reasoning_tokens":8})
        );
        assert_eq!(messages[3]["params"]["type"], "status");
        assert_eq!(messages[0]["params"]["seq"], 1);
        assert_eq!(messages[3]["params"]["seq"], 4);
        assert!(!state.lock().unwrap().sessions.contains_key("session-1"));
    }

    #[tokio::test]
    async fn event_broadcast_is_bounded_and_reusable_by_pipe_clients() {
        let (sender, _intents) = mpsc::channel(4);
        let state = initialized_state();
        let (bus, mut receiver) = broadcast::channel(2);
        state.lock().unwrap().event_broadcast = Some(bus);
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            session_state(sender, Some("turn-1"), HashMap::new()),
        );
        let (out, mut output) = mpsc::channel(8);
        let event = SessionEvent {
            metadata: rivo_session::EventMetadata {
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                timestamp_utc_ms: 1,
            },
            event: AgentEvent::ModelDelta { text: "x".into() },
        };
        emit_session_event("session-1", event, &state, &out).await;
        assert_eq!(
            receiver.recv().await.unwrap()["params"]["type"],
            "message_start"
        );
        assert_eq!(
            receiver.recv().await.unwrap()["params"]["type"],
            "message_delta"
        );
        assert_eq!(
            output.recv().await.unwrap()["params"]["type"],
            "message_start"
        );
    }

    #[tokio::test]
    async fn session_subscribe_replays_events_after_a_cursor() {
        let state = initialized_state();
        state.lock().unwrap().event_history.insert(
            "session-1".into(),
            VecDeque::from([
                event_notification(
                    "session-1",
                    "turn-1",
                    1,
                    "status",
                    json!({"status":"running"}),
                ),
                event_notification(
                    "session-1",
                    "turn-1",
                    2,
                    "message_delta",
                    json!({"delta":{"text":"x"}}),
                ),
            ]),
        );
        let (out, _) = mpsc::channel(1);
        let result = dispatch(
            "session/subscribe",
            json!({"session_ref":"session-1","from_seq":1}),
            state,
            out,
        )
        .await
        .unwrap();
        assert_eq!(result["next_seq"], 2);
        assert_eq!(result["events"].as_array().unwrap().len(), 1);
        assert_eq!(result["events"][0]["params"]["seq"], 2);
    }

    #[test]
    fn durable_history_reprojects_events_with_stable_notification_sequences() {
        let event = |turn_id: &str, sequence: u64, value: AgentEvent| {
            let body = serde_json::to_string(&value).unwrap();
            rivo_storage::StoredEvent {
                session_id: "session-1".into(),
                turn_id: turn_id.into(),
                sequence,
                kind: serde_json::to_value(&value).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .into(),
                body,
            }
        };
        let stored = vec![
            event(
                "turn-1",
                1,
                AgentEvent::ModelDelta {
                    text: "hello".into(),
                },
            ),
            event(
                "turn-1",
                2,
                AgentEvent::FinalAnswer {
                    text: "done".into(),
                    stop_reason: rivo_llm::StopReason::Final,
                    usage: rivo_llm::Usage::default(),
                },
            ),
        ];
        let store = rivo_storage::Store::open_memory().unwrap();
        for stored_event in &stored {
            store.append_event(stored_event).unwrap();
        }
        store
            .append_message(&rivo_storage::StoredMessage {
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                role: "user".into(),
                body: "inspect the project".into(),
            })
            .unwrap();
        let persisted = durable_history_notifications_from_store("session-1", &store).unwrap();
        assert_eq!(
            persisted
                .iter()
                .map(|event| (&event["params"]["seq"], &event["params"]["type"]))
                .collect::<Vec<_>>(),
            project_durable_history(
                "session-1",
                &stored,
                &[rivo_storage::StoredMessage {
                    session_id: "session-1".into(),
                    turn_id: "turn-1".into(),
                    sequence: 1,
                    role: "user".into(),
                    body: "inspect the project".into(),
                }],
            )
            .unwrap()
            .iter()
            .map(|event| (&event["params"]["seq"], &event["params"]["type"]))
            .collect::<Vec<_>>()
        );
        let notifications = persisted;
        assert_eq!(
            notifications
                .iter()
                .map(|event| event["params"]["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(notifications[0]["params"]["type"], "user_message");
        assert_eq!(
            notifications[0]["params"]["payload"]["text"],
            "inspect the project"
        );
        assert_eq!(notifications[1]["params"]["type"], "message_start");
        assert_eq!(notifications[2]["params"]["type"], "message_delta");
        assert_eq!(notifications[3]["params"]["type"], "message_end");
        assert_eq!(notifications[4]["params"]["type"], "turn_complete");
        assert_eq!(notifications[5]["params"]["type"], "status");

        let first_page = durable_replay_page(notifications.clone(), 0);
        assert_eq!(first_page.len(), notifications.len());
        let second_page = durable_replay_page(notifications, 4);
        assert_eq!(
            second_page
                .iter()
                .map(|event| event["params"]["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
    }

    #[test]
    fn durable_history_page_is_bounded_by_rpc_line_size() {
        let notifications = (1..=MAX_REPLAY_EVENTS as u64)
            .map(|seq| {
                event_notification(
                    "session-1",
                    "turn-1",
                    seq,
                    "message_delta",
                    json!({"delta":{"text":"x".repeat(MAX_EVENT_TEXT_BYTES)}}),
                )
            })
            .collect::<Vec<_>>();
        let page = durable_replay_page(notifications, 0);
        assert!(page.len() < MAX_REPLAY_EVENTS);
        let response = json!({
            "session_ref": "session-1",
            "from_seq": 0,
            "next_seq": page.last().unwrap()["params"]["seq"],
            "events": page,
        });
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_REPLAY_BYTES);
    }

    #[tokio::test]
    async fn emitted_unanswerable_interaction_cancels_the_local_turn() {
        let (sender, mut intents) = mpsc::channel(4);
        let state = initialized_state();
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            session_state(sender, Some("turn-1"), HashMap::new()),
        );
        let (out, mut received) = mpsc::channel(4);
        let event = SessionEvent {
            metadata: rivo_session::EventMetadata {
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                sequence: 1,
                timestamp_utc_ms: 1,
            },
            event: AgentEvent::InteractionRequired {
                request: rivo_session::InteractionRequest {
                    request_id: "unanswerable".into(),
                    title: "fixture".into(),
                    body: "fixture".into(),
                    options: Vec::new(),
                    allow_freeform: false,
                },
            },
        };
        emit_session_event("session-1", event, &state, &out).await;
        assert_eq!(received.try_recv().unwrap()["params"]["type"], "error");
        assert!(matches!(intents.try_recv(), Ok(Intent::CancelTurn)));
    }

    #[test]
    fn unanswerable_interaction_is_not_exposed_to_the_host() {
        let (sender, _) = mpsc::channel(1);
        let mut session = SessionState {
            session: None,
            sender,
            seq: 0,
            active: Some(TurnState {
                id: "turn-1".into(),
                interrupted: false,
                message_started: false,
            }),
            interactions: HashMap::new(),
            subagent_tasks: HashMap::new(),
            open_tool_calls: BTreeMap::new(),
            default_settings: RuntimeSettings::default(),
            closing: false,
            redaction_keys: Vec::new(),
            inbound: messaging::InboundMode::default(),
        };
        let events = event_notifications(
            &AgentEvent::InteractionRequired {
                request: rivo_session::InteractionRequest {
                    request_id: "request-1".into(),
                    title: "unanswerable".into(),
                    body: "body".into(),
                    options: Vec::new(),
                    allow_freeform: false,
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(events[0].0, "error");
        assert_eq!(events[0].1["code"], "unsupported_capability");
        assert!(session.interactions.is_empty());
    }

    #[test]
    fn started_projection_preserves_route_without_adding_notifications() {
        let (sender, _) = mpsc::channel(8);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        let selection = rivo_session::ModelSelection {
            vendor: "openai".into(),
            model: "gpt-5".into(),
            provider: rivo_session::ProviderSelection::Auto,
        };
        let route = rivo_session::ResolvedRoute {
            vendor: selection.vendor.clone(),
            model: selection.model.clone(),
            provider_id: "b".into(),
            protocol: rivo_llm::ProviderKind::OpenAiResponses,
            base_url: "https://fixture.example/v1".into(),
            priority: 200,
        };
        let event = AgentEvent::Started {
            provider: route.protocol,
            model: route.model.clone(),
            selection: Some(selection.clone()),
            route: Some(route.clone()),
            turn_id: "turn-1".into(),
            capability_profile: CapabilityProfile::General,
            policy_profile: PolicyProfile::Strict,
        };
        let notifications = event_notifications(&event, &mut session, "turn-1");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "status");
        assert_eq!(notifications[0].1["status"], "running");
        assert_eq!(notifications[0].1["selection"], json!(selection));
        assert_eq!(notifications[0].1["route"], json!(route));
    }

    #[test]
    fn subagent_projection_namespaces_child_calls_and_closes_them_on_completion() {
        let (sender, _) = mpsc::channel(8);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        let agent_id = AgentId("agent-7".into());
        let task_id = AgentTaskId("task-7".into());
        let queued = event_notifications(
            &AgentEvent::SubagentQueued {
                agent_id: agent_id.clone(),
                task_id: task_id.clone(),
                call_id: "root-call".into(),
                label: "audit".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(queued[0].0, "subagent");
        assert_eq!(queued[0].1["status"], "queued");
        assert_eq!(queued[0].1["tool_call_id"], "root-call");
        let started = event_notifications(
            &AgentEvent::SubagentStarted {
                agent_id: agent_id.clone(),
                task_id: task_id.clone(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(started[0].1["status"], "running");
        let activity = |activity| AgentEvent::SubagentActivity {
            agent_id: agent_id.clone(),
            task_id: task_id.clone(),
            activity,
        };
        let requested = event_notifications(
            &activity(SubagentToolActivity::Requested {
                call_id: "call_0".into(),
                name: "read_file".into(),
            }),
            &mut session,
            "turn-1",
        );
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[0].1["tool_call_id"], "task-7:call_0");
        assert_eq!(requested[1].1["status"], "running");
        assert_eq!(requested[1].1["task_id"], "task-7");
        let observed = event_notifications(
            &activity(SubagentToolActivity::Observed {
                call_id: "call_0".into(),
                name: "read_file".into(),
                failed: true,
                error: Some("工具执行失败: 目标文件版本已变化".into()),
            }),
            &mut session,
            "turn-1",
        );
        assert_eq!(observed[0].1["status"], "failed");
        assert_eq!(observed[0].1["error_code"], "version_conflict");

        let confirmation = event_notifications(
            &AgentEvent::SubagentConfirmationRequired {
                agent_id: agent_id.clone(),
                task_id: task_id.clone(),
                request_id: "task-7:approval".into(),
                call_id: "call_1".into(),
                turn_id: "turn-1".into(),
                step_id: "call_1".into(),
                tool_name: "write_file".into(),
                arguments_summary: "write".into(),
                targets: vec![],
                program: None,
                side_effects: vec![],
                capability_profile: CapabilityProfile::Coding,
                policy_profile: PolicyProfile::Medium,
                constraints: rivo_policy::ExecutionConstraints {
                    workspace_root: PathBuf::from("."),
                    read_path_scope: rivo_policy::PathScope::WorkspaceOnly,
                    mutate_path_scope: rivo_policy::PathScope::WorkspaceOnly,
                    timeout_ms: 1,
                    output_limit: 1,
                    network_scope: rivo_policy::NetworkScope::PublicHttpsOnly,
                    redact_environment: true,
                },
                session_scope_available: false,
                session_scope_category: None,
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(
            confirmation
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            ["tool_call", "interaction_request"],
            "a waiting subagent must not change the root turn status"
        );
        assert_eq!(confirmation[0].1["status"], "waiting_interaction");
        assert_eq!(confirmation[1].1["tool_call_id"], "task-7:call_1");
        assert_eq!(confirmation[1].1["agent_id"], "agent-7");
        assert!(matches!(
            session.interactions["task-7:approval"].kind,
            InteractionKind::Confirmation
        ));
        assert!(
            event_notifications(
                &activity(SubagentToolActivity::Requested {
                    call_id: "call_1".into(),
                    name: "write_file".into(),
                }),
                &mut session,
                "turn-1",
            )
            .is_empty(),
            "a late request activity must not override the waiting state"
        );
        let resolved = event_notifications(
            &activity(SubagentToolActivity::ConfirmationResolved {
                request_id: "approval".into(),
                call_id: "call_1".into(),
                tool_name: "write_file".into(),
                allowed: true,
            }),
            &mut session,
            "turn-1",
        );
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1["status"], "running");

        let completed = event_notifications(
            &AgentEvent::SubagentCompleted {
                result: rivo_core::SubagentResult {
                    agent_id: agent_id.clone(),
                    task_id: task_id.clone(),
                    state: rivo_core::AgentTaskState::Cancelled,
                    summary: "private summary".into(),
                    evidence: vec![],
                    truncated: false,
                    usage: Default::default(),
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].1["tool_call_id"], "task-7:call_1");
        assert_eq!(completed[0].1["status"], "failed");
        assert_eq!(completed[0].1["error_code"], "cancelled");
        assert_eq!(completed[0].1["name"], "write_file");
        assert_eq!(completed[0].1["task_id"], "task-7");
        assert_eq!(completed[1].0, "subagent");
        assert_eq!(completed[1].1["status"], "cancelled");
        assert_eq!(completed[1].1["tool_call_id"], "root-call");
        assert!(!completed[1].1.to_string().contains("private summary"));
        assert!(session.subagent_tasks.is_empty());
    }

    #[test]
    fn turn_end_closes_tool_calls_the_runtime_never_finished() {
        let (sender, _) = mpsc::channel(8);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        session.active.as_mut().unwrap().interrupted = true;
        for call_id in ["delegate-a", "delegate-b"] {
            event_notifications(
                &AgentEvent::ToolRequested {
                    call_id: call_id.into(),
                    name: "delegate_task".into(),
                    arguments: json!({}),
                },
                &mut session,
                "turn-1",
            );
        }
        event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "delegate-b".into(),
                name: "delegate_task".into(),
                result: json!({"state":"completed"}),
            },
            &mut session,
            "turn-1",
        );
        let stopped = event_notifications(
            &AgentEvent::Stopped {
                reason: "用户取消".into(),
            },
            &mut session,
            "turn-1",
        );
        let kinds = stopped
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["tool_call", "turn_complete", "status"]);
        assert_eq!(stopped[0].1["tool_call_id"], "delegate-a");
        assert_eq!(stopped[0].1["name"], "delegate_task");
        assert_eq!(stopped[0].1["status"], "failed");
        assert_eq!(stopped[0].1["error_code"], "cancelled");
        assert_eq!(stopped[1].1["outcome"], "interrupted");
        assert!(session.open_tool_calls.is_empty());
    }

    #[test]
    fn event_projection_covers_message_tools_interactions_and_terminal_outcomes() {
        let (sender, _) = mpsc::channel(8);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        session.redaction_keys = vec!["secret".into()];
        assert_eq!(
            event_notifications(
                &AgentEvent::Started {
                    provider: rivo_llm::ProviderKind::OpenAiChatCompletions,
                    model: "fixture".into(),
                    selection: None,
                    route: None,
                    turn_id: "turn-1".into(),
                    capability_profile: CapabilityProfile::Coding,
                    policy_profile: PolicyProfile::Medium,
                },
                &mut session,
                "turn-1",
            )[0]
            .0,
            "status"
        );
        let delta = event_notifications(
            &AgentEvent::ModelDelta {
                text: "secret answer".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(delta[0].0, "message_start");
        assert!(!delta[1].1.to_string().contains("secret"));
        assert_eq!(
            event_notifications(
                &AgentEvent::ModelDelta {
                    text: "next".into()
                },
                &mut session,
                "turn-1",
            )
            .len(),
            1
        );
        let requested = event_notifications(
            &AgentEvent::ToolRequested {
                call_id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[1].1["status"], "running");
        let failed_observation = event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "call-1".into(),
                name: "read_file".into(),
                result: json!({"error":"denied"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(failed_observation[0].1["status"], "failed");

        let confirmation = event_notifications(
            &AgentEvent::ConfirmationRequired {
                request_id: "approval-1".into(),
                call_id: "call-2".into(),
                turn_id: "turn-1".into(),
                step_id: "step-1".into(),
                tool_name: "write_file".into(),
                arguments_summary: "secret change".into(),
                targets: vec![],
                program: None,
                side_effects: vec![],
                capability_profile: CapabilityProfile::Coding,
                policy_profile: PolicyProfile::Medium,
                constraints: rivo_policy::ExecutionConstraints {
                    workspace_root: PathBuf::from("."),
                    read_path_scope: rivo_policy::PathScope::WorkspaceOnly,
                    mutate_path_scope: rivo_policy::PathScope::WorkspaceOnly,
                    timeout_ms: 1,
                    output_limit: 1,
                    network_scope: rivo_policy::NetworkScope::PublicHttpsOnly,
                    redact_environment: true,
                },
                session_scope_available: false,
                session_scope_category: None,
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(confirmation[1].0, "interaction_request");
        assert!(matches!(
            session.interactions["approval-1"].kind,
            InteractionKind::Confirmation
        ));
        assert_eq!(confirmation[1].1["tool_call_id"], "call-2");
        let resumed = event_notifications(
            &AgentEvent::ConfirmationResolved {
                request_id: "approval-1".into(),
                call_id: "call-2".into(),
                tool_name: "write_file".into(),
                allowed: true,
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(resumed[0].0, "tool_call");
        assert_eq!(resumed[0].1["tool_call_id"], "call-2");
        assert_eq!(resumed[0].1["status"], "running");
        assert_eq!(resumed[1], ("status".into(), json!({"status":"running"})));
        let denied = event_notifications(
            &AgentEvent::ConfirmationResolved {
                request_id: "approval-1".into(),
                call_id: "call-2".into(),
                tool_name: "write_file".into(),
                allowed: false,
            },
            &mut session,
            "turn-1",
        );
        // The denial is handed back to the model; the following tool observation
        // closes the call, so only the turn status changes here.
        assert_eq!(denied, vec![("status".into(), json!({"status":"running"}))]);
        let denied_observation = event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "call-2".into(),
                name: "write_file".into(),
                result: json!({"error":"策略拒绝: 用户拒绝了此操作","error_code":"policy_denied"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(denied_observation[0].1["status"], "failed");
        assert_eq!(denied_observation[0].1["error_code"], "policy_denied");
        let unsupported = event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "call-3".into(),
                name: "generate_image".into(),
                result: json!({"error_code":"unsupported_capability","retryable":false}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(unsupported[0].1["status"], "failed");
        assert_eq!(unsupported[0].1["error_code"], "unsupported_capability");
        let input = event_notifications(
            &AgentEvent::InteractionRequired {
                request: rivo_session::InteractionRequest {
                    request_id: "input-1".into(),
                    title: "input".into(),
                    body: "secret body".into(),
                    options: vec![rivo_session::InteractionOption {
                        id: "yes".into(),
                        label: "Yes".into(),
                        description: String::new(),
                    }],
                    allow_freeform: true,
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(input[0].0, "interaction_request");
        assert!(matches!(
            session.interactions["input-1"].kind,
            InteractionKind::Input
        ));

        let final_events = event_notifications(
            &AgentEvent::FinalAnswer {
                text: "secret final".into(),
                stop_reason: rivo_llm::StopReason::Final,
                usage: rivo_llm::Usage::default(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(final_events.len(), 3);
        assert_eq!(final_events[1].0, "turn_complete");
        assert!(!final_events[0].1.to_string().contains("secret"));
        let persistence = event_notifications(
            &AgentEvent::Failure {
                message: "storage failed".into(),
                details: rivo_session::FailureDetails {
                    stage: rivo_session::FailureStage::Persistence,
                    code: "io".into(),
                    retryable: false,
                    recovery: String::new(),
                    cause: String::new(),
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(persistence[0].1["code"], "storage_unavailable");
        session.active = Some(TurnState {
            id: "turn-1".into(),
            interrupted: true,
            message_started: true,
        });
        let stopped = event_notifications(
            &AgentEvent::Stopped {
                reason: "用户取消".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(stopped[0].1["outcome"], "interrupted");
        assert_eq!(
            event_notifications(
                &AgentEvent::Stopped {
                    reason: "startup failed".into(),
                },
                &mut session,
                "",
            )[0]
            .1["status"],
            "stopped"
        );
    }

    #[test]
    fn failed_turn_submission_can_clear_its_reserved_active_turn() {
        let (sender, _) = mpsc::channel(1);
        let state = Arc::new(Mutex::new(ServerState::default()));
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            SessionState {
                session: None,
                sender,
                seq: 0,
                active: Some(TurnState {
                    id: "turn-1".into(),
                    interrupted: false,
                    message_started: false,
                }),
                interactions: HashMap::new(),
                subagent_tasks: HashMap::new(),
                open_tool_calls: BTreeMap::new(),
                default_settings: RuntimeSettings::default(),
                closing: false,
                redaction_keys: Vec::new(),
                inbound: messaging::InboundMode::default(),
            },
        );
        clear_active_turn(&state, "session-1", "turn-1");
        assert!(state.lock().unwrap().sessions["session-1"].active.is_none());
    }

    #[tokio::test]
    async fn failed_interaction_delivery_keeps_the_interaction_retryable() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let state = Arc::new(Mutex::new(ServerState {
            initialized: true,
            ..ServerState::default()
        }));
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            SessionState {
                session: None,
                sender,
                seq: 0,
                active: Some(TurnState {
                    id: "turn-1".into(),
                    interrupted: false,
                    message_started: false,
                }),
                interactions: HashMap::from([(
                    "interaction-1".into(),
                    InteractionState {
                        turn_id: "turn-1".into(),
                        kind: InteractionKind::Confirmation,
                        option_ids: HashSet::new(),
                        allow_freeform: false,
                    },
                )]),
                subagent_tasks: HashMap::new(),
                open_tool_calls: BTreeMap::new(),
                default_settings: RuntimeSettings::default(),
                closing: false,
                redaction_keys: Vec::new(),
                inbound: messaging::InboundMode::default(),
            },
        );
        let (out, _) = mpsc::channel(1);
        let result = dispatch(
            "interaction/respond",
            json!({"session_ref":"session-1","interaction_id":"interaction-1","decision":"allow","idempotency_key":"interaction-key"}),
            state.clone(),
            out,
        )
        .await;
        assert_eq!(result.unwrap_err().code, "runtime_unavailable");
        assert!(state.lock().unwrap().sessions["session-1"]
            .interactions
            .contains_key("interaction-1"));
    }

    #[tokio::test]
    async fn idempotency_cache_replays_stable_errors_without_reexecuting() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let first = cached_or(
            "turn/start",
            "session-1",
            "key-1",
            json!({"prompt":"same"}),
            &state,
            || async {
                Err(RpcError::new(
                    "turn_in_progress",
                    "session already has an active turn",
                    false,
                ))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(first.code, "turn_in_progress");
        let second = cached_or(
            "turn/start",
            "session-1",
            "key-1",
            json!({"prompt":"same"}),
            &state,
            || async { Ok(json!({"unexpected":true})) },
        )
        .await
        .unwrap_err();
        assert_eq!(second.code, "turn_in_progress");
    }

    #[tokio::test]
    async fn close_reclaims_a_session_when_its_driver_is_already_gone() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let state = Arc::new(Mutex::new(ServerState {
            initialized: true,
            ..ServerState::default()
        }));
        state.lock().unwrap().sessions.insert(
            "session-1".into(),
            SessionState {
                session: None,
                sender,
                seq: 0,
                active: None,
                interactions: HashMap::new(),
                subagent_tasks: HashMap::new(),
                open_tool_calls: BTreeMap::new(),
                default_settings: RuntimeSettings::default(),
                closing: false,
                redaction_keys: Vec::new(),
                inbound: messaging::InboundMode::default(),
            },
        );
        let (out, _) = mpsc::channel(1);
        let result = dispatch(
            "session/close",
            json!({"session_ref":"session-1","idempotency_key":"close-key"}),
            state.clone(),
            out,
        )
        .await
        .unwrap();
        assert_eq!(result["closed"], true);
        assert!(!state.lock().unwrap().sessions.contains_key("session-1"));
    }

    #[tokio::test]
    async fn protocol_rejects_unknown_methods_fields_and_oversized_lines() {
        let state = Arc::new(Mutex::new(ServerState {
            initialized: true,
            ..ServerState::default()
        }));
        let (out, _) = mpsc::channel(1);
        let unknown = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"not/a/method","params":{}}"#,
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], "unsupported_method");
        let field = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"health","params":{},"extra":true}"#,
            state.clone(),
            out.clone(),
        )
        .await
        .unwrap();
        assert_eq!(field["error"]["code"], "invalid_request");
        let oversized = handle_line(&"x".repeat(MAX_LINE_BYTES + 1), state, out)
            .await
            .unwrap();
        assert_eq!(oversized["error"]["code"], "invalid_request");
    }

    #[test]
    fn event_projection_covers_stream_tool_input_failure_and_terminal_paths() {
        let (sender, _receiver) = mpsc::channel(4);
        let mut session = session_state(sender, Some("turn-1"), HashMap::new());
        session.redaction_keys = vec!["projection-secret".into()];

        let first_delta = event_notifications(
            &AgentEvent::ModelDelta {
                text: "first projection-secret delta".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(first_delta.len(), 2);
        assert_eq!(first_delta[0].0, "message_start");
        assert!(!first_delta[1].1.to_string().contains("projection-secret"));

        let next_delta = event_notifications(
            &AgentEvent::ModelDelta {
                text: "next delta".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(next_delta.len(), 1);

        let tool = event_notifications(
            &AgentEvent::ToolRequested {
                call_id: "tool-1".into(),
                name: "read_file".into(),
                arguments: json!({"path":"ignored"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(tool.len(), 2);
        assert_eq!(tool[0].1["status"], "requested");
        assert_eq!(tool[1].1["status"], "running");

        let completed = event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "tool-1".into(),
                name: "read_file".into(),
                result: json!({"content":"bounded"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(completed[0].1["status"], "completed");
        let failed = event_notifications(
            &AgentEvent::ToolObservation {
                call_id: "tool-2".into(),
                name: "read_file".into(),
                result: json!({"error":"denied"}),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(failed[0].1["status"], "failed");

        let input = event_notifications(
            &AgentEvent::InteractionRequired {
                request: rivo_session::InteractionRequest {
                    request_id: "question-1".into(),
                    title: "question".into(),
                    body: "projection-secret choose one".into(),
                    options: vec![rivo_session::InteractionOption {
                        id: "yes".into(),
                        label: "Yes".into(),
                        description: "continue".into(),
                    }],
                    allow_freeform: true,
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(input[0].0, "interaction_request");
        assert_eq!(input[1].1["status"], "waiting_interaction");
        assert!(session.interactions.contains_key("question-1"));
        assert!(!input[0].1.to_string().contains("projection-secret"));

        let failure = event_notifications(
            &AgentEvent::Failure {
                message: "write failed".into(),
                details: rivo_session::FailureDetails {
                    stage: rivo_session::FailureStage::Persistence,
                    code: "storage_write".into(),
                    retryable: true,
                    recovery: "retry".into(),
                    cause: "fixture".into(),
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(failure[0].1["code"], "storage_unavailable");

        let provider_failure = event_notifications(
            &AgentEvent::Failure {
                message: "ignored message".into(),
                details: rivo_session::FailureDetails {
                    stage: rivo_session::FailureStage::Stream,
                    code: "provider_stream_failed".into(),
                    retryable: true,
                    recovery: "retry later".into(),
                    cause: "Responses stream error: Our servers are currently overloaded. Please try again later. (Server, HTTP 503, provider code server_is_overloaded) projection-secret".into(),
                },
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(provider_failure[0].1["code"], "provider_failed");
        assert_eq!(provider_failure[0].1["retryable"], true);
        assert_eq!(provider_failure[0].1["message"], "Provider request failed");
        assert_eq!(provider_failure[0].1["response"]["status"], 503);
        assert_eq!(
            provider_failure[0].1["response"]["provider_code"],
            "server_is_overloaded"
        );
        assert!(provider_failure[0].1["response"]["body"]
            .as_str()
            .unwrap()
            .contains("Our servers are currently overloaded. Please try again later."));
        assert!(!provider_failure[0].1["response"]["body"]
            .as_str()
            .unwrap()
            .contains("HTTP 503"));
        assert!(!provider_failure[0]
            .1
            .to_string()
            .contains("projection-secret"));

        let final_events = event_notifications(
            &AgentEvent::FinalAnswer {
                text: "done".into(),
                stop_reason: rivo_llm::StopReason::Final,
                usage: rivo_llm::Usage::default(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(final_events[0].0, "message_end");
        assert_eq!(final_events[1].1["outcome"], "completed");

        let stopped = event_notifications(
            &AgentEvent::Stopped {
                reason: "用户取消".into(),
            },
            &mut session,
            "turn-1",
        );
        assert_eq!(stopped[0].1["outcome"], "interrupted");
    }

    #[tokio::test]
    async fn idempotency_conflict_and_eviction_preserve_only_the_latest_requests() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let first = cached_or(
            "session/start",
            "session-1",
            "same-key",
            json!({"prompt":"first"}),
            &state,
            || async { Ok(json!({"accepted":true})) },
        )
        .await
        .unwrap();
        assert_eq!(first["accepted"], true);
        let conflict = cached_or(
            "session/start",
            "session-1",
            "same-key",
            json!({"prompt":"changed"}),
            &state,
            || async { Ok(json!({"must_not_run":true})) },
        )
        .await
        .unwrap_err();
        assert_eq!(conflict.code, "invalid_request");

        for index in 0..=MAX_CACHE_ENTRIES {
            cached_or(
                "turn/start",
                "session-1",
                &format!("key-{index}"),
                json!({"index":index}),
                &state,
                || async { Ok(json!({"index":index})) },
            )
            .await
            .unwrap();
        }
        let locked = state.lock().unwrap();
        assert_eq!(locked.idempotency.len(), MAX_CACHE_ENTRIES);
        assert!(!locked
            .idempotency
            .contains_key("session-1\u{1f}turn/start\u{1f}key-0"));
        assert!(locked.idempotency.contains_key(&format!(
            "session-1\u{1f}turn/start\u{1f}key-{MAX_CACHE_ENTRIES}"
        )));
    }

    #[test]
    fn response_language_is_canonicalized_and_rejects_invalid_values() {
        let options = RuntimeSettings {
            response_language: Some("ZH-cn".into()),
            ..RuntimeSettings::default()
        };
        assert_eq!(turn_options(&options).unwrap().locale, "zh-CN");
        let invalid = RuntimeSettings {
            response_language: Some("not_a_locale".into()),
            ..RuntimeSettings::default()
        };
        assert_eq!(
            turn_options(&invalid).unwrap_err().code,
            "unsupported_setting"
        );
    }
}

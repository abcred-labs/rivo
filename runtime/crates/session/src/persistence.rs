use super::storage_lock;
use futures::StreamExt;
use std::sync::{Arc, Mutex};

pub(super) async fn blocking_storage<T>(
    storage: Option<Arc<Mutex<rivo_storage::Store>>>,
    operation: impl FnOnce(&rivo_storage::Store) -> Result<T, rivo_storage::StorageError>
        + Send
        + 'static,
) -> Result<Option<T>, super::SessionError>
where
    T: Send + 'static,
{
    let Some(store) = storage else {
        return Ok(None);
    };
    let result = tokio::task::spawn_blocking(move || {
        let store = storage_lock(&store)?;
        operation(&store)
    })
    .await
    .map_err(|error| {
        super::SessionError::Storage(rivo_storage::StorageError::Unavailable(format!(
            "历史存储 worker 已中断: {error}"
        )))
    })?;
    result.map(Some).map_err(super::SessionError::Storage)
}

pub(super) async fn append_message(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    role: &str,
    body: &str,
    api_keys: &[String],
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Ok(());
    };
    let safe_body =
        rivo_telemetry::redact_api_keys(&serde_json::Value::String(body.to_owned()), api_keys)
            .as_str()
            .unwrap_or(body)
            .to_owned();
    let session_id = session_id.to_owned();
    let turn_id = turn_id.to_owned();
    let role = role.to_owned();
    tokio::task::spawn_blocking(move || {
        let store = storage_lock(&store).map_err(|error| error.to_string())?;
        let sequence = store
            .next_message_sequence(&session_id)
            .map_err(|error| error.to_string())?;
        store
            .append_message(&rivo_storage::StoredMessage {
                session_id,
                turn_id,
                sequence,
                role,
                body: safe_body,
            })
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn recall_memories(
    store: &Arc<Mutex<rivo_storage::Store>>,
    workspace: &str,
    mode: &str,
) -> Result<Vec<rivo_storage::Memory>, String> {
    let store = store.clone();
    let workspace = workspace.to_owned();
    let mode = mode.to_owned();
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .recall(&workspace, &mode, 5)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn next_event_sequence(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
) -> Result<u64, String> {
    let Some(store) = storage.clone() else {
        return Ok(0);
    };
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .next_event_sequence(&session_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn last_event_sequence(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
) -> Result<u64, String> {
    let Some(store) = storage.clone() else {
        return Ok(0);
    };
    let session_id = session_id.to_owned();
    let turn_id = turn_id.to_owned();
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .last_event_sequence(&session_id, &turn_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn save_session_permission_grant(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    grant: rivo_storage::SessionPermissionGrant,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Err("当前会话未配置历史存储".into());
    };
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .save_session_permission_grant(&grant)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn save_model_selection(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    route: Option<rivo_storage::StoredModelRoute>,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Ok(());
    };
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .set_model_route(&session_id, route.as_ref())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn load_a2a_peers(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    peer_ids: Vec<String>,
) -> Result<Vec<rivo_storage::StoredA2aPeer>, super::SessionError> {
    let store = storage.clone().ok_or_else(|| {
        super::SessionError::InvalidConfig("A2A peer 选择需要本机持久化 trust store".into())
    })?;
    tokio::task::spawn_blocking(move || {
        let store = storage_lock(&store)?;
        peer_ids
            .into_iter()
            .map(|peer_id| {
                store
                    .a2a_peer(&rivo_a2a::A2aPeerId(peer_id.clone()))?
                    .ok_or_else(|| {
                        super::SessionError::InvalidConfig(format!(
                            "A2A peer 未配对或不存在: {peer_id}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, super::SessionError>>()
    })
    .await
    .map_err(|error| {
        super::SessionError::Storage(rivo_storage::StorageError::Unavailable(format!(
            "历史存储 worker 已中断: {error}"
        )))
    })?
}

pub(super) async fn save_memory_candidate(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    candidate: rivo_storage::MemoryCandidate,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Err("当前会话未配置历史存储".into());
    };
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .save_memory_candidate(&candidate)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn confirm_memory(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    candidate_id: String,
    body_sha256: String,
) -> Result<rivo_storage::Memory, String> {
    let Some(store) = storage.clone() else {
        return Err("当前会话未配置历史存储".into());
    };
    tokio::task::spawn_blocking(move || {
        let store = storage_lock(&store).map_err(|error| error.to_string())?;
        let candidate = store
            .get_memory_candidate(&candidate_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "记忆候选不存在或已过期".to_owned())?;
        store
            .confirm_memory(&candidate, &body_sha256)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn reject_memory(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    candidate_id: String,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Err("当前会话未配置历史存储".into());
    };
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .reject_memory(&candidate_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn delete_memory(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    memory_id: String,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Err("当前会话未配置历史存储".into());
    };
    tokio::task::spawn_blocking(move || {
        storage_lock(&store)
            .map_err(|error| error.to_string())?
            .delete_memory(&memory_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn persist_agent_task_event(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    event: &super::AgentEvent,
    api_keys: &[String],
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Ok(());
    };
    let session_id = session_id.to_owned();
    let turn_id = turn_id.to_owned();
    let event = event.clone();
    let api_keys = api_keys.to_vec();
    tokio::task::spawn_blocking(move || match event {
        super::AgentEvent::SubagentQueued {
            agent_id,
            task_id,
            label,
            ..
        } => {
            let safe_label =
                rivo_telemetry::redact_api_keys(&serde_json::Value::String(label), &api_keys)
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
            storage_lock(&store)
                .map_err(|error| error.to_string())?
                .save_agent_task(&rivo_storage::StoredAgentTask {
                    task_id: task_id.0,
                    agent_id: agent_id.0,
                    session_id,
                    turn_id,
                    label: safe_label,
                    state: "queued".into(),
                    summary: None,
                    usage_json: None,
                    terminal_reason: None,
                })
                .map_err(|error| error.to_string())
        }
        super::AgentEvent::SubagentStarted { task_id, .. } => storage_lock(&store)
            .map_err(|error| error.to_string())?
            .update_agent_task(&task_id.0, "running", None)
            .map_err(|error| error.to_string()),
        super::AgentEvent::SubagentCompleted { result } => {
            let safe_summary = rivo_telemetry::redact_api_keys(
                &serde_json::Value::String(result.summary.clone()),
                &api_keys,
            )
            .as_str()
            .unwrap_or(&result.summary)
            .to_owned();
            let usage_json =
                serde_json::to_string(&result.usage).map_err(|error| error.to_string())?;
            let state = format!("{:?}", result.state).to_lowercase();
            storage_lock(&store)
                .map_err(|error| error.to_string())?
                .finish_agent_task(
                    &result.task_id.0,
                    &state,
                    &safe_summary,
                    &usage_json,
                    &state,
                )
                .map_err(|error| error.to_string())
        }
        _ => Ok(()),
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn active_agent_tasks(
    storage: &Arc<Mutex<rivo_storage::Store>>,
    session_id: &str,
    turn_id: &str,
) -> Result<Vec<rivo_storage::StoredAgentTask>, String> {
    let store = storage.clone();
    let session_id = session_id.to_owned();
    let turn_id = turn_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let tasks = storage_lock(&store)
            .map_err(|error| error.to_string())?
            .agent_tasks(&session_id)
            .map_err(|error| error.to_string())?;
        Ok(tasks
            .into_iter()
            .filter(|task| {
                task.turn_id == turn_id && matches!(task.state.as_str(), "queued" | "running")
            })
            .collect())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn handle_management_intent(
    session: Arc<super::Session>,
    intent: super::Intent,
) -> Result<super::AgentEvent, String> {
    tokio::task::spawn_blocking(move || {
        let mut events = session
            .legacy_handle(intent)
            .map_err(|error| error.to_string())?;
        futures::executor::block_on(events.next())
            .ok_or_else(|| "管理 Intent 未产生事件".to_owned())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

pub(super) async fn persist_standalone_event(
    storage: &Option<Arc<Mutex<rivo_storage::Store>>>,
    session_id: &str,
    turn_id: &str,
    event: &super::AgentEvent,
) -> Result<(), String> {
    let Some(store) = storage.clone() else {
        return Ok(());
    };
    let value = serde_json::to_value(event).map_err(|error| error.to_string())?;
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("event")
        .to_owned();
    let body = value.to_string();
    let session_id = session_id.to_owned();
    let turn_id = turn_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let store = storage_lock(&store).map_err(|error| error.to_string())?;
        let sequence = store
            .next_event_sequence(&session_id)
            .map_err(|error| error.to_string())?;
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id,
                turn_id,
                sequence,
                kind,
                body,
            })
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("历史存储 worker 已中断: {error}"))?
}

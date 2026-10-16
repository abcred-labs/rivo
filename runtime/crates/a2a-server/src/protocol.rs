//! A2A JSON-RPC and task-wire projection helpers.
//!
//! These functions are deliberately transport-neutral: the HTTPS listener,
//! mTLS authentication and persistent task state remain in their dedicated
//! owners. Keeping the projection here makes the public wire semantics
//! independently reviewable without changing the crate facade.

use crate::ServerError;
use rivo_a2a::{A2aTask, A2aTaskId, A2aTaskState, JsonRpcError, JsonRpcResponse};
use serde_json::{json, Value};

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(super) fn rfc3339(unix: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(i64::try_from(unix).unwrap_or(i64::MAX))
        .ok()
        .and_then(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

pub(super) fn extract_send_message(request: &Value) -> Result<(String, String), ServerError> {
    let message = request
        .get("message")
        .and_then(Value::as_object)
        .ok_or(ServerError::InvalidRequest)?;
    let skill = message
        .get("metadata")
        .or_else(|| request.get("metadata"))
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("io.rivo.a2a.v1"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("skill_id"))
        .and_then(Value::as_str)
        .ok_or(ServerError::InvalidRequest)?;
    let parts = message
        .get("parts")
        .and_then(Value::as_array)
        .filter(|parts| parts.len() == 1)
        .ok_or(ServerError::InvalidRequest)?;
    let content = parts[0]
        .get("text")
        .and_then(Value::as_str)
        .ok_or(ServerError::InvalidRequest)?;
    if content.trim().is_empty() {
        return Err(ServerError::InvalidRequest);
    }
    Ok((skill.into(), content.into()))
}

pub(super) fn task_id_from(params: &Value) -> Result<A2aTaskId, ServerError> {
    params
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(|id| A2aTaskId(id.into()))
        .ok_or(ServerError::InvalidRequest)
}

pub(super) fn allowed_transition(from: A2aTaskState, to: A2aTaskState) -> bool {
    matches!(
        (from, to),
        (
            A2aTaskState::Submitted,
            A2aTaskState::Working | A2aTaskState::Failed
        ) | (
            A2aTaskState::Working,
            A2aTaskState::InputRequired
                | A2aTaskState::Completed
                | A2aTaskState::Failed
                | A2aTaskState::Canceled
                | A2aTaskState::Rejected
        ) | (
            A2aTaskState::InputRequired,
            A2aTaskState::Working
                | A2aTaskState::Rejected
                | A2aTaskState::Canceled
                | A2aTaskState::Failed
        )
    )
}

pub(super) fn rpc_result(id: Value, value: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(value),
        error: None,
    }
}

pub(super) fn rpc_error(id: Value, error: ServerError) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code: -32602,
            message: error.to_string(),
            data: None,
        }),
    }
}

pub(super) fn task_wire(task: &A2aTask) -> Value {
    json!({"id": task.id.0, "status": {"state": format!("{:?}", task.state).to_ascii_uppercase(), "message": task.message}, "artifacts": task.artifacts, "metadata": {"io.rivo.a2a.v1": {"revision": task.revision}}})
}

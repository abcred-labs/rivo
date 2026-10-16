//! Inbound A2A task orchestration after HTTP authentication succeeds.

use rivo_a2a::{A2aPeerId, JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;

use crate::protocol::{rpc_error, rpc_result, task_id_from, task_wire};
use crate::{A2aHttpsListener, ServerError};

impl A2aHttpsListener {
    pub(crate) async fn handle_inbound_message(
        &self,
        peer: &A2aPeerId,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let response_id = Value::String(request.id);
        let message_id = request
            .params
            .get("message")
            .and_then(Value::as_object)
            .and_then(|value| value.get("messageId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let reservation =
            self.server
                .lock()
                .await
                .reserve_inbound(peer, &message_id, request.params);
        let task = match reservation {
            Err(error) => return rpc_error(response_id, error),
            Ok(reservation) => {
                if reservation.newly_created {
                    // Persist before starting local work so a restart cannot create a duplicate session.
                    if self.persist_task(peer, &reservation.task.id).await.is_err() {
                        return rpc_error(response_id, ServerError::InvalidRequest);
                    }
                    let start_result = match &self.session_driver {
                        Some(driver) => {
                            driver
                                .start(reservation.start.clone(), self.session_projector())
                                .await
                        }
                        // A listener without a local executor returns a stable failure, never SUBMITTED.
                        None => Err(ServerError::InvalidRequest),
                    };
                    if start_result.is_err() {
                        let _ = self
                            .server
                            .lock()
                            .await
                            .fail_unstarted(peer, &reservation.task.id);
                        let _ = self.persist_task(peer, &reservation.task.id).await;
                    }
                }
                match self
                    .server
                    .lock()
                    .await
                    .get_task(peer, &reservation.task.id)
                {
                    Ok(task) => task,
                    Err(error) => return rpc_error(response_id, error),
                }
            }
        };
        if self.persist_task(peer, &task.id).await.is_err() {
            return rpc_error(response_id, ServerError::InvalidRequest);
        }
        rpc_result(response_id, task_wire(&task))
    }

    pub(crate) async fn handle_inbound_cancel(
        &self,
        peer: &A2aPeerId,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let response_id = Value::String(request.id);
        let task_id = match task_id_from(&request.params) {
            Ok(task_id) => task_id,
            Err(error) => return rpc_error(response_id, error),
        };
        let current = match self.server.lock().await.get_task(peer, &task_id) {
            Ok(task) => task,
            Err(error) => return rpc_error(response_id, error),
        };
        if !current.state.terminal() {
            let local_session = match self.server.lock().await.local_session_ref(peer, &task_id) {
                Ok(value) => value,
                Err(error) => return rpc_error(response_id, error),
            };
            let Some(driver) = &self.session_driver else {
                return rpc_error(response_id, ServerError::InvalidRequest);
            };
            // Never report cancellation while local work may still be running.
            if let Err(error) = driver.cancel(&local_session).await {
                return rpc_error(response_id, error);
            }
        }
        let task = match self.server.lock().await.cancel(peer, &task_id) {
            Ok(task) => task,
            Err(error) => return rpc_error(response_id, error),
        };
        if self.persist_task(peer, &task.id).await.is_err() {
            return rpc_error(response_id, ServerError::InvalidRequest);
        }
        rpc_result(response_id, task_wire(&task))
    }
}

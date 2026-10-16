//! Authenticated A2A task JSON-RPC and SSE route.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rivo_a2a::psk::PskEnvelope;
use rivo_a2a::{parse_unique_json, A2aTask, JsonRpcRequest};

use crate::http_response::{bounded_body, error_response, json_response, psk_json_response};
use crate::{A2aHttpsListener, ServerError};

impl A2aHttpsListener {
    pub(crate) async fn handle_task_route(
        &self,
        request: Request<Incoming>,
        certificate_fingerprint: Option<&str>,
    ) -> Response<Full<Bytes>> {
        let psk_auth = match self.authenticate_psk_headers(&request).await {
            Ok(key) => key,
            Err(error) => return error_response(error),
        };
        let psk_key = psk_auth.as_ref().map(|value| &value.request_key);
        let wants_sse = request
            .headers()
            .get("accept")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.contains("text/event-stream")
                    || value.contains("application/vnd.rivo.a2a.psk+sse")
            });
        let psk_request_id = request
            .headers()
            .get("x-rivo-psk-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match bounded_body(request).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let body = if let Some(key) = &psk_key {
            let envelope: PskEnvelope = match serde_json::from_str(&body) {
                Ok(value) => value,
                Err(_) => return error_response(ServerError::PskInvalid),
            };
            match envelope.decode(key) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(value) => value,
                    Err(_) => return error_response(ServerError::PskInvalid),
                },
                Err(_) => return error_response(ServerError::PskInvalid),
            }
        } else {
            body
        };
        let request = match parse_unique_json(&body)
            .ok()
            .and_then(|value| serde_json::from_value::<JsonRpcRequest>(value).ok())
        {
            Some(request) if request.jsonrpc == "2.0" => request,
            _ => return error_response(ServerError::InvalidRequest),
        };
        if psk_key.is_some() && psk_request_id.as_deref() != Some(request.id.as_str()) {
            return error_response(ServerError::PskInvalid);
        }
        let peer = match self
            .authenticated_peer(certificate_fingerprint, psk_key.is_some())
            .await
        {
            Ok(peer) => peer,
            Err(error) => return error_response(error),
        };
        if let Some(response) = self
            .handle_resubscribe(
                &peer,
                &request,
                wants_sse,
                psk_key,
                psk_request_id.as_deref(),
                psk_auth.as_ref(),
            )
            .await
        {
            return response;
        }
        let response = match request.method.as_str() {
            "message/send" | "message/stream" => self.handle_inbound_message(&peer, request).await,
            "tasks/cancel" => self.handle_inbound_cancel(&peer, request).await,
            _ => self.server.lock().await.handle_json_rpc(&peer, request),
        };
        if let Some(task) = response
            .result
            .as_ref()
            .and_then(|value| A2aTask::from_wire(value.clone()).ok())
        {
            let _ = self.persist_task(&peer, &task.id).await;
        }
        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        if let Some(key) = &psk_key {
            psk_json_response(
                StatusCode::OK,
                body,
                key,
                psk_request_id.as_deref().unwrap_or_default(),
                psk_auth
                    .as_ref()
                    .and_then(|value| value.bootstrap_response.as_ref()),
            )
        } else {
            json_response(StatusCode::OK, body)
        }
    }
}

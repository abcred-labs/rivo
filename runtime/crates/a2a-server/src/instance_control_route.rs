//! Owner-controlled instance-control HTTP route.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rivo_a2a::psk::PskEnvelope;
use rivo_a2a::{parse_unique_json, JsonRpcRequest, JsonRpcResponse};
use rivo_instance_control::{validate_lan_control, LanControlEnvelope};
use serde_json::Value;

use crate::http_response::{bounded_body, error_response, json_response, psk_json_response};
use crate::protocol::{rpc_error, unix_now};
use crate::{A2aHttpsListener, ServerError};

impl A2aHttpsListener {
    pub(crate) async fn handle_instance_control(
        &self,
        request: Request<Incoming>,
        certificate_fingerprint: Option<&str>,
    ) -> Response<Full<Bytes>> {
        let psk_auth = match self.authenticate_psk_headers(&request).await {
            Ok(key) => key,
            Err(error) => return error_response(error),
        };
        let psk_key = psk_auth.as_ref().map(|value| &value.request_key);
        let psk_request_id = request
            .headers()
            .get("x-rivo-psk-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match bounded_body(request).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let body = if let Some(key) = psk_key {
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
        let peer = match self
            .authenticated_peer(certificate_fingerprint, psk_key.is_some())
            .await
        {
            Ok(peer) => peer,
            Err(error) => return error_response(error),
        };
        let profile = self.server.lock().await.profile.clone();
        if profile.capability_profile != "instance_control" || profile.skill_id != "process-control"
        {
            return error_response(ServerError::SkillNotAllowed);
        }
        let request = match parse_unique_json(&body)
            .ok()
            .and_then(|value| serde_json::from_value::<JsonRpcRequest>(value).ok())
        {
            Some(request) if request.jsonrpc == "2.0" && request.method == "instance/control" => {
                request
            }
            _ => return error_response(ServerError::InvalidRequest),
        };
        if psk_key.is_some() && psk_request_id.as_deref() != Some(request.id.as_str()) {
            return error_response(ServerError::PskInvalid);
        }
        let envelope: LanControlEnvelope = match serde_json::from_value(request.params) {
            Ok(envelope) => envelope,
            Err(_) => return error_response(ServerError::InvalidRequest),
        };
        if envelope.peer_id != peer.0
            || validate_lan_control(&envelope, (unix_now() as u128) * 1000).is_err()
        {
            return error_response(ServerError::InvalidRequest);
        }
        let now_ms = (unix_now() as u128) * 1000;
        {
            let mut nonces = self.instance_control_nonces.lock().await;
            nonces.retain(|_, expires_at| *expires_at >= now_ms);
            let nonce_key = format!("{}\u{1f}{}", peer.0, envelope.nonce);
            if nonces.contains_key(&nonce_key) {
                return error_response(ServerError::InvalidRequest);
            }
            nonces.insert(nonce_key, envelope.expires_at_unix_ms);
        }
        let Some(handler) = &self.instance_control_handler else {
            return error_response(ServerError::InvalidRequest);
        };
        let response = match handler.handle(&peer, envelope).await {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Value::String(request.id),
                result: Some(result),
                error: None,
            },
            Err(error) => rpc_error(Value::String(request.id), error),
        };
        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        if let Some(key) = psk_key {
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

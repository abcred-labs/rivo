//! Snapshot-first task resubscription projection.

use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;
use rivo_a2a::psk::PskSecret;
use rivo_a2a::{A2aPeerId, JsonRpcRequest};

use crate::http_response::{error_response, psk_sse_response, sse_response};
use crate::protocol::task_id_from;
use crate::{A2aHttpsListener, PskRequestAuthentication};

impl A2aHttpsListener {
    pub(crate) async fn handle_resubscribe(
        &self,
        peer: &A2aPeerId,
        request: &JsonRpcRequest,
        wants_sse: bool,
        psk_key: Option<&PskSecret>,
        psk_request_id: Option<&str>,
        psk_auth: Option<&PskRequestAuthentication>,
    ) -> Option<Response<Full<Bytes>>> {
        if request.method != "tasks/resubscribe" || !wants_sse {
            return None;
        }
        // The wire response begins with the full task snapshot, not JSON-RPC.
        let task_id = match task_id_from(&request.params) {
            Ok(task_id) => task_id,
            Err(error) => return Some(error_response(error)),
        };
        let snapshot = match self.server.lock().await.sse_snapshot(peer, &task_id) {
            Ok(snapshot) => snapshot,
            Err(error) => return Some(error_response(error)),
        };
        Some(if let Some(key) = psk_key {
            psk_sse_response(
                snapshot,
                key,
                psk_request_id.unwrap_or_default(),
                psk_auth.and_then(|value| value.bootstrap_response.as_ref()),
            )
        } else {
            sse_response(snapshot)
        })
    }
}

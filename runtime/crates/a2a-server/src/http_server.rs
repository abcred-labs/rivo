//! TCP/TLS listener lifecycle for the A2A HTTP server.
//!
//! Route dispatch, authentication, and response formatting intentionally remain
//! separate so accepting a connection cannot alter protocol behavior.

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rivo_a2a::{parse_unique_json, A2aRequestId, LAN_PAIRING_VERSION};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::net::TcpListener;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;

use crate::http_response::{bounded_body, error_response, json_response};
use crate::protocol::{rfc3339, unix_now};
use crate::{
    is_allowed_bind_address, local_test_mode_enabled, A2aHttpsListener, PairingState, ServerError,
};

impl A2aHttpsListener {
    /// 永久运行到外部 task 取消。调用方必须仅在 `rivo a2a serve <profile>` 的
    /// 本机交互路径调用；此函数不会为公网或 wildcard 地址选择回退绑定。
    pub async fn serve(self: Arc<Self>) -> Result<(), ServerError> {
        let bind = self.server.lock().await.profile.bind;
        if !is_allowed_bind_address(bind.ip(), local_test_mode_enabled()) {
            return Err(ServerError::BindRejected);
        }
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|_| ServerError::BindRejected)?;
        self.serve_on_listener(listener).await
    }

    async fn serve_on_listener(self: Arc<Self>, listener: TcpListener) -> Result<(), ServerError> {
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|_| ServerError::BindRejected)?;
            let listener = self.clone();
            tokio::spawn(async move {
                let Ok(tls) = listener.acceptor.accept(stream).await else {
                    return;
                };
                let fingerprint = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certificates| certificates.first())
                    .map(|certificate| hex::encode(Sha256::digest(certificate.as_ref())));
                let service = hyper::service::service_fn(move |request| {
                    let listener = listener.clone();
                    let fingerprint = fingerprint.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            listener.handle_http(request, fingerprint).await,
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // E2E fixture entry; production must keep `serve` private-LAN-only.
    pub(crate) async fn serve_test_listener(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> Result<(), ServerError> {
        self.serve_on_listener(listener).await
    }

    pub(crate) async fn handle_pairing_claim(
        &self,
        request: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        let request_id = request
            .uri()
            .path()
            .strip_prefix("/rivo-a2a-lan-v1/pairings/")
            .and_then(|tail| tail.strip_suffix("/claim"))
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .map(|id| A2aRequestId(id.into()));
        let Some(request_id) = request_id else {
            return error_response(ServerError::InvalidPairing);
        };
        let body = match bounded_body(request).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ClaimWire {
            version: String,
            request_id: String,
            claim_token: String,
            claim_proof: String,
        }
        let wire = parse_unique_json(&body)
            .ok()
            .and_then(|value| serde_json::from_value::<ClaimWire>(value).ok());
        let Some(wire) = wire.filter(|wire| {
            wire.version == LAN_PAIRING_VERSION
                && wire.request_id == request_id.0
                && !wire.claim_token.contains('=')
        }) else {
            return error_response(ServerError::InvalidPairing);
        };
        let token = match base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(wire.claim_token.as_bytes())
            .ok()
            .and_then(|token| <[u8; 32]>::try_from(token).ok())
        {
            Some(token) => token,
            None => return error_response(ServerError::InvalidPairing),
        };
        // `pairings approve|reject` 是另一个 owner-only CLI process，因此批准结果
        // 只以脱敏 state 落 SQLite；领取前把它同步到运行中 registry。没有持久化
        // state 的 pairing 不得因为控制面故障而被默认批准。
        if let Some(state) = &self.state {
            let persisted = match state.lock().await.pairing_state(&request_id, unix_now()) {
                Ok(value) => value,
                Err(error) => return error_response(error),
            };
            let mut pairings = self.pairings.lock().await;
            let sync = match persisted {
                PairingState::Approved => pairings.approve(&request_id, unix_now()),
                PairingState::Rejected => pairings.reject(&request_id, unix_now()),
                PairingState::Expired => Err(ServerError::PairingExpired),
                PairingState::Pending | PairingState::Claimed => {
                    Err(ServerError::PairingNotApproved)
                }
            };
            if let Err(error) = sync {
                return error_response(error);
            }
        }
        // `claim` 需要独占 registry；在成功后更新 SQLite 摘要前必须先释放该锁，
        // 否则同一 async task 会在再次 `lock` 时永久自锁，导致客户端领取超时。
        let claim_result = {
            let profile = self.server.lock().await.profile().clone();
            let mut pairings = self.pairings.lock().await;
            pairings.claim(
                &request_id,
                &token,
                &wire.claim_proof,
                self.issuer.as_ref(),
                &profile,
                unix_now(),
            )
        };
        match claim_result {
            Ok(claim) => {
                if let Some(state) = &self.state {
                    let mut pairings = self.pairings.lock().await;
                    if let Some(summary) = pairings
                        .list(unix_now())
                        .into_iter()
                        .find(|item| item.request_id == request_id)
                    {
                        if state.lock().await.save_pairing_summary(&summary).is_err() {
                            return error_response(ServerError::InvalidRequest);
                        }
                    }
                    let grant = pairings.grants.get(&claim.peer_id).cloned();
                    drop(pairings);
                    let Some(grant) = grant else {
                        return error_response(ServerError::InvalidRequest);
                    };
                    if state.lock().await.save_peer_grant(&grant).is_err() {
                        // The certificate may already have been constructed, but it must not
                        // become usable when its matching grant was not durably recorded.
                        self.pairings.lock().await.revoke(&claim.peer_id);
                        return error_response(ServerError::InvalidRequest);
                    }
                }
                json_response(
                    StatusCode::OK,
                    serde_json::to_string(&json!({
                        "version": LAN_PAIRING_VERSION, "request_id": request_id.0, "peer_id": claim.peer_id.0,
                        "certificate_chain_pem": claim.certificate_chain_pem, "expires_at": rfc3339(claim.expires_unix)
                    }))
                    .unwrap_or_else(|_| "{}".into()),
                )
            }
            Err(error) => error_response(error),
        }
    }

    pub(crate) async fn handle_pairing_begin(
        &self,
        request: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        let body = match bounded_body(request).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PairingWire {
            version: String,
            csr_pem: String,
        }
        let wire: PairingWire = match parse_unique_json(&body)
            .ok()
            .and_then(|value| serde_json::from_value::<PairingWire>(value).ok())
        {
            Some(wire) if wire.version == LAN_PAIRING_VERSION => wire,
            _ => return error_response(ServerError::InvalidPairing),
        };
        let now = unix_now();
        match self
            .pairings
            .lock()
            .await
            .begin(&wire.csr_pem, &self.server_spki_sha256, now)
        {
            Ok(challenge) => {
                if let Some(state) = &self.state {
                    let _ = state.lock().await.save_pairing_summary(&challenge.pairing);
                }
                json_response(
                    StatusCode::OK,
                    serde_json::to_string(&json!({
                        "version": LAN_PAIRING_VERSION, "request_id": challenge.pairing.request_id.0,
                        "server_spki_sha256": challenge.pairing.server_spki_sha256,
                        "client_public_key_sha256": challenge.pairing.client_public_key_sha256,
                        "expires_at": rfc3339(challenge.pairing.expires_unix),
                        "sas_words": challenge.pairing.sas_words,
                        "claim_token": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge.claim_token),
                    }))
                    .unwrap_or_else(|_| "{}".into()),
                )
            }
            Err(error) => error_response(error),
        }
    }
}

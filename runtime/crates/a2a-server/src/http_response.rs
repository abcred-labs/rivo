//! HTTP response projection shared by the A2A listener routes.
//!
//! This module owns only byte limits and wire response formatting. Route selection,
//! authentication, and protocol state remain with `A2aHttpsListener`.

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rivo_a2a::psk::{server_response_proof, PskEnvelope, PskSecret};
use rivo_a2a::MAX_MESSAGE_BYTES;

use crate::ServerError;

pub(crate) struct PskBootstrapResponse {
    pub(crate) server_nonce: [u8; 32],
    pub(crate) server_public_key: Vec<u8>,
}

pub(crate) async fn bounded_body(
    request: Request<Incoming>,
) -> Result<String, Response<Full<Bytes>>> {
    let bytes = request
        .into_body()
        .collect()
        .await
        .map_err(|_| empty_response(StatusCode::BAD_REQUEST))?
        .to_bytes();
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(empty_response(StatusCode::PAYLOAD_TOO_LARGE));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| empty_response(StatusCode::BAD_REQUEST))
}

pub(crate) fn json_response(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR))
}

pub(crate) fn sse_response(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR))
}

pub(crate) fn empty_response(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

pub(crate) fn error_response(error: ServerError) -> Response<Full<Bytes>> {
    json_response(
        StatusCode::UNAUTHORIZED,
        format!(r#"{{"error":"{}"}}"#, error),
    )
}

pub(crate) fn psk_json_response(
    status: StatusCode,
    body: String,
    key: &PskSecret,
    aad: &str,
    bootstrap: Option<&PskBootstrapResponse>,
) -> Response<Full<Bytes>> {
    psk_response(
        status,
        "application/vnd.rivo.a2a.psk+json",
        body,
        key,
        aad,
        bootstrap,
    )
}

pub(crate) fn psk_sse_response(
    body: String,
    key: &PskSecret,
    aad: &str,
    bootstrap: Option<&PskBootstrapResponse>,
) -> Response<Full<Bytes>> {
    psk_response(
        StatusCode::OK,
        "application/vnd.rivo.a2a.psk+sse",
        body,
        key,
        aad,
        bootstrap,
    )
}

fn psk_response(
    status: StatusCode,
    content_type: &str,
    body: String,
    key: &PskSecret,
    aad: &str,
    bootstrap: Option<&PskBootstrapResponse>,
) -> Response<Full<Bytes>> {
    let envelope = PskEnvelope::encode(key, body.as_bytes(), aad)
        .ok()
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or_else(|| "{}".into());
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header(
            "x-rivo-psk-server-proof",
            base64::engine::general_purpose::STANDARD.encode(server_response_proof(key, aad)),
        );
    if let Some(bootstrap) = bootstrap {
        builder = builder
            .header(
                "x-rivo-psk-server-nonce",
                base64::engine::general_purpose::STANDARD.encode(bootstrap.server_nonce),
            )
            .header(
                "x-rivo-psk-server-ephemeral",
                base64::engine::general_purpose::STANDARD.encode(&bootstrap.server_public_key),
            );
    }
    builder
        .body(Full::new(Bytes::from(envelope)))
        .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR))
}

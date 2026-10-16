//! Peer authentication and trust projection for A2A HTTP routes.
//!
//! TLS/PSK handshakes establish transport credentials elsewhere. This module maps
//! those credentials to an allowlisted, non-revoked peer and verifies durable grants.

use base64::Engine as _;
use hyper::body::Incoming;
use hyper::Request;
use p256::ecdh::EphemeralSecret;
use p256::{EncodedPoint, PublicKey};
use rand_core::OsRng;
use rivo_a2a::psk::{
    derive_ecdh_session_key, derive_request_key, derive_session_request_key, proof as psk_proof,
    transcript_nonce, PskEcdhTranscript, PskSecret,
};
use rivo_a2a::{canonical_json_sha256, parse_unique_json, psk_peer_id, A2aPeerId};
use std::time::{Duration, Instant};

use crate::http_response::PskBootstrapResponse;
use crate::protocol::unix_now;
use crate::{A2aHttpsListener, ServerError};

pub(crate) struct PskServerSession {
    session_key: PskSecret,
    expires: Instant,
}

pub(crate) struct PskRequestAuthentication {
    pub(crate) request_key: PskSecret,
    pub(crate) bootstrap_response: Option<PskBootstrapResponse>,
}

impl A2aHttpsListener {
    pub(crate) async fn authenticate_psk_headers(
        &self,
        request: &Request<Incoming>,
    ) -> Result<Option<PskRequestAuthentication>, ServerError> {
        let Some(secret) = &self.psk_secret else {
            return Ok(None);
        };
        let decode = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
        };
        let nonce = decode("x-rivo-psk-nonce").ok_or(ServerError::PskRequired)?;
        let client_public_key = decode("x-rivo-psk-ephemeral").ok_or(ServerError::PskInvalid)?;
        let received = decode("x-rivo-psk-proof").ok_or(ServerError::PskInvalid)?;
        let nonce: [u8; 32] = nonce.try_into().map_err(|_| ServerError::PskInvalid)?;
        let received: [u8; 32] = received.try_into().map_err(|_| ServerError::PskInvalid)?;
        if client_public_key.len() != 65 {
            return Err(ServerError::PskInvalid);
        }
        let client_public =
            PublicKey::from_sec1_bytes(&client_public_key).map_err(|_| ServerError::PskInvalid)?;
        let request_id = request
            .headers()
            .get("x-rivo-psk-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or(ServerError::PskInvalid)?;
        let (profile, card) = {
            let server = self.server.lock().await;
            (
                server.profile.clone(),
                server
                    .public_card_json()
                    .map_err(|_| ServerError::PskInvalid)?,
            )
        };
        let card_hash =
            canonical_json_sha256(&parse_unique_json(&card).map_err(|_| ServerError::PskInvalid)?)
                .map_err(|_| ServerError::PskInvalid)?;
        let endpoint = format!("https://{}/a2a", profile.bind);
        let expected = psk_proof(
            secret,
            "client",
            &endpoint,
            &card_hash,
            &nonce,
            &client_public_key,
        );
        if received != expected {
            return Err(ServerError::PskInvalid);
        }
        self.psk_replay
            .consume_request(nonce, request_id)
            .map_err(|_| ServerError::PskReplay)?;
        let bootstrap = request
            .headers()
            .get("x-rivo-psk-bootstrap")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "1");
        let provisional_key =
            derive_request_key(secret, &nonce, &client_public_key, &endpoint, &card_hash);
        let mut sessions = self.psk_sessions.lock().await;
        let now = Instant::now();
        sessions.retain(|_, session| session.expires > now);
        if bootstrap {
            if sessions.contains_key(&nonce) {
                return Err(ServerError::PskReplay);
            }
            let server_private_key = EphemeralSecret::random(&mut OsRng);
            let server_public_key = EncodedPoint::from(server_private_key.public_key())
                .to_bytes()
                .to_vec();
            let server_nonce = transcript_nonce();
            let shared = server_private_key.diffie_hellman(&client_public);
            let session_key = derive_ecdh_session_key(
                secret,
                shared.raw_secret_bytes().as_slice(),
                PskEcdhTranscript {
                    client_nonce: &nonce,
                    server_nonce: &server_nonce,
                    client_public_key: &client_public_key,
                    server_public_key: &server_public_key,
                    endpoint: &endpoint,
                    card_hash: &card_hash,
                },
            );
            sessions.insert(
                nonce,
                PskServerSession {
                    session_key,
                    expires: now + Duration::from_secs(30),
                },
            );
            return Ok(Some(PskRequestAuthentication {
                request_key: derive_session_request_key(&provisional_key, request_id),
                bootstrap_response: Some(PskBootstrapResponse {
                    server_nonce,
                    server_public_key,
                }),
            }));
        }
        let session = sessions
            .get(&nonce)
            .ok_or(ServerError::PskSessionRequired)?;
        Ok(Some(PskRequestAuthentication {
            request_key: derive_session_request_key(&session.session_key, request_id),
            bootstrap_response: None,
        }))
    }

    pub(crate) async fn authenticated_peer(
        &self,
        fingerprint: Option<&str>,
        psk_authenticated: bool,
    ) -> Result<A2aPeerId, ServerError> {
        if psk_authenticated {
            let server = self.server.lock().await;
            let card = server.public_card_json()?;
            let card_hash = canonical_json_sha256(&parse_unique_json(&card)?)?;
            let endpoint = format!("https://{}/a2a", server.profile.bind);
            let peer = psk_peer_id(&endpoint, &card_hash)?;
            if server.profile.allowed_peers.contains(&peer) {
                return Ok(peer);
            }
            eprintln!("A2A PSK peer not allowlisted: {}", peer.0);
            return Err(ServerError::PeerNotTrusted);
        }
        let fingerprint = fingerprint.ok_or(ServerError::PeerNotTrusted)?;
        let now = unix_now();
        let profile_name = self.server.lock().await.profile().name.clone();
        let pairings = self.pairings.lock().await;
        let peer = pairings
            .grants
            .iter()
            .find_map(|(peer, grant)| {
                (grant.certificate_fingerprint == fingerprint
                    && !grant.revoked
                    && grant.expires_unix > now)
                    .then(|| peer.clone())
            })
            .ok_or(ServerError::PeerNotTrusted)?;
        drop(pairings);
        if let Some(state) = &self.state {
            let grants = state.lock().await.load_peer_grants(&profile_name)?;
            if !grants.iter().any(|grant| {
                grant.peer_id == peer
                    && grant.certificate_fingerprint == fingerprint
                    && !grant.revoked
                    && grant.expires_unix > now
            }) {
                return Err(ServerError::PeerNotTrusted);
            }
        }
        Ok(peer)
    }
}

//! Cross-platform PSK bootstrap primitives.
//!
//! The PSK is never serialized to configuration or logs. These helpers only
//! cover transcript authentication and payload sealing; transport integration
//! must perform an ephemeral TLS handshake before exchanging proofs.

use base64::Engine as _;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::PublicKey;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use thiserror::Error;
use zeroize::Zeroizing;

pub const PSK_PROTOCOL_VERSION: &str = "rivo-a2a-psk-v1";
pub const PSK_MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
pub const PSK_HANDSHAKE_TTL: Duration = Duration::from_secs(30);
pub const PSK_MAX_ENVELOPE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PskEnvelope {
    pub version: String,
    pub aad: String,
    pub ciphertext_b64: String,
}

impl PskEnvelope {
    pub fn encode(key: &[u8; 32], plaintext: &[u8], aad: &str) -> Result<Self, PskError> {
        if plaintext.len() > PSK_MAX_ENVELOPE_BYTES || aad.len() > 512 {
            return Err(PskError::MessageTooLarge);
        }
        Ok(Self {
            version: PSK_PROTOCOL_VERSION.into(),
            aad: aad.into(),
            ciphertext_b64: base64::engine::general_purpose::STANDARD.encode(seal(
                key,
                plaintext,
                aad.as_bytes(),
            )?),
        })
    }

    pub fn decode(&self, key: &[u8; 32]) -> Result<Vec<u8>, PskError> {
        if self.version != PSK_PROTOCOL_VERSION
            || self.aad.len() > 512
            || self.ciphertext_b64.len() > PSK_MAX_ENVELOPE_BYTES * 2
        {
            return Err(PskError::InvalidEnvelope);
        }
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(&self.ciphertext_b64)
            .map_err(|_| PskError::InvalidEnvelope)?;
        open(key, &sealed, self.aad.as_bytes())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PskHandshake {
    pub version: String,
    pub role: String,
    pub endpoint: String,
    pub card_hash: String,
    pub nonce_b64: String,
    pub ephemeral_key_b64: String,
    pub proof_b64: String,
    pub issued_unix: u64,
}

impl PskHandshake {
    pub fn validate(&self, now_unix: u64) -> Result<(), PskError> {
        if self.version != PSK_PROTOCOL_VERSION
            || !matches!(self.role.as_str(), "client" | "server")
            || self.endpoint.len() > 512
            || self.card_hash.len() != 64
            || self.nonce_b64.len() > 128
            || self.ephemeral_key_b64.len() > 512
            || self.proof_b64.len() > 128
            || self.issued_unix > now_unix.saturating_add(PSK_HANDSHAKE_TTL.as_secs())
            || now_unix.saturating_sub(self.issued_unix) > PSK_HANDSHAKE_TTL.as_secs()
        {
            return Err(PskError::InvalidHandshake);
        }
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(&self.nonce_b64)
            .map_err(|_| PskError::InvalidHandshake)?;
        let proof = base64::engine::general_purpose::STANDARD
            .decode(&self.proof_b64)
            .map_err(|_| PskError::InvalidHandshake)?;
        let ephemeral_key = base64::engine::general_purpose::STANDARD
            .decode(&self.ephemeral_key_b64)
            .map_err(|_| PskError::InvalidHandshake)?;
        if nonce.len() != 32
            || proof.len() != 32
            || ephemeral_key.len() != 65
            || PublicKey::from_sec1_bytes(&ephemeral_key).is_err()
        {
            return Err(PskError::InvalidHandshake);
        }
        Ok(())
    }
}

pub fn verify_proof(expected: &[u8; 32], received: &[u8; 32]) -> Result<(), PskError> {
    if expected == received {
        Ok(())
    } else {
        Err(PskError::ProofMismatch)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PskError {
    #[error("a2a_psk_invalid_secret")]
    InvalidSecret,
    #[error("a2a_psk_invalid_handshake")]
    InvalidHandshake,
    #[error("a2a_psk_proof_mismatch")]
    ProofMismatch,
    #[error("a2a_psk_replay")]
    Replay,
    #[error("a2a_psk_seal_failed")]
    SealFailed,
    #[error("a2a_psk_open_failed")]
    OpenFailed,
    #[error("a2a_psk_invalid_envelope")]
    InvalidEnvelope,
    #[error("a2a_psk_message_too_large")]
    MessageTooLarge,
}

#[derive(Default)]
pub struct ReplayGuard(Mutex<HashMap<[u8; 32], Instant>>);

impl ReplayGuard {
    pub fn consume(&self, nonce: [u8; 32]) -> Result<(), PskError> {
        self.consume_key(nonce, 1024, Instant::now())
    }

    pub fn consume_request(&self, nonce: [u8; 32], request_id: &str) -> Result<(), PskError> {
        let mut material = Vec::with_capacity(32 + request_id.len());
        material.extend_from_slice(&nonce);
        material.extend_from_slice(request_id.as_bytes());
        let digest: [u8; 32] = Sha256::digest(material).into();
        self.consume_key(digest, 4096, Instant::now())
    }

    fn consume_key(&self, key: [u8; 32], capacity: usize, now: Instant) -> Result<(), PskError> {
        let mut seen = self.0.lock().map_err(|_| PskError::Replay)?;
        seen.retain(|_, issued| {
            now.checked_duration_since(*issued)
                .is_some_and(|elapsed| elapsed <= PSK_HANDSHAKE_TTL)
        });
        if seen.contains_key(&key) || seen.len() >= capacity {
            return Err(PskError::Replay);
        }
        seen.insert(key, now);
        Ok(())
    }
}

pub type PskSecret = Zeroizing<[u8; 32]>;

pub fn random_secret() -> PskSecret {
    let mut secret = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(secret.as_mut());
    secret
}

pub fn transcript_nonce() -> [u8; 32] {
    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

pub fn proof(
    secret: &[u8; 32],
    role: &str,
    endpoint: &str,
    card_hash: &str,
    nonce: &[u8; 32],
    ephemeral_key: &[u8],
) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("fixed key size");
    mac.update(PSK_PROTOCOL_VERSION.as_bytes());
    mac.update(&[0]);
    mac.update(role.as_bytes());
    mac.update(&[0]);
    mac.update(endpoint.as_bytes());
    mac.update(&[0]);
    mac.update(card_hash.as_bytes());
    mac.update(&[0]);
    mac.update(nonce);
    mac.update(ephemeral_key);
    mac.finalize().into_bytes().into()
}

pub fn derive_session_key(
    secret: &[u8; 32],
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
    transcript: &[u8],
) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(transcript), secret);
    let mut key = Zeroizing::new([0u8; 32]);
    let mut info = Vec::with_capacity(64 + transcript.len());
    info.extend_from_slice(b"rivo-a2a-psk-session-v1");
    info.extend_from_slice(client_nonce);
    info.extend_from_slice(server_nonce);
    info.extend_from_slice(transcript);
    hk.expand(&info, key.as_mut()).expect("fixed HKDF output");
    key
}

/// Public transcript fields bound into a P-256 ECDH application session.
pub struct PskEcdhTranscript<'a> {
    pub client_nonce: &'a [u8; 32],
    pub server_nonce: &'a [u8; 32],
    pub client_public_key: &'a [u8],
    pub server_public_key: &'a [u8],
    pub endpoint: &'a str,
    pub card_hash: &'a str,
}

/// Derives an application session key only after both endpoints have completed
/// a P-256 ECDH exchange. The PSK remains part of the extract input, so an
/// unauthenticated curve key cannot establish a usable session by itself.
pub fn derive_ecdh_session_key(
    secret: &[u8; 32],
    shared_secret: &[u8],
    context: PskEcdhTranscript<'_>,
) -> Zeroizing<[u8; 32]> {
    let mut extract_input = Sha256::new();
    extract_input.update(b"rivo-a2a-psk-ecdh-v1\0");
    extract_input.update(secret);
    extract_input.update(shared_secret);
    let extract_key: [u8; 32] = extract_input.finalize().into();
    let mut transcript = Vec::with_capacity(
        128 + context.client_public_key.len()
            + context.server_public_key.len()
            + context.endpoint.len()
            + context.card_hash.len(),
    );
    transcript.extend_from_slice(b"rivo-a2a-psk-ecdh-v1\0");
    transcript.extend_from_slice(context.client_nonce);
    transcript.extend_from_slice(context.server_nonce);
    transcript.extend_from_slice(context.client_public_key);
    transcript.extend_from_slice(context.server_public_key);
    transcript.extend_from_slice(context.endpoint.as_bytes());
    transcript.push(0);
    transcript.extend_from_slice(context.card_hash.as_bytes());
    derive_session_key(
        &extract_key,
        context.client_nonce,
        context.server_nonce,
        &transcript,
    )
}

pub fn derive_request_key(
    secret: &[u8; 32],
    nonce: &[u8; 32],
    ephemeral_key: &[u8],
    endpoint: &str,
    card_hash: &str,
) -> Zeroizing<[u8; 32]> {
    let mut transcript = Vec::with_capacity(64 + endpoint.len() + card_hash.len());
    transcript.extend_from_slice(nonce);
    transcript.extend_from_slice(ephemeral_key);
    transcript.extend_from_slice(endpoint.as_bytes());
    transcript.extend_from_slice(card_hash.as_bytes());
    derive_session_key(secret, nonce, &[0u8; 32], &transcript)
}

/// Derives a request key from a reusable bootstrap transcript plus a unique
/// request id. This permits a connection session to reuse its bootstrap
/// material while keeping every payload cryptographically distinct.
pub fn derive_session_request_key(session_key: &[u8; 32], request_id: &str) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(b"rivo-a2a-psk-request-v1"), session_key);
    let mut key = Zeroizing::new([0u8; 32]);
    hk.expand(request_id.as_bytes(), key.as_mut())
        .expect("fixed HKDF output");
    key
}

/// Proof emitted by the server after it has authenticated the client proof.
/// It is keyed by the request session key, so a response cannot be forged by
/// an intermediary even when TLS is terminated elsewhere.
pub fn server_response_proof(key: &[u8; 32], aad: &str) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("fixed key size");
    mac.update(b"rivo-a2a-psk-server-response-v1");
    mac.update(&[0]);
    mac.update(aad.as_bytes());
    mac.finalize().into_bytes().into()
}

pub fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, PskError> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| PskError::SealFailed)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| PskError::SealFailed)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn open(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, PskError> {
    if sealed.len() < 12 {
        return Err(PskError::OpenFailed);
    }
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| PskError::OpenFailed)?;
    cipher
        .decrypt(
            Nonce::from_slice(&sealed[..12]),
            chacha20poly1305::aead::Payload {
                msg: &sealed[12..],
                aad,
            },
        )
        .map_err(|_| PskError::OpenFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::{ecdh::EphemeralSecret, EncodedPoint};

    #[test]
    fn proof_and_session_key_are_direction_bound() {
        let secret = [7u8; 32];
        let nonce = [8u8; 32];
        let key = [9u8; 32];
        assert_ne!(
            proof(
                &secret,
                "client",
                "https://10.0.0.1:8443/a2a",
                "a".repeat(64).as_str(),
                &nonce,
                &key
            ),
            proof(
                &secret,
                "server",
                "https://10.0.0.1:8443/a2a",
                "a".repeat(64).as_str(),
                &nonce,
                &key
            )
        );
        assert_ne!(
            derive_session_key(&secret, &nonce, &[1u8; 32], b"t"),
            derive_session_key(&secret, &[1u8; 32], &nonce, b"t")
        );
    }

    #[test]
    fn ecdh_session_key_binds_both_ephemeral_public_keys() {
        let client_secret = EphemeralSecret::random(&mut OsRng);
        let client_public = EncodedPoint::from(client_secret.public_key()).to_bytes();
        let server_secret = EphemeralSecret::random(&mut OsRng);
        let server_public = EncodedPoint::from(server_secret.public_key()).to_bytes();
        let parsed_client = PublicKey::from_sec1_bytes(&client_public).unwrap();
        let parsed_server = PublicKey::from_sec1_bytes(&server_public).unwrap();
        let client_shared = client_secret.diffie_hellman(&parsed_server);
        let server_shared = server_secret.diffie_hellman(&parsed_client);
        let secret = [7u8; 32];
        let client_nonce = [1u8; 32];
        let server_nonce = [2u8; 32];
        let card_hash = "a".repeat(64);
        let client_key = derive_ecdh_session_key(
            &secret,
            client_shared.raw_secret_bytes().as_slice(),
            PskEcdhTranscript {
                client_nonce: &client_nonce,
                server_nonce: &server_nonce,
                client_public_key: &client_public,
                server_public_key: &server_public,
                endpoint: "https://10.0.0.1:8443/a2a",
                card_hash: &card_hash,
            },
        );
        let server_key = derive_ecdh_session_key(
            &secret,
            server_shared.raw_secret_bytes().as_slice(),
            PskEcdhTranscript {
                client_nonce: &client_nonce,
                server_nonce: &server_nonce,
                client_public_key: &client_public,
                server_public_key: &server_public,
                endpoint: "https://10.0.0.1:8443/a2a",
                card_hash: &card_hash,
            },
        );
        assert_eq!(client_key, server_key);
        assert_ne!(
            client_key,
            derive_ecdh_session_key(
                &secret,
                server_shared.raw_secret_bytes().as_slice(),
                PskEcdhTranscript {
                    client_nonce: &client_nonce,
                    server_nonce: &server_nonce,
                    client_public_key: &server_public,
                    server_public_key: &client_public,
                    endpoint: "https://10.0.0.1:8443/a2a",
                    card_hash: &card_hash,
                },
            )
        );
    }

    #[test]
    fn seal_round_trip_and_tamper_rejection() {
        let key = [3u8; 32];
        let sealed = seal(&key, b"payload", b"task-1").unwrap();
        assert_eq!(open(&key, &sealed, b"task-1").unwrap(), b"payload");
        assert_eq!(open(&key, &sealed, b"task-2"), Err(PskError::OpenFailed));
    }

    #[test]
    fn handshake_validation_and_replay_guard_are_strict() {
        let ephemeral_secret = EphemeralSecret::random(&mut OsRng);
        let ephemeral_key = EncodedPoint::from(ephemeral_secret.public_key()).to_bytes();
        let handshake = PskHandshake {
            version: PSK_PROTOCOL_VERSION.into(),
            role: "client".into(),
            endpoint: "https://10.0.0.1:8443/a2a".into(),
            card_hash: "a".repeat(64),
            nonce_b64: base64::engine::general_purpose::STANDARD.encode([1u8; 32]),
            ephemeral_key_b64: base64::engine::general_purpose::STANDARD.encode(ephemeral_key),
            proof_b64: base64::engine::general_purpose::STANDARD.encode([3u8; 32]),
            issued_unix: 100,
        };
        assert!(handshake.validate(100).is_ok());
        assert_eq!(handshake.validate(131), Err(PskError::InvalidHandshake));
        assert_eq!(handshake.validate(60), Err(PskError::InvalidHandshake));
        let guard = ReplayGuard::default();
        assert!(guard.consume([1u8; 32]).is_ok());
        assert_eq!(guard.consume([1u8; 32]), Err(PskError::Replay));
        let requests = ReplayGuard::default();
        assert!(requests.consume_request([2u8; 32], "r1").is_ok());
        assert!(requests.consume_request([2u8; 32], "r2").is_ok());
        assert_eq!(
            requests.consume_request([2u8; 32], "r1"),
            Err(PskError::Replay)
        );
    }

    #[test]
    fn replay_entries_expire_with_the_handshake_window() {
        let guard = ReplayGuard::default();
        let now = Instant::now();
        let nonce = [3u8; 32];
        assert!(guard.consume_key(nonce, 1, now).is_ok());
        assert_eq!(guard.consume_key(nonce, 1, now), Err(PskError::Replay));
        assert!(guard
            .consume_key(nonce, 1, now + PSK_HANDSHAKE_TTL + Duration::from_secs(1))
            .is_ok());
    }

    #[test]
    fn envelope_binds_aad_and_version() {
        let key = [4u8; 32];
        let envelope = PskEnvelope::encode(&key, b"json", "task-1").unwrap();
        assert_eq!(envelope.decode(&key).unwrap(), b"json");
        let mut bad = envelope.clone();
        bad.aad = "task-2".into();
        assert_eq!(bad.decode(&key), Err(PskError::OpenFailed));
        bad = envelope;
        bad.version = "rivo-a2a-lan-v1".into();
        assert_eq!(bad.decode(&key), Err(PskError::InvalidEnvelope));
    }

    #[test]
    fn server_response_proof_is_bound_to_aad() {
        let key = [5u8; 32];
        let first = server_response_proof(&key, "request-1");
        assert_eq!(first, server_response_proof(&key, "request-1"));
        assert_ne!(first, server_response_proof(&key, "request-2"));
        assert_ne!(first, server_response_proof(&[6u8; 32], "request-1"));
    }
}

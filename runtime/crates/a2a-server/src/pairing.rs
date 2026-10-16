//! Pairing and trust validation primitives shared by the registry and listener.

use crate::{ServerError, ServiceProfile, PAIRING_TTL};
use p256::ecdsa::VerifyingKey;
use p256::pkcs8::EncodePublicKey;
use rand_core::{OsRng, RngCore};
use rcgen::{CertificateSigningRequestParams, PublicKeyData};
use rivo_a2a::{derive_sas_words, verify_claim_proof, A2aPeerId, A2aRequestId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    Pending,
    Approved,
    Rejected,
    Claimed,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub request_id: A2aRequestId,
    pub client_public_key_sha256: String,
    pub server_spki_sha256: String,
    pub sas_words: [String; 6],
    pub expires_unix: u64,
    pub state: PairingState,
    pub claim_token_sha256: String,
    #[serde(skip)]
    pub(super) csr_pem: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingChallenge {
    pub pairing: PendingPairing,
    pub claim_token: [u8; 32],
}

pub trait ClientCertificateIssuer: Send + Sync {
    fn issue_client_certificate(
        &self,
        peer_id: &A2aPeerId,
        csr_pem: &str,
        valid_for: std::time::Duration,
    ) -> Result<String, ServerError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerGrant {
    pub profile_name: String,
    pub peer_id: A2aPeerId,
    pub issuer_ref: String,
    pub certificate_serial: String,
    pub certificate_fingerprint: String,
    pub expires_unix: u64,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimResult {
    pub peer_id: A2aPeerId,
    pub certificate_chain_pem: String,
    pub expires_unix: u64,
}

#[derive(Debug, Default)]
pub struct PairingRegistry {
    pub(crate) pending: BTreeMap<A2aRequestId, PendingPairing>,
    pub(crate) grants: BTreeMap<A2aPeerId, PeerGrant>,
    pub(crate) serial: u64,
}

impl PairingRegistry {
    pub fn from_peer_grants(grants: impl IntoIterator<Item = PeerGrant>) -> Self {
        let mut registry = Self::default();
        for grant in grants {
            registry.serial = registry.serial.max(serial_suffix(&grant.peer_id.0));
            registry.grants.insert(grant.peer_id.clone(), grant);
        }
        registry
    }

    pub fn begin(
        &mut self,
        csr_pem: &str,
        server_spki_sha256: &str,
        now_unix: u64,
    ) -> Result<PairingChallenge, ServerError> {
        if !valid_sha256(server_spki_sha256) || csr_pem.len() > 32 * 1024 {
            return Err(ServerError::InvalidPairing);
        }
        let key = verifying_key_from_csr(csr_pem)?;
        let key_der = key
            .to_public_key_der()
            .map_err(|_| ServerError::InvalidPairing)?;
        let client_public_key_sha256 = hex::encode(Sha256::digest(key_der.as_bytes()));
        let mut token = [0_u8; 32];
        OsRng.fill_bytes(&mut token);
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(ServerError::InvalidPairing)?;
        let request_id = A2aRequestId(format!("pair-{:016x}", self.serial));
        let server: [u8; 32] = hex::decode(server_spki_sha256)
            .map_err(|_| ServerError::InvalidPairing)?
            .try_into()
            .map_err(|_| ServerError::InvalidPairing)?;
        let client: [u8; 32] = hex::decode(&client_public_key_sha256)
            .map_err(|_| ServerError::InvalidPairing)?
            .try_into()
            .map_err(|_| ServerError::InvalidPairing)?;
        let pairing = PendingPairing {
            request_id: request_id.clone(),
            client_public_key_sha256,
            server_spki_sha256: server_spki_sha256.into(),
            sas_words: derive_sas_words(&request_id.0, &server, &client)
                .map_err(ServerError::from)?,
            expires_unix: now_unix.saturating_add(PAIRING_TTL.as_secs()),
            state: PairingState::Pending,
            claim_token_sha256: hex::encode(Sha256::digest(token)),
            csr_pem: csr_pem.into(),
        };
        self.pending.insert(request_id, pairing.clone());
        Ok(PairingChallenge {
            pairing,
            claim_token: token,
        })
    }

    pub fn list(&mut self, now_unix: u64) -> Vec<PendingPairing> {
        self.expire(now_unix);
        self.pending.values().cloned().collect()
    }

    pub fn approve(&mut self, request_id: &A2aRequestId, now_unix: u64) -> Result<(), ServerError> {
        self.set_state(request_id, PairingState::Approved, now_unix)
    }

    pub fn reject(&mut self, request_id: &A2aRequestId, now_unix: u64) -> Result<(), ServerError> {
        self.set_state(request_id, PairingState::Rejected, now_unix)
    }

    fn set_state(
        &mut self,
        id: &A2aRequestId,
        state: PairingState,
        now: u64,
    ) -> Result<(), ServerError> {
        self.expire(now);
        let pairing = self
            .pending
            .get_mut(id)
            .ok_or(ServerError::InvalidPairing)?;
        if pairing.state != PairingState::Pending {
            return Err(ServerError::InvalidPairing);
        }
        pairing.state = state;
        Ok(())
    }

    pub fn claim(
        &mut self,
        request_id: &A2aRequestId,
        token: &[u8; 32],
        proof: &str,
        issuer: &dyn ClientCertificateIssuer,
        profile: &ServiceProfile,
        now_unix: u64,
    ) -> Result<ClaimResult, ServerError> {
        self.expire(now_unix);
        let pairing = self
            .pending
            .get_mut(request_id)
            .ok_or(ServerError::InvalidPairing)?;
        if pairing.state != PairingState::Approved {
            return Err(if pairing.state == PairingState::Expired {
                ServerError::PairingExpired
            } else {
                ServerError::PairingNotApproved
            });
        }
        if hex::encode(Sha256::digest(token)) != pairing.claim_token_sha256 {
            return Err(ServerError::InvalidPairing);
        }
        verify_claim_proof(
            &verifying_key_from_csr(&pairing.csr_pem)?,
            &request_id.0,
            token,
            proof,
        )
        .map_err(ServerError::from)?;
        let peer_id = A2aPeerId(format!("peer-{:016x}", self.serial));
        let certificate_chain_pem = issuer.issue_client_certificate(
            &peer_id,
            &pairing.csr_pem,
            Duration::from_secs(365 * 24 * 60 * 60),
        )?;
        let fingerprint = certificate_leaf_fingerprint(&certificate_chain_pem)?;
        let certificate_serial = certificate_leaf_serial(&certificate_chain_pem)?;
        let expires_unix = now_unix.saturating_add(365 * 24 * 60 * 60);
        self.grants.insert(
            peer_id.clone(),
            PeerGrant {
                profile_name: profile.name.clone(),
                peer_id: peer_id.clone(),
                issuer_ref: profile.issuer_ref.clone(),
                certificate_serial,
                certificate_fingerprint: fingerprint,
                expires_unix,
                revoked: false,
            },
        );
        pairing.state = PairingState::Claimed;
        Ok(ClaimResult {
            peer_id,
            certificate_chain_pem,
            expires_unix,
        })
    }

    pub fn revoke(&mut self, peer: &A2aPeerId) -> bool {
        if let Some(grant) = self.grants.get_mut(peer) {
            grant.revoked = true;
            true
        } else {
            false
        }
    }

    pub fn authenticate(
        &self,
        peer: &A2aPeerId,
        fingerprint: &str,
        now_unix: u64,
    ) -> Result<(), ServerError> {
        let grant = self.grants.get(peer).ok_or(ServerError::PeerNotTrusted)?;
        if grant.revoked
            || grant.expires_unix <= now_unix
            || grant.certificate_fingerprint != fingerprint
        {
            Err(ServerError::PeerNotTrusted)
        } else {
            Ok(())
        }
    }

    fn expire(&mut self, now_unix: u64) {
        for pairing in self.pending.values_mut() {
            if pairing.expires_unix <= now_unix
                && !matches!(
                    pairing.state,
                    PairingState::Claimed | PairingState::Rejected
                )
            {
                pairing.state = PairingState::Expired;
            }
        }
    }
}

pub(super) fn serial_suffix(value: &str) -> u64 {
    value
        .rsplit_once('-')
        .and_then(|(_, suffix)| u64::from_str_radix(suffix, 16).ok())
        .unwrap_or(0)
}

pub(super) fn certificate_leaf_fingerprint(
    certificate_chain_pem: &str,
) -> Result<String, ServerError> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_chain_pem.as_bytes())
        .map_err(|_| ServerError::InvalidRequest)?;
    if pem.label != "CERTIFICATE" {
        return Err(ServerError::InvalidRequest);
    }
    Ok(hex::encode(Sha256::digest(pem.contents)))
}

pub(super) fn certificate_leaf_serial(certificate_chain_pem: &str) -> Result<String, ServerError> {
    use x509_parser::prelude::{FromDer, X509Certificate};

    let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_chain_pem.as_bytes())
        .map_err(|_| ServerError::InvalidRequest)?;
    let (_, certificate) =
        X509Certificate::from_der(&pem.contents).map_err(|_| ServerError::InvalidRequest)?;
    Ok(certificate.tbs_certificate.raw_serial_as_string())
}

pub(super) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn verifying_key_from_csr(csr_pem: &str) -> Result<VerifyingKey, ServerError> {
    let csr = CertificateSigningRequestParams::from_pem(csr_pem)
        .map_err(|_| ServerError::InvalidPairing)?;
    if csr.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        return Err(ServerError::InvalidPairing);
    }
    VerifyingKey::from_sec1_bytes(csr.public_key.der_bytes())
        .map_err(|_| ServerError::InvalidPairing)
}

#[cfg(test)]
mod tests {
    use super::{serial_suffix, valid_sha256};

    #[test]
    fn sha256_format_is_lowercase_and_exactly_32_bytes() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(!valid_sha256(&"a".repeat(63)));
        assert!(!valid_sha256(&"g".repeat(64)));
    }

    #[test]
    fn serial_suffix_only_accepts_the_final_hex_component() {
        assert_eq!(serial_suffix("peer-00000000000000ff"), 255);
        assert_eq!(serial_suffix("peer-invalid"), 0);
        assert_eq!(serial_suffix("peer-1-extra"), 0);
    }
}

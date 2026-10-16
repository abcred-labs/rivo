//! Step 020/021 共用的 A2A v1.0.1 协议 facade。
//!
//! 本 crate 有意不依赖运行时的 Session、Agent、ToolRegistry 或任何前端。它将远端
//! wire 输入收口为可验证的数据类型，而网络、凭据存储和 policy 仍由上层负责。

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use p256::ecdh::EphemeralSecret;
use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::DecodePrivateKey;
use p256::{EncodedPoint, PublicKey};
use rivo_instance_control::{validate_lan_control, LanControlEnvelope};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{parse_x509_certificate, ASN1Time};
use zeroize::Zeroizing;

pub mod psk;
use psk::{
    derive_ecdh_session_key, derive_request_key, derive_session_request_key, proof as psk_proof,
    server_response_proof, transcript_nonce, PskEcdhTranscript, PskEnvelope, PskSecret,
};

pub const SPEC_REVISION: &str = "v1.0.1";
pub const WIRE_PROTOCOL_VERSION: &str = "1.0";
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_OBSERVATION_BYTES: usize = 256 * 1024;
pub const MAX_TASK_DURATION_SECONDS: u64 = 30 * 60;
pub const MAX_ACTIVE_TASKS_PER_PEER: usize = 1;
pub const MAX_ACTIVE_TASKS: usize = 3;

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
pub const A2A_KEYRING_SERVICE: &str = "rivo-a2a";
pub const MDNS_SERVICE_NAME: &str = "_rivo-a2a._tcp.local.";
pub const MIN_MDNS_TTL_SECONDS: u64 = 10;
pub const MAX_MDNS_TTL_SECONDS: u64 = 300;
pub const A2A_CARD_PATH: &str = "/.well-known/agent-card.json";
pub const LAN_PAIRING_VERSION: &str = "rivo-a2a-lan-v1";

/// Stable peer identifier for an explicit PSK trust snapshot. Both endpoints
/// derive this from the pinned interface and canonical Agent Card hash.
pub fn psk_peer_id(interface_url: &str, card_hash: &str) -> Result<A2aPeerId, A2aError> {
    if !is_private_a2a_interface_url(interface_url) || !is_lower_hex_sha256(card_hash) {
        return Err(A2aError::InvalidTrustSnapshot);
    }
    let digest = sha256_hex(format!("psk\0{interface_url}\0{card_hash}").as_bytes());
    Ok(A2aPeerId(format!("peer-{}", &digest[..32])))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct A2aPeerId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct A2aTaskId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct A2aRequestId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum A2aTrustState {
    Candidate,
    Pending,
    Trusted,
    Suspended,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aTrustSnapshot {
    pub peer_id: A2aPeerId,
    /// Card 中唯一且经配对确认的 JSON-RPC interface URL。
    pub interface_url: String,
    /// 首期唯一允许的 binding；保留为快照字段，避免在 Card 更新后静默降级。
    pub protocol_binding: String,
    /// Card interface 明确宣布时的租户；未宣布时必须为 None。
    pub tenant: Option<String>,
    /// SHA-256(SPKI)，采用小写十六进制，不保存证书正文。
    pub spki_sha256: String,
    pub agent_card_id: String,
    pub agent_card_jcs_sha256: String,
    pub allowed_skills: BTreeSet<String>,
    /// 规范发布基线，固定为 v1.0.1；不可用 wire version 替代。
    pub spec_revision: String,
    /// Card interface 的线协议版本，固定为 1.0。
    pub wire_protocol_version: String,
    pub status: A2aTrustState,
    /// 本机 owner 完成配对批准的 UTC RFC3339 时间。
    pub approved_at: String,
}

/// 尚未建立信任的 LAN 发现记录。它不能转换为 trusted peer，也不包含可用身份材料；
/// 上层必须经 CSR、SAS 和本机批准才能开始配对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aDiscoveryCandidate {
    pub candidate_id: String,
    pub instance_name: String,
    pub interface_url: String,
    pub card_url: String,
    pub spki_sha256: String,
    pub instance_nonce: String,
    pub expires_unix: u64,
}

impl A2aDiscoveryCandidate {
    /// 发现 adapter 或存储投影必须在显示/配对前删除已过期候选；候选过期不能影响
    /// 已配对 peer，也不得被用于自动刷新其信任快照。
    pub fn is_expired_at(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_unix
    }
}

/// mDNS/DNS-SD adapter 传入的未经信任记录。保留 TXT 键值列表，以便 facade
/// 拒绝重复键，而不是让后一个值静默覆盖前一个值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsAdvertisement {
    pub service_name: String,
    pub candidate_id: String,
    pub instance_name: String,
    pub address: IpAddr,
    pub port: u16,
    pub ttl_seconds: u64,
    pub observed_unix: u64,
    pub txt: Vec<(String, String)>,
}

impl MdnsAdvertisement {
    pub fn into_candidate(self) -> Result<A2aDiscoveryCandidate, A2aError> {
        if self.service_name != MDNS_SERVICE_NAME
            || self.candidate_id.trim().is_empty()
            || self.candidate_id.len() > 128
            || self.instance_name.trim().is_empty()
            || self.port == 0
            || !(MIN_MDNS_TTL_SECONDS..=MAX_MDNS_TTL_SECONDS).contains(&self.ttl_seconds)
            || !is_private_ip(self.address)
        {
            return Err(A2aError::InvalidDiscovery);
        }
        let mut values = BTreeMap::new();
        for (key, value) in self.txt {
            if !matches!(
                key.as_str(),
                "v" | "card_path" | "spki_sha256" | "instance_nonce" | "expires_unix"
            ) || values.insert(key, value).is_some()
            {
                return Err(A2aError::InvalidDiscovery);
            }
        }
        if values.len() != 5
            || values.get("v").map(String::as_str) != Some(WIRE_PROTOCOL_VERSION)
            || values.get("card_path").map(String::as_str) != Some(A2A_CARD_PATH)
            || !values
                .get("spki_sha256")
                .is_some_and(|value| is_lower_hex_sha256(value))
            || !values
                .get("instance_nonce")
                .is_some_and(|value| !value.is_empty() && value.len() <= 128)
        {
            return Err(A2aError::InvalidDiscovery);
        }
        let expires_unix = values
            .get("expires_unix")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| {
                *value >= self.observed_unix
                    && *value <= self.observed_unix.saturating_add(self.ttl_seconds)
            })
            .ok_or(A2aError::InvalidDiscovery)?;
        let authority = match self.address {
            IpAddr::V4(address) => address.to_string(),
            IpAddr::V6(address) => format!("[{address}]"),
        };
        let interface_url = format!("https://{authority}:{}/a2a", self.port);
        let card_url = format!("https://{authority}:{}{}", self.port, A2A_CARD_PATH);
        Ok(A2aDiscoveryCandidate {
            candidate_id: self.candidate_id,
            instance_name: self.instance_name,
            interface_url,
            card_url,
            spki_sha256: values.remove("spki_sha256").expect("validated"),
            instance_nonce: values.remove("instance_nonce").expect("validated"),
            expires_unix,
        })
    }
}

/// 在一个有界时间窗口内浏览私网 mDNS/DNS-SD 候选。无效广播被忽略而非升级为
/// 信任记录；每条返回记录仍须由配对流程重新获取 Card、TLS 身份和本机确认。
pub fn discover_lan_candidates(timeout: Duration) -> Result<Vec<A2aDiscoveryCandidate>, A2aError> {
    if timeout.is_zero() || timeout > Duration::from_secs(MAX_MDNS_TTL_SECONDS) {
        return Err(A2aError::InvalidDiscovery);
    }
    let daemon = mdns_sd::ServiceDaemon::new().map_err(|_| A2aError::DiscoveryUnavailable)?;
    let receiver = daemon
        .browse(MDNS_SERVICE_NAME)
        .map_err(|_| A2aError::DiscoveryUnavailable)?;
    let deadline = std::time::Instant::now() + timeout;
    let mut candidates = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(event) = receiver.recv_timeout(remaining) else {
            break;
        };
        let mdns_sd::ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let observed_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| A2aError::DiscoveryUnavailable)?
            .as_secs();
        let txt = info
            .get_properties()
            .iter()
            .map(|property| (property.key().into(), property.val_str().into()))
            .collect::<Vec<_>>();
        for address in info.get_addresses() {
            let nonce = info
                .get_property_val_str("instance_nonce")
                .unwrap_or_default();
            let candidate_id = sha256_hex(
                format!(
                    "{}\0{}\0{}\0{}",
                    info.get_fullname(),
                    address,
                    nonce,
                    observed_unix
                )
                .as_bytes(),
            );
            let advertisement = MdnsAdvertisement {
                service_name: info.get_type().into(),
                candidate_id,
                instance_name: info.get_fullname().into(),
                address: *address,
                port: info.get_port(),
                ttl_seconds: u64::from(info.get_other_ttl()),
                observed_unix,
                txt: txt.clone(),
            };
            if let Ok(candidate) = advertisement.into_candidate() {
                candidates.push(candidate);
            }
        }
    }
    let _ = daemon.stop_browse(MDNS_SERVICE_NAME);
    let _ = daemon.shutdown();
    Ok(candidates)
}

/// 本机身份材料的最小抽象。`identity_ref` 仅是 keychain account 名称；调用方
/// 不得把私钥、证书正文或配对 token 放入配置、SQLite、日志或事件。
pub trait A2aIdentityStore: Send + Sync {
    fn delete(&self, identity_ref: &str) -> Result<(), A2aError>;
    /// 仅供配对实现保存 keychain 中的身份材料；调用方不得将该内容写入配置、
    /// SQLite、日志、事件或诊断包。
    fn store_secret(&self, _identity_ref: &str, _kind: &str, _value: &str) -> Result<(), A2aError> {
        Err(A2aError::IdentityStore)
    }
    fn load_secret(&self, _identity_ref: &str, _kind: &str) -> Result<String, A2aError> {
        Err(A2aError::IdentityStore)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringA2aIdentityStore;

impl KeyringA2aIdentityStore {
    pub fn validate_identity_ref(identity_ref: &str) -> Result<(), A2aError> {
        if identity_ref.is_empty()
            || identity_ref.len() > 128
            || !identity_ref
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(A2aError::InvalidIdentityRef);
        }
        Ok(())
    }
}

impl A2aIdentityStore for KeyringA2aIdentityStore {
    fn delete(&self, identity_ref: &str) -> Result<(), A2aError> {
        Self::validate_identity_ref(identity_ref)?;
        let mut accounts = vec![identity_ref.to_owned()];
        for kind in ["client-p256-key", "claim-token", "client-certificate"] {
            if let Ok(account) = keyring_identity_account(identity_ref, kind) {
                accounts.push(account);
            }
        }
        for account in accounts {
            let entry = keyring::Entry::new(A2A_KEYRING_SERVICE, &account)
                .map_err(|_| A2aError::IdentityStore)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => return Err(A2aError::IdentityStore),
            }
        }
        Ok(())
    }

    fn store_secret(&self, identity_ref: &str, kind: &str, value: &str) -> Result<(), A2aError> {
        let account = keyring_identity_account(identity_ref, kind)?;
        keyring::Entry::new(A2A_KEYRING_SERVICE, &account)
            .map_err(|_| A2aError::IdentityStore)?
            .set_password(value)
            .map_err(|_| A2aError::IdentityStore)
    }

    fn load_secret(&self, identity_ref: &str, kind: &str) -> Result<String, A2aError> {
        let account = keyring_identity_account(identity_ref, kind)?;
        keyring::Entry::new(A2A_KEYRING_SERVICE, &account)
            .map_err(|_| A2aError::IdentityStore)?
            .get_password()
            .map_err(|_| A2aError::IdentityStore)
    }
}

fn keyring_identity_account(identity_ref: &str, kind: &str) -> Result<String, A2aError> {
    KeyringA2aIdentityStore::validate_identity_ref(identity_ref)?;
    if !matches!(
        kind,
        "client-p256-key" | "claim-token" | "client-certificate"
    ) {
        return Err(A2aError::InvalidIdentityRef);
    }
    if identity_ref
        .len()
        .saturating_add(kind.len())
        .saturating_add(1)
        > 128
    {
        return Err(A2aError::InvalidIdentityRef);
    }
    let account = format!("{identity_ref}.{kind}");
    KeyringA2aIdentityStore::validate_identity_ref(&account)?;
    Ok(account)
}

/// 配对私钥只通过 identity_ref 的 OS keychain 条目管理。结构中没有私钥字段，
/// 因而不能被调用方误写入配置或本机 SQLite。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aClientPairingIdentity {
    pub identity_ref: String,
    pub csr_pem: String,
    pub client_public_key_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct A2aPairingRequest {
    pub version: String,
    pub csr_pem: String,
}

impl A2aPairingRequest {
    pub fn for_identity(identity: &A2aClientPairingIdentity) -> Self {
        Self {
            version: LAN_PAIRING_VERSION.into(),
            csr_pem: identity.csr_pem.clone(),
        }
    }
}

/// 只从已经校验的完整 A2A interface 推导 pairing 路径，不能接受任意 URL、
/// query 或 fragment。
pub fn pairing_url_from_interface(interface_url: &str) -> Result<String, A2aError> {
    if !is_private_a2a_interface_url(interface_url) {
        return Err(A2aError::InvalidEndpoint);
    }
    Ok(format!(
        "{}{}",
        interface_url.trim_end_matches("/a2a"),
        "/rivo-a2a-lan-v1/pairings"
    ))
}

pub fn create_client_pairing_identity(
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
) -> Result<A2aClientPairingIdentity, A2aError> {
    KeyringA2aIdentityStore::validate_identity_ref(identity_ref)?;
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|_| A2aError::IdentityStore)?;
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|_| A2aError::IdentityStore)?;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let csr_pem = params
        .serialize_request(&key)
        .and_then(|csr| csr.pem())
        .map_err(|_| A2aError::IdentityStore)?;
    identities.store_secret(identity_ref, "client-p256-key", &key.serialize_pem())?;
    Ok(A2aClientPairingIdentity {
        identity_ref: identity_ref.into(),
        csr_pem,
        client_public_key_sha256: sha256_hex(&key.public_key_der()),
    })
}

/// 为已存在的本机 P-256 身份重新生成 CSR。证书轮换必须复用同一 keychain
/// 私钥：调用方不会拿到私钥正文，也不会因为轮换而创建另一份可被遗忘撤销的身份。
pub fn create_csr_for_existing_identity(
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
) -> Result<A2aClientPairingIdentity, A2aError> {
    KeyringA2aIdentityStore::validate_identity_ref(identity_ref)?;
    let private_key_pem = identities.load_secret(identity_ref, "client-p256-key")?;
    let key = rcgen::KeyPair::from_pem(&private_key_pem).map_err(|_| A2aError::IdentityStore)?;
    let params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|_| A2aError::IdentityStore)?;
    let csr_pem = params
        .serialize_request(&key)
        .and_then(|csr| csr.pem())
        .map_err(|_| A2aError::IdentityStore)?;
    Ok(A2aClientPairingIdentity {
        identity_ref: identity_ref.into(),
        csr_pem,
        client_public_key_sha256: sha256_hex(&key.public_key_der()),
    })
}

pub fn sign_claim_proof_with_identity(
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
    request_id: &str,
    claim_token: &[u8; 32],
) -> Result<String, A2aError> {
    let key = SigningKey::from_pkcs8_pem(&identities.load_secret(identity_ref, "client-p256-key")?)
        .map_err(|_| A2aError::IdentityStore)?;
    sign_claim_proof(&key, request_id, claim_token)
}

/// 配对 endpoint 返回的短暂挑战。claim_token 仅作为本机内存/OS keychain 材料使用，
/// 调用方不得将它持久化到 SQLite、配置或日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aPairingResponse {
    pub request_id: A2aRequestId,
    pub server_spki_sha256: String,
    pub client_public_key_sha256: String,
    pub expires_at: String,
    pub sas_words: [String; 6],
    pub claim_token: [u8; 32],
}

impl A2aPairingResponse {
    pub fn parse_json(input: &str) -> Result<Self, A2aError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            version: String,
            request_id: String,
            server_spki_sha256: String,
            client_public_key_sha256: String,
            expires_at: String,
            sas_words: Vec<String>,
            claim_token: String,
        }
        let wire: Wire = serde_json::from_value(parse_unique_json(input)?)
            .map_err(|_| A2aError::InvalidPairing)?;
        if wire.version != LAN_PAIRING_VERSION
            || wire.request_id.is_empty()
            || wire.request_id.len() > 256
            || !is_lower_hex_sha256(&wire.server_spki_sha256)
            || !is_lower_hex_sha256(&wire.client_public_key_sha256)
            || !is_rfc3339_utc(&wire.expires_at)
            || wire.sas_words.len() != 6
            || wire
                .sas_words
                .iter()
                .any(|word| word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_lowercase()))
            || wire.claim_token.contains('=')
            || !wire.claim_token.bytes().all(is_base64url)
        {
            return Err(A2aError::InvalidPairing);
        }
        let claim_token: [u8; 32] = URL_SAFE_NO_PAD
            .decode(wire.claim_token)
            .map_err(|_| A2aError::InvalidPairing)?
            .try_into()
            .map_err(|_| A2aError::InvalidPairing)?;
        let sas_words = wire
            .sas_words
            .try_into()
            .map_err(|_| A2aError::InvalidPairing)?;
        Ok(Self {
            request_id: A2aRequestId(wire.request_id),
            server_spki_sha256: wire.server_spki_sha256,
            client_public_key_sha256: wire.client_public_key_sha256,
            expires_at: wire.expires_at,
            sas_words,
            claim_token,
        })
    }

    pub fn verify_for_identity(
        &self,
        identity: &A2aClientPairingIdentity,
        expected_server_spki_sha256: &str,
    ) -> Result<(), A2aError> {
        if self.client_public_key_sha256 != identity.client_public_key_sha256
            || self.server_spki_sha256 != expected_server_spki_sha256
        {
            return Err(A2aError::InvalidPairing);
        }
        let server: [u8; 32] = hex::decode(&self.server_spki_sha256)
            .map_err(|_| A2aError::InvalidPairing)?
            .try_into()
            .map_err(|_| A2aError::InvalidPairing)?;
        let client: [u8; 32] = hex::decode(&self.client_public_key_sha256)
            .map_err(|_| A2aError::InvalidPairing)?
            .try_into()
            .map_err(|_| A2aError::InvalidPairing)?;
        if derive_sas_words(&self.request_id.0, &server, &client)? != self.sas_words {
            return Err(A2aError::InvalidPairing);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct A2aClaimRequest {
    pub version: String,
    pub request_id: String,
    pub claim_token: String,
    pub claim_proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aClaimResponse {
    pub peer_id: A2aPeerId,
    pub certificate_chain_pem: String,
    pub expires_at: String,
}

impl A2aClaimResponse {
    pub fn parse_json(input: &str, request_id: &A2aRequestId) -> Result<Self, A2aError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            version: String,
            request_id: String,
            peer_id: String,
            certificate_chain_pem: String,
            expires_at: String,
        }
        let wire: Wire = serde_json::from_value(parse_unique_json(input)?)
            .map_err(|_| A2aError::InvalidPairing)?;
        if wire.version != LAN_PAIRING_VERSION
            || wire.request_id != request_id.0
            || wire.peer_id.trim().is_empty()
            || wire.peer_id.len() > 128
            || wire.certificate_chain_pem.len() > 32 * 1024
            || !wire
                .certificate_chain_pem
                .contains("-----BEGIN CERTIFICATE-----")
            || !is_rfc3339_utc(&wire.expires_at)
        {
            return Err(A2aError::InvalidPairing);
        }
        Ok(Self {
            peer_id: A2aPeerId(wire.peer_id),
            certificate_chain_pem: wire.certificate_chain_pem,
            expires_at: wire.expires_at,
        })
    }
}

pub fn make_claim_request(
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
    response: &A2aPairingResponse,
) -> Result<A2aClaimRequest, A2aError> {
    Ok(A2aClaimRequest {
        version: LAN_PAIRING_VERSION.into(),
        request_id: response.request_id.0.clone(),
        claim_token: URL_SAFE_NO_PAD.encode(response.claim_token),
        claim_proof: sign_claim_proof_with_identity(
            identities,
            identity_ref,
            &response.request_id.0,
            &response.claim_token,
        )?,
    })
}

fn claim_url_from_interface(
    interface_url: &str,
    request_id: &A2aRequestId,
) -> Result<String, A2aError> {
    if !is_private_a2a_interface_url(interface_url)
        || request_id.0.is_empty()
        || request_id.0.len() > 256
        || !request_id
            .0
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(A2aError::InvalidPairing);
    }
    Ok(format!(
        "{}/rivo-a2a-lan-v1/pairings/{}/claim",
        interface_url.trim_end_matches("/a2a"),
        request_id.0
    ))
}

/// 配对初始请求使用候选广播/用户确认的 SPKI pin 建立 TLS；它不使用本机系统 CA
/// 或 client certificate。调用方必须先在本机展示并确认返回的 SAS，才能调用 claim。
pub async fn begin_pairing(
    interface_url: &str,
    expected_server_spki_sha256: &str,
    identity: &A2aClientPairingIdentity,
) -> Result<A2aPairingResponse, A2aError> {
    let client = build_pinned_tls_client(interface_url, expected_server_spki_sha256)?;
    let response = client
        .post(pairing_url_from_interface(interface_url)?)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&A2aPairingRequest::for_identity(identity))
        .send()
        .await
        .map_err(|_| A2aError::Transport)?;
    if !response.status().is_success() {
        return Err(A2aError::Transport);
    }
    let body = response.text().await.map_err(|_| A2aError::Transport)?;
    if body.len() > MAX_OBSERVATION_BYTES {
        return Err(A2aError::ObservationTooLarge);
    }
    let pairing = A2aPairingResponse::parse_json(&body)?;
    pairing.verify_for_identity(identity, expected_server_spki_sha256)?;
    Ok(pairing)
}

/// 本机确认 SAS 后一次性领取 client certificate。claim token 只存在于传入的
/// pairing response 和本函数栈帧；领取成功才把证书链放入 keychain。
pub async fn claim_pairing(
    interface_url: &str,
    identities: &dyn A2aIdentityStore,
    identity: &A2aClientPairingIdentity,
    pairing: &A2aPairingResponse,
) -> Result<A2aClaimResponse, A2aError> {
    let client = build_pinned_tls_client(interface_url, &pairing.server_spki_sha256)?;
    let claim = make_claim_request(identities, &identity.identity_ref, pairing)?;
    let response = client
        .post(claim_url_from_interface(
            interface_url,
            &pairing.request_id,
        )?)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&claim)
        .send()
        .await
        .map_err(|_| A2aError::Transport)?;
    if !response.status().is_success() {
        return Err(A2aError::Transport);
    }
    let body = response.text().await.map_err(|_| A2aError::Transport)?;
    if body.len() > MAX_OBSERVATION_BYTES {
        return Err(A2aError::ObservationTooLarge);
    }
    let claim = A2aClaimResponse::parse_json(&body, &pairing.request_id)?;
    validate_claimed_client_certificate(
        &claim.certificate_chain_pem,
        &identity.client_public_key_sha256,
    )?;
    identities.store_secret(
        &identity.identity_ref,
        "client-certificate",
        &claim.certificate_chain_pem,
    )?;
    Ok(claim)
}

fn validate_claimed_client_certificate(
    certificate_chain_pem: &str,
    expected_public_key_sha256: &str,
) -> Result<(), A2aError> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};

    let certificate = CertificateDer::from_pem_slice(certificate_chain_pem.as_bytes())
        .map_err(|_| A2aError::CertificateRejected)?;
    let (_, certificate) =
        parse_x509_certificate(certificate.as_ref()).map_err(|_| A2aError::CertificateRejected)?;
    if !certificate.validity().is_valid()
        || sha256_hex(certificate.public_key().raw) != expected_public_key_sha256
        || !certificate
            .extended_key_usage()
            .map_err(|_| A2aError::CertificateRejected)?
            .is_some_and(|usage| usage.value.client_auth)
        || !certificate
            .key_usage()
            .map_err(|_| A2aError::CertificateRejected)?
            .is_some_and(|usage| usage.value.digital_signature())
    {
        return Err(A2aError::CertificateRejected);
    }
    Ok(())
}

impl A2aTrustSnapshot {
    pub fn validate_transport(&self) -> Result<(), A2aError> {
        if self.status != A2aTrustState::Trusted {
            return Err(A2aError::PeerNotTrusted);
        }
        if !is_private_a2a_interface_url(&self.interface_url) {
            return Err(A2aError::InvalidEndpoint);
        }
        if !matches!(self.protocol_binding.as_str(), "JSONRPC" | "PSK")
            || self.spec_revision != SPEC_REVISION
            || self.wire_protocol_version != WIRE_PROTOCOL_VERSION
            || self.agent_card_id.trim().is_empty()
            || !is_rfc3339_utc(&self.approved_at)
        {
            return Err(A2aError::ProtocolVersion);
        }
        if !is_lower_hex_sha256(&self.spki_sha256)
            || !is_lower_hex_sha256(&self.agent_card_jcs_sha256)
        {
            return Err(A2aError::InvalidTrustSnapshot);
        }
        Ok(())
    }

    pub fn validate_for_delegate(&self, skill_id: &str) -> Result<(), A2aError> {
        self.validate_transport()?;
        if !self.allowed_skills.contains(skill_id) {
            return Err(A2aError::SkillNotGranted);
        }
        Ok(())
    }

    /// Validate the separately granted LAN instance-control capability and its
    /// expiring, peer-bound request envelope.
    pub fn validate_for_instance_control(
        &self,
        envelope: &LanControlEnvelope,
        now_unix_ms: u128,
    ) -> Result<(), A2aError> {
        self.validate_transport()?;
        if envelope.peer_id != self.peer_id.0 || !self.allowed_skills.contains("process-control") {
            return Err(A2aError::SkillNotGranted);
        }
        validate_lan_control(envelope, now_unix_ms).map_err(|_| A2aError::InvalidTrustSnapshot)
    }

    /// Card、SPKI、endpoint、协议或技能声明发生变化时，一律降级为 suspended。
    /// 调用方必须持久化返回值，不能继续使用旧快照发起请求。
    #[allow(clippy::too_many_arguments)] // 字段来自冻结 trust snapshot，避免弱化任一比较项。
    pub fn reconcile(
        &self,
        interface_url: &str,
        protocol_binding: &str,
        tenant: Option<String>,
        spki_sha256: &str,
        agent_card_id: &str,
        agent_card_jcs_sha256: &str,
        allowed_skills: BTreeSet<String>,
        spec_revision: &str,
        wire_protocol_version: &str,
    ) -> Self {
        let changed = self.interface_url != interface_url
            || self.protocol_binding != protocol_binding
            || self.tenant != tenant
            || self.spki_sha256 != spki_sha256
            || self.agent_card_id != agent_card_id
            || self.agent_card_jcs_sha256 != agent_card_jcs_sha256
            || self.allowed_skills != allowed_skills
            || self.spec_revision != spec_revision
            || self.wire_protocol_version != wire_protocol_version;
        Self {
            peer_id: self.peer_id.clone(),
            interface_url: interface_url.into(),
            protocol_binding: protocol_binding.into(),
            tenant,
            spki_sha256: spki_sha256.into(),
            agent_card_id: agent_card_id.into(),
            agent_card_jcs_sha256: agent_card_jcs_sha256.into(),
            allowed_skills,
            spec_revision: spec_revision.into(),
            wire_protocol_version: wire_protocol_version.into(),
            status: if changed {
                A2aTrustState::Suspended
            } else {
                self.status
            },
            approved_at: self.approved_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum A2aTaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

impl A2aTaskState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    fn parse_wire(value: &str) -> Result<Self, A2aError> {
        match value.strip_prefix("TASK_STATE_").unwrap_or(value) {
            "SUBMITTED" => Ok(Self::Submitted),
            "WORKING" => Ok(Self::Working),
            "INPUT_REQUIRED" => Ok(Self::InputRequired),
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed),
            "CANCELED" => Ok(Self::Canceled),
            "REJECTED" => Ok(Self::Rejected),
            _ => Err(A2aError::InvalidTask),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aTask {
    pub id: A2aTaskId,
    pub state: A2aTaskState,
    #[serde(default)]
    pub revision: Option<u64>,
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<A2aArtifact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aArtifact {
    pub id: String,
    pub name: String,
    pub value: Value,
}

impl A2aTask {
    /// 将 A2A JSON-RPC task 投影为本机受限状态。未知状态、二进制/raw/url Part
    /// 和无 ID 的 Artifact 均拒绝，防止它们沿普通工具观察路径进入模型上下文。
    pub fn from_wire(value: Value) -> Result<Self, A2aError> {
        let object = value.as_object().ok_or(A2aError::InvalidTask)?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or(A2aError::InvalidTask)?;
        let state = object
            .get("status")
            .and_then(Value::as_object)
            .and_then(|status| status.get("state"))
            .or_else(|| object.get("state"))
            .and_then(Value::as_str)
            .ok_or(A2aError::InvalidTask)
            .and_then(A2aTaskState::parse_wire)?;
        let revision = object
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get("io.rivo.a2a.v1"))
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get("revision"))
            .and_then(Value::as_u64);
        let message = object
            .get("status")
            .and_then(Value::as_object)
            .and_then(|status| status.get("message"))
            .cloned();
        let artifacts = object
            .get("artifacts")
            .map(|value| {
                value
                    .as_array()
                    .ok_or(A2aError::InvalidTask)?
                    .iter()
                    .map(|artifact| {
                        let object = artifact.as_object().ok_or(A2aError::InvalidTask)?;
                        let id = object
                            .get("artifactId")
                            .or_else(|| object.get("id"))
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty() && value.len() <= 128)
                            .ok_or(A2aError::InvalidTask)?;
                        let name = object
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("artifact");
                        let parts = object
                            .get("parts")
                            .and_then(Value::as_array)
                            .filter(|parts| parts.len() == 1)
                            .ok_or(A2aError::InvalidTask)?;
                        let part = parts[0].as_object().ok_or(A2aError::InvalidTask)?;
                        let value = match (part.get("text"), part.get("data")) {
                            (Some(text), None) if text.is_string() => text.clone(),
                            (None, Some(data)) => data.clone(),
                            _ => return Err(A2aError::InvalidTask),
                        };
                        Ok(A2aArtifact {
                            id: id.into(),
                            name: name.into(),
                            value,
                        })
                    })
                    .collect::<Result<Vec<_>, A2aError>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            id: A2aTaskId(id.into()),
            state,
            revision,
            message,
            artifacts,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClassification {
    UserProvided,
    PublicExternal,
    WorkspaceDerived,
    PrivateMemory,
    Credential,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aArtifactInput {
    pub name: Option<String>,
    pub kind: A2aArtifactKind,
    /// text 的 UTF-8 正文，或 json 的 RFC 8785 JCS 文本。保持原始字符串是为了
    /// 将确认绑定到确切的外发字节，而不是 serde 的二次序列化结果。
    pub content: String,
    pub classification: DataClassification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum A2aArtifactKind {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aDelegationRequest {
    pub peer_id: A2aPeerId,
    pub skill_id: String,
    pub message: String,
    pub stream: bool,
    #[serde(default)]
    pub artifacts: Vec<A2aArtifactInput>,
}

impl A2aDelegationRequest {
    pub fn validate(&self) -> Result<(), A2aError> {
        if self.message.len() > MAX_MESSAGE_BYTES || self.message.trim().is_empty() {
            return Err(A2aError::MessageTooLarge);
        }
        if self.skill_id.trim().is_empty() || self.peer_id.0.trim().is_empty() {
            return Err(A2aError::InvalidRequest);
        }
        if self.peer_id.0.len() > 128 || self.skill_id.len() > 128 || self.artifacts.len() > 16 {
            return Err(A2aError::InvalidRequest);
        }
        let mut total_bytes = self.message.len();
        for artifact in &self.artifacts {
            if artifact
                .name
                .as_deref()
                .is_some_and(|name| name.is_empty() || name.len() > 128 || !is_artifact_name(name))
            {
                return Err(A2aError::InvalidArtifact);
            }
            if artifact.content.len() > MAX_MESSAGE_BYTES {
                return Err(A2aError::MessageTooLarge);
            }
            total_bytes = total_bytes.saturating_add(artifact.content.len());
            if matches!(artifact.kind, A2aArtifactKind::Json) {
                let parsed = parse_unique_json(&artifact.content)?;
                if canonical_json(&parsed)? != artifact.content {
                    return Err(A2aError::InvalidArtifact);
                }
            }
            if matches!(
                artifact.classification,
                DataClassification::PrivateMemory | DataClassification::Credential
            ) {
                return Err(A2aError::SensitiveDataForbidden);
            }
        }
        if total_bytes > MAX_MESSAGE_BYTES {
            return Err(A2aError::MessageTooLarge);
        }
        Ok(())
    }

    pub fn confirmation_binding(&self) -> Result<A2aConfirmationBinding, A2aError> {
        self.validate()?;
        let artifact_hashes = self
            .artifacts
            .iter()
            .map(|artifact| sha256_hex(artifact.content.as_bytes()))
            .collect::<Vec<_>>();
        Ok(A2aConfirmationBinding {
            peer_id: self.peer_id.clone(),
            skill_id: self.skill_id.clone(),
            message_sha256: sha256_hex(self.message.as_bytes()),
            artifact_sha256: artifact_hashes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aConfirmationBinding {
    pub peer_id: A2aPeerId,
    pub skill_id: String,
    pub message_sha256: String,
    pub artifact_sha256: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Card 声明的稳定身份；旧 Card 没有 id 时退化为 name，再退化为 JCS hash。
    pub id: String,
    /// Card 的服务版本（例如 `1.0.0`）；它不是 A2A wire protocol version。
    pub service_version: String,
    pub protocol_version: String,
    pub url: String,
    pub skills: BTreeSet<String>,
    pub canonical_sha256: String,
}

impl AgentCard {
    /// 从原始 HTTP 正文解析 Card。重复 key 必须在反序列化时拒绝，不能依赖
    /// `serde_json::Value` 的“后值覆盖前值”行为计算信任哈希。
    pub fn parse_json(input: &str) -> Result<Self, A2aError> {
        Self::parse(&parse_unique_json(input)?)
    }

    pub fn parse(value: &Value) -> Result<Self, A2aError> {
        let object = value.as_object().ok_or(A2aError::InvalidAgentCard)?;
        // v1.0.1 Card 的 `version` 是服务版本；wire version 由本冻结 facade 固定。
        let service_version = object
            .get("version")
            .and_then(Value::as_str)
            .ok_or(A2aError::InvalidAgentCard)?
            .to_owned();
        let (url, protocol_version) =
            if let Some(interfaces) = object.get("supportedInterfaces").and_then(Value::as_array) {
                let matches = interfaces
                    .iter()
                    .filter_map(Value::as_object)
                    .filter(|interface| {
                        interface.get("protocolBinding").and_then(Value::as_str) == Some("JSONRPC")
                            && interface.get("protocolVersion").and_then(Value::as_str)
                                == Some(WIRE_PROTOCOL_VERSION)
                    })
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(A2aError::InvalidAgentCard);
                }
                (
                    matches[0]
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or(A2aError::InvalidAgentCard)?
                        .to_owned(),
                    WIRE_PROTOCOL_VERSION.into(),
                )
            } else {
                (
                    object
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or(A2aError::InvalidAgentCard)?
                        .to_owned(),
                    WIRE_PROTOCOL_VERSION.into(),
                )
            };
        if service_version.trim().is_empty() || !is_private_a2a_interface_url(&url) {
            return Err(A2aError::InvalidAgentCard);
        }
        let skills = object
            .get("skills")
            .and_then(Value::as_array)
            .ok_or(A2aError::InvalidAgentCard)?
            .iter()
            .map(|skill| {
                skill
                    .as_object()
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or(A2aError::InvalidAgentCard)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if skills.is_empty() {
            return Err(A2aError::InvalidAgentCard);
        }
        let canonical_sha256 = canonical_json_sha256(value)?;
        let id = object
            .get("id")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty() && id.len() <= 128)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("card-{canonical_sha256}"));
        Ok(Self {
            id,
            service_version,
            protocol_version,
            url,
            skills,
            canonical_sha256,
        })
    }
}

/// 在已固定 SPKI 的 TLS 连接中取得 Card；Card interface 必须精确等于发起配对的
/// endpoint，不能借由 Card 重定向到另一台设备。
pub async fn fetch_agent_card(
    interface_url: &str,
    expected_server_spki_sha256: &str,
) -> Result<AgentCard, A2aError> {
    let client = build_pinned_tls_client(interface_url, expected_server_spki_sha256)?;
    let card_url = format!(
        "{}{}",
        interface_url.trim_end_matches("/a2a"),
        A2A_CARD_PATH
    );
    let response = client
        .get(card_url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| A2aError::Transport)?;
    if !response.status().is_success() {
        return Err(A2aError::Transport);
    }
    let body = response.text().await.map_err(|_| A2aError::Transport)?;
    if body.len() > MAX_OBSERVATION_BYTES {
        return Err(A2aError::ObservationTooLarge);
    }
    let card = AgentCard::parse_json(&body)?;
    if card.url != interface_url {
        return Err(A2aError::InvalidAgentCard);
    }
    Ok(card)
}

/// 解析 JSON，并在任意对象层级拒绝重复 key。该函数是所有 A2A HTTP Card、
/// JSON-RPC 和 pairing 原始文本入口的推荐解析器。
pub fn parse_unique_json(input: &str) -> Result<Value, A2aError> {
    struct UniqueValue(Value);

    impl<'de> Deserialize<'de> for UniqueValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct UniqueVisitor;

            impl<'de> Visitor<'de> for UniqueVisitor {
                type Value = UniqueValue;

                fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a JSON value without duplicate object keys")
                }

                fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Bool(value)))
                }

                fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    Ok(UniqueValue(Value::Number(value.into())))
                }

                fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    Ok(UniqueValue(Value::Number(value.into())))
                }

                fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    serde_json::Number::from_f64(value)
                        .map(Value::Number)
                        .map(UniqueValue)
                        .ok_or_else(|| E::custom("JSON number is not finite"))
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::String(value.to_owned())))
                }

                fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::String(value)))
                }

                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Null))
                }

                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(UniqueValue(Value::Null))
                }

                fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut values = Vec::new();
                    while let Some(value) = sequence.next_element::<UniqueValue>()? {
                        values.push(value.0);
                    }
                    Ok(UniqueValue(Value::Array(values)))
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut values = Map::new();
                    while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                        if values.insert(key.clone(), value.0).is_some() {
                            return Err(de::Error::custom(format!("duplicate object key: {key}")));
                        }
                    }
                    Ok(UniqueValue(Value::Object(values)))
                }
            }

            deserializer.deserialize_any(UniqueVisitor)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = UniqueValue::deserialize(&mut deserializer)
        .map_err(|_| A2aError::InvalidJson)?
        .0;
    deserializer.end().map_err(|_| A2aError::InvalidJson)?;
    Ok(value)
}

/// 以 RFC 8785 所要求的确定键序生成 JSON。serde_json 使用有限 JSON number 模型；
/// 协议 facade 只接受已解析的 JSON 值，HTTP 边界必须先拒绝含重复 key 的原始 Card。
pub fn canonical_json(value: &Value) -> Result<String, A2aError> {
    fn write(value: &Value, out: &mut String) -> Result<(), A2aError> {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => out.push_str(&value.to_string()),
            Value::String(value) => {
                out.push_str(&serde_json::to_string(value).map_err(|_| A2aError::InvalidJson)?);
            }
            Value::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(value, out)?;
                }
                out.push(']');
            }
            Value::Object(values) => {
                out.push('{');
                let mut ordered: BTreeMap<&str, &Value> = BTreeMap::new();
                for (key, value) in values {
                    ordered.insert(key, value);
                }
                for (index, (key, value)) in ordered.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).map_err(|_| A2aError::InvalidJson)?);
                    out.push(':');
                    write(value, out)?;
                }
                out.push('}');
            }
        }
        Ok(())
    }
    let mut output = String::new();
    write(value, &mut output)?;
    Ok(output)
}

pub fn canonical_json_sha256(value: &Value) -> Result<String, A2aError> {
    Ok(sha256_hex(canonical_json(value)?.as_bytes()))
}

/// 按 rivo-a2a-lan-v1 固定输入派生六词 SAS。
pub fn derive_sas_words(
    request_id: &str,
    server_spki_sha256: &[u8; 32],
    client_public_key_sha256: &[u8; 32],
) -> Result<[String; 6], A2aError> {
    if request_id.len() > u32::MAX as usize {
        return Err(A2aError::InvalidRequest);
    }
    let mut input = b"rivo-a2a-lan-v1/sas\0".to_vec();
    input.extend_from_slice(&(request_id.len() as u32).to_be_bytes());
    input.extend_from_slice(request_id.as_bytes());
    input.extend_from_slice(server_spki_sha256);
    input.extend_from_slice(client_public_key_sha256);
    let digest = Sha256::digest(input);
    let words = include_str!("../protocol/v1.0.1/bip39-english.txt")
        .lines()
        .collect::<Vec<_>>();
    if words.len() != 2048 {
        return Err(A2aError::ProtocolFixtureInvalid);
    }
    // 将前 9 个字节左折叠后移除低 6 bit，得到严格的前 66 bit。不能只装入
    // 8 个字节，否则第一个 11-bit group 已丢失了 digest[8] 的高两位。
    let prefix = digest
        .iter()
        .take(9)
        .fold(0_u128, |value, byte| (value << 8) | u128::from(*byte))
        >> 6;
    let mut result: Vec<String> = Vec::with_capacity(6);
    for position in 0..6 {
        let index = ((prefix >> ((5 - position) * 11)) & 0x7ff) as usize;
        result.push(words[index].to_owned());
    }
    result
        .try_into()
        .map_err(|_| A2aError::ProtocolFixtureInvalid)
}

pub fn claim_proof_payload(request_id: &str, claim_token: &[u8; 32]) -> Result<[u8; 32], A2aError> {
    if request_id.len() > u32::MAX as usize {
        return Err(A2aError::InvalidRequest);
    }
    let mut input = b"rivo-a2a-lan-v1/claim-proof\0".to_vec();
    input.extend_from_slice(&(request_id.len() as u32).to_be_bytes());
    input.extend_from_slice(request_id.as_bytes());
    input.extend_from_slice(claim_token);
    Ok(Sha256::digest(input).into())
}

pub fn sign_claim_proof(
    key: &SigningKey,
    request_id: &str,
    claim_token: &[u8; 32],
) -> Result<String, A2aError> {
    let payload = claim_proof_payload(request_id, claim_token)?;
    let signature: Signature = key
        .sign_prehash(&payload)
        .map_err(|_| A2aError::InvalidClaimProof)?;
    Ok(URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()))
}

pub fn verify_claim_proof(
    key: &VerifyingKey,
    request_id: &str,
    claim_token: &[u8; 32],
    encoded: &str,
) -> Result<(), A2aError> {
    if encoded.contains('=') || !encoded.bytes().all(is_base64url) {
        return Err(A2aError::InvalidClaimProof);
    }
    let der = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| A2aError::InvalidClaimProof)?;
    let signature = Signature::from_der(&der).map_err(|_| A2aError::InvalidClaimProof)?;
    key.verify_prehash(&claim_proof_payload(request_id, claim_token)?, &signature)
        .map_err(|_| A2aError::InvalidClaimProof)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// 解析 UTF-8 SSE 文本。调用方应先校验单帧长度，再将 data 解析为 A2A Task。
pub fn parse_sse_events(input: &str) -> Result<Vec<SseEvent>, A2aError> {
    let mut events = Vec::new();
    let mut id = None;
    let mut event = None;
    let mut data = Vec::new();
    for line in input.lines() {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(SseEvent {
                    id: id.take(),
                    event: event.take(),
                    data: data.join("\n"),
                });
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => id = Some(value.to_owned()),
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value.to_owned()),
            "retry" => {}
            _ => return Err(A2aError::InvalidSse),
        }
    }
    if !data.is_empty() {
        events.push(SseEvent {
            id,
            event,
            data: data.join("\n"),
        });
    }
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

/// 将已通过本机 policy 的委派输入投影为冻结的 JSON-RPC 请求。这里不接受 URL、
/// 二进制 Artifact 或任何远端提供的 transport 字段；请求 ID 由规范化的业务输入
/// 派生，以便 Server 的 `message_id + request_hash` 幂等边界稳定工作。
pub fn build_delegation_jsonrpc_request(
    trust: &A2aTrustSnapshot,
    request: &A2aDelegationRequest,
) -> Result<JsonRpcRequest, A2aError> {
    request.validate()?;
    if request.peer_id != trust.peer_id {
        return Err(A2aError::PeerNotTrusted);
    }
    trust.validate_for_delegate(&request.skill_id)?;
    let mut parts = vec![serde_json::json!({"kind": "text", "text": request.message})];
    for artifact in &request.artifacts {
        match artifact.kind {
            A2aArtifactKind::Text => parts.push(serde_json::json!({
                "kind": "text", "text": artifact.content, "filename": artifact.name,
            })),
            A2aArtifactKind::Json => parts.push(serde_json::json!({
                "kind": "data", "data": parse_unique_json(&artifact.content)?, "filename": artifact.name,
            })),
        }
    }
    let id = canonical_json_sha256(&serde_json::json!({
        "peer_id": request.peer_id.0,
        "skill_id": request.skill_id,
        "message": request.message,
        "stream": request.stream,
        "artifacts": request.artifacts,
    }))?;
    Ok(JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: id.clone(),
        method: if request.stream {
            "message/stream".into()
        } else {
            "message/send".into()
        },
        params: serde_json::json!({
            "message": {
                "messageId": id,
                "role": "user",
                "parts": parts,
                "metadata": {"io.rivo.a2a.v1": {
                    "request_kind": "delegation",
                    "skill_id": request.skill_id,
                }},
            },
        }),
    })
}

pub fn build_instance_control_jsonrpc_request(
    trust: &A2aTrustSnapshot,
    envelope: &LanControlEnvelope,
) -> Result<JsonRpcRequest, A2aError> {
    trust.validate_for_instance_control(envelope, now_unix_ms())?;
    Ok(JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: envelope.request_id.clone(),
        method: "instance/control".into(),
        params: serde_json::to_value(envelope).map_err(|_| A2aError::InvalidRequest)?,
    })
}

impl JsonRpcResponse {
    pub fn into_result(self) -> Result<Value, A2aError> {
        if self.jsonrpc != "2.0" || (self.result.is_some() == self.error.is_some()) {
            return Err(A2aError::InvalidJsonRpc);
        }
        if let Some(error) = self.error {
            return Err(A2aError::Remote {
                code: error.code,
                message: bounded_remote_message(&error.message),
            });
        }
        self.result.ok_or(A2aError::InvalidJsonRpc)
    }
}

#[async_trait]
pub trait A2aTransport: Send + Sync {
    async fn request(
        &self,
        trust: &A2aTrustSnapshot,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, A2aError>;

    /// 读取 `tasks/resubscribe` 的 SSE 响应。默认实现明确拒绝，避免不支持流的
    /// transport 被误当成可以安全恢复任务；真实 HTTP transport 必须覆盖它。
    async fn subscribe_sse(
        &self,
        _trust: &A2aTrustSnapshot,
        _task_id: &A2aTaskId,
    ) -> Result<Vec<SseEvent>, A2aError> {
        Err(A2aError::StreamingUnsupported)
    }
}

/// Client-side task lifecycle facade. It never retries SendMessage: after an
/// ambiguous transport failure the caller must query/subscribe using the durable task mapping,
/// rather than risk submitting the business request twice.
#[derive(Clone)]
pub struct A2aTaskClient {
    transport: std::sync::Arc<dyn A2aTransport>,
}

impl A2aTaskClient {
    pub fn new(transport: std::sync::Arc<dyn A2aTransport>) -> Self {
        Self { transport }
    }

    pub async fn get_task(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, A2aError> {
        self.call_task_method(trust, "tasks/get", task_id).await
    }

    /// 提交一条新委派。调用方只能在本机 policy 已完成数据外流确认后调用；该方法
    /// 绝不在 transport 失败后重试，未知结果必须走 `recover_task`。
    pub async fn send_delegation(
        &self,
        trust: &A2aTrustSnapshot,
        request: &A2aDelegationRequest,
    ) -> Result<A2aTask, A2aError> {
        let wire = build_delegation_jsonrpc_request(trust, request)?;
        let result = self.transport.request(trust, wire).await?.into_result()?;
        A2aTask::from_wire(result)
    }

    pub async fn cancel_task(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, A2aError> {
        self.call_task_method(trust, "tasks/cancel", task_id).await
    }

    /// Subscription creation is modeled as JSON-RPC setup. The HTTP transport's SSE reader
    /// remains separate so callers can persist the last revision before opening a stream.
    pub async fn subscribe_task(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, A2aError> {
        self.call_task_method(trust, "tasks/resubscribe", task_id)
            .await
    }

    /// 断线后的安全恢复路径：先取得完整 snapshot，再用同一已验证 transport 建立
    /// SSE `tasks/resubscribe`。任何读流失败都不会触发对原业务消息的重发。
    pub async fn recover_task(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<Vec<A2aTask>, A2aError> {
        let snapshot = self.get_task(trust, task_id).await?;
        if snapshot.state.terminal() {
            return Ok(vec![snapshot]);
        }
        let events = self.transport.subscribe_sse(trust, task_id).await?;
        self.merge_sse_updates(snapshot, &events)
    }

    /// 对 SSE payload 进行 task/revision 去重。调用方必须先持久化 `snapshot`，再将
    /// 每个接受的返回值写入 durable task mapping；缺少 revision 的对端只接受状态或
    /// Artifact 投影发生变化的更新，避免断线重连后无限重复同一事件。
    pub fn merge_sse_updates(
        &self,
        snapshot: A2aTask,
        events: &[SseEvent],
    ) -> Result<Vec<A2aTask>, A2aError> {
        let mut accepted = vec![snapshot.clone()];
        let mut current = snapshot;
        for event in events {
            if event.data.len() > MAX_OBSERVATION_BYTES {
                return Err(A2aError::ObservationTooLarge);
            }
            let next = A2aTask::from_wire(parse_unique_json(&event.data)?)?;
            if next.id != current.id {
                return Err(A2aError::InvalidTask);
            }
            let accept = match (current.revision, next.revision) {
                (Some(previous), Some(revision)) => revision > previous,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (None, None) => next.state != current.state || next.artifacts != current.artifacts,
            };
            if accept {
                current = next.clone();
                accepted.push(next);
            }
        }
        Ok(accepted)
    }

    async fn call_task_method(
        &self,
        trust: &A2aTrustSnapshot,
        method: &str,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, A2aError> {
        if task_id.0.trim().is_empty() || task_id.0.len() > 128 {
            return Err(A2aError::InvalidTask);
        }
        let id = sha256_hex(format!("{method}\0{}", task_id.0).as_bytes());
        let result = self
            .transport
            .request(
                trust,
                JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id,
                    method: method.into(),
                    params: serde_json::json!({"id": task_id.0}),
                },
            )
            .await?
            .into_result()?;
        A2aTask::from_wire(result)
    }
}

/// 默认 HTTPS JSON-RPC transport。mTLS identity 的选择和服务器 SPKI pin 由调用方在
/// `reqwest::Client` 创建时完成；本实现仍在每次请求前复核 trust snapshot。
#[derive(Clone)]
pub struct HttpsJsonRpcTransport {
    client: reqwest::Client,
}

/// 每次请求从 OS keychain 取得所选 peer 的 identity，并构建只绑定该 peer SPKI 的
/// mTLS client。identity_ref 仅由本机 storage 注入，模型与远端都不能影响它。
pub struct KeyringMtlsTransport {
    identities: Arc<dyn A2aIdentityStore>,
    identity_refs: BTreeMap<A2aPeerId, String>,
}

/// Explicit PSK transport. TLS still protects the network; the PSK proof binds
/// each request to the endpoint and Agent Card snapshot. The secret is held in
/// memory only and is never persisted by this type.
pub struct PskTransport {
    client: reqwest::Client,
    secret: PskSecret,
    session: std::sync::Mutex<Option<PskClientSession>>,
}

struct PskClientSession {
    endpoint: String,
    card_hash: String,
    client_nonce: [u8; 32],
    client_private_key: Option<EphemeralSecret>,
    client_public_key: Vec<u8>,
    bootstrap_key: Zeroizing<[u8; 32]>,
    session_key: Option<Zeroizing<[u8; 32]>>,
    created_at: Instant,
    established: bool,
}

type PskRequestHeaders = (Vec<(String, String)>, Zeroizing<[u8; 32]>, bool);

impl PskTransport {
    pub fn new(trust: &A2aTrustSnapshot, secret: PskSecret) -> Result<Self, A2aError> {
        if secret.iter().all(|byte| *byte == 0) {
            return Err(A2aError::PskInvalidSecret);
        }
        Ok(Self {
            client: build_pinned_tls_client(&trust.interface_url, &trust.spki_sha256)?,
            secret,
            session: std::sync::Mutex::new(None),
        })
    }

    fn proof_headers(
        &self,
        trust: &A2aTrustSnapshot,
        request_id: &str,
    ) -> Result<PskRequestHeaders, A2aError> {
        let mut session = self.session.lock().map_err(|_| A2aError::PskTransport)?;
        if session
            .as_ref()
            .is_some_and(|value| value.created_at.elapsed() > std::time::Duration::from_secs(30))
        {
            *session = None;
        }
        let state = session.get_or_insert_with(|| {
            let client_nonce = transcript_nonce();
            let client_private_key = EphemeralSecret::random(&mut rand_core::OsRng);
            let client_public_key = EncodedPoint::from(client_private_key.public_key())
                .to_bytes()
                .to_vec();
            let bootstrap_key = derive_request_key(
                &self.secret,
                &client_nonce,
                &client_public_key,
                &trust.interface_url,
                &trust.agent_card_jcs_sha256,
            );
            PskClientSession {
                endpoint: trust.interface_url.clone(),
                card_hash: trust.agent_card_jcs_sha256.clone(),
                client_nonce,
                client_private_key: Some(client_private_key),
                client_public_key,
                bootstrap_key,
                session_key: None,
                created_at: Instant::now(),
                established: false,
            }
        });
        if state.endpoint != trust.interface_url || state.card_hash != trust.agent_card_jcs_sha256 {
            *session = None;
            return Err(A2aError::PskTransport);
        }
        let bootstrap = !state.established;
        let request_key = if bootstrap {
            derive_session_request_key(&state.bootstrap_key, request_id)
        } else {
            derive_session_request_key(
                state.session_key.as_ref().ok_or(A2aError::PskTransport)?,
                request_id,
            )
        };
        let proof = psk_proof(
            &self.secret,
            "client",
            &trust.interface_url,
            &trust.agent_card_jcs_sha256,
            &state.client_nonce,
            &state.client_public_key,
        );
        let mut headers = vec![
            (
                "x-rivo-psk-nonce".into(),
                STANDARD.encode(state.client_nonce),
            ),
            (
                "x-rivo-psk-ephemeral".into(),
                STANDARD.encode(&state.client_public_key),
            ),
            ("x-rivo-psk-proof".into(), STANDARD.encode(proof)),
        ];
        if bootstrap {
            headers.push(("x-rivo-psk-bootstrap".into(), "1".into()));
        }
        Ok((headers, request_key, bootstrap))
    }

    fn accept_bootstrap_response(
        &self,
        trust: &A2aTrustSnapshot,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<(), A2aError> {
        let decode = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| STANDARD.decode(value).ok())
        };
        let server_nonce: [u8; 32] = decode("x-rivo-psk-server-nonce")
            .and_then(|value| value.try_into().ok())
            .ok_or(A2aError::PskTransport)?;
        let server_public_key = decode("x-rivo-psk-server-ephemeral")
            .filter(|value| value.len() == 65)
            .ok_or(A2aError::PskTransport)?;
        let server_public =
            PublicKey::from_sec1_bytes(&server_public_key).map_err(|_| A2aError::PskTransport)?;
        let mut session = self.session.lock().map_err(|_| A2aError::PskTransport)?;
        let state = session.as_mut().ok_or(A2aError::PskTransport)?;
        if state.established
            || state.endpoint != trust.interface_url
            || state.card_hash != trust.agent_card_jcs_sha256
        {
            return Err(A2aError::PskTransport);
        }
        let client_private_key = state
            .client_private_key
            .take()
            .ok_or(A2aError::PskTransport)?;
        let shared = client_private_key.diffie_hellman(&server_public);
        state.session_key = Some(derive_ecdh_session_key(
            &self.secret,
            shared.raw_secret_bytes().as_slice(),
            PskEcdhTranscript {
                client_nonce: &state.client_nonce,
                server_nonce: &server_nonce,
                client_public_key: &state.client_public_key,
                server_public_key: &server_public_key,
                endpoint: &trust.interface_url,
                card_hash: &trust.agent_card_jcs_sha256,
            },
        ));
        state.established = true;
        Ok(())
    }
}

#[async_trait]
impl A2aTransport for PskTransport {
    async fn request(
        &self,
        trust: &A2aTrustSnapshot,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, A2aError> {
        trust.validate_transport()?;
        let endpoint =
            reqwest::Url::parse(&trust.interface_url).map_err(|_| A2aError::InvalidEndpoint)?;
        let (headers, key, bootstrap) = self.proof_headers(trust, &request.id)?;
        let envelope = PskEnvelope::encode(
            &key,
            &serde_json::to_vec(&request).map_err(|_| A2aError::InvalidJson)?,
            &request.id,
        )
        .map_err(|_| A2aError::PskTransport)?;
        let mut builder = self
            .client
            .post(endpoint)
            .header("content-type", "application/vnd.rivo.a2a.psk+json")
            .header("accept", "application/vnd.rivo.a2a.psk+json")
            .header("x-rivo-psk-request-id", request.id.clone())
            .json(&envelope);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|_| A2aError::PskTransport)?;
        if !response.status().is_success() {
            let status = i64::from(response.status().as_u16());
            let body = response.text().await.unwrap_or_default();
            return Err(psk_remote_error(status, &body));
        }
        let response_proof = response
            .headers()
            .get("x-rivo-psk-server-proof")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| STANDARD.decode(value).ok())
            .ok_or(A2aError::PskTransport)?;
        if response_proof.as_slice() != server_response_proof(&key, &request.id) {
            return Err(A2aError::PskTransport);
        }
        if bootstrap {
            self.accept_bootstrap_response(trust, response.headers())?;
        }
        let body = response.text().await.map_err(|_| A2aError::PskTransport)?;
        let envelope: PskEnvelope =
            serde_json::from_str(&body).map_err(|_| A2aError::PskTransport)?;
        let plain = envelope.decode(&key).map_err(|_| A2aError::PskTransport)?;
        parse_json_rpc_response(std::str::from_utf8(&plain).map_err(|_| A2aError::PskTransport)?)
    }

    async fn subscribe_sse(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<Vec<SseEvent>, A2aError> {
        trust.validate_transport()?;
        if task_id.0.trim().is_empty() || task_id.0.len() > 128 {
            return Err(A2aError::InvalidTask);
        }
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            // A recovery subscription is safe to repeat because it only reads an
            // existing task, but its wire request id must remain unique within a
            // PSK session. Reusing a deterministic id would be indistinguishable
            // from an envelope replay to the server-side replay guard.
            id: psk_resubscribe_request_id(task_id),
            method: "tasks/resubscribe".into(),
            params: serde_json::json!({"id": task_id.0}),
        };
        let (headers, key, bootstrap) = self.proof_headers(trust, &request.id)?;
        let envelope = PskEnvelope::encode(
            &key,
            &serde_json::to_vec(&request).map_err(|_| A2aError::InvalidJson)?,
            &request.id,
        )
        .map_err(|_| A2aError::PskTransport)?;
        let mut builder = self
            .client
            .post(&trust.interface_url)
            .header("content-type", "application/vnd.rivo.a2a.psk+json")
            .header("accept", "application/vnd.rivo.a2a.psk+sse")
            .header("x-rivo-psk-request-id", request.id.clone())
            .json(&envelope);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|_| A2aError::PskTransport)?;
        if !response.status().is_success() {
            let status = i64::from(response.status().as_u16());
            let body = response.text().await.unwrap_or_default();
            return Err(psk_remote_error(status, &body));
        }
        let response_proof = response
            .headers()
            .get("x-rivo-psk-server-proof")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| STANDARD.decode(value).ok())
            .ok_or(A2aError::InvalidSse)?;
        if response_proof.as_slice() != server_response_proof(&key, &request.id) {
            return Err(A2aError::InvalidSse);
        }
        if bootstrap {
            self.accept_bootstrap_response(trust, response.headers())?;
        }
        let body = response.bytes().await.map_err(|_| A2aError::PskTransport)?;
        let envelope: PskEnvelope =
            serde_json::from_slice(&body).map_err(|_| A2aError::PskTransport)?;
        let plain = envelope.decode(&key).map_err(|_| A2aError::PskTransport)?;
        parse_sse_events(std::str::from_utf8(&plain).map_err(|_| A2aError::PskTransport)?)
    }
}

fn psk_resubscribe_request_id(task_id: &A2aTaskId) -> String {
    let nonce = transcript_nonce();
    sha256_hex(format!("tasks/resubscribe\0{}\0{}", task_id.0, hex::encode(nonce)).as_bytes())
}

fn psk_remote_error(status: i64, body: &str) -> A2aError {
    let error = serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    match error.as_deref() {
        Some("a2a_peer_not_trusted") => A2aError::PeerNotTrusted,
        Some("a2a_psk_session_required") => A2aError::PskSessionRequired,
        Some("a2a_psk_replay") => A2aError::PskReplay,
        Some("a2a_psk_invalid") | Some("a2a_psk_required") => A2aError::PskTransport,
        _ => A2aError::Remote {
            code: status,
            message: bounded_remote_message(body),
        },
    }
}

impl KeyringMtlsTransport {
    pub fn new(
        identities: Arc<dyn A2aIdentityStore>,
        identity_refs: BTreeMap<A2aPeerId, String>,
    ) -> Self {
        Self {
            identities,
            identity_refs,
        }
    }

    async fn ensure_card_matches_snapshot(trust: &A2aTrustSnapshot) -> Result<(), A2aError> {
        // 每次外发前重新读取同一 SPKI 下的 Card。对端不能借 Card 更换 endpoint：
        // fetch_agent_card 已固定起始 interface，reconcile 的任一变更都会降级为
        // suspended，从而使本次及后续请求 fail closed。持久 store 的调用方仍应将
        // suspended 投影写回，避免在离线诊断时把它显示成可用 peer。
        let card = fetch_agent_card(&trust.interface_url, &trust.spki_sha256).await?;
        let reconciled = trust.reconcile(
            &card.url,
            "JSONRPC",
            trust.tenant.clone(),
            &trust.spki_sha256,
            &card.id,
            &card.canonical_sha256,
            card.skills,
            SPEC_REVISION,
            WIRE_PROTOCOL_VERSION,
        );
        if reconciled.status != A2aTrustState::Trusted {
            return Err(A2aError::PeerNotTrusted);
        }
        Ok(())
    }
}

#[async_trait]
impl A2aTransport for KeyringMtlsTransport {
    async fn request(
        &self,
        trust: &A2aTrustSnapshot,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, A2aError> {
        Self::ensure_card_matches_snapshot(trust).await?;
        let identity_ref = self
            .identity_refs
            .get(&trust.peer_id)
            .ok_or(A2aError::PeerNotTrusted)?;
        HttpsJsonRpcTransport::new(build_pinned_mtls_client(
            trust,
            self.identities.as_ref(),
            identity_ref,
        )?)
        .request(trust, request)
        .await
    }

    async fn subscribe_sse(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<Vec<SseEvent>, A2aError> {
        Self::ensure_card_matches_snapshot(trust).await?;
        let identity_ref = self
            .identity_refs
            .get(&trust.peer_id)
            .ok_or(A2aError::PeerNotTrusted)?;
        HttpsJsonRpcTransport::new(build_pinned_mtls_client(
            trust,
            self.identities.as_ref(),
            identity_ref,
        )?)
        .subscribe_sse(trust, task_id)
        .await
    }
}

/// 由配对快照构造的 TLS verifier。它以本机确认的 SPKI pin 作为唯一信任锚，
/// 并额外校验证书有效期与 interface IP SAN；它不会接受系统/公网 CA 的替代证书。
/// TLS CertificateVerify 仍使用 rustls 支持的算法逐一验证，不能以 pin 检查代替
/// 握手签名验证。
#[derive(Debug)]
struct PinnedServerVerifier {
    expected_spki_sha256: [u8; 32],
    expected_ip: IpAddr,
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    fn from_trust(trust: &A2aTrustSnapshot) -> Result<Self, A2aError> {
        trust.validate_transport()?;
        let expected_spki_sha256 = hex::decode(&trust.spki_sha256)
            .map_err(|_| A2aError::InvalidTrustSnapshot)?
            .try_into()
            .map_err(|_| A2aError::InvalidTrustSnapshot)?;
        let expected_ip = interface_ip(&trust.interface_url)?;
        Ok(Self {
            expected_spki_sha256,
            expected_ip,
            supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        })
    }

    fn verify_pinned_certificate(
        &self,
        certificate_der: &[u8],
        now_unix: u64,
    ) -> Result<(), A2aError> {
        let (_, certificate) =
            parse_x509_certificate(certificate_der).map_err(|_| A2aError::CertificateRejected)?;
        let now = i64::try_from(now_unix)
            .ok()
            .and_then(|seconds| ASN1Time::from_timestamp(seconds).ok())
            .ok_or(A2aError::CertificateRejected)?;
        if !certificate.validity().is_valid_at(now)
            || Sha256::digest(certificate.public_key().raw).as_slice()
                != self.expected_spki_sha256.as_slice()
        {
            return Err(A2aError::CertificateRejected);
        }
        let expected = match self.expected_ip {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let has_expected_ip_san = certificate
            .subject_alternative_name()
            .map_err(|_| A2aError::CertificateRejected)?
            .is_some_and(|extension| {
                extension.value.general_names.iter().any(|name| {
                    matches!(name, GeneralName::IPAddress(value) if *value == expected.as_slice())
                })
            });
        if !has_expected_ip_san {
            return Err(A2aError::CertificateRejected);
        }
        Ok(())
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.verify_pinned_certificate(end_entity.as_ref(), now.as_secs())
            .map_err(|_| {
                rustls::Error::General(
                    "A2A peer certificate did not match its local trust snapshot".into(),
                )
            })?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_handshake_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_handshake_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

impl PinnedServerVerifier {
    fn verify_handshake_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = self
            .supported
            .mapping
            .iter()
            .find(|(scheme, _)| *scheme == dss.scheme)
            .map(|(_, algorithms)| *algorithms)
            .ok_or_else(|| {
                rustls::Error::General("A2A server used an unsupported TLS signature scheme".into())
            })?;
        let certificate = webpki::EndEntityCert::try_from(cert)
            .map_err(|_| rustls::Error::General("A2A server certificate is malformed".into()))?;
        if algorithms.iter().any(|algorithm| {
            certificate
                .verify_signature(*algorithm, message, dss.signature())
                .is_ok()
        }) {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        } else {
            Err(rustls::Error::General(
                "A2A server TLS handshake signature is invalid".into(),
            ))
        }
    }
}

/// 从本机 keychain 的 mTLS identity 构造只连接一个已配对 peer 的 HTTP client。
/// 证书与私钥只在建连瞬间从受保护存储读入进程内存，绝不写入 SQLite、配置或事件。
pub fn build_pinned_mtls_client(
    trust: &A2aTrustSnapshot,
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
) -> Result<reqwest::Client, A2aError> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

    let verifier = PinnedServerVerifier::from_trust(trust)?;
    let certificate_pem = identities.load_secret(identity_ref, "client-certificate")?;
    let private_key_pem = identities.load_secret(identity_ref, "client-p256-key")?;
    let certificates = CertificateDer::pem_slice_iter(certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| A2aError::IdentityStore)?;
    if certificates.is_empty() {
        return Err(A2aError::IdentityStore);
    }
    let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
        .map_err(|_| A2aError::IdentityStore)?;
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(certificates, private_key)
        .map_err(|_| A2aError::IdentityStore)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        // 配对、Card、JSON-RPC 与 SSE 都是固定的信任快照 URL；禁止 HTTP 层将
        // 请求重定向到另一条路径或主机，避免 pin 验证被“成功请求”掩盖。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| A2aError::Transport)
}

/// 配对 bootstrap 的 TLS client。SPKI pin 必须来自受限 mDNS candidate 或显式
/// 本机输入，且调用方仍须对 pairing response 的 SAS 做本机确认。
pub fn build_pinned_tls_client(
    interface_url: &str,
    expected_server_spki_sha256: &str,
) -> Result<reqwest::Client, A2aError> {
    if !is_private_a2a_interface_url(interface_url)
        || !is_lower_hex_sha256(expected_server_spki_sha256)
    {
        return Err(A2aError::InvalidEndpoint);
    }
    let expected_spki_sha256 = hex::decode(expected_server_spki_sha256)
        .map_err(|_| A2aError::InvalidTrustSnapshot)?
        .try_into()
        .map_err(|_| A2aError::InvalidTrustSnapshot)?;
    let verifier = PinnedServerVerifier {
        expected_spki_sha256,
        expected_ip: interface_ip(interface_url)?,
        supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| A2aError::Transport)
}

/// 为 Step 026 等非 A2A 私网控制面复用同一 SPKI/IP-SAN/TLS-signature 信任锚。
/// 它刻意要求固定私网 IP 与显式端口，但不接受 A2A 的 `/a2a` 路径限制。
pub fn build_pinned_tls_client_for_private_endpoint(
    endpoint: &str,
    expected_server_spki_sha256: &str,
) -> Result<reqwest::Client, A2aError> {
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| A2aError::InvalidEndpoint)?;
    let host = parsed.host_str().ok_or(A2aError::InvalidEndpoint)?;
    let ip: IpAddr = host.parse().map_err(|_| A2aError::InvalidEndpoint)?;
    if parsed.scheme() != "https"
        || parsed.port().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !is_allowed_client_ip(ip)
        || !is_lower_hex_sha256(expected_server_spki_sha256)
    {
        return Err(A2aError::InvalidEndpoint);
    }
    let expected_spki_sha256 = hex::decode(expected_server_spki_sha256)
        .map_err(|_| A2aError::InvalidTrustSnapshot)?
        .try_into()
        .map_err(|_| A2aError::InvalidTrustSnapshot)?;
    let verifier = PinnedServerVerifier {
        expected_spki_sha256,
        expected_ip: ip,
        supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| A2aError::Transport)
}

/// 首次 enrollment 的 bootstrap mTLS client。它以 keychain 内与 CSR 相同的私钥
/// 生成一次性 self-signed leaf，以便 8443 可以在 TLS handshake 层要求 client
/// certificate。服务端仅在注册码成功且该 leaf 的公钥与 CSR 相同才签发受管证书；
/// 该 bootstrap leaf 不会被保存、授权或用于后续 Agent API。
pub fn build_pinned_bootstrap_mtls_client_for_private_endpoint(
    endpoint: &str,
    expected_server_spki_sha256: &str,
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
) -> Result<reqwest::Client, A2aError> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| A2aError::InvalidEndpoint)?;
    let ip: IpAddr = parsed
        .host_str()
        .ok_or(A2aError::InvalidEndpoint)?
        .parse()
        .map_err(|_| A2aError::InvalidEndpoint)?;
    if parsed.scheme() != "https"
        || parsed.port().is_none()
        || !is_allowed_client_ip(ip)
        || !is_lower_hex_sha256(expected_server_spki_sha256)
    {
        return Err(A2aError::InvalidEndpoint);
    }
    let expected_spki_sha256 = hex::decode(expected_server_spki_sha256)
        .map_err(|_| A2aError::InvalidTrustSnapshot)?
        .try_into()
        .map_err(|_| A2aError::InvalidTrustSnapshot)?;
    let verifier = PinnedServerVerifier {
        expected_spki_sha256,
        expected_ip: ip,
        supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    let private_key_pem = identities.load_secret(identity_ref, "client-p256-key")?;
    let key = rcgen::KeyPair::from_pem(&private_key_pem).map_err(|_| A2aError::IdentityStore)?;
    let params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|_| A2aError::IdentityStore)?;
    let bootstrap_certificate = params
        .self_signed(&key)
        .map_err(|_| A2aError::IdentityStore)?
        .pem();
    let certificate = CertificateDer::from_pem_slice(bootstrap_certificate.as_bytes())
        .map_err(|_| A2aError::IdentityStore)?;
    let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
        .map_err(|_| A2aError::IdentityStore)?;
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![certificate], private_key)
        .map_err(|_| A2aError::IdentityStore)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| A2aError::Transport)
}

/// 受管 Agent 的 mTLS 连接：私钥/证书只从 OS keychain 临时读入，endpoint 仍需 SPKI pin。
pub fn build_pinned_mtls_client_for_private_endpoint(
    endpoint: &str,
    expected_server_spki_sha256: &str,
    identities: &dyn A2aIdentityStore,
    identity_ref: &str,
) -> Result<reqwest::Client, A2aError> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| A2aError::InvalidEndpoint)?;
    let ip: IpAddr = parsed
        .host_str()
        .ok_or(A2aError::InvalidEndpoint)?
        .parse()
        .map_err(|_| A2aError::InvalidEndpoint)?;
    if parsed.scheme() != "https"
        || parsed.port().is_none()
        || !is_allowed_client_ip(ip)
        || !is_lower_hex_sha256(expected_server_spki_sha256)
    {
        return Err(A2aError::InvalidEndpoint);
    }
    let expected_spki_sha256 = hex::decode(expected_server_spki_sha256)
        .map_err(|_| A2aError::InvalidTrustSnapshot)?
        .try_into()
        .map_err(|_| A2aError::InvalidTrustSnapshot)?;
    let verifier = PinnedServerVerifier {
        expected_spki_sha256,
        expected_ip: ip,
        supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    let certificate_pem = identities.load_secret(identity_ref, "client-certificate")?;
    let certificates = CertificateDer::pem_slice_iter(certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| A2aError::IdentityStore)?;
    let private_key_pem = identities.load_secret(identity_ref, "client-p256-key")?;
    let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
        .map_err(|_| A2aError::IdentityStore)?;
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(certificates, private_key)
        .map_err(|_| A2aError::IdentityStore)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| A2aError::Transport)
}

impl HttpsJsonRpcTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl A2aTransport for HttpsJsonRpcTransport {
    async fn request(
        &self,
        trust: &A2aTrustSnapshot,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, A2aError> {
        trust.validate_transport()?;
        let endpoint = if request.method == "instance/control" {
            let mut url =
                reqwest::Url::parse(&trust.interface_url).map_err(|_| A2aError::InvalidEndpoint)?;
            url.set_path("/rivo-a2a-lan-v1/instance-control");
            url
        } else {
            reqwest::Url::parse(&trust.interface_url).map_err(|_| A2aError::InvalidEndpoint)?
        };
        let response = self
            .client
            // endpoint 已是完整的 `/a2a` interface URL，禁止二次拼接。
            .post(endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|_| A2aError::Transport)?;
        if !response.status().is_success() {
            return Err(A2aError::Transport);
        }
        let body = response.text().await.map_err(|_| A2aError::Transport)?;
        parse_json_rpc_response(&body)
    }

    async fn subscribe_sse(
        &self,
        trust: &A2aTrustSnapshot,
        task_id: &A2aTaskId,
    ) -> Result<Vec<SseEvent>, A2aError> {
        trust.validate_transport()?;
        if task_id.0.trim().is_empty() || task_id.0.len() > 128 {
            return Err(A2aError::InvalidTask);
        }
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: sha256_hex(format!("tasks/resubscribe\\0{}", task_id.0).as_bytes()),
            method: "tasks/resubscribe".into(),
            params: serde_json::json!({"id": task_id.0}),
        };
        let response = self
            .client
            .post(&trust.interface_url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&request)
            .send()
            .await
            .map_err(|_| A2aError::Transport)?;
        if !response.status().is_success()
            || !response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(A2aError::InvalidSse);
        }
        collect_bounded_sse(response).await
    }
}

/// 单个 A2A task 最长 30 分钟、每条观察 256 KiB。网络层仍需要一个总上限，
/// 以免恶意对端以无限多有效帧耗尽内存；该限制允许至多 32 个最大观察并且不改变
/// 每帧的协议限制。消费方会在持久化每个 accepted revision 后继续处理。
const MAX_SSE_RESPONSE_BYTES: usize = MAX_OBSERVATION_BYTES * 32;

async fn collect_bounded_sse(response: reqwest::Response) -> Result<Vec<SseEvent>, A2aError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SSE_RESPONSE_BYTES as u64)
    {
        return Err(A2aError::ObservationTooLarge);
    }
    use futures::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| A2aError::Transport)?;
        if body.len().saturating_add(chunk.len()) > MAX_SSE_RESPONSE_BYTES {
            return Err(A2aError::ObservationTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let body = std::str::from_utf8(&body).map_err(|_| A2aError::InvalidSse)?;
    let events = parse_sse_events(body)?;
    if events.is_empty()
        || events
            .iter()
            .any(|event| event.data.len() > MAX_OBSERVATION_BYTES)
    {
        return Err(A2aError::InvalidSse);
    }
    Ok(events)
}

fn parse_json_rpc_response(body: &str) -> Result<JsonRpcResponse, A2aError> {
    if body.len() > MAX_OBSERVATION_BYTES {
        return Err(A2aError::ObservationTooLarge);
    }
    serde_json::from_value(parse_unique_json(body)?).map_err(|_| A2aError::InvalidJsonRpc)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum A2aError {
    #[error("a2a_peer_not_trusted")]
    PeerNotTrusted,
    #[error("a2a_skill_not_granted")]
    SkillNotGranted,
    #[error("a2a_protocol_version_incompatible")]
    ProtocolVersion,
    #[error("a2a_invalid_endpoint")]
    InvalidEndpoint,
    #[error("a2a_invalid_discovery")]
    InvalidDiscovery,
    #[error("a2a_discovery_unavailable")]
    DiscoveryUnavailable,
    #[error("a2a_invalid_trust_snapshot")]
    InvalidTrustSnapshot,
    #[error("a2a_invalid_agent_card")]
    InvalidAgentCard,
    #[error("a2a_invalid_task")]
    InvalidTask,
    #[error("a2a_message_too_large")]
    MessageTooLarge,
    #[error("a2a_observation_too_large")]
    ObservationTooLarge,
    #[error("a2a_invalid_artifact")]
    InvalidArtifact,
    #[error("a2a_sensitive_data_forbidden")]
    SensitiveDataForbidden,
    #[error("a2a_invalid_request")]
    InvalidRequest,
    #[error("a2a_invalid_json")]
    InvalidJson,
    #[error("a2a_protocol_fixture_invalid")]
    ProtocolFixtureInvalid,
    #[error("a2a_invalid_claim_proof")]
    InvalidClaimProof,
    #[error("a2a_invalid_pairing")]
    InvalidPairing,
    #[error("a2a_invalid_identity_ref")]
    InvalidIdentityRef,
    #[error("a2a_identity_store_failure")]
    IdentityStore,
    #[error("a2a_invalid_sse")]
    InvalidSse,
    #[error("a2a_streaming_unsupported")]
    StreamingUnsupported,
    #[error("a2a_psk_invalid_secret")]
    PskInvalidSecret,
    #[error("a2a_psk_transport")]
    PskTransport,
    #[error("a2a_psk_session_required")]
    PskSessionRequired,
    #[error("a2a_psk_replay")]
    PskReplay,
    #[error("a2a_invalid_jsonrpc")]
    InvalidJsonRpc,
    #[error("a2a_peer_certificate_rejected")]
    CertificateRejected,
    #[error("a2a_transport_failure")]
    Transport,
    #[error("a2a_remote_error:{code}")]
    Remote { code: i64, message: String },
}

impl Display for A2aPeerId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn sha256_hex(input: &[u8]) -> String {
    hex::encode(Sha256::digest(input))
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn is_artifact_name(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// 冻结 pairing wire 的最小 RFC3339 UTC 检查。实际到期比较由网络配对 adapter
/// 使用可信本机时钟完成；这里拒绝空值、时区缺失和控制字符。
fn is_rfc3339_utc(value: &str) -> bool {
    value.len() >= 20
        && value.len() <= 64
        && value.ends_with('Z')
        && value.as_bytes().get(10) == Some(&b'T')
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// A2A LAN v1 只允许明确的 RFC1918 IPv4 或 ULA IPv6 interface URL。验证必须在
/// DNS / HTTP 前完成，避免受污染的 Card 将 Client 导向 loopback、公网或意外路径。
fn is_private_a2a_interface_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/a2a"
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // URL host_str may preserve IPv6 authority brackets; brackets are syntax rather than
    // part of the address itself.
    let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    else {
        return false;
    };
    is_allowed_client_ip(ip)
}

fn interface_ip(value: &str) -> Result<IpAddr, A2aError> {
    let url = reqwest::Url::parse(value).map_err(|_| A2aError::InvalidEndpoint)?;
    let host = url.host_str().ok_or(A2aError::InvalidEndpoint)?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .map_err(|_| A2aError::InvalidEndpoint)?;
    is_allowed_client_ip(ip)
        .then_some(ip)
        .ok_or(A2aError::InvalidEndpoint)
}

fn is_allowed_client_ip(ip: IpAddr) -> bool {
    (std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1") && ip.is_loopback())
        || is_private_ip(ip)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
        }
        IpAddr::V6(ip) => (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn bounded_remote_message(value: &str) -> String {
    value.chars().take(256).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn trusted() -> A2aTrustSnapshot {
        A2aTrustSnapshot {
            peer_id: A2aPeerId("peer-1".into()),
            interface_url: "https://10.0.0.3:8443/a2a".into(),
            protocol_binding: "JSONRPC".into(),
            tenant: None,
            spki_sha256: "a".repeat(64),
            agent_card_id: "card-1".into(),
            agent_card_jcs_sha256: "b".repeat(64),
            allowed_skills: ["research".into()].into_iter().collect(),
            spec_revision: SPEC_REVISION.into(),
            wire_protocol_version: WIRE_PROTOCOL_VERSION.into(),
            status: A2aTrustState::Trusted,
            approved_at: "2026-07-31T00:00:00Z".into(),
        }
    }

    #[test]
    fn card_is_hashed_canonically_and_requires_https() {
        let card = AgentCard::parse(&json!({
            "url": "https://10.0.0.3:8443/a2a",
            "skills": [{"id":"research"}],
            "version": "1.0.0"
        }))
        .unwrap();
        assert_eq!(card.skills, ["research".into()].into_iter().collect());
        assert_eq!(card.canonical_sha256.len(), 64);
        assert!(
            AgentCard::parse(&json!({"url":"http://x", "skills":[], "version":"1.0.0"})).is_err()
        );
        assert!(parse_unique_json(r#"{"outer":{"id":"a","id":"b"}}"#).is_err());
        assert!(AgentCard::parse_json(
            r#"{"url":"https://10.0.0.3:8443/a2a","skills":[{"id":"research"}],"version":"1.0.0"}"#
        )
        .is_ok());
        let fixture = include_str!("../protocol/v1.0.1/fixtures/agent-card.json");
        let fixture_card = AgentCard::parse_json(fixture).unwrap();
        assert_eq!(fixture_card.url, "https://10.0.0.12:8443/a2a");
        assert_eq!(fixture_card.service_version, "1.0.0");
        assert_eq!(fixture_card.protocol_version, WIRE_PROTOCOL_VERSION);
        let standard = AgentCard::parse(&json!({
            "version":"1.0.0",
            "supportedInterfaces":[{
                "url":"https://10.0.0.12:8443/a2a",
                "protocolBinding":"JSONRPC",
                "protocolVersion":"1.0"
            }],
            "skills":[{"id":"research"}]
        }))
        .unwrap();
        assert_eq!(standard.url, "https://10.0.0.12:8443/a2a");
    }

    #[test]
    fn trust_changes_suspend_before_delegate() {
        let peer = trusted();
        assert!(peer.validate_for_delegate("research").is_ok());
        let changed = peer.reconcile(
            "https://10.0.0.3:8443/a2a",
            "JSONRPC",
            None,
            &"c".repeat(64),
            &peer.agent_card_id,
            &peer.agent_card_jcs_sha256,
            peer.allowed_skills.clone(),
            SPEC_REVISION,
            WIRE_PROTOCOL_VERSION,
        );
        assert_eq!(changed.status, A2aTrustState::Suspended);
        assert_eq!(
            changed.validate_for_delegate("research"),
            Err(A2aError::PeerNotTrusted)
        );
    }

    #[test]
    fn instance_control_requires_explicit_process_skill_and_peer_binding() {
        let mut peer = trusted();
        peer.allowed_skills.insert("process-control".into());
        let envelope = rivo_instance_control::LanControlEnvelope {
            peer_id: "peer-1".into(),
            target_instance_id: "instance-1".into(),
            method: "thread/read".into(),
            params: serde_json::json!({"thread_ref":"thread-1"}),
            request_id: "request-1".into(),
            nonce: "1234567890123456".into(),
            expires_at_unix_ms: 1_000,
            control: rivo_instance_control::ControlRequest {
                scope: rivo_instance_control::ControlScope::Lan,
                confirmation: true,
                idempotency_key: "1234567890123456".into(),
            },
        };
        assert!(peer.validate_for_instance_control(&envelope, 900).is_ok());
        let mut wrong_peer = envelope.clone();
        wrong_peer.peer_id = "peer-2".into();
        assert_eq!(
            peer.validate_for_instance_control(&wrong_peer, 900),
            Err(A2aError::SkillNotGranted)
        );
    }

    #[test]
    fn interface_urls_are_exact_private_ip_addresses() {
        let mut snapshot = trusted();
        for rejected in [
            "https://10.0.0.3/a2a",
            "https://10.0.0.3:8443/a2a/",
            "https://localhost:8443/a2a",
            "https://127.0.0.1:8443/a2a",
            "https://8.8.8.8:8443/a2a",
            "https://10.0.0.3:8443/a2a?next=x",
            "https://user@10.0.0.3:8443/a2a",
        ] {
            snapshot.interface_url = rejected.into();
            assert_eq!(
                snapshot.validate_transport(),
                Err(A2aError::InvalidEndpoint)
            );
        }
        snapshot.interface_url = "https://[fd12:3456::1]:8443/a2a".into();
        assert!(snapshot.validate_transport().is_ok());
    }

    #[test]
    fn psk_binding_is_explicitly_supported_without_relaxing_other_bindings() {
        let mut snapshot = trusted();
        snapshot.protocol_binding = "PSK".into();
        assert!(snapshot.validate_transport().is_ok());
        snapshot.protocol_binding = "HTTP".into();
        assert_eq!(
            snapshot.validate_transport(),
            Err(A2aError::ProtocolVersion)
        );
    }

    #[test]
    fn mdns_advertisements_only_create_expiring_untrusted_candidates() {
        let advertisement = MdnsAdvertisement {
            service_name: MDNS_SERVICE_NAME.into(),
            candidate_id: "candidate-01".into(),
            instance_name: "research-lan".into(),
            address: "10.0.0.12".parse().unwrap(),
            port: 8443,
            ttl_seconds: 60,
            observed_unix: 1_000,
            txt: vec![
                ("v".into(), WIRE_PROTOCOL_VERSION.into()),
                ("card_path".into(), A2A_CARD_PATH.into()),
                ("spki_sha256".into(), "c".repeat(64)),
                ("instance_nonce".into(), "nonce-01".into()),
                ("expires_unix".into(), "1060".into()),
            ],
        };
        let candidate = advertisement.clone().into_candidate().unwrap();
        assert_eq!(candidate.interface_url, "https://10.0.0.12:8443/a2a");
        assert_eq!(
            candidate.card_url,
            "https://10.0.0.12:8443/.well-known/agent-card.json"
        );
        assert_eq!(candidate.expires_unix, 1_060);
        assert!(!candidate.is_expired_at(1_059));
        assert!(candidate.is_expired_at(1_060));
        let mut duplicate_txt = advertisement;
        duplicate_txt
            .txt
            .push(("spki_sha256".into(), "d".repeat(64)));
        assert_eq!(
            duplicate_txt.into_candidate(),
            Err(A2aError::InvalidDiscovery)
        );
    }

    #[test]
    fn mdns_advertisements_reject_public_dns_and_bad_expiry() {
        for address in ["8.8.8.8", "127.0.0.1"] {
            let advertisement = MdnsAdvertisement {
                service_name: MDNS_SERVICE_NAME.into(),
                candidate_id: "candidate-02".into(),
                instance_name: "research-lan".into(),
                address: address.parse().unwrap(),
                port: 8443,
                ttl_seconds: 60,
                observed_unix: 1_000,
                txt: vec![
                    ("v".into(), WIRE_PROTOCOL_VERSION.into()),
                    ("card_path".into(), A2A_CARD_PATH.into()),
                    ("spki_sha256".into(), "c".repeat(64)),
                    ("instance_nonce".into(), "nonce-02".into()),
                    ("expires_unix".into(), "1_061".into()),
                ],
            };
            assert_eq!(
                advertisement.into_candidate(),
                Err(A2aError::InvalidDiscovery)
            );
        }
    }

    #[test]
    fn mdns_discovery_window_is_bounded_before_opening_network_resources() {
        assert_eq!(
            discover_lan_candidates(Duration::ZERO),
            Err(A2aError::InvalidDiscovery)
        );
        assert_eq!(
            discover_lan_candidates(Duration::from_secs(MAX_MDNS_TTL_SECONDS + 1)),
            Err(A2aError::InvalidDiscovery)
        );
    }

    #[test]
    fn delegation_hard_rejects_sensitive_data_and_binds_hashes() {
        let request = A2aDelegationRequest {
            peer_id: A2aPeerId("peer-1".into()),
            skill_id: "research".into(),
            message: "find sources".into(),
            stream: true,
            artifacts: vec![A2aArtifactInput {
                name: Some("brief".into()),
                kind: A2aArtifactKind::Json,
                content: r#"{"topic":"a2a"}"#.into(),
                classification: DataClassification::UserProvided,
            }],
        };
        let binding = request.confirmation_binding().unwrap();
        assert_eq!(binding.artifact_sha256.len(), 1);
        let wire = build_delegation_jsonrpc_request(&trusted(), &request).unwrap();
        assert_eq!(wire.method, "message/stream");
        assert_eq!(
            wire.params["message"]["metadata"]["io.rivo.a2a.v1"]["skill_id"],
            "research"
        );
        assert_eq!(
            wire.params["message"]["parts"][1]["data"],
            json!({"topic":"a2a"})
        );
        let mut sensitive = request;
        sensitive.artifacts[0].classification = DataClassification::Credential;
        assert_eq!(sensitive.validate(), Err(A2aError::SensitiveDataForbidden));
        let mut non_canonical_json = sensitive;
        non_canonical_json.artifacts[0].classification = DataClassification::UserProvided;
        non_canonical_json.artifacts[0].content = r#"{"z":1,"a":2}"#.into();
        assert_eq!(
            non_canonical_json.validate(),
            Err(A2aError::InvalidArtifact)
        );
    }

    #[test]
    fn sas_and_claim_proof_are_deterministic_and_strict() {
        let server = [1_u8; 32];
        let client = [2_u8; 32];
        let first = derive_sas_words("req-1", &server, &client).unwrap();
        assert_eq!(first, derive_sas_words("req-1", &server, &client).unwrap());
        assert_eq!(first.len(), 6);
        let key = SigningKey::from_bytes((&[9_u8; 32]).into()).unwrap();
        let token = [4_u8; 32];
        let proof = sign_claim_proof(&key, "req-1", &token).unwrap();
        verify_claim_proof(key.verifying_key(), "req-1", &token, &proof).unwrap();
        assert_eq!(
            verify_claim_proof(key.verifying_key(), "req-2", &token, &proof),
            Err(A2aError::InvalidClaimProof)
        );
    }

    #[test]
    fn frozen_sas_and_claim_proof_vectors_match_the_protocol_inputs() {
        let server: [u8; 32] =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap()
                .try_into()
                .unwrap();
        let client: [u8; 32] =
            hex::decode("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            derive_sas_words("pair-01HVALID", &server, &client).unwrap(),
            ["rural", "oyster", "blast", "auto", "weird", "brand"].map(str::to_owned)
        );

        use p256::pkcs8::DecodePublicKey;
        let claim_fixture: Value = serde_json::from_str(include_str!(
            "../protocol/v1.0.1/fixtures/claim-proof-vector.json"
        ))
        .unwrap();
        let public_der = URL_SAFE_NO_PAD
            .decode(
                claim_fixture["csr_p256_spki_der_base64url"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        let key = VerifyingKey::from_public_key_der(&public_der).unwrap();
        let token: [u8; 32] = URL_SAFE_NO_PAD
            .decode(claim_fixture["claim_token"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        verify_claim_proof(
            &key,
            "pair-01HCLAIM",
            &token,
            claim_fixture["claim_proof"].as_str().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sse_and_jsonrpc_reject_ambiguous_or_invalid_input() {
        let events =
            parse_sse_events("id: 1\nevent: task\ndata: {\"id\":\"t1\"}\n\ndata: done\n").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id.as_deref(), Some("1"));
        assert_eq!(
            parse_sse_events("unknown: field\n"),
            Err(A2aError::InvalidSse)
        );
        assert_eq!(
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: json!(1),
                result: Some(json!({})),
                error: Some(JsonRpcError {
                    code: -1,
                    message: "bad".into(),
                    data: None
                })
            }
            .into_result(),
            Err(A2aError::InvalidJsonRpc)
        );
        assert!(
            parse_json_rpc_response(r#"{"jsonrpc":"2.0","id":"a","result":{},"result":{}}"#)
                .is_err()
        );
        assert_eq!(
            parse_json_rpc_response(&"x".repeat(MAX_OBSERVATION_BYTES + 1)),
            Err(A2aError::ObservationTooLarge)
        );
    }

    #[test]
    fn identity_references_are_non_secret_and_strictly_bounded() {
        assert!(KeyringA2aIdentityStore::validate_identity_ref("a2a-peer_01.test").is_ok());
        for invalid in ["", "peer/ref", "peer ref", &"a".repeat(129)] {
            assert!(KeyringA2aIdentityStore::validate_identity_ref(invalid).is_err());
        }
    }

    #[derive(Default)]
    struct MemoryIdentityStore(Mutex<BTreeMap<(String, String), String>>);

    impl A2aIdentityStore for MemoryIdentityStore {
        fn delete(&self, identity_ref: &str) -> Result<(), A2aError> {
            self.0
                .lock()
                .unwrap()
                .retain(|(stored_ref, _), _| stored_ref != identity_ref);
            Ok(())
        }

        fn store_secret(
            &self,
            identity_ref: &str,
            kind: &str,
            value: &str,
        ) -> Result<(), A2aError> {
            self.0
                .lock()
                .unwrap()
                .insert((identity_ref.into(), kind.into()), value.into());
            Ok(())
        }

        fn load_secret(&self, identity_ref: &str, kind: &str) -> Result<String, A2aError> {
            self.0
                .lock()
                .unwrap()
                .get(&(identity_ref.into(), kind.into()))
                .cloned()
                .ok_or(A2aError::IdentityStore)
        }
    }

    #[test]
    fn pairing_identity_stays_in_identity_store_and_signs_claim_proof() {
        let store = MemoryIdentityStore::default();
        let identity = create_client_pairing_identity(&store, "pairing-peer-1").unwrap();
        assert!(identity.csr_pem.contains("BEGIN CERTIFICATE REQUEST"));
        assert_eq!(identity.client_public_key_sha256.len(), 64);
        let token = [3_u8; 32];
        let proof =
            sign_claim_proof_with_identity(&store, &identity.identity_ref, "request-1", &token)
                .unwrap();
        let key = SigningKey::from_pkcs8_pem(
            &store
                .load_secret(&identity.identity_ref, "client-p256-key")
                .unwrap(),
        )
        .unwrap();
        verify_claim_proof(key.verifying_key(), "request-1", &token, &proof).unwrap();
    }

    #[test]
    fn pairing_response_is_strict_and_binds_sas_to_the_generated_identity() {
        let fixture = include_str!("../protocol/v1.0.1/fixtures/pairing-response.valid.json");
        assert!(A2aPairingResponse::parse_json(fixture).is_ok());
        assert!(A2aPairingResponse::parse_json(
            r#"{"version":"rivo-a2a-lan-v1","request_id":"x","server_spki_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","client_public_key_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","expires_at":"2030-01-01T00:00:00Z","sas_words":["a","b","c","d","e","f"],"claim_token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","extra":true}"#
        )
        .is_err());
        let store = MemoryIdentityStore::default();
        let identity = create_client_pairing_identity(&store, "pairing-peer-2").unwrap();
        let pairing_request = A2aPairingRequest::for_identity(&identity);
        assert_eq!(pairing_request.version, LAN_PAIRING_VERSION);
        assert!(pairing_request
            .csr_pem
            .contains("BEGIN CERTIFICATE REQUEST"));
        assert_eq!(
            pairing_url_from_interface("https://10.0.0.12:8443/a2a").unwrap(),
            "https://10.0.0.12:8443/rivo-a2a-lan-v1/pairings"
        );
        assert!(pairing_url_from_interface("https://public.example/a2a").is_err());
        let server = "a".repeat(64);
        let server_bytes: [u8; 32] = hex::decode(&server).unwrap().try_into().unwrap();
        let client_bytes: [u8; 32] = hex::decode(&identity.client_public_key_sha256)
            .unwrap()
            .try_into()
            .unwrap();
        let response = A2aPairingResponse {
            request_id: A2aRequestId("request-2".into()),
            server_spki_sha256: server.clone(),
            client_public_key_sha256: identity.client_public_key_sha256.clone(),
            expires_at: "2030-01-01T00:00:00Z".into(),
            sas_words: derive_sas_words("request-2", &server_bytes, &client_bytes).unwrap(),
            claim_token: [6_u8; 32],
        };
        response.verify_for_identity(&identity, &server).unwrap();
        let claim = make_claim_request(&store, &identity.identity_ref, &response).unwrap();
        assert_eq!(claim.version, LAN_PAIRING_VERSION);
        assert_eq!(claim.claim_token, URL_SAFE_NO_PAD.encode([6_u8; 32]));
    }

    #[test]
    fn pinned_tls_verifier_requires_matching_spki_ip_san_and_validity() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["10.0.0.3".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let mut peer = trusted();
        peer.spki_sha256 = sha256_hex(&key.public_key_der());
        let verifier = PinnedServerVerifier::from_trust(&peer).unwrap();
        assert!(verifier
            .verify_pinned_certificate(certificate.der().as_ref(), 1_800_000_000)
            .is_ok());

        peer.spki_sha256 = "f".repeat(64);
        assert_eq!(
            PinnedServerVerifier::from_trust(&peer)
                .unwrap()
                .verify_pinned_certificate(certificate.der().as_ref(), 1_800_000_000),
            Err(A2aError::CertificateRejected)
        );
        let wrong_san = rcgen::CertificateParams::new(vec!["10.0.0.4".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        peer.spki_sha256 = sha256_hex(&key.public_key_der());
        assert_eq!(
            PinnedServerVerifier::from_trust(&peer)
                .unwrap()
                .verify_pinned_certificate(wrong_san.der().as_ref(), 1_800_000_000),
            Err(A2aError::CertificateRejected)
        );
    }

    #[test]
    fn claim_response_rejects_ambiguous_or_mismatched_request() {
        let request_id = A2aRequestId("pair-01HVALID".into());
        let fixture = include_str!("../protocol/v1.0.1/fixtures/claim-response.valid.json");
        assert!(A2aClaimResponse::parse_json(fixture, &request_id).is_ok());
        assert_eq!(
            A2aClaimResponse::parse_json(
                r#"{"version":"rivo-a2a-lan-v1","request_id":"other","peer_id":"peer-1","certificate_chain_pem":"-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----","expires_at":"2031-01-01T00:00:00Z"}"#,
                &request_id,
            ),
            Err(A2aError::InvalidPairing)
        );
        assert_eq!(
            A2aClaimResponse::parse_json(
                r#"{"version":"rivo-a2a-lan-v1","request_id":"pair-01HVALID","request_id":"pair-01HVALID","peer_id":"peer-1","certificate_chain_pem":"-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----","expires_at":"2031-01-01T00:00:00Z"}"#,
                &request_id,
            ),
            Err(A2aError::InvalidJson)
        );
    }

    #[test]
    fn task_wire_projection_rejects_untrusted_artifact_forms() {
        let task = A2aTask::from_wire(json!({
            "id": "task-1",
            "status": {"state": "TASK_STATE_WORKING", "message": {"parts":[]}},
            "metadata": {"io.rivo.a2a.v1": {"revision": 3}},
            "artifacts": [{"artifactId":"artifact-1","name":"answer","parts":[{"data":{"ok":true}}]}]
        }))
        .unwrap();
        assert_eq!(task.state, A2aTaskState::Working);
        assert_eq!(task.revision, Some(3));
        assert_eq!(task.artifacts[0].value, json!({"ok":true}));
        assert!(A2aTask::from_wire(json!({
            "id":"task-1",
            "status":{"state":"WORKING"},
            "artifacts":[{"artifactId":"a","parts":[{"url":"https://bad.invalid"}]}]
        }))
        .is_err());
    }

    struct TaskTransport(Mutex<Vec<String>>);

    #[async_trait]
    impl A2aTransport for TaskTransport {
        async fn request(
            &self,
            _trust: &A2aTrustSnapshot,
            request: JsonRpcRequest,
        ) -> Result<JsonRpcResponse, A2aError> {
            self.0.lock().unwrap().push(request.method);
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Value::String(request.id),
                result: Some(json!({"id":"task-1","status":{"state":"COMPLETED"}})),
                error: None,
            })
        }
    }

    struct StreamingTaskTransport(Mutex<Vec<String>>);

    #[async_trait]
    impl A2aTransport for StreamingTaskTransport {
        async fn request(
            &self,
            _trust: &A2aTrustSnapshot,
            request: JsonRpcRequest,
        ) -> Result<JsonRpcResponse, A2aError> {
            self.0.lock().unwrap().push(request.method);
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Value::String(request.id),
                result: Some(json!({
                    "id":"task-1", "status":{"state":"WORKING"},
                    "metadata":{"io.rivo.a2a.v1":{"revision":2}}
                })),
                error: None,
            })
        }

        async fn subscribe_sse(
            &self,
            _trust: &A2aTrustSnapshot,
            task_id: &A2aTaskId,
        ) -> Result<Vec<SseEvent>, A2aError> {
            self.0.lock().unwrap().push("tasks/resubscribe:sse".into());
            Ok(vec![SseEvent {
                id: Some("event-3".into()),
                event: Some("task".into()),
                data: format!(
                    r#"{{"id":"{}","status":{{"state":"COMPLETED"}},"metadata":{{"io.rivo.a2a.v1":{{"revision":3}}}}}}"#,
                    task_id.0
                ),
            }])
        }
    }

    #[tokio::test]
    async fn task_client_uses_stable_jsonrpc_lifecycle_methods() {
        let transport = Arc::new(TaskTransport(Mutex::new(Vec::new())));
        let client = A2aTaskClient::new(transport.clone());
        let task_id = A2aTaskId("task-1".into());
        assert_eq!(
            client.get_task(&trusted(), &task_id).await.unwrap().state,
            A2aTaskState::Completed
        );
        client.cancel_task(&trusted(), &task_id).await.unwrap();
        client.subscribe_task(&trusted(), &task_id).await.unwrap();
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec!["tasks/get", "tasks/cancel", "tasks/resubscribe"]
        );
    }

    #[tokio::test]
    async fn recovery_gets_snapshot_then_reads_sse_without_resending_business_message() {
        let transport = Arc::new(StreamingTaskTransport(Mutex::new(Vec::new())));
        let client = A2aTaskClient::new(transport.clone());
        let recovered = client
            .recover_task(&trusted(), &A2aTaskId("task-1".into()))
            .await
            .unwrap();
        assert_eq!(
            recovered.iter().map(|task| task.state).collect::<Vec<_>>(),
            vec![A2aTaskState::Working, A2aTaskState::Completed]
        );
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec!["tasks/get", "tasks/resubscribe:sse"]
        );
    }

    #[test]
    fn psk_resubscribe_ids_are_unique_for_repeated_recovery() {
        let task = A2aTaskId("task-1".into());
        let first = psk_resubscribe_request_id(&task);
        let second = psk_resubscribe_request_id(&task);
        assert_ne!(first, second);
        assert!(is_lower_hex_sha256(&first));
        assert!(is_lower_hex_sha256(&second));
    }

    #[test]
    fn sse_task_updates_require_same_task_and_strictly_increase_revision() {
        let client = A2aTaskClient::new(Arc::new(TaskTransport(Mutex::new(Vec::new()))));
        let snapshot = A2aTask::from_wire(json!({"id":"task-1","status":{"state":"WORKING"},"metadata":{"io.rivo.a2a.v1":{"revision":2}}})).unwrap();
        let updates = client.merge_sse_updates(snapshot, &[SseEvent {
            id: Some("event-3".into()), event: Some("task".into()),
            data: r#"{"id":"task-1","status":{"state":"COMPLETED"},"metadata":{"io.rivo.a2a.v1":{"revision":3}}}"#.into(),
        }]).unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1].state, A2aTaskState::Completed);
        let stale = client.merge_sse_updates(updates[0].clone(), &[SseEvent { id: None, event: None, data: r#"{"id":"task-1","status":{"state":"COMPLETED"},"metadata":{"io.rivo.a2a.v1":{"revision":2}}}"#.into() }]).unwrap();
        assert_eq!(stale.len(), 1);
    }
}

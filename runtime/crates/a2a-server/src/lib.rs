//! Step 021 的受控 LAN A2A Server 核心。
//!
//! 本 crate 只收口不可信网络输入、Server profile、配对状态、任务状态机和
//! JSON-RPC/SSE 投影和 HTTPS/mTLS listener；keychain CA 与本机 Session/审批执行器
//! 必须由宿主在本机显式组装，不能被远端配置替换。

mod authentication;
mod http_response;
mod http_server;
mod inbound_tasks;
mod instance_control_route;
mod pairing;
mod protocol;
mod sse;
mod state_store;
mod task_route;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use rand_core::{OsRng, RngCore};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use rivo_a2a::psk::{PskSecret, ReplayGuard};
use rivo_a2a::{
    canonical_json_sha256, parse_unique_json, psk_peer_id, A2aError, A2aPeerId, A2aTask, A2aTaskId,
    A2aTaskState, JsonRpcError, JsonRpcRequest, JsonRpcResponse, MAX_MESSAGE_BYTES, SPEC_REVISION,
    WIRE_PROTOCOL_VERSION,
};
use rivo_instance_control::LanControlEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

pub(crate) use authentication::PskRequestAuthentication;
use authentication::PskServerSession;
use http_response::{empty_response, json_response};
use pairing::{serial_suffix, valid_sha256};
pub use pairing::{
    ClaimResult, ClientCertificateIssuer, PairingChallenge, PairingRegistry, PairingState,
    PeerGrant, PendingPairing,
};
use protocol::{allowed_transition, extract_send_message, task_id_from, task_wire, unix_now};
pub use state_store::{
    LocalApprovalStatus, PersistedServerTask, ServerStateStore, StoredLocalApproval,
    StoredPairingSummary,
};

#[cfg(test)]
use rivo_a2a::{A2aRequestId, LAN_PAIRING_VERSION};

pub const A2A_SERVER_SCHEMA_VERSION: u32 = 1;
pub const PAIRING_TTL: Duration = Duration::from_secs(600);
pub const DEFAULT_APPROVAL_TIMEOUT_SECONDS: u64 = 300;

/// 独立的 owner-only `a2a-server.toml`。它不会扩展既有 FileConfig，因此旧二进制
/// 无法静默读取或降级 A2A 配置；所有路径/材料均须在启动时经本机验证。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A2aServerFileConfig {
    pub schema_version: u32,
    pub state_db: PathBuf,
    pub workspace_aliases: BTreeMap<String, PathBuf>,
    pub profiles: Vec<ConfiguredServiceProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredServiceProfile {
    pub profile: ServiceProfile,
    /// TLS server certificate/key bundle ref, never its content.
    pub identity_ref: String,
}

impl A2aServerFileConfig {
    pub fn load_at(path: &Path) -> Result<Self, ServerError> {
        let input = std::fs::read_to_string(path).map_err(|_| ServerError::InvalidRequest)?;
        let config: Self = toml::from_str(&input).map_err(|_| ServerError::InvalidRequest)?;
        config.validate(path.parent().unwrap_or_else(|| Path::new(".")))?;
        Ok(config)
    }
    pub fn validate(&self, base: &Path) -> Result<(), ServerError> {
        if self.schema_version != A2A_SERVER_SCHEMA_VERSION
            || self.profiles.is_empty()
            || self.state_db.as_os_str().is_empty()
        {
            return Err(ServerError::InvalidRequest);
        }
        let mut names = BTreeSet::new();
        for configured in &self.profiles {
            configured.profile.validate()?;
            if !names.insert(configured.profile.name.clone())
                || !self
                    .workspace_aliases
                    .contains_key(&configured.profile.workspace_alias)
            {
                return Err(ServerError::InvalidRequest);
            }
            validate_identity_ref(&configured.identity_ref)?;
            let workspace = base.join(
                self.workspace_aliases
                    .get(&configured.profile.workspace_alias)
                    .expect("validated"),
            );
            if !workspace
                .canonicalize()
                .map_err(|_| ServerError::InvalidRequest)?
                .is_dir()
            {
                return Err(ServerError::InvalidRequest);
            }
        }
        Ok(())
    }
    pub fn profile(&self, name: &str) -> Result<&ConfiguredServiceProfile, ServerError> {
        self.profiles
            .iter()
            .find(|value| value.profile.name == name)
            .ok_or(ServerError::InvalidRequest)
    }
}

/// TLS 材料只从本机 keychain/受保护的宿主适配器传入。这个对象不可 serde，防止私钥
/// 进入 config、SQLite 或审计。`client_ca_certificates` 是该 profile 的独立 CA。
#[derive(Debug)]
pub struct ServerTlsMaterial {
    pub certificate_chain: Vec<CertificateDer<'static>>,
    pub private_key: PrivateKeyDer<'static>,
    pub client_ca_certificates: Vec<CertificateDer<'static>>,
}

/// 仅包含 CA 公钥证书和私钥的本机 keychain 条目。它从不实现 Serialize，避免
/// CA 私钥被写入 TOML、SQLite、审计或诊断材料。
#[derive(Debug)]
pub struct ClientIssuerMaterial {
    pub certificate: CertificateDer<'static>,
    pub private_key: PrivateKeyDer<'static>,
}

/// Server 私钥与 CA 私钥仅在 OS keychain 内按 identity ref 保存；普通配置和 SQLite
/// 只能保存 ref。值为 base64 DER 的封装，不会在 Display/Debug/审计中暴露。
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringServerMaterialStore;

const SERVER_KEYRING_SERVICE: &str = "rivo-a2a-server";
const SERVER_TLS_BUNDLE_KIND: &str = "tls-bundle-v1";
const SERVER_CLIENT_ISSUER_KIND: &str = "client-issuer-v1";
const SERVER_CERT_CHAIN_KIND_V2: &str = "server-certificate-chain-v2";
const SERVER_PRIVATE_KEY_KIND_V2: &str = "server-private-key-v2";
const SERVER_CLIENT_CA_KIND_V2: &str = "client-ca-certificate-v2";
const WINDOWS_CREDENTIAL_MAX_BYTES: usize = 2_560;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TlsBundleWire {
    certificate_chain_b64: Vec<String>,
    private_key_b64: String,
    client_ca_certificates_b64: Vec<String>,
}

fn keychain_error(error: keyring::Error, writing: bool) -> ServerError {
    let detail = format!("{error:?}");
    if detail.contains("TooLong") {
        ServerError::KeychainEntryTooLarge
    } else if detail.contains("Permission") || detail.contains("Denied") {
        ServerError::KeychainAccessDenied
    } else if writing {
        ServerError::KeychainWriteFailed
    } else {
        ServerError::KeychainUnavailable
    }
}

fn credential_exists(identity_ref: &str, kind: &str) -> bool {
    keyring::Entry::new(SERVER_KEYRING_SERVICE, &format!("{identity_ref}.{kind}"))
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .is_some()
}

fn set_credential(identity_ref: &str, kind: &str, value: &str) -> Result<(), ServerError> {
    let entry = keyring::Entry::new(SERVER_KEYRING_SERVICE, &format!("{identity_ref}.{kind}"))
        .map_err(|error| keychain_error(error, true))?;
    entry
        .set_password(value)
        .map_err(|error| keychain_error(error, true))
}

fn read_credential(identity_ref: &str, kind: &str) -> Result<String, ServerError> {
    let entry = keyring::Entry::new(SERVER_KEYRING_SERVICE, &format!("{identity_ref}.{kind}"))
        .map_err(|error| keychain_error(error, false))?;
    entry
        .get_password()
        .map_err(|error| keychain_error(error, false))
}

fn delete_credential(identity_ref: &str, kind: &str) -> Result<(), ServerError> {
    let entry = keyring::Entry::new(SERVER_KEYRING_SERVICE, &format!("{identity_ref}.{kind}"))
        .map_err(|error| keychain_error(error, false))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(keychain_error(error, false)),
    }
}

impl KeyringServerMaterialStore {
    pub fn save(
        &self,
        identity_ref: &str,
        material: &ServerTlsMaterial,
    ) -> Result<(), ServerError> {
        validate_identity_ref(identity_ref)?;
        let refs = [
            SERVER_CERT_CHAIN_KIND_V2,
            SERVER_PRIVATE_KEY_KIND_V2,
            SERVER_CLIENT_CA_KIND_V2,
        ];
        for kind in refs {
            if credential_exists(identity_ref, kind) {
                return Err(ServerError::KeychainAlreadyExists);
            }
        }
        let certs = serde_json::to_string(
            &material
                .certificate_chain
                .iter()
                .map(|v| base64::engine::general_purpose::STANDARD.encode(v.as_ref()))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| ServerError::KeychainWriteFailed)?;
        let private_key =
            base64::engine::general_purpose::STANDARD.encode(material.private_key.secret_der());
        let ca_certs = serde_json::to_string(
            &material
                .client_ca_certificates
                .iter()
                .map(|v| base64::engine::general_purpose::STANDARD.encode(v.as_ref()))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| ServerError::KeychainWriteFailed)?;
        let values = [
            (SERVER_CERT_CHAIN_KIND_V2, certs),
            (SERVER_PRIVATE_KEY_KIND_V2, private_key),
            (SERVER_CLIENT_CA_KIND_V2, ca_certs),
        ];
        if values
            .iter()
            .any(|(_, value)| value.len() > WINDOWS_CREDENTIAL_MAX_BYTES)
        {
            return Err(ServerError::KeychainEntryTooLarge);
        }
        let mut written = Vec::new();
        for (kind, value) in values {
            if let Err(error) = set_credential(identity_ref, kind, &value) {
                for prior in written {
                    let _ = delete_credential(identity_ref, prior);
                }
                return Err(error);
            }
            written.push(kind);
        }
        Ok(())
    }
    pub fn load(&self, identity_ref: &str) -> Result<ServerTlsMaterial, ServerError> {
        validate_identity_ref(identity_ref)?;
        let certs_v2 = read_credential(identity_ref, SERVER_CERT_CHAIN_KIND_V2);
        let private_key_v2 = read_credential(identity_ref, SERVER_PRIVATE_KEY_KIND_V2);
        let ca_certs_v2 = read_credential(identity_ref, SERVER_CLIENT_CA_KIND_V2);
        let present_v2 = [
            certs_v2.is_ok(),
            private_key_v2.is_ok(),
            ca_certs_v2.is_ok(),
        ];
        if present_v2.iter().any(|present| *present) && !present_v2.iter().all(|present| *present) {
            return Err(ServerError::KeychainUnavailable);
        }
        if let (Ok(certs), Ok(private_key), Ok(ca_certs)) = (certs_v2, private_key_v2, ca_certs_v2)
        {
            let decode = |value: String| {
                base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map(CertificateDer::from)
                    .map_err(|_| ServerError::InvalidRequest)
            };
            let certificate_chain = serde_json::from_str::<Vec<String>>(&certs)
                .map_err(|_| ServerError::InvalidRequest)?
                .into_iter()
                .map(decode)
                .collect::<Result<Vec<_>, _>>()?;
            let client_ca_certificates = serde_json::from_str::<Vec<String>>(&ca_certs)
                .map_err(|_| ServerError::InvalidRequest)?
                .into_iter()
                .map(decode)
                .collect::<Result<Vec<_>, _>>()?;
            let private_key = base64::engine::general_purpose::STANDARD
                .decode(private_key)
                .map_err(|_| ServerError::InvalidRequest)?;
            if certificate_chain.is_empty()
                || client_ca_certificates.is_empty()
                || private_key.is_empty()
            {
                return Err(ServerError::InvalidRequest);
            }
            return Ok(ServerTlsMaterial {
                certificate_chain,
                private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(private_key).into(),
                client_ca_certificates,
            });
        }
        let value = keyring::Entry::new(
            SERVER_KEYRING_SERVICE,
            &format!("{identity_ref}.{SERVER_TLS_BUNDLE_KIND}"),
        )
        .map_err(|_| ServerError::InvalidRequest)?
        .get_password()
        .map_err(|_| ServerError::InvalidRequest)?;
        let wire: TlsBundleWire =
            serde_json::from_str(&value).map_err(|_| ServerError::InvalidRequest)?;
        let decode_certificate = |value: String| {
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .map(CertificateDer::from)
                .map_err(|_| ServerError::InvalidRequest)
        };
        let certificate_chain = wire
            .certificate_chain_b64
            .into_iter()
            .map(decode_certificate)
            .collect::<Result<Vec<_>, _>>()?;
        if certificate_chain.is_empty() {
            return Err(ServerError::InvalidRequest);
        }
        let private_key = base64::engine::general_purpose::STANDARD
            .decode(wire.private_key_b64)
            .map_err(|_| ServerError::InvalidRequest)?;
        let client_ca_certificates = wire
            .client_ca_certificates_b64
            .into_iter()
            .map(decode_certificate)
            .collect::<Result<Vec<_>, _>>()?;
        if client_ca_certificates.is_empty() {
            return Err(ServerError::InvalidRequest);
        }
        Ok(ServerTlsMaterial {
            certificate_chain,
            private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(private_key).into(),
            client_ca_certificates,
        })
    }
    pub fn delete(&self, identity_ref: &str) -> Result<(), ServerError> {
        validate_identity_ref(identity_ref)?;
        let _ = delete_credential(identity_ref, SERVER_CERT_CHAIN_KIND_V2);
        let _ = delete_credential(identity_ref, SERVER_PRIVATE_KEY_KIND_V2);
        let _ = delete_credential(identity_ref, SERVER_CLIENT_CA_KIND_V2);
        match keyring::Entry::new(
            SERVER_KEYRING_SERVICE,
            &format!("{identity_ref}.{SERVER_TLS_BUNDLE_KIND}"),
        ) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keychain_error(error, false)),
            },
            Err(error) => Err(keychain_error(error, false)),
        }
    }

    /// 保存 profile 专用的 Client CA。调用者必须是本机 owner 的 provisioning
    /// 流程；`issuer_ref` 只是 keychain 引用而不是配置中的密钥内容。
    pub fn save_client_issuer(
        &self,
        issuer_ref: &str,
        material: &ClientIssuerMaterial,
    ) -> Result<(), ServerError> {
        validate_identity_ref(issuer_ref)?;
        if credential_exists(issuer_ref, "client-ca-certificate-v2")
            || credential_exists(issuer_ref, "client-ca-private-key-v2")
        {
            return Err(ServerError::KeychainAlreadyExists);
        }
        let certificate =
            base64::engine::general_purpose::STANDARD.encode(material.certificate.as_ref());
        let private_key =
            base64::engine::general_purpose::STANDARD.encode(material.private_key.secret_der());
        if certificate.len() > WINDOWS_CREDENTIAL_MAX_BYTES
            || private_key.len() > WINDOWS_CREDENTIAL_MAX_BYTES
        {
            return Err(ServerError::KeychainEntryTooLarge);
        }
        set_credential(issuer_ref, "client-ca-certificate-v2", &certificate)?;
        if let Err(error) = set_credential(issuer_ref, "client-ca-private-key-v2", &private_key) {
            let _ = delete_credential(issuer_ref, "client-ca-certificate-v2");
            return Err(error);
        }
        Ok(())
    }

    pub fn load_client_issuer(
        &self,
        issuer_ref: &str,
    ) -> Result<KeyringClientCertificateIssuer, ServerError> {
        validate_identity_ref(issuer_ref)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct IssuerWire {
            certificate_b64: String,
            private_key_b64: String,
        }
        let cert_v2 = read_credential(issuer_ref, "client-ca-certificate-v2");
        let key_v2 = read_credential(issuer_ref, "client-ca-private-key-v2");
        if cert_v2.is_ok() != key_v2.is_ok() {
            return Err(ServerError::KeychainUnavailable);
        }
        if let (Ok(cert), Ok(key)) = (cert_v2, key_v2) {
            let certificate = CertificateDer::from(
                base64::engine::general_purpose::STANDARD
                    .decode(cert)
                    .map_err(|_| ServerError::InvalidRequest)?,
            );
            let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(
                base64::engine::general_purpose::STANDARD
                    .decode(key)
                    .map_err(|_| ServerError::InvalidRequest)?,
            )
            .into();
            return Ok(KeyringClientCertificateIssuer {
                material: ClientIssuerMaterial {
                    certificate,
                    private_key,
                },
            });
        }
        let value = keyring::Entry::new(
            SERVER_KEYRING_SERVICE,
            &format!("{issuer_ref}.{SERVER_CLIENT_ISSUER_KIND}"),
        )
        .map_err(|_| ServerError::InvalidRequest)?
        .get_password()
        .map_err(|_| ServerError::InvalidRequest)?;
        let wire: IssuerWire =
            serde_json::from_str(&value).map_err(|_| ServerError::InvalidRequest)?;
        let certificate = CertificateDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(wire.certificate_b64)
                .map_err(|_| ServerError::InvalidRequest)?,
        );
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(wire.private_key_b64)
                .map_err(|_| ServerError::InvalidRequest)?,
        )
        .into();
        Ok(KeyringClientCertificateIssuer {
            material: ClientIssuerMaterial {
                certificate,
                private_key,
            },
        })
    }

    pub fn delete_client_issuer(&self, issuer_ref: &str) -> Result<(), ServerError> {
        validate_identity_ref(issuer_ref)?;
        let _ = delete_credential(issuer_ref, "client-ca-certificate-v2");
        let _ = delete_credential(issuer_ref, "client-ca-private-key-v2");
        match keyring::Entry::new(
            SERVER_KEYRING_SERVICE,
            &format!("{issuer_ref}.{SERVER_CLIENT_ISSUER_KIND}"),
        ) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keychain_error(error, false)),
            },
            Err(error) => Err(keychain_error(error, false)),
        }
    }
}

/// 以 keychain CA 签发配对 Client 证书。此类型刻意不暴露证书或密钥材料；其
/// `ClientCertificateIssuer` 输出只包含给对端的证书链。
#[derive(Debug)]
pub struct KeyringClientCertificateIssuer {
    material: ClientIssuerMaterial,
}

impl KeyringClientCertificateIssuer {
    /// Constructs an issuer from owner-provided in-memory material. The
    /// caller owns the material lifetime; this does not write to keychain.
    pub fn from_material(material: ClientIssuerMaterial) -> Self {
        Self { material }
    }
}

impl ClientCertificateIssuer for KeyringClientCertificateIssuer {
    fn issue_client_certificate(
        &self,
        peer_id: &A2aPeerId,
        csr_pem: &str,
        valid_for: Duration,
    ) -> Result<String, ServerError> {
        if valid_for != Duration::from_secs(365 * 24 * 60 * 60) {
            return Err(ServerError::InvalidRequest);
        }
        let csr = CertificateSigningRequestParams::from_pem(csr_pem)
            .map_err(|_| ServerError::InvalidPairing)?;
        if csr.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
            return Err(ServerError::InvalidPairing);
        }
        let ca_key = KeyPair::try_from(self.material.private_key.secret_der())
            .map_err(|_| ServerError::InvalidRequest)?;
        let ca_params = CertificateParams::from_ca_cert_der(&self.material.certificate)
            .map_err(|_| ServerError::InvalidRequest)?;
        // rcgen cannot reconstruct Certificate directly from DER and a key. Rebuilding it
        // from the same public CA/key preserves the trust anchor and allows signing without
        // exporting that key from the keychain boundary.
        let ca = ca_params
            .self_signed(&ca_key)
            .map_err(|_| ServerError::InvalidRequest)?;
        let mut params = csr.params;
        params.subject_alt_names = vec![SanType::URI(
            format!("urn:rivo:a2a:peer:{}", peer_id.0)
                .try_into()
                .map_err(|_| ServerError::InvalidRequest)?,
        )];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let mut serial = [0_u8; 16];
        OsRng.fill_bytes(&mut serial);
        serial[0] &= 0x7f;
        params.serial_number = Some(SerialNumber::from_slice(&serial));
        let certificate = params
            .signed_by(&csr.public_key, &ca, &ca_key)
            .map_err(|_| ServerError::InvalidRequest)?;
        Ok(format!("{}\n{}", certificate.pem(), ca.pem()))
    }
}

fn validate_identity_ref(value: &str) -> Result<(), ServerError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(ServerError::InvalidRequest)
    } else {
        Ok(())
    }
}

/// 计算 leaf server certificate 的 SubjectPublicKeyInfo pin。只对 keychain 返回的
/// 本机 TLS 材料调用，不接受网络证书或配置文本。
pub fn server_spki_sha256(material: &ServerTlsMaterial) -> Result<String, ServerError> {
    use x509_parser::prelude::{FromDer, X509Certificate};
    let leaf = material
        .certificate_chain
        .first()
        .ok_or(ServerError::InvalidRequest)?;
    let (_, certificate) =
        X509Certificate::from_der(leaf.as_ref()).map_err(|_| ServerError::InvalidRequest)?;
    Ok(hex::encode(Sha256::digest(
        certificate.tbs_certificate.subject_pki.raw,
    )))
}

impl ServerTlsMaterial {
    pub fn build_config(self) -> Result<rustls::ServerConfig, ServerError> {
        // reqwest may enable aws-lc while this LAN listener uses ring; pin a deterministic
        // provider once for the process rather than relying on Cargo feature selection.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for certificate in self.client_ca_certificates {
            roots
                .add(certificate)
                .map_err(|_| ServerError::InvalidRequest)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            // Card 与 CSR pairing 必须可匿名读取；task API 在 HTTP boundary 再强制 mTLS。
            .allow_unauthenticated()
            .build()
            .map_err(|_| ServerError::InvalidRequest)?;
        rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.certificate_chain, self.private_key)
            .map_err(|_| ServerError::InvalidRequest)
    }

    pub fn build_config_without_client_auth(self) -> Result<rustls::ServerConfig, ServerError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(self.certificate_chain, self.private_key)
            .map_err(|_| ServerError::InvalidRequest)
    }

    /// PSK bootstrap is a separate transport and is deliberately TLS 1.3
    /// only. The ordinary mTLS listener retains its existing compatibility
    /// policy above.
    pub fn build_psk_config(self) -> Result<rustls::ServerConfig, ServerError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(self.certificate_chain, self.private_key)
            .map_err(|_| ServerError::InvalidRequest)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ServerError {
    #[error("a2a_disabled")]
    Disabled,
    #[error("a2a_server_bind_rejected")]
    BindRejected,
    #[error("a2a_peer_not_trusted")]
    PeerNotTrusted,
    #[error("a2a_skill_not_allowed")]
    SkillNotAllowed,
    #[error("a2a_message_too_large")]
    MessageTooLarge,
    #[error("a2a_quota_exceeded")]
    QuotaExceeded,
    #[error("a2a_idempotency_conflict")]
    IdempotencyConflict,
    #[error("a2a_task_not_found")]
    TaskNotFound,
    #[error("a2a_local_approval_unavailable")]
    LocalApprovalUnavailable,
    #[error("a2a_invalid_pairing")]
    InvalidPairing,
    #[error("a2a_pairing_expired")]
    PairingExpired,
    #[error("a2a_pairing_not_approved")]
    PairingNotApproved,
    #[error("a2a_task_terminal")]
    TaskTerminal,
    #[error("a2a_invalid_request")]
    InvalidRequest,
    #[error("a2a_state_schema_unsupported")]
    UnsupportedStateSchema,
    #[error("a2a_psk_required")]
    PskRequired,
    #[error("a2a_psk_invalid")]
    PskInvalid,
    #[error("a2a_psk_replay")]
    PskReplay,
    #[error("a2a_psk_session_required")]
    PskSessionRequired,
    #[error("keychain_entry_too_large")]
    KeychainEntryTooLarge,
    #[error("keychain_unavailable")]
    KeychainUnavailable,
    #[error("keychain_access_denied")]
    KeychainAccessDenied,
    #[error("keychain_write_failed")]
    KeychainWriteFailed,
    #[error("keychain_already_exists")]
    KeychainAlreadyExists,
}

impl From<A2aError> for ServerError {
    fn from(_: A2aError) -> Self {
        Self::InvalidRequest
    }
}

/// ServiceProfile 是只读配置的可序列化形状。启动前必经 validate；远端 JSON-RPC
/// 请求没有任何字段可以覆盖这些限制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceProfile {
    pub name: String,
    pub bind: SocketAddr,
    pub skill_id: String,
    pub workspace_alias: String,
    pub capability_profile: String,
    pub policy_ceiling: String,
    #[serde(default)]
    pub network_enabled: bool,
    #[serde(default)]
    pub browser_enabled: bool,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub mcp_toolsets: BTreeMap<String, BTreeSet<String>>,
    #[serde(default = "one")]
    pub per_peer_max_active_tasks: usize,
    #[serde(default = "three")]
    pub max_active_tasks: usize,
    #[serde(default = "max_message_bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "max_duration")]
    pub max_duration_seconds: u64,
    #[serde(default = "approval_timeout")]
    pub approval_timeout_seconds: u64,
    #[serde(default)]
    pub allowed_peers: BTreeSet<A2aPeerId>,
    pub issuer_ref: String,
    #[serde(default = "retention")]
    pub idempotency_retention_seconds: u64,
    #[serde(default = "audit_retention")]
    pub audit_retention_days: u64,
}
fn one() -> usize {
    1
}
fn three() -> usize {
    3
}
fn max_message_bytes() -> usize {
    MAX_MESSAGE_BYTES
}
fn max_output_bytes() -> usize {
    256 * 1024
}
fn max_duration() -> u64 {
    30 * 60
}
fn approval_timeout() -> u64 {
    DEFAULT_APPROVAL_TIMEOUT_SECONDS
}
fn retention() -> u64 {
    24 * 60 * 60
}
fn audit_retention() -> u64 {
    30
}

impl ServiceProfile {
    pub fn validate(&self) -> Result<(), ServerError> {
        if !is_allowed_bind_address(self.bind.ip(), local_test_mode_enabled())
            || self.bind.port() == 0
            || self.name.is_empty()
            || self.name.len() > 128
            || self.skill_id.is_empty()
            || self.workspace_alias.is_empty()
            || self.issuer_ref.is_empty()
            || self.max_message_bytes == 0
            || self.max_message_bytes > MAX_MESSAGE_BYTES
            || self.max_output_bytes == 0
            || self.max_duration_seconds == 0
            || self.approval_timeout_seconds == 0
            || self.idempotency_retention_seconds == 0
            || self.audit_retention_days == 0
            || self.per_peer_max_active_tasks == 0
            || self.max_active_tasks == 0
            || self.per_peer_max_active_tasks > self.max_active_tasks
        {
            return Err(ServerError::InvalidRequest);
        }
        if !matches!(
            self.capability_profile.as_str(),
            "research" | "coding" | "instance_control"
        ) || !matches!(self.policy_ceiling.as_str(), "strict" | "medium")
            || (self.capability_profile == "research" && self.policy_ceiling != "strict")
            || (self.capability_profile == "instance_control"
                && (self.policy_ceiling != "strict"
                    || self.skill_id != "process-control"
                    || self.network_enabled
                    || self.browser_enabled
                    || !self.mcp_servers.is_empty()
                    || !self.mcp_toolsets.is_empty()))
        {
            return Err(ServerError::InvalidRequest);
        }
        Ok(())
    }

    pub fn interface_url(&self) -> String {
        let ip = match self.bind.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        format!("https://{ip}:{}/a2a", self.bind.port())
    }

    pub fn minimal_card(&self) -> Value {
        json!({
            "name": self.name, "version": "1.0.0", "protocolVersion": SPEC_REVISION,
            "supportedInterfaces": [{"url": self.interface_url(), "protocolBinding": "JSONRPC", "protocolVersion": WIRE_PROTOCOL_VERSION}],
            "securitySchemes": {"mtls": {"type": "mutualTLS"}},
            "security": [{"mtls": []}],
            "skills": [{"id": self.skill_id, "name": self.skill_id}],
            "capabilities": {"streaming": true, "extendedAgentCard": true},
            "defaultInputModes": ["text", "application/json"], "defaultOutputModes": ["text", "application/json"]
        })
    }

    pub fn minimal_card_hash(&self) -> Result<String, ServerError> {
        Ok(canonical_json_sha256(&self.minimal_card())?)
    }
}

/// Production listeners are restricted to RFC1918/ULA addresses. The sole
/// exception is an explicitly enabled local integration test, which may bind
/// loopback so its independent client and server processes never leave the
/// host. Wildcards, DNS names, and public addresses remain rejected.
pub fn is_allowed_bind_address(ip: IpAddr, local_test_mode: bool) -> bool {
    if local_test_mode && ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V4(v) => v.is_private(),
        IpAddr::V6(v) => v.is_unique_local(),
    }
}

fn local_test_mode_enabled() -> bool {
    std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1")
}

/// 入站 Session 将本机 policy 的确认请求投递到这一边界。远端没有批准 API；
/// 任何没有可用 broker 的请求均明确拒绝，而不是降级为客户端确认。
pub trait ApprovalBroker: Send + Sync {
    fn request_local_approval(&self, request: &LocalApprovalRequest) -> LocalApproval;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalApprovalRequest {
    pub task_id: A2aTaskId,
    pub peer_id: A2aPeerId,
    pub skill_id: String,
    /// 本机 Turn 与 policy request 的不可混用绑定。它们不会出现在远端 task。
    pub turn_id: String,
    pub request_id: String,
    pub call_id: String,
    pub tool_name: String,
    /// RFC 8785 canonical 工具参数的 SHA-256。原始参数永不写入审批 SQLite。
    pub canonical_args_sha256: String,
    /// 仅供 Server 所在本机 owner 阅读的已脱敏摘要；远端永远只见固定摘要。
    pub arguments_summary: String,
    pub targets: Vec<String>,
    pub program: Option<String>,
    pub side_effects: Vec<String>,
    /// 面向远端和审计的描述不得包含路径、命令参数或凭据。
    pub redacted_summary: String,
    pub expires_unix: u64,
}

impl LocalApprovalRequest {
    fn validate(&self) -> Result<(), ServerError> {
        let bounded = |value: &str, maximum: usize| !value.is_empty() && value.len() <= maximum;
        if !bounded(&self.task_id.0, 128)
            || !bounded(&self.peer_id.0, 128)
            || !bounded(&self.skill_id, 128)
            || !bounded(&self.turn_id, 256)
            || !bounded(&self.request_id, 256)
            || !bounded(&self.call_id, 256)
            || !bounded(&self.tool_name, 128)
            || !valid_sha256(&self.canonical_args_sha256)
            || self.arguments_summary.len() > 2048
            || self.targets.len() > 32
            || self.targets.iter().any(|target| target.len() > 512)
            || self.side_effects.len() > 32
            || self.side_effects.iter().any(|effect| effect.len() > 256)
            || self
                .program
                .as_ref()
                .is_some_and(|program| program.len() > 512)
            || self.expires_unix == 0
        {
            return Err(ServerError::InvalidRequest);
        }
        Ok(())
    }

    pub fn approval_id(&self) -> String {
        let mut digest = Sha256::new();
        for value in [
            &self.task_id.0,
            &self.peer_id.0,
            &self.turn_id,
            &self.request_id,
            &self.call_id,
            &self.tool_name,
            &self.canonical_args_sha256,
        ] {
            digest.update((value.len() as u32).to_be_bytes());
            digest.update(value.as_bytes());
        }
        format!("approval-{}", hex::encode(digest.finalize()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalApproval {
    ApprovedOnce,
    Rejected,
    TimedOut,
    Unavailable,
}

/// SQLite-backed owner-only approval queue. `serve` 使用它阻塞等待本机裁决；另一
/// 本机进程通过 `rivo a2a approvals` 操作相同 state DB。该类型没有网络接口，
/// 也没有任何“远端 allow”分支。
#[derive(Debug, Clone)]
pub struct LocalApprovalQueue {
    state_path: PathBuf,
    poll_interval: Duration,
}

impl LocalApprovalQueue {
    pub fn open(state_path: PathBuf) -> Result<Self, ServerError> {
        // 提前创建 schema，避免首个 policy confirmation 在运行线程里才失败。
        let _ = ServerStateStore::open(&state_path)?;
        Ok(Self {
            state_path,
            poll_interval: Duration::from_millis(100),
        })
    }

    pub fn invalidate_task(&self, task_id: &A2aTaskId) -> Result<usize, ServerError> {
        ServerStateStore::open(&self.state_path)?.invalidate_task_approvals(task_id)
    }

    fn wait_for_decision(&self, approval_id: &str, expires_unix: u64) -> LocalApproval {
        let started = Instant::now();
        loop {
            let now = unix_now();
            if now >= expires_unix {
                return ServerStateStore::open(&self.state_path)
                    .and_then(|state| state.take_local_approval(approval_id, now))
                    .ok()
                    .flatten()
                    .unwrap_or(LocalApproval::TimedOut);
            }
            match ServerStateStore::open(&self.state_path)
                .and_then(|state| state.take_local_approval(approval_id, now))
            {
                Ok(Some(decision)) => return decision,
                Ok(None) => {}
                Err(_) => return LocalApproval::Unavailable,
            }
            // Polling is intentionally bounded and uses SQLite's transaction boundary so a
            // separately launched owner CLI can decide without sharing process state.
            thread::sleep(self.poll_interval);
            if started.elapsed() > Duration::from_secs(24 * 60 * 60) {
                return LocalApproval::TimedOut;
            }
        }
    }
}

impl ApprovalBroker for LocalApprovalQueue {
    fn request_local_approval(&self, request: &LocalApprovalRequest) -> LocalApproval {
        let state = match ServerStateStore::open(&self.state_path) {
            Ok(state) => state,
            Err(_) => return LocalApproval::Unavailable,
        };
        let approval_id = match state.enqueue_local_approval(request) {
            Ok(value) => value,
            Err(_) => return LocalApproval::Unavailable,
        };
        self.wait_for_decision(&approval_id, request.expires_unix)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StoredTask {
    peer: A2aPeerId,
    request_hash: String,
    message_id: String,
    /// 仅用于本机 A2A Server → Session driver 的不透明关联。它永不写入 A2A
    /// wire、SSE、审计摘要或远端错误；也不是 SessionId 或 TurnId。
    local_session_ref: InboundSessionRef,
    task: A2aTask,
}

/// A2A task 的本机 Session driver 关联。该值由 Server 创建，不能由远端 task、
/// context 或 message 提供，也不能作为本机 SessionId/TurnId 的替代。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InboundSessionRef(String);

/// 只在 Server 验证 peer、skill、配额和幂等后传给本机 driver 的冻结输入。远端
/// 没有字段可选择 workspace、policy、MCP、网络、浏览器、模型或审批策略。
#[derive(Debug, Clone)]
pub struct InboundSessionStart {
    pub task_id: A2aTaskId,
    pub peer_id: A2aPeerId,
    pub local_session_ref: InboundSessionRef,
    pub profile: ServiceProfile,
    pub message: String,
}

/// 本机 Session 的脱敏状态投影。`Completed` 仅允许普通文本结果；driver 不得把
/// tool 参数、路径、凭据或隐藏推理投影到 A2A task。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundSessionEvent {
    Working,
    LocalApprovalRequired,
    Completed { text: String },
    Failed,
    Rejected,
    Canceled,
}

/// Session driver 通过此接口投影状态。实现者必须验证 task owner 和本机关联；
/// 该接口没有远端 approval 或任意 Intent 注入能力。
#[async_trait]
pub trait InboundSessionProjector: Send + Sync {
    async fn project(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
        local_session: &InboundSessionRef,
        event: InboundSessionEvent,
    ) -> Result<A2aTask, ServerError>;
}

/// 由 CLI/TUI 等本机宿主实现的反向适配器。A2A crate 不依赖 Session；因此不能由
/// 网络请求替换本机执行器、审批 broker 或固定的 ServiceProfile。
#[async_trait]
pub trait InboundSessionDriver: Send + Sync {
    async fn start(
        &self,
        request: InboundSessionStart,
        projector: Arc<dyn InboundSessionProjector>,
    ) -> Result<(), ServerError>;

    async fn cancel(&self, local_session: &InboundSessionRef) -> Result<(), ServerError>;
}

/// Host adapter for explicitly paired LAN instance-control requests. The server
/// validates the peer, profile capability and envelope before invoking it.
#[async_trait]
pub trait InstanceControlHandler: Send + Sync {
    async fn handle(
        &self,
        peer: &A2aPeerId,
        envelope: LanControlEnvelope,
    ) -> Result<Value, ServerError>;
}

#[derive(Debug, Clone)]
struct InboundReservation {
    task: A2aTask,
    start: InboundSessionStart,
    newly_created: bool,
}

#[derive(Debug)]
pub struct A2aServer {
    profile: ServiceProfile,
    tasks: BTreeMap<A2aTaskId, StoredTask>,
    idempotency: BTreeMap<(A2aPeerId, String), (String, A2aTaskId)>,
    next_task: u64,
}

impl A2aServer {
    pub fn new(profile: ServiceProfile) -> Result<Self, ServerError> {
        profile.validate()?;
        Ok(Self {
            profile,
            tasks: BTreeMap::new(),
            idempotency: BTreeMap::new(),
            next_task: 0,
        })
    }
    /// 从启动前已 terminalize 的最小 SQLite 投影恢复可查询 task 与幂等映射。
    /// 调用方必须先对同一 profile 执行 `recover_unfinished`：恢复路径从不重建
    /// Session driver、不读取原消息，因而不可能重放远端业务请求。
    pub fn restore(
        profile: ServiceProfile,
        persisted: impl IntoIterator<Item = PersistedServerTask>,
    ) -> Result<Self, ServerError> {
        let mut server = Self::new(profile)?;
        for record in persisted {
            if record.profile_name != server.profile.name
                || !record.task.state.terminal()
                || !valid_sha256(&record.request_hash)
                || record.message_id.len() > 256
            {
                return Err(ServerError::InvalidRequest);
            }
            let key = (record.peer_id.clone(), record.message_id.clone());
            if server.idempotency.contains_key(&key) || server.tasks.contains_key(&record.task.id) {
                return Err(ServerError::InvalidRequest);
            }
            server.next_task = server.next_task.max(serial_suffix(&record.task.id.0));
            server
                .idempotency
                .insert(key, (record.request_hash.clone(), record.task.id.clone()));
            server.tasks.insert(
                record.task.id.clone(),
                StoredTask {
                    peer: record.peer_id,
                    request_hash: record.request_hash,
                    message_id: record.message_id,
                    // This is deliberately a new opaque marker, never a resumed Session
                    // reference. Every restored task is terminal and cannot be driven.
                    local_session_ref: InboundSessionRef(format!("recovered-{}", record.task.id.0)),
                    task: record.task,
                },
            );
        }
        Ok(server)
    }
    pub fn profile(&self) -> &ServiceProfile {
        &self.profile
    }
    pub fn public_card_json(&self) -> Result<String, ServerError> {
        serde_json::to_string(&self.profile.minimal_card()).map_err(|_| ServerError::InvalidRequest)
    }
    pub fn send_message(
        &mut self,
        peer: &A2aPeerId,
        message_id: &str,
        request: Value,
    ) -> Result<A2aTask, ServerError> {
        Ok(self.reserve_inbound(peer, message_id, request)?.task)
    }

    /// 原子保留入站 task，并生成只能由本机 driver 使用的不透明关联。调用方必须在
    /// task 已保留后启动 driver；同一 idempotency 请求不会再次启动第二个 Session。
    fn reserve_inbound(
        &mut self,
        peer: &A2aPeerId,
        message_id: &str,
        request: Value,
    ) -> Result<InboundReservation, ServerError> {
        if self.profile.capability_profile == "instance_control" {
            return Err(ServerError::SkillNotAllowed);
        }
        if !self.profile.allowed_peers.contains(peer) {
            return Err(ServerError::PeerNotTrusted);
        }
        let request_hash = canonical_json_sha256(&request)?;
        let key = (peer.clone(), message_id.into());
        if let Some((previous_hash, task_id)) = self.idempotency.get(&key) {
            return if previous_hash == &request_hash {
                {
                    let stored = self.tasks.get(task_id).expect("mapping task");
                    Ok(InboundReservation {
                        task: stored.task.clone(),
                        start: InboundSessionStart {
                            task_id: stored.task.id.clone(),
                            peer_id: stored.peer.clone(),
                            local_session_ref: stored.local_session_ref.clone(),
                            profile: self.profile.clone(),
                            // 绝不从 idempotency 记录恢复或返回正文。重复请求不启动
                            // 新 Session，故 driver 也不需要原始 message。
                            message: String::new(),
                        },
                        newly_created: false,
                    })
                }
            } else {
                Err(ServerError::IdempotencyConflict)
            };
        }
        let (skill, message) = extract_send_message(&request)?;
        if skill != self.profile.skill_id {
            return Err(ServerError::SkillNotAllowed);
        }
        if message.len() > self.profile.max_message_bytes {
            return Err(ServerError::MessageTooLarge);
        }
        let active_total = self
            .tasks
            .values()
            .filter(|t| !t.task.state.terminal())
            .count();
        let active_peer = self
            .tasks
            .values()
            .filter(|t| t.peer == *peer && !t.task.state.terminal())
            .count();
        if active_total >= self.profile.max_active_tasks
            || active_peer >= self.profile.per_peer_max_active_tasks
        {
            return Err(ServerError::QuotaExceeded);
        }
        self.next_task += 1;
        let task_id = A2aTaskId(format!("task-{:016x}", self.next_task));
        let local_session_ref = InboundSessionRef(format!("inbound-{:016x}", self.next_task));
        let task = A2aTask {
            id: task_id.clone(),
            state: A2aTaskState::Submitted,
            revision: Some(1),
            message: None,
            artifacts: Vec::new(),
        };
        self.idempotency
            .insert(key, (request_hash.clone(), task_id.clone()));
        self.tasks.insert(
            task_id.clone(),
            StoredTask {
                peer: peer.clone(),
                request_hash,
                message_id: message_id.into(),
                local_session_ref: local_session_ref.clone(),
                task: task.clone(),
            },
        );
        Ok(InboundReservation {
            task: task.clone(),
            start: InboundSessionStart {
                task_id,
                peer_id: peer.clone(),
                local_session_ref,
                profile: self.profile.clone(),
                message,
            },
            newly_created: true,
        })
    }
    pub fn get_task(&self, peer: &A2aPeerId, task_id: &A2aTaskId) -> Result<A2aTask, ServerError> {
        let stored = self.tasks.get(task_id).ok_or(ServerError::TaskNotFound)?;
        if &stored.peer != peer {
            return Err(ServerError::TaskNotFound);
        }
        Ok(stored.task.clone())
    }
    fn persisted_task(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<PersistedServerTask, ServerError> {
        let stored = self.tasks.get(task_id).ok_or(ServerError::TaskNotFound)?;
        if &stored.peer != peer {
            return Err(ServerError::TaskNotFound);
        }
        Ok(PersistedServerTask {
            profile_name: self.profile.name.clone(),
            peer_id: stored.peer.clone(),
            message_id: stored.message_id.clone(),
            request_hash: stored.request_hash.clone(),
            task: stored.task.clone(),
        })
    }
    pub fn transition(
        &mut self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
        next: A2aTaskState,
        message: Option<Value>,
    ) -> Result<A2aTask, ServerError> {
        let stored = self
            .tasks
            .get_mut(task_id)
            .ok_or(ServerError::TaskNotFound)?;
        if &stored.peer != peer {
            return Err(ServerError::TaskNotFound);
        }
        if stored.task.state.terminal() {
            return Err(ServerError::TaskTerminal);
        }
        if !allowed_transition(stored.task.state, next) {
            return Err(ServerError::InvalidRequest);
        }
        stored.task.state = next;
        stored.task.revision = Some(stored.task.revision.unwrap_or(0) + 1);
        stored.task.message = message;
        Ok(stored.task.clone())
    }
    pub fn cancel(
        &mut self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, ServerError> {
        let current = self.get_task(peer, task_id)?;
        if current.state.terminal() {
            return Ok(current);
        }
        self.transition(peer, task_id, A2aTaskState::Canceled, None)
    }

    fn local_session_ref(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<InboundSessionRef, ServerError> {
        let stored = self.tasks.get(task_id).ok_or(ServerError::TaskNotFound)?;
        if &stored.peer != peer {
            return Err(ServerError::TaskNotFound);
        }
        Ok(stored.local_session_ref.clone())
    }

    fn fail_unstarted(
        &mut self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<A2aTask, ServerError> {
        self.transition(peer, task_id, A2aTaskState::Failed, None)
    }

    fn project_session_event(
        &mut self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
        local_session: &InboundSessionRef,
        event: InboundSessionEvent,
    ) -> Result<A2aTask, ServerError> {
        let stored = self.tasks.get(task_id).ok_or(ServerError::TaskNotFound)?;
        if &stored.peer != peer || &stored.local_session_ref != local_session {
            return Err(ServerError::TaskNotFound);
        }
        if stored.task.state.terminal() {
            return Err(ServerError::TaskTerminal);
        }
        let (state, message) = match event {
            InboundSessionEvent::Working => (A2aTaskState::Working, None),
            InboundSessionEvent::LocalApprovalRequired => (
                A2aTaskState::InputRequired,
                Some(json!({
                    "request_kind": "local_approval",
                    "request_id": format!("approval-{}", task_id.0),
                    "summary": "等待服务端本机授权"
                })),
            ),
            InboundSessionEvent::Completed { text } => {
                if text.len() > self.profile.max_output_bytes {
                    return self.transition(peer, task_id, A2aTaskState::Failed, None);
                }
                (
                    A2aTaskState::Completed,
                    Some(json!({"parts": [{"text": text}]})),
                )
            }
            InboundSessionEvent::Failed => (A2aTaskState::Failed, None),
            InboundSessionEvent::Rejected => (A2aTaskState::Rejected, None),
            InboundSessionEvent::Canceled => (A2aTaskState::Canceled, None),
        };
        self.transition(peer, task_id, state, message)
    }
    /// 将需要本机确认的 task 投影为脱敏 INPUT_REQUIRED。只有 broker 返回一次性
    /// 批准才可让 Session 继续；远端永远不能直接调用该裁决。
    pub fn request_local_approval(
        &mut self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
        broker: Option<&dyn ApprovalBroker>,
        now_unix: u64,
    ) -> Result<A2aTask, ServerError> {
        let redacted = json!({
            "request_kind": "local_approval",
            "request_id": format!("approval-{}", task_id.0),
            "summary": "等待服务端本机授权"
        });
        self.transition(peer, task_id, A2aTaskState::InputRequired, Some(redacted))?;
        let request = LocalApprovalRequest {
            task_id: task_id.clone(),
            peer_id: peer.clone(),
            skill_id: self.profile.skill_id.clone(),
            // 此兼容 helper 没有真实 Session event；生产 driver 必须以实际
            // turn/request/call/tool/参数 hash 构造完整请求。
            turn_id: format!("compat-{}", task_id.0),
            request_id: format!("approval-{}", task_id.0),
            call_id: format!("approval-{}", task_id.0),
            tool_name: "unknown".into(),
            canonical_args_sha256: hex::encode(Sha256::digest(b"{}")),
            arguments_summary: "本机副作用等待授权".into(),
            targets: Vec::new(),
            program: None,
            side_effects: Vec::new(),
            redacted_summary: "等待服务端本机授权".into(),
            expires_unix: now_unix.saturating_add(self.profile.approval_timeout_seconds),
        };
        match broker.map(|broker| broker.request_local_approval(&request)) {
            Some(LocalApproval::ApprovedOnce) => {
                self.transition(peer, task_id, A2aTaskState::Working, None)
            }
            Some(LocalApproval::Rejected) | Some(LocalApproval::TimedOut) => {
                self.transition(peer, task_id, A2aTaskState::Rejected, None)
            }
            Some(LocalApproval::Unavailable) | None => {
                self.transition(peer, task_id, A2aTaskState::Rejected, None)?;
                Err(ServerError::LocalApprovalUnavailable)
            }
        }
    }
    pub fn sse_snapshot(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<String, ServerError> {
        let task = self.get_task(peer, task_id)?;
        let wire = task_wire(&task);
        Ok(format!(
            "event: snapshot\nid: {}\ndata: {}\n\n",
            task.revision.unwrap_or(0),
            serde_json::to_string(&wire).map_err(|_| ServerError::InvalidRequest)?
        ))
    }
    pub fn handle_json_rpc(
        &mut self,
        peer: &A2aPeerId,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = Value::String(request.id);
        let result = match request.method.as_str() {
            "message/send" | "message/stream" => {
                let message_id = request
                    .params
                    .get("message")
                    .and_then(Value::as_object)
                    .and_then(|v| v.get("messageId"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.send_message(peer, message_id, request.params.clone())
                    .map(|t| task_wire(&t))
            }
            "tasks/get" => task_id_from(&request.params)
                .and_then(|task| self.get_task(peer, &task))
                .map(|t| task_wire(&t)),
            "tasks/cancel" => task_id_from(&request.params)
                .and_then(|task| self.cancel(peer, &task))
                .map(|t| task_wire(&t)),
            _ => Err(ServerError::InvalidRequest),
        };
        match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: error.to_string(),
                    data: None,
                }),
            },
        }
    }
}

/// 最小 HTTPS/mTLS listener。它只绑定已由 `ServiceProfile::validate` 接受的私网
/// 地址；公开 Card/配对起始不要求客户端证书，而一切 Task JSON-RPC 都要求已验证
/// 的 profile grant。宿主负责将 `PairingRegistry` 和任务映射持久化后再重启 listener。
pub struct A2aHttpsListener {
    pub server: Arc<Mutex<A2aServer>>,
    pub pairings: Arc<Mutex<PairingRegistry>>,
    pub server_spki_sha256: String,
    issuer: Arc<dyn ClientCertificateIssuer>,
    state: Option<Arc<Mutex<ServerStateStore>>>,
    session_driver: Option<Arc<dyn InboundSessionDriver>>,
    instance_control_handler: Option<Arc<dyn InstanceControlHandler>>,
    instance_control_nonces: Arc<Mutex<BTreeMap<String, u128>>>,
    acceptor: TlsAcceptor,
    psk_secret: Option<PskSecret>,
    psk_replay: Arc<ReplayGuard>,
    psk_sessions: Arc<Mutex<BTreeMap<[u8; 32], PskServerSession>>>,
}

struct ListenerSessionProjector {
    server: Arc<Mutex<A2aServer>>,
    state: Option<Arc<Mutex<ServerStateStore>>>,
}

#[async_trait]
impl InboundSessionProjector for ListenerSessionProjector {
    async fn project(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
        local_session: &InboundSessionRef,
        event: InboundSessionEvent,
    ) -> Result<A2aTask, ServerError> {
        let task =
            self.server
                .lock()
                .await
                .project_session_event(peer, task_id, local_session, event)?;
        if let Some(state) = &self.state {
            let record = self.server.lock().await.persisted_task(peer, &task.id)?;
            state.lock().await.record_task(&record, unix_now())?;
        }
        Ok(task)
    }
}

impl A2aHttpsListener {
    pub fn new(
        server: A2aServer,
        pairings: PairingRegistry,
        server_spki_sha256: String,
        tls: ServerTlsMaterial,
        issuer: Arc<dyn ClientCertificateIssuer>,
    ) -> Result<Self, ServerError> {
        if !valid_sha256(&server_spki_sha256) {
            return Err(ServerError::InvalidRequest);
        }
        Ok(Self {
            server: Arc::new(Mutex::new(server)),
            pairings: Arc::new(Mutex::new(pairings)),
            server_spki_sha256,
            issuer,
            state: None,
            session_driver: None,
            instance_control_handler: None,
            instance_control_nonces: Arc::new(Mutex::new(BTreeMap::new())),
            acceptor: TlsAcceptor::from(Arc::new(tls.build_config()?)),
            psk_secret: None,
            psk_replay: Arc::new(ReplayGuard::default()),
            psk_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn new_psk(
        server: A2aServer,
        pairings: PairingRegistry,
        server_spki_sha256: String,
        tls: ServerTlsMaterial,
        issuer: Arc<dyn ClientCertificateIssuer>,
        secret: PskSecret,
    ) -> Result<Self, ServerError> {
        if !valid_sha256(&server_spki_sha256) || secret.iter().all(|byte| *byte == 0) {
            return Err(ServerError::InvalidRequest);
        }
        Ok(Self {
            server: Arc::new(Mutex::new(server)),
            pairings: Arc::new(Mutex::new(pairings)),
            server_spki_sha256,
            issuer,
            state: None,
            session_driver: None,
            instance_control_handler: None,
            instance_control_nonces: Arc::new(Mutex::new(BTreeMap::new())),
            acceptor: TlsAcceptor::from(Arc::new(tls.build_psk_config()?)),
            psk_secret: Some(secret),
            psk_replay: Arc::new(ReplayGuard::default()),
            psk_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn with_state_store(mut self, state: ServerStateStore) -> Self {
        self.state = Some(Arc::new(Mutex::new(state)));
        self
    }

    /// 明确装配本机 Session driver。没有该适配器时 listener 仍可提供 Card、配对
    /// 和查询，但会将新的入站 task 固定为 FAILED，绝不假装已经执行。
    pub fn with_session_driver(mut self, driver: Arc<dyn InboundSessionDriver>) -> Self {
        self.session_driver = Some(driver);
        self
    }

    pub fn with_instance_control_handler(
        mut self,
        handler: Arc<dyn InstanceControlHandler>,
    ) -> Self {
        self.instance_control_handler = Some(handler);
        self
    }

    pub(crate) fn session_projector(&self) -> Arc<dyn InboundSessionProjector> {
        Arc::new(ListenerSessionProjector {
            server: self.server.clone(),
            state: self.state.clone(),
        })
    }

    pub(crate) async fn persist_task(
        &self,
        peer: &A2aPeerId,
        task_id: &A2aTaskId,
    ) -> Result<(), ServerError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        let record = self.server.lock().await.persisted_task(peer, task_id)?;
        state.lock().await.record_task(&record, unix_now())
    }

    async fn handle_http(
        &self,
        request: Request<Incoming>,
        certificate_fingerprint: Option<String>,
    ) -> Response<Full<Bytes>> {
        if request.method() == Method::POST
            && request
                .uri()
                .path()
                .strip_prefix("/rivo-a2a-lan-v1/pairings/")
                .and_then(|tail| tail.strip_suffix("/claim"))
                .is_some()
        {
            return self.handle_pairing_claim(request).await;
        }
        if request.method() == Method::POST
            && request.uri().path() == "/rivo-a2a-lan-v1/instance-control"
        {
            return self
                .handle_instance_control(request, certificate_fingerprint.as_deref())
                .await;
        }
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/.well-known/agent-card.json") => {
                let server = self.server.lock().await;
                match server.public_card_json() {
                    Ok(body) => {
                        if self.psk_secret.is_some() {
                            let peer = parse_unique_json(&body)
                                .ok()
                                .and_then(|value| canonical_json_sha256(&value).ok())
                                .and_then(|hash| {
                                    psk_peer_id(
                                        &format!("https://{}/a2a", server.profile.bind),
                                        &hash,
                                    )
                                    .ok()
                                });
                            let mut builder = Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json");
                            if let Some(peer) = peer {
                                builder = builder.header("x-rivo-psk-peer-id", peer.0);
                            }
                            builder
                                .body(Full::new(Bytes::from(body)))
                                .unwrap_or_else(|_| {
                                    empty_response(StatusCode::INTERNAL_SERVER_ERROR)
                                })
                        } else {
                            json_response(StatusCode::OK, body)
                        }
                    }
                    Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
                }
            }
            (&Method::POST, "/rivo-a2a-lan-v1/pairings") => {
                self.handle_pairing_begin(request).await
            }
            (&Method::POST, "/a2a") => {
                self.handle_task_route(request, certificate_fingerprint.as_deref())
                    .await
            }
            _ => empty_response(StatusCode::NOT_FOUND),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use p256::ecdsa::SigningKey;
    use p256::pkcs8::DecodePrivateKey;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
    use rivo_a2a::{parse_unique_json, sign_claim_proof};
    use rusqlite::Connection;
    use std::str::FromStr;
    use tokio::net::TcpListener;
    fn profile() -> ServiceProfile {
        ServiceProfile {
            name: "research-lan".into(),
            bind: SocketAddr::from_str("10.0.0.12:8443").unwrap(),
            skill_id: "research".into(),
            workspace_alias: "research".into(),
            capability_profile: "research".into(),
            policy_ceiling: "strict".into(),
            network_enabled: false,
            browser_enabled: false,
            mcp_servers: vec![],
            mcp_toolsets: BTreeMap::new(),
            per_peer_max_active_tasks: 1,
            max_active_tasks: 3,
            max_message_bytes: MAX_MESSAGE_BYTES,
            max_output_bytes: 256 * 1024,
            max_duration_seconds: 1800,
            approval_timeout_seconds: 300,
            allowed_peers: [A2aPeerId("peer-1".into())].into_iter().collect(),
            issuer_ref: "issuer".into(),
            idempotency_retention_seconds: 86400,
            audit_retention_days: 30,
        }
    }

    #[test]
    fn local_test_bind_permission_is_limited_to_explicit_loopback() {
        let loopback_v4: IpAddr = "127.0.0.1".parse().unwrap();
        let loopback_v6: IpAddr = "::1".parse().unwrap();
        let private_v4: IpAddr = "192.168.1.10".parse().unwrap();
        let public_v4: IpAddr = "203.0.113.10".parse().unwrap();

        assert!(!is_allowed_bind_address(loopback_v4, false));
        assert!(!is_allowed_bind_address(loopback_v6, false));
        assert!(is_allowed_bind_address(loopback_v4, true));
        assert!(is_allowed_bind_address(loopback_v6, true));
        assert!(is_allowed_bind_address(private_v4, false));
        assert!(is_allowed_bind_address(private_v4, true));
        assert!(!is_allowed_bind_address(public_v4, false));
        assert!(!is_allowed_bind_address(public_v4, true));
    }

    fn request(message: &str) -> Value {
        json!({"message": {"messageId":"message-1", "parts":[{"text":message}]}, "metadata":{"io.rivo.a2a.v1":{"skill_id":"research"}}})
    }
    fn local_approval_request() -> LocalApprovalRequest {
        LocalApprovalRequest {
            task_id: A2aTaskId("task-0000000000000001".into()),
            peer_id: A2aPeerId("peer-1".into()),
            skill_id: "coding".into(),
            turn_id: "turn-1".into(),
            request_id: "request-1".into(),
            call_id: "call-1".into(),
            tool_name: "write_file".into(),
            canonical_args_sha256: "a".repeat(64),
            arguments_summary: "写入一个受控文件".into(),
            targets: vec!["notes.txt".into()],
            program: None,
            side_effects: vec!["file_write".into()],
            redacted_summary: "等待服务端本机授权".into(),
            expires_unix: 500,
        }
    }
    #[test]
    fn profile_rejects_public_wildcard_and_full_control() {
        let mut p = profile();
        p.bind = "0.0.0.0:8443".parse().unwrap();
        assert_eq!(p.validate(), Err(ServerError::InvalidRequest));
        let mut p = profile();
        p.policy_ceiling = "full_control".into();
        assert_eq!(p.validate(), Err(ServerError::InvalidRequest));
    }

    #[test]
    fn instance_control_profile_is_strict_and_explicit() {
        let mut p = profile();
        p.name = "process-control-lan".into();
        p.skill_id = "process-control".into();
        p.workspace_alias = "instance-control".into();
        p.capability_profile = "instance_control".into();
        assert!(p.validate().is_ok());
        p.network_enabled = true;
        assert_eq!(p.validate(), Err(ServerError::InvalidRequest));
    }

    #[test]
    fn sqlite_approval_queue_binds_once_and_cancellation_invalidates_pending() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let state = ServerStateStore::open(&path).unwrap();
        let request = local_approval_request();
        let approval_id = state.enqueue_local_approval(&request).unwrap();
        let pending = state.pending_local_approvals(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].approval_id, approval_id);
        assert_eq!(pending[0].canonical_args_sha256, "a".repeat(64));
        assert_eq!(pending[0].targets, vec!["notes.txt"]);
        assert_eq!(pending[0].status, LocalApprovalStatus::Pending);

        // Same stable identity cannot be rebound to different canonical arguments.
        let mut changed = request.clone();
        changed.canonical_args_sha256 = "b".repeat(64);
        assert_ne!(changed.approval_id(), approval_id);
        state.enqueue_local_approval(&changed).unwrap();
        assert_eq!(state.pending_local_approvals(100).unwrap().len(), 2);

        state
            .decide_local_approval(&approval_id, true, 101)
            .unwrap();
        assert_eq!(
            state.take_local_approval(&approval_id, 102).unwrap(),
            Some(LocalApproval::ApprovedOnce)
        );
        assert_eq!(
            state.take_local_approval(&approval_id, 103).unwrap(),
            Some(LocalApproval::Unavailable)
        );

        let changed_id = changed.approval_id();
        assert_eq!(
            state.invalidate_task_approvals(&request.task_id).unwrap(),
            1
        );
        assert_eq!(
            state.take_local_approval(&changed_id, 104).unwrap(),
            Some(LocalApproval::Rejected)
        );
    }

    #[test]
    fn sqlite_approval_queue_expires_without_remote_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let state = ServerStateStore::open(&directory.path().join("state.db")).unwrap();
        let mut request = local_approval_request();
        request.expires_unix = 100;
        let approval_id = state.enqueue_local_approval(&request).unwrap();
        assert!(state.pending_local_approvals(100).unwrap().is_empty());
        assert_eq!(
            state.take_local_approval(&approval_id, 101).unwrap(),
            Some(LocalApproval::TimedOut)
        );
        assert_eq!(
            state.decide_local_approval(&approval_id, true, 102),
            Err(ServerError::LocalApprovalUnavailable)
        );
    }

    #[test]
    fn owner_queue_waits_for_separate_local_sqlite_decision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let queue = LocalApprovalQueue::open(path.clone()).unwrap();
        let state = ServerStateStore::open(&path).unwrap();
        let mut request = local_approval_request();
        request.expires_unix = unix_now().saturating_add(5);
        let approval_id = request.approval_id();
        let waiting = std::thread::spawn(move || queue.request_local_approval(&request));
        let mut visible = false;
        for _ in 0..20 {
            if state.pending_local_approvals(unix_now()).unwrap().len() == 1 {
                visible = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            visible,
            "queue request must be visible to a separate owner process"
        );
        state
            .decide_local_approval(&approval_id, true, unix_now())
            .unwrap();
        assert_eq!(waiting.join().unwrap(), LocalApproval::ApprovedOnce);
    }

    #[test]
    fn owner_queue_returns_rejected_when_task_is_cancelled_while_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let queue = LocalApprovalQueue::open(path.clone()).unwrap();
        let state = ServerStateStore::open(&path).unwrap();
        let mut request = local_approval_request();
        request.expires_unix = unix_now().saturating_add(5);
        let task_id = request.task_id.clone();
        let invalidator = queue.clone();
        let waiting = std::thread::spawn(move || queue.request_local_approval(&request));

        let mut visible = false;
        for _ in 0..20 {
            if state.pending_local_approvals(unix_now()).unwrap().len() == 1 {
                visible = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(visible, "queue request must be visible before cancellation");
        assert_eq!(invalidator.invalidate_task(&task_id).unwrap(), 1);
        assert_eq!(waiting.join().unwrap(), LocalApproval::Rejected);
    }

    #[test]
    fn card_is_private_and_does_not_expose_workspace() {
        let server = A2aServer::new(profile()).unwrap();
        let card = server.public_card_json().unwrap();
        assert!(card.contains("https://10.0.0.12:8443/a2a"));
        assert!(!card.contains("workspace"));
        assert!(canonical_json_sha256(&parse_unique_json(&card).unwrap()).is_ok());
    }
    #[test]
    fn task_idempotency_ownership_and_terminal_cas_hold() {
        let mut server = A2aServer::new(profile()).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let task = server
            .send_message(&peer, "message-1", request("hello"))
            .unwrap();
        assert_eq!(
            server
                .send_message(&peer, "message-1", request("hello"))
                .unwrap()
                .id,
            task.id
        );
        assert_eq!(
            server.send_message(&peer, "message-1", request("changed")),
            Err(ServerError::IdempotencyConflict)
        );
        assert_eq!(
            server.get_task(&A2aPeerId("peer-2".into()), &task.id),
            Err(ServerError::TaskNotFound)
        );
        server
            .transition(&peer, &task.id, A2aTaskState::Working, None)
            .unwrap();
        server
            .transition(&peer, &task.id, A2aTaskState::Completed, None)
            .unwrap();
        assert_eq!(
            server.transition(&peer, &task.id, A2aTaskState::Canceled, None),
            Err(ServerError::TaskTerminal)
        );
    }

    #[test]
    fn inbound_session_mapping_is_private_frozen_and_replay_does_not_start_second_session() {
        let mut server = A2aServer::new(profile()).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let reservation = server
            .reserve_inbound(&peer, "message-1", request("only this task"))
            .unwrap();
        assert!(reservation.newly_created);
        assert_eq!(reservation.start.profile.workspace_alias, "research");
        assert_eq!(reservation.start.profile.policy_ceiling, "strict");
        assert_eq!(reservation.start.message, "only this task");
        let wire = task_wire(&reservation.task).to_string();
        assert!(!wire.contains(&reservation.start.local_session_ref.0));
        assert!(!wire.contains("workspace_alias"));
        assert!(!wire.contains("only this task"));

        let replay = server
            .reserve_inbound(&peer, "message-1", request("only this task"))
            .unwrap();
        assert!(!replay.newly_created);
        assert_eq!(replay.task.id, reservation.task.id);
        assert!(replay.start.message.is_empty());
        assert_eq!(
            replay.start.local_session_ref,
            reservation.start.local_session_ref
        );

        assert_eq!(
            server.project_session_event(
                &peer,
                &reservation.task.id,
                &InboundSessionRef("forged-remote-ref".into()),
                InboundSessionEvent::Working,
            ),
            Err(ServerError::TaskNotFound)
        );
        let working = server
            .project_session_event(
                &peer,
                &reservation.task.id,
                &reservation.start.local_session_ref,
                InboundSessionEvent::Working,
            )
            .unwrap();
        assert_eq!(working.state, A2aTaskState::Working);
    }

    #[test]
    fn instance_control_profile_cannot_accept_generic_a2a_tasks() {
        let mut control_profile = profile();
        control_profile.name = "control-lan".into();
        control_profile.skill_id = "process-control".into();
        control_profile.capability_profile = "instance_control".into();
        control_profile.workspace_alias = "instance-control".into();
        let mut server = A2aServer::new(control_profile).unwrap();
        assert!(matches!(
            server.reserve_inbound(
                &A2aPeerId("peer-1".into()),
                "message-control",
                request("must use instance/control"),
            ),
            Err(ServerError::SkillNotAllowed)
        ));
    }

    #[test]
    fn local_approval_projection_is_redacted_and_never_accepts_remote_resolution() {
        let mut server = A2aServer::new(profile()).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let reservation = server
            .reserve_inbound(&peer, "message-1", request("secret input"))
            .unwrap();
        server
            .project_session_event(
                &peer,
                &reservation.task.id,
                &reservation.start.local_session_ref,
                InboundSessionEvent::Working,
            )
            .unwrap();
        let input_required = server
            .project_session_event(
                &peer,
                &reservation.task.id,
                &reservation.start.local_session_ref,
                InboundSessionEvent::LocalApprovalRequired,
            )
            .unwrap();
        assert_eq!(input_required.state, A2aTaskState::InputRequired);
        let text = task_wire(&input_required).to_string();
        assert!(text.contains("等待服务端本机授权"));
        assert!(!text.contains("secret input"));
        assert!(!text.contains(&reservation.start.local_session_ref.0));
        // A remote task transition is not an approval API; only the local driver can
        // project a matching local ref, and it may only return to WORKING/terminal.
        assert_eq!(
            server.transition(
                &peer,
                &reservation.task.id,
                A2aTaskState::Completed,
                Some(json!({"allow": true})),
            ),
            Err(ServerError::InvalidRequest)
        );
    }
    #[test]
    fn sse_starts_with_snapshot_and_monotonic_revision() {
        let mut server = A2aServer::new(profile()).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let task = server
            .send_message(&peer, "message-1", request("hello"))
            .unwrap();
        let snapshot = server.sse_snapshot(&peer, &task.id).unwrap();
        assert!(snapshot.starts_with("event: snapshot\nid: 1\ndata: "));
        server
            .transition(&peer, &task.id, A2aTaskState::Working, None)
            .unwrap();
        assert!(server
            .sse_snapshot(&peer, &task.id)
            .unwrap()
            .contains("id: 2"));
    }

    fn test_ca() -> (rcgen::Certificate, KeyPair) {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let certificate = params.self_signed(&key).unwrap();
        (certificate, key)
    }

    fn client_csr(key: &KeyPair) -> String {
        CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(key)
            .unwrap()
            .pem()
            .unwrap()
    }

    async fn begin_pairing(client: &reqwest::Client, base_url: &str, csr_pem: &str) -> Value {
        let response = client
            .post(format!("{base_url}/rivo-a2a-lan-v1/pairings"))
            .json(&json!({"version": LAN_PAIRING_VERSION, "csr_pem": csr_pem}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn claim_pairing(
        listener: &Arc<A2aHttpsListener>,
        client: &reqwest::Client,
        base_url: &str,
        pairing: &Value,
        client_key: &KeyPair,
    ) -> Value {
        let request_id = pairing["request_id"].as_str().unwrap();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(pairing["claim_token"].as_str().unwrap())
            .unwrap();
        let token: [u8; 32] = token.try_into().unwrap();
        // 模拟 owner-only `rivo a2a pairings approve` 对共享 SQLite 状态的裁决。
        // 领取 handler 会重新读取该状态，而非因为内存中存在 request 就默认批准。
        listener
            .state
            .as_ref()
            .unwrap()
            .lock()
            .await
            .set_pairing_state(
                &A2aRequestId(request_id.into()),
                PairingState::Approved,
                unix_now(),
            )
            .unwrap();
        let signing_key = SigningKey::from_pkcs8_pem(&client_key.serialize_pem()).unwrap();
        let proof = sign_claim_proof(&signing_key, request_id, &token).unwrap();
        let response = client
            .post(format!(
                "{base_url}/rivo-a2a-lan-v1/pairings/{request_id}/claim"
            ))
            .json(&json!({
                "version": LAN_PAIRING_VERSION,
                "request_id": request_id,
                "claim_token": pairing["claim_token"],
                "claim_proof": proof,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    fn mtls_client(
        ca: &rcgen::Certificate,
        claim: &Value,
        client_key: &KeyPair,
    ) -> reqwest::Client {
        let root = reqwest::Certificate::from_der(ca.der().as_ref()).unwrap();
        let pem = format!(
            "{}\n{}",
            claim["certificate_chain_pem"].as_str().unwrap(),
            client_key.serialize_pem()
        );
        reqwest::Client::builder()
            .add_root_certificate(root)
            .identity(reqwest::Identity::from_pem(pem.as_bytes()).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    struct WorkingFixtureDriver;

    #[async_trait::async_trait]
    impl InboundSessionDriver for WorkingFixtureDriver {
        async fn start(
            &self,
            request: InboundSessionStart,
            projector: Arc<dyn InboundSessionProjector>,
        ) -> Result<(), ServerError> {
            projector
                .project(
                    &request.peer_id,
                    &request.task_id,
                    &request.local_session_ref,
                    InboundSessionEvent::Working,
                )
                .await
                .map(|_| ())
        }

        async fn cancel(&self, _local_session: &InboundSessionRef) -> Result<(), ServerError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn https_mtls_pairing_task_and_sse_fixture_preserves_peer_isolation() {
        // This listener is deliberately pre-bound to loopback only under `cfg(test)`.
        // The production `serve` path still validates and binds only RFC1918/ULA addresses.
        let (ca, ca_key) = test_ca();
        let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let server_certificate = CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&server_key, &ca, &ca_key)
            .unwrap();
        let tls = ServerTlsMaterial {
            certificate_chain: vec![server_certificate.der().clone()],
            private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der())
                .into(),
            client_ca_certificates: vec![ca.der().clone()],
        };
        let spki = server_spki_sha256(&tls).unwrap();
        let mut fixture_profile = profile();
        fixture_profile.allowed_peers = [A2aPeerId("peer-0000000000000001".into())]
            .into_iter()
            .collect();
        let temporary = tempfile::tempdir().unwrap();
        let listener = Arc::new(
            A2aHttpsListener::new(
                A2aServer::new(fixture_profile).unwrap(),
                PairingRegistry::default(),
                spki,
                tls,
                Arc::new(KeyringClientCertificateIssuer {
                    material: ClientIssuerMaterial {
                        certificate: ca.der().clone(),
                        private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(
                            ca_key.serialize_der(),
                        )
                        .into(),
                    },
                }),
            )
            .unwrap()
            .with_state_store(ServerStateStore::open(&temporary.path().join("state.db")).unwrap())
            .with_session_driver(Arc::new(WorkingFixtureDriver)),
        );
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("https://{}", tcp.local_addr().unwrap());
        let serving = tokio::spawn(listener.clone().serve_test_listener(tcp));

        let anonymous = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_der(ca.der().as_ref()).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let card = anonymous
            .get(format!("{base_url}/.well-known/agent-card.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(card.status(), StatusCode::OK);
        assert!(card.text().await.unwrap().contains("mutualTLS"));

        // 无 client certificate 不能访问 task API，即使 Card 和 pairing endpoint 可匿名用。
        let unauthenticated = anonymous
            .post(format!("{base_url}/a2a"))
            .json(&json!({"jsonrpc":"2.0", "id":"no-cert", "method":"tasks/get", "params":{"id":"task-1"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let client_one_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let pairing_one = begin_pairing(&anonymous, &base_url, &client_csr(&client_one_key)).await;
        let claim_one = claim_pairing(
            &listener,
            &anonymous,
            &base_url,
            &pairing_one,
            &client_one_key,
        )
        .await;
        assert_eq!(claim_one["peer_id"], "peer-0000000000000001");
        let client_one = mtls_client(&ca, &claim_one, &client_one_key);

        let task_response = client_one
            .post(format!("{base_url}/a2a"))
            .json(&json!({
                "jsonrpc": "2.0", "id": "send-1", "method": "message/send",
                "params": request("verified mTLS task")
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(task_response.status(), StatusCode::OK);
        let task_body: Value = task_response.json().await.unwrap();
        let task_id = task_body["result"]["id"].as_str().unwrap().to_owned();
        assert_eq!(task_body["result"]["status"]["state"], "WORKING");

        let snapshot = client_one
            .post(format!("{base_url}/a2a"))
            .header("accept", "text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": "sse-1", "method": "tasks/resubscribe",
                "params": {"id": task_id}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(snapshot.status(), StatusCode::OK);
        assert_eq!(
            snapshot.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let snapshot_body = snapshot.text().await.unwrap();
        assert!(snapshot_body.starts_with("event: snapshot\nid: 2\ndata: "));
        assert!(snapshot_body.contains("WORKING"));

        // A separately paired certificate is mTLS-valid, but cannot enumerate peer one's
        // task through the resubscribe stream.
        let client_two_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let pairing_two = begin_pairing(&anonymous, &base_url, &client_csr(&client_two_key)).await;
        let claim_two = claim_pairing(
            &listener,
            &anonymous,
            &base_url,
            &pairing_two,
            &client_two_key,
        )
        .await;
        assert_eq!(claim_two["peer_id"], "peer-0000000000000002");
        let client_two = mtls_client(&ca, &claim_two, &client_two_key);
        let forbidden = client_two
            .post(format!("{base_url}/a2a"))
            .header("accept", "text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": "sse-other", "method": "tasks/resubscribe",
                "params": {"id": task_id}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            forbidden.text().await.unwrap(),
            r#"{"error":"a2a_task_not_found"}"#
        );

        serving.abort();
    }

    struct TestIssuer;
    impl ClientCertificateIssuer for TestIssuer {
        fn issue_client_certificate(
            &self,
            _peer_id: &A2aPeerId,
            _csr_pem: &str,
            _valid_for: Duration,
        ) -> Result<String, ServerError> {
            let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(|_| ServerError::InvalidRequest)?;
            CertificateParams::new(Vec::<String>::new())
                .map_err(|_| ServerError::InvalidRequest)?
                .self_signed(&key)
                .map(|certificate| certificate.pem())
                .map_err(|_| ServerError::InvalidRequest)
        }
    }

    #[test]
    fn pairing_requires_approval_valid_csr_proof_and_is_single_use() {
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(&key_pair)
            .unwrap()
            .pem()
            .unwrap();
        let mut registry = PairingRegistry::default();
        let challenge = registry.begin(&csr, &"a".repeat(64), 100).unwrap();
        assert_eq!(challenge.pairing.state, PairingState::Pending);
        assert_eq!(
            registry.claim(
                &challenge.pairing.request_id,
                &challenge.claim_token,
                "invalid",
                &TestIssuer,
                &profile(),
                101
            ),
            Err(ServerError::PairingNotApproved)
        );
        registry
            .approve(&challenge.pairing.request_id, 101)
            .unwrap();
        let signing_key = SigningKey::from_pkcs8_pem(&key_pair.serialize_pem()).unwrap();
        let proof = sign_claim_proof(
            &signing_key,
            &challenge.pairing.request_id.0,
            &challenge.claim_token,
        )
        .unwrap();
        let result = registry
            .claim(
                &challenge.pairing.request_id,
                &challenge.claim_token,
                &proof,
                &TestIssuer,
                &profile(),
                101,
            )
            .unwrap();
        assert!(result.certificate_chain_pem.contains("CERTIFICATE"));
        assert_eq!(
            registry.claim(
                &challenge.pairing.request_id,
                &challenge.claim_token,
                &proof,
                &TestIssuer,
                &profile(),
                102,
            ),
            Err(ServerError::PairingNotApproved)
        );
        assert!(registry.revoke(&result.peer_id));
    }

    #[test]
    fn keychain_issuer_output_is_a_client_auth_der_fingerprinted_chain() {
        use x509_parser::prelude::{FromDer, X509Certificate};
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let client_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(&client_key)
            .unwrap()
            .pem()
            .unwrap();
        let issuer = KeyringClientCertificateIssuer {
            material: ClientIssuerMaterial {
                certificate: ca.der().clone(),
                private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(ca_key.serialize_der())
                    .into(),
            },
        };
        let chain = issuer
            .issue_client_certificate(
                &A2aPeerId("peer-0000000000000001".into()),
                &csr,
                Duration::from_secs(365 * 24 * 60 * 60),
            )
            .unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(chain.as_bytes()).unwrap();
        let (_, certificate) = X509Certificate::from_der(&pem.contents).unwrap();
        assert!(
            certificate
                .extended_key_usage()
                .unwrap()
                .unwrap()
                .value
                .client_auth
        );
        assert_eq!(
            pairing::certificate_leaf_fingerprint(&chain).unwrap(),
            hex::encode(Sha256::digest(pem.contents))
        );
    }

    struct RejectingBroker;
    impl ApprovalBroker for RejectingBroker {
        fn request_local_approval(&self, _request: &LocalApprovalRequest) -> LocalApproval {
            LocalApproval::Rejected
        }
    }

    #[test]
    fn remote_never_approves_local_side_effects() {
        let mut server = A2aServer::new(profile()).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let task = server
            .send_message(&peer, "message-1", request("hello"))
            .unwrap();
        server
            .transition(&peer, &task.id, A2aTaskState::Working, None)
            .unwrap();
        let rejected = server
            .request_local_approval(&peer, &task.id, Some(&RejectingBroker), 100)
            .unwrap();
        assert_eq!(rejected.state, A2aTaskState::Rejected);

        let task = server
            .send_message(&peer, "message-2", {
                let mut value = request("hello again");
                value["message"]["messageId"] = Value::String("message-2".into());
                value
            })
            .unwrap();
        server
            .transition(&peer, &task.id, A2aTaskState::Working, None)
            .unwrap();
        assert_eq!(
            server.request_local_approval(&peer, &task.id, None, 100),
            Err(ServerError::LocalApprovalUnavailable)
        );
        assert_eq!(
            server.get_task(&peer, &task.id).unwrap().state,
            A2aTaskState::Rejected
        );
    }

    #[test]
    fn tls_material_builds_optional_mtls_config() {
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let server = CertificateParams::new(vec!["10.0.0.12".into()])
            .unwrap()
            .signed_by(&server_key, &ca, &ca_key)
            .unwrap();
        let material = ServerTlsMaterial {
            certificate_chain: vec![server.der().clone()],
            private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der())
                .into(),
            client_ca_certificates: vec![ca.der().clone()],
        };
        assert!(material.build_config().is_ok());
    }

    #[test]
    fn psk_tls_material_only_enables_tls_13() {
        let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let server = CertificateParams::new(vec!["10.0.0.12".into()])
            .unwrap()
            .self_signed(&server_key)
            .unwrap();
        let material = ServerTlsMaterial {
            certificate_chain: vec![server.der().clone()],
            private_key: rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der())
                .into(),
            client_ca_certificates: Vec::new(),
        };

        let server_config = material.build_psk_config().unwrap();
        let client_config =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from("10.0.0.12").unwrap();
        let mut client =
            rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let mut client_hello = Vec::new();
        client.write_tls(&mut client_hello).unwrap();

        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        server
            .read_tls(&mut std::io::Cursor::new(client_hello))
            .unwrap();
        assert!(server.process_new_packets().is_err());
    }

    #[test]
    fn server_keychain_identity_refs_cannot_escape_the_local_namespace() {
        assert!(validate_identity_ref("a2a-server-research").is_ok());
        assert!(validate_identity_ref("../../key").is_err());
        assert!(validate_identity_ref("").is_err());
        assert!(validate_identity_ref(&"a".repeat(97)).is_err());
    }

    #[test]
    fn server_file_config_requires_registered_canonical_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("research");
        std::fs::create_dir(&workspace).unwrap();
        let config = A2aServerFileConfig {
            schema_version: A2A_SERVER_SCHEMA_VERSION,
            state_db: PathBuf::from("server.db"),
            workspace_aliases: [("research".into(), PathBuf::from("research"))]
                .into_iter()
                .collect(),
            profiles: vec![ConfiguredServiceProfile {
                profile: profile(),
                identity_ref: "a2a-server-research".into(),
            }],
        };
        config.validate(directory.path()).unwrap();
        let mut bad = config;
        bad.profiles[0].profile.workspace_alias = "remote-path".into();
        assert_eq!(
            bad.validate(directory.path()),
            Err(ServerError::InvalidRequest)
        );
    }

    #[test]
    fn sqlite_state_recovers_unfinished_tasks_without_replaying() {
        let directory = tempfile::tempdir().unwrap();
        let state = ServerStateStore::open(&directory.path().join("state.db")).unwrap();
        let peer = A2aPeerId("peer-1".into());
        state
            .record_task(
                &PersistedServerTask {
                    profile_name: "research-lan".into(),
                    peer_id: peer,
                    message_id: "message-1".into(),
                    request_hash: "a".repeat(64),
                    task: A2aTask {
                        id: A2aTaskId("task-1".into()),
                        state: A2aTaskState::Working,
                        revision: Some(2),
                        message: None,
                        artifacts: vec![],
                    },
                },
                100,
            )
            .unwrap();
        assert_eq!(state.recover_unfinished("research-lan", 101).unwrap(), 1);
        assert_eq!(state.recover_unfinished("research-lan", 102).unwrap(), 0);
    }

    #[test]
    fn sqlite_state_rejects_pre_recovery_schema_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE a2a_server_tasks(task_id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL, updated_unix INTEGER NOT NULL); INSERT INTO a2a_server_tasks VALUES('task-old-1','peer-1','COMPLETED',2,100); INSERT INTO a2a_server_tasks VALUES('task-old-2','peer-1','FAILED',2,100);").unwrap();
        drop(connection);
        assert!(matches!(
            ServerStateStore::open(&path),
            Err(ServerError::UnsupportedStateSchema)
        ));
        let connection = Connection::open(&path).unwrap();
        let rows: Vec<(String, String)> = connection
            .prepare("SELECT task_id,state FROM a2a_server_tasks ORDER BY task_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("task-old-1".into(), "COMPLETED".into()),
                ("task-old-2".into(), "FAILED".into())
            ]
        );
        let has_schema_version: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='a2a_server_schema_version')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!has_schema_version);
    }

    #[test]
    fn restart_restores_terminal_mapping_but_fails_active_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let state = ServerStateStore::open(&directory.path().join("state.db")).unwrap();
        let peer = A2aPeerId("peer-1".into());
        let original = request("same request");
        let hash = canonical_json_sha256(&original).unwrap();
        for (id, message_id, task_state, revision) in [
            (
                "task-0000000000000007",
                "message-terminal",
                A2aTaskState::Completed,
                3,
            ),
            (
                "task-0000000000000008",
                "message-active",
                A2aTaskState::Working,
                2,
            ),
        ] {
            state
                .record_task(
                    &PersistedServerTask {
                        profile_name: "research-lan".into(),
                        peer_id: peer.clone(),
                        message_id: message_id.into(),
                        request_hash: if message_id == "message-terminal" {
                            hash.clone()
                        } else {
                            "b".repeat(64)
                        },
                        task: A2aTask {
                            id: A2aTaskId(id.into()),
                            state: task_state,
                            revision: Some(revision),
                            message: None,
                            artifacts: Vec::new(),
                        },
                    },
                    100,
                )
                .unwrap();
        }
        assert_eq!(state.recover_unfinished("research-lan", 101).unwrap(), 1);
        let mut restored =
            A2aServer::restore(profile(), state.load_tasks("research-lan").unwrap()).unwrap();
        let terminal = A2aTaskId("task-0000000000000007".into());
        let failed = A2aTaskId("task-0000000000000008".into());
        assert_eq!(
            restored.get_task(&peer, &terminal).unwrap().state,
            A2aTaskState::Completed
        );
        assert_eq!(
            restored.get_task(&peer, &failed).unwrap().state,
            A2aTaskState::Failed
        );
        // Same key returns the persisted terminal mapping without a new Session reservation;
        // a changed body cannot steal the key after process restart.
        assert_eq!(
            restored
                .send_message(&peer, "message-terminal", original)
                .unwrap()
                .id,
            terminal
        );
        assert_eq!(
            restored.send_message(&peer, "message-terminal", request("changed")),
            Err(ServerError::IdempotencyConflict)
        );
        assert_eq!(
            restored.get_task(&A2aPeerId("peer-2".into()), &terminal),
            Err(ServerError::TaskNotFound)
        );
        // The generated task sequence also advances beyond recovered IDs, avoiding an ID
        // collision with a terminal mapping.
        let created = restored
            .send_message(&peer, "message-new", request("new"))
            .unwrap();
        assert_eq!(created.id, A2aTaskId("task-0000000000000009".into()));
    }

    #[test]
    fn peer_grant_reloads_with_profile_boundary_and_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let state = ServerStateStore::open(&directory.path().join("state.db")).unwrap();
        let grant = PeerGrant {
            profile_name: "research-lan".into(),
            peer_id: A2aPeerId("peer-0000000000000007".into()),
            issuer_ref: "issuer".into(),
            certificate_serial: "01ab".into(),
            certificate_fingerprint: "c".repeat(64),
            expires_unix: 200,
            revoked: false,
        };
        state.save_peer_grant(&grant).unwrap();
        let registry =
            PairingRegistry::from_peer_grants(state.load_peer_grants("research-lan").unwrap());
        registry
            .authenticate(&grant.peer_id, &grant.certificate_fingerprint, 100)
            .unwrap();
        assert_eq!(
            PairingRegistry::from_peer_grants(state.load_peer_grants("other-profile").unwrap())
                .authenticate(&grant.peer_id, &grant.certificate_fingerprint, 100),
            Err(ServerError::PeerNotTrusted)
        );
        let mut revoked = grant.clone();
        revoked.revoked = true;
        state.save_peer_grant(&revoked).unwrap();
        assert_eq!(
            PairingRegistry::from_peer_grants(state.load_peer_grants("research-lan").unwrap())
                .authenticate(&grant.peer_id, &grant.certificate_fingerprint, 100),
            Err(ServerError::PeerNotTrusted)
        );
    }

    #[test]
    fn sqlite_pairing_decision_is_pending_only_and_never_stores_secret_fields() {
        let directory = tempfile::tempdir().unwrap();
        let state = ServerStateStore::open(&directory.path().join("state.db")).unwrap();
        let pairing = PendingPairing {
            request_id: A2aRequestId("pair-1".into()),
            client_public_key_sha256: "a".repeat(64),
            server_spki_sha256: "b".repeat(64),
            sas_words: std::array::from_fn(|_| "word".into()),
            expires_unix: 200,
            state: PairingState::Pending,
            claim_token_sha256: "c".repeat(64),
            csr_pem: "public-but-not-persisted".into(),
        };
        state.save_pairing_summary(&pairing).unwrap();
        state
            .set_pairing_state(&pairing.request_id, PairingState::Approved, 100)
            .unwrap();
        assert_eq!(
            state.pairing_state(&pairing.request_id, 100).unwrap(),
            PairingState::Approved
        );
        assert_eq!(
            state.set_pairing_state(&pairing.request_id, PairingState::Rejected, 100),
            Err(ServerError::InvalidPairing)
        );
        let raw = std::fs::read(directory.path().join("state.db")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("public-but-not-persisted"));
        assert!(!String::from_utf8_lossy(&raw).contains(&pairing.claim_token_sha256));
    }
}

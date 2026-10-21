use crate::{
    LanguageError, LanguageResource, LocaleId, EMBEDDED_EN_US, MAX_DISPLAY_NAME_CHARS,
    MAX_LANGUAGE_FILE_BYTES, MAX_REVISION_CHARS,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::Duration;
use thiserror::Error;

pub const MAX_LANGUAGE_REGISTRY_BYTES: usize = 256 * 1024;
pub const MAX_LANGUAGE_REGISTRY_ENTRIES: usize = 256;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone)]
pub struct RemoteLanguageClient {
    client: reqwest::Client,
}

impl RemoteLanguageClient {
    pub fn new(user_agent: &str) -> Result<Self, RemoteLanguageError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .https_only(true)
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let url = attempt.url();
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many language download redirects")
                } else if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    attempt.error("language download redirect must use HTTPS without userinfo")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| RemoteLanguageError::HttpClient)?;
        Ok(Self { client })
    }

    pub async fn fetch_registry(
        &self,
        registry_url: &str,
    ) -> Result<RemoteLanguageRegistry, RemoteLanguageError> {
        let url = parse_https_url(registry_url)?;
        let bytes = self.fetch_bounded(url, MAX_LANGUAGE_REGISTRY_BYTES).await?;
        RemoteLanguageRegistry::parse(&bytes)
    }

    pub async fn fetch_package(
        &self,
        entry: RemoteLanguageEntry,
    ) -> Result<RemoteLanguagePackage, RemoteLanguageError> {
        let bytes = self
            .fetch_bounded(entry.download_url.clone(), entry.bytes)
            .await?;
        RemoteLanguagePackage::validate(entry, bytes)
    }

    async fn fetch_bounded(
        &self,
        url: Url,
        maximum: usize,
    ) -> Result<Vec<u8>, RemoteLanguageError> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| RemoteLanguageError::Network)?;
        if !response.status().is_success() {
            return Err(RemoteLanguageError::HttpStatus(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > maximum as u64)
        {
            return Err(RemoteLanguageError::ResponseTooLarge { maximum });
        }

        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0)
                .min(maximum),
        );
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| RemoteLanguageError::Network)?
        {
            if bytes.len().saturating_add(chunk.len()) > maximum {
                return Err(RemoteLanguageError::ResponseTooLarge { maximum });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLanguageEntry {
    pub locale: LocaleId,
    pub display_name: String,
    pub revision: String,
    pub download_url: Url,
    pub bytes: usize,
    pub sha256: String,
}

impl RemoteLanguageEntry {
    pub fn receipt_url(&self) -> String {
        let mut url = self.download_url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteLanguageReceipt {
    pub schema: u32,
    pub locale: String,
    pub registry_url: String,
    pub download_url: String,
    pub revision: String,
    pub bytes: usize,
    pub sha256: String,
    pub installed_at_unix: u64,
}

impl RemoteLanguageReceipt {
    pub fn new(
        registry_url: &str,
        entry: &RemoteLanguageEntry,
        installed_at_unix: u64,
    ) -> Result<Self, RemoteLanguageError> {
        Ok(Self {
            schema: 1,
            locale: entry.locale.to_string(),
            registry_url: sanitize_https_url(registry_url)?,
            download_url: entry.receipt_url(),
            revision: entry.revision.clone(),
            bytes: entry.bytes,
            sha256: entry.sha256.clone(),
            installed_at_unix,
        })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, RemoteLanguageError> {
        let receipt: Self = serde_json::from_slice(bytes)?;
        if receipt.schema != 1 {
            return Err(RemoteLanguageError::UnsupportedReceiptSchema(
                receipt.schema,
            ));
        }
        let locale = LocaleId::parse(&receipt.locale)?;
        if locale.to_string() != receipt.locale {
            return Err(RemoteLanguageError::NonCanonicalLocale(receipt.locale));
        }
        if sanitize_https_url(&receipt.registry_url)? != receipt.registry_url
            || sanitize_https_url(&receipt.download_url)? != receipt.download_url
        {
            return Err(RemoteLanguageError::ReceiptContainsSensitiveUrl);
        }
        validate_short_text("revision", &receipt.revision, MAX_REVISION_CHARS)?;
        if receipt.bytes > MAX_LANGUAGE_FILE_BYTES {
            return Err(RemoteLanguageError::InvalidFileSize(receipt.bytes as u64));
        }
        validate_sha256(&receipt.sha256)?;
        Ok(receipt)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, RemoteLanguageError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn locale_id(&self) -> Result<LocaleId, RemoteLanguageError> {
        Ok(LocaleId::parse(&self.locale)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLanguageRegistry {
    entries: Vec<RemoteLanguageEntry>,
}

impl RemoteLanguageRegistry {
    pub fn parse(bytes: &[u8]) -> Result<Self, RemoteLanguageError> {
        if bytes.len() > MAX_LANGUAGE_REGISTRY_BYTES {
            return Err(RemoteLanguageError::RegistryTooLarge {
                actual: bytes.len(),
                maximum: MAX_LANGUAGE_REGISTRY_BYTES,
            });
        }
        let wire: RegistryWire = serde_json::from_slice(bytes)?;
        if wire.schema != 1 {
            return Err(RemoteLanguageError::UnsupportedRegistrySchema(wire.schema));
        }
        if wire.languages.len() > MAX_LANGUAGE_REGISTRY_ENTRIES {
            return Err(RemoteLanguageError::TooManyEntries {
                actual: wire.languages.len(),
                maximum: MAX_LANGUAGE_REGISTRY_ENTRIES,
            });
        }

        let mut locales = BTreeSet::new();
        let mut entries = Vec::with_capacity(wire.languages.len());
        for candidate in wire.languages {
            let locale = LocaleId::parse(&candidate.locale)?;
            if locale.to_string() != candidate.locale {
                return Err(RemoteLanguageError::NonCanonicalLocale(candidate.locale));
            }
            if !locales.insert(locale.clone()) {
                return Err(RemoteLanguageError::DuplicateLocale(locale));
            }
            validate_short_text(
                "display_name",
                &candidate.display_name,
                MAX_DISPLAY_NAME_CHARS,
            )?;
            validate_short_text("revision", &candidate.revision, MAX_REVISION_CHARS)?;
            let download_url = parse_https_url(&candidate.download_url)?;
            let bytes = usize::try_from(candidate.bytes)
                .map_err(|_| RemoteLanguageError::InvalidFileSize(candidate.bytes))?;
            if bytes > MAX_LANGUAGE_FILE_BYTES {
                return Err(RemoteLanguageError::InvalidFileSize(candidate.bytes));
            }
            validate_sha256(&candidate.sha256)?;
            entries.push(RemoteLanguageEntry {
                locale,
                display_name: candidate.display_name,
                revision: candidate.revision,
                download_url,
                bytes,
                sha256: candidate.sha256,
            });
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[RemoteLanguageEntry] {
        &self.entries
    }

    pub fn find(&self, locale: &LocaleId) -> Option<&RemoteLanguageEntry> {
        self.entries.iter().find(|entry| &entry.locale == locale)
    }
}

#[derive(Debug)]
pub struct RemoteLanguagePackage {
    entry: RemoteLanguageEntry,
    bytes: Vec<u8>,
}

impl RemoteLanguagePackage {
    pub fn validate(
        entry: RemoteLanguageEntry,
        bytes: Vec<u8>,
    ) -> Result<Self, RemoteLanguageError> {
        if bytes.len() != entry.bytes {
            return Err(RemoteLanguageError::LengthMismatch {
                expected: entry.bytes,
                actual: bytes.len(),
            });
        }
        let actual_sha256 = sha256_hex(&bytes);
        if actual_sha256 != entry.sha256 {
            return Err(RemoteLanguageError::Sha256Mismatch {
                expected: entry.sha256,
                actual: actual_sha256,
            });
        }

        let canonical =
            LanguageResource::parse(EMBEDDED_EN_US.as_bytes(), &LocaleId::parse("en-US")?, None)?;
        let resource =
            LanguageResource::parse(&bytes, &entry.locale, Some(canonical.message_schema()))?;
        if resource.metadata().display_name != entry.display_name
            || resource.metadata().revision != entry.revision
        {
            return Err(RemoteLanguageError::MetadataMismatch);
        }
        Ok(Self { entry, bytes })
    }

    pub fn entry(&self) -> &RemoteLanguageEntry {
        &self.entry
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Error)]
pub enum RemoteLanguageError {
    #[error(transparent)]
    Language(#[from] LanguageError),
    #[error("language registry is {actual} bytes; maximum is {maximum}")]
    RegistryTooLarge { actual: usize, maximum: usize },
    #[error("language registry JSON is invalid: {0}")]
    InvalidRegistry(#[from] serde_json::Error),
    #[error("unsupported language registry schema: {0}")]
    UnsupportedRegistrySchema(u32),
    #[error("unsupported language receipt schema: {0}")]
    UnsupportedReceiptSchema(u32),
    #[error("language registry contains {actual} entries; maximum is {maximum}")]
    TooManyEntries { actual: usize, maximum: usize },
    #[error("language registry locale is not canonical: {0}")]
    NonCanonicalLocale(String),
    #[error("language registry contains duplicate locale: {0}")]
    DuplicateLocale(LocaleId),
    #[error("language registry {field} is invalid")]
    InvalidText { field: &'static str },
    #[error("language URL must be HTTPS without userinfo")]
    InvalidUrl,
    #[error("language receipt URL cannot contain query or fragment")]
    ReceiptContainsSensitiveUrl,
    #[error("cannot create the language HTTP client")]
    HttpClient,
    #[error("language HTTP request failed")]
    Network,
    #[error("language HTTP request returned status {0}")]
    HttpStatus(u16),
    #[error("language HTTP response exceeds {maximum} bytes")]
    ResponseTooLarge { maximum: usize },
    #[error("language file size is invalid: {0}")]
    InvalidFileSize(u64),
    #[error("language SHA-256 must be 64 lowercase hexadecimal characters")]
    InvalidSha256,
    #[error("language file length differs from registry: expected {expected}, found {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("language file SHA-256 differs from registry: expected {expected}, found {actual}")]
    Sha256Mismatch { expected: String, actual: String },
    #[error("language file metadata differs from registry")]
    MetadataMismatch,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryWire {
    schema: u32,
    languages: Vec<EntryWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryWire {
    locale: String,
    display_name: String,
    revision: String,
    download_url: String,
    bytes: u64,
    sha256: String,
}

fn parse_https_url(value: &str) -> Result<Url, RemoteLanguageError> {
    let url = Url::parse(value).map_err(|_| RemoteLanguageError::InvalidUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(RemoteLanguageError::InvalidUrl);
    }
    Ok(url)
}

fn sanitize_https_url(value: &str) -> Result<String, RemoteLanguageError> {
    let mut url = parse_https_url(value)?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn validate_sha256(value: &str) -> Result<(), RemoteLanguageError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(RemoteLanguageError::InvalidSha256)
    }
}

fn validate_short_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), RemoteLanguageError> {
    if value.is_empty() || value.chars().count() > maximum || value.chars().any(char::is_control) {
        Err(RemoteLanguageError::InvalidText { field })
    } else {
        Ok(())
    }
}

/// Lowercase hex SHA-256; sha2 0.11 digests no longer implement `LowerHex`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    fn package() -> Vec<u8> {
        b"### rivo-language-schema: 1\n### locale: fr-FR\n### display-name: Francais\n### revision: test.1\n\ncommon-ok = Oui\n".to_vec()
    }

    fn registry(file: &[u8], url: &str) -> Vec<u8> {
        format!(
            r#"{{"schema":1,"languages":[{{"locale":"fr-FR","display_name":"Francais","revision":"test.1","download_url":"{url}","bytes":{},"sha256":"{}"}}]}}"#,
            file.len(),
            sha256_hex(file)
        )
        .into_bytes()
    }

    async fn https_fixture(
        package: Vec<u8>,
    ) -> (String, reqwest::Client, tokio::task::JoinHandle<()>) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let server = CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&server_key, &ca, &ca_key)
            .unwrap();
        let tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![server.der().clone()],
                PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("https://{}", listener.local_addr().unwrap());
        let registry = registry(&package, &format!("{base_url}/fr-FR.ftl"));
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let serving = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                let mut request = vec![0_u8; 4096];
                let length = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /index.json ") {
                    &registry
                } else {
                    &package
                };
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let client = reqwest::Client::builder()
            .https_only(true)
            .add_root_certificate(reqwest::Certificate::from_der(ca.der().as_ref()).unwrap())
            .build()
            .unwrap();
        (base_url, client, serving)
    }

    #[test]
    fn registry_accepts_bounded_https_entries_and_redacts_receipt_url() {
        let file = package();
        let registry = RemoteLanguageRegistry::parse(&registry(
            &file,
            "https://languages.example/fr-FR.ftl?token=secret#fragment",
        ))
        .unwrap();
        let entry = registry.entries().first().unwrap();
        assert_eq!(entry.locale.to_string(), "fr-FR");
        assert_eq!(entry.receipt_url(), "https://languages.example/fr-FR.ftl");
        RemoteLanguagePackage::validate(entry.clone(), file).unwrap();
    }

    #[test]
    fn registry_rejects_http_userinfo_and_duplicate_locales() {
        let file = package();
        assert!(matches!(
            RemoteLanguageRegistry::parse(&registry(&file, "http://languages.example/fr-FR.ftl")),
            Err(RemoteLanguageError::InvalidUrl)
        ));
        assert!(matches!(
            RemoteLanguageRegistry::parse(&registry(
                &file,
                "https://user:secret@languages.example/fr-FR.ftl"
            )),
            Err(RemoteLanguageError::InvalidUrl)
        ));

        let mut duplicate: serde_json::Value =
            serde_json::from_slice(&registry(&file, "https://languages.example/fr-FR.ftl"))
                .unwrap();
        let entry = duplicate["languages"][0].clone();
        duplicate["languages"].as_array_mut().unwrap().push(entry);
        assert!(matches!(
            RemoteLanguageRegistry::parse(&serde_json::to_vec(&duplicate).unwrap()),
            Err(RemoteLanguageError::DuplicateLocale(_))
        ));
    }

    #[test]
    fn package_rejects_length_hash_and_metadata_mismatches() {
        let file = package();
        let registry =
            RemoteLanguageRegistry::parse(&registry(&file, "https://languages.example/fr-FR.ftl"))
                .unwrap();
        let entry = registry.entries()[0].clone();
        assert!(matches!(
            RemoteLanguagePackage::validate(entry.clone(), file[..file.len() - 1].to_vec()),
            Err(RemoteLanguageError::LengthMismatch { .. })
        ));

        let mut altered = file.clone();
        *altered.last_mut().unwrap() = b'!';
        assert!(matches!(
            RemoteLanguagePackage::validate(entry.clone(), altered),
            Err(RemoteLanguageError::Sha256Mismatch { .. })
        ));

        let mut wrong_metadata = entry;
        wrong_metadata.display_name = "French".to_owned();
        assert!(matches!(
            RemoteLanguagePackage::validate(wrong_metadata, file),
            Err(RemoteLanguageError::MetadataMismatch)
        ));
    }

    #[test]
    fn receipt_removes_sensitive_url_parts_and_round_trips() {
        let file = package();
        let registry = RemoteLanguageRegistry::parse(&registry(
            &file,
            "https://languages.example/fr-FR.ftl?token=secret",
        ))
        .unwrap();
        let receipt = RemoteLanguageReceipt::new(
            "https://languages.example/index.json?token=secret#value",
            &registry.entries()[0],
            42,
        )
        .unwrap();
        assert_eq!(receipt.registry_url, "https://languages.example/index.json");
        assert_eq!(receipt.download_url, "https://languages.example/fr-FR.ftl");
        assert_eq!(
            RemoteLanguageReceipt::parse(&receipt.to_bytes().unwrap()).unwrap(),
            receipt
        );
    }

    #[tokio::test]
    async fn client_fetches_registry_and_package_over_trusted_https() {
        let expected = package();
        let (base_url, client, serving) = https_fixture(expected.clone()).await;
        let client = RemoteLanguageClient { client };
        let registry = client
            .fetch_registry(&format!("{base_url}/index.json"))
            .await
            .unwrap();
        let package = client
            .fetch_package(registry.entries()[0].clone())
            .await
            .unwrap();
        assert_eq!(package.bytes(), expected);
        serving.await.unwrap();
    }
}

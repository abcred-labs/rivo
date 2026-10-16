use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use fs2::FileExt;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::SessionError;

const FORMAT_VERSION: u32 = 1;
const PREFIX: &str = "enc:v1:";
// This protects against accidental plaintext disclosure only. It is deliberately not presented as
// a replacement for an OS keychain or a user-provided passphrase.
const APPLICATION_KEY: [u8; 32] = [
    0x9b, 0xe4, 0x32, 0x71, 0x17, 0x65, 0x2a, 0x8c, 0x41, 0x0d, 0xd6, 0x8f, 0x35, 0xb9, 0x53, 0x20,
    0x79, 0x1a, 0xc4, 0x6e, 0xaf, 0x08, 0x95, 0x3b, 0xe2, 0x54, 0x60, 0x1f, 0x8a, 0xcd, 0x44, 0xf0,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialStatus {
    Encrypted,
    Plaintext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialDocument {
    #[serde(default = "default_format_version")]
    format_version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

impl Default for CredentialDocument {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

fn default_format_version() -> u32 {
    0
}

pub fn credentials_file_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("credentials.toml")
}

pub fn credential_names(
    config_path: &Path,
) -> Result<Vec<(String, CredentialStatus)>, SessionError> {
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let document = read_document(&path)?;
        Ok(document
            .credentials
            .into_iter()
            .map(|(name, value)| {
                let status = if value.starts_with(PREFIX) {
                    CredentialStatus::Encrypted
                } else {
                    CredentialStatus::Plaintext
                };
                (name, status)
            })
            .collect())
    })
}

pub fn credential_exists(config_path: &Path, name: &str) -> Result<bool, SessionError> {
    upgrade_credentials(config_path)?;
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let document = read_document(&path)?;
        Ok(document.credentials.contains_key(name))
    })
}

pub fn get_credential(config_path: &Path, name: &str) -> Result<Option<String>, SessionError> {
    upgrade_credentials(config_path)?;
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let mut document = read_document(&path)?;
        let Some(value) = document.credentials.get(name).cloned() else {
            return Ok(None);
        };
        if value.starts_with(PREFIX) {
            return decrypt(name, &value).map(Some);
        }
        let encrypted = encrypt(name, &value)?;
        document.credentials.insert(name.to_owned(), encrypted);
        write_document(&path, &document)?;
        Ok(Some(value))
    })
}

pub fn set_credential(config_path: &Path, name: &str, value: &str) -> Result<(), SessionError> {
    validate_name(name)?;
    if value.trim().is_empty() {
        return Err(SessionError::Credential("API key 不能为空".into()));
    }
    upgrade_credentials(config_path)?;
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let mut document = read_document(&path)?;
        document
            .credentials
            .insert(name.to_owned(), encrypt(name, value)?);
        write_document(&path, &document)
    })
}

pub fn unset_credential(config_path: &Path, name: &str) -> Result<(), SessionError> {
    upgrade_credentials(config_path)?;
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let mut document = read_document(&path)?;
        document.credentials.remove(name);
        write_document(&path, &document)
    })
}

pub fn migrate_credentials(config_path: &Path) -> Result<usize, SessionError> {
    upgrade_credentials(config_path)
}

/// Upgrade the whole Credential document on write-capable paths so users do
/// not retain a mixture of legacy plaintext and current encrypted values.
pub fn upgrade_credentials(config_path: &Path) -> Result<usize, SessionError> {
    let path = credentials_file_path(config_path);
    with_lock(&path, || {
        let mut document = read_unvalidated_document(&path)?;
        if document.format_version > FORMAT_VERSION {
            return Err(SessionError::Credential(format!(
                "凭据文件版本 {} 高于当前支持的版本 {}；程序不会覆盖原文件",
                document.format_version, FORMAT_VERSION
            )));
        }
        if document.format_version != 0 && document.format_version != FORMAT_VERSION {
            return Err(SessionError::Credential(format!(
                "不支持的凭据文件版本: {}",
                document.format_version
            )));
        }
        let mut migrated = 0;
        for (name, value) in &mut document.credentials {
            if !value.starts_with(PREFIX) {
                *value = encrypt(name, value)?;
                migrated += 1;
            }
        }
        let needs_version_upgrade = document.format_version != FORMAT_VERSION;
        document.format_version = FORMAT_VERSION;
        if migrated > 0 || needs_version_upgrade {
            write_document(&path, &document)?;
        }
        Ok(migrated)
    })
}

fn validate_name(name: &str) -> Result<(), SessionError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(SessionError::Credential(
            "凭据名称只能包含 ASCII 字母、数字、_ 或 -，且长度为 1..=128".into(),
        ));
    }
    Ok(())
}

fn with_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    let parent = path
        .parent()
        .ok_or_else(|| SessionError::Credential("无法确定凭据目录".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| SessionError::Credential(error.to_string()))?;
    let lock_path = path.with_extension("toml.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| SessionError::Credential(format!("无法打开凭据锁: {error}")))?;
    lock.lock_exclusive()
        .map_err(|error| SessionError::Credential(format!("无法锁定凭据文件: {error}")))?;
    let result = operation();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

fn read_document(path: &Path) -> Result<CredentialDocument, SessionError> {
    let document = read_unvalidated_document(path)?;
    if document.format_version != FORMAT_VERSION {
        return Err(SessionError::Credential(format!(
            "凭据文件需要升级或版本不受支持: {}",
            document.format_version
        )));
    }
    Ok(document)
}

fn read_unvalidated_document(path: &Path) -> Result<CredentialDocument, SessionError> {
    if !path.exists() {
        return Ok(CredentialDocument::default());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| SessionError::Credential(format!("无法读取凭据文件: {error}")))?;
    let document: CredentialDocument = toml::from_str(&content)
        .map_err(|error| SessionError::Credential(format!("凭据文件格式错误: {error}")))?;
    Ok(document)
}

fn write_document(path: &Path, document: &CredentialDocument) -> Result<(), SessionError> {
    let content = toml::to_string_pretty(document)
        .map_err(|error| SessionError::Credential(format!("凭据序列化失败: {error}")))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| SessionError::Credential(format!("无法创建凭据临时文件: {error}")))?;
    temporary
        .write_all(content.as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| SessionError::Credential(format!("无法写入凭据文件: {error}")))?;
    temporary
        .persist(path)
        .map_err(|error| SessionError::Credential(format!("无法替换凭据文件: {}", error.error)))?;
    Ok(())
}

fn associated_data(name: &str) -> Vec<u8> {
    format!("rivo:credentials:v1:{name}").into_bytes()
}

fn encrypt(name: &str, plaintext: &str) -> Result<String, SessionError> {
    let cipher = XChaCha20Poly1305::new((&APPLICATION_KEY).into());
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: &associated_data(name),
            },
        )
        .map_err(|_| SessionError::Credential("凭据加密失败".into()))?;
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&ciphertext);
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn decrypt(name: &str, encoded: &str) -> Result<String, SessionError> {
    let value = encoded
        .strip_prefix(PREFIX)
        .ok_or_else(|| SessionError::Credential("不支持的凭据密文版本".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| SessionError::Credential("凭据密文编码无效".into()))?;
    if bytes.len() <= 24 {
        return Err(SessionError::Credential("凭据密文长度无效".into()));
    }
    let (nonce, ciphertext) = bytes.split_at(24);
    let cipher = XChaCha20Poly1305::new((&APPLICATION_KEY).into());
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &associated_data(name),
            },
        )
        .map_err(|_| SessionError::Credential("凭据解密失败".into()))?;
    String::from_utf8(plaintext).map_err(|_| SessionError::Credential("凭据内容不是 UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_path(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("config.toml")
    }

    #[test]
    fn credentials_are_separate_per_name_and_encrypted_at_rest() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        set_credential(&config, "openai-prod", "secret-one").unwrap();
        set_credential(&config, "anthropic-prod", "secret-two").unwrap();
        assert_eq!(
            get_credential(&config, "openai-prod").unwrap().as_deref(),
            Some("secret-one")
        );
        assert_eq!(
            get_credential(&config, "anthropic-prod")
                .unwrap()
                .as_deref(),
            Some("secret-two")
        );
        let stored = std::fs::read_to_string(credentials_file_path(&config)).unwrap();
        assert!(stored.contains("enc:v1:"));
        assert!(!stored.contains("secret-one"));
        assert_eq!(credential_names(&config).unwrap().len(), 2);
    }

    #[test]
    fn credential_exists_checks_only_the_named_entry() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        set_credential(&config, "present", "secret").unwrap();

        assert!(credential_exists(&config, "present").unwrap());
        assert!(!credential_exists(&config, "missing").unwrap());
    }

    #[test]
    fn credential_store_rejects_malformed_documents_and_invalid_ciphertext() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        assert!(credential_names(&config).unwrap().is_empty());
        assert!(set_credential(&config, "invalid name", "value").is_err());
        assert!(set_credential(&config, "valid-name", "  ").is_err());

        let path = credentials_file_path(&config);
        std::fs::write(&path, "format_version = 2").unwrap();
        assert!(read_document(&path)
            .unwrap_err()
            .to_string()
            .contains("版本"));
        std::fs::write(&path, "format_version = 1\nunknown = true").unwrap();
        assert!(credential_names(&config)
            .unwrap_err()
            .to_string()
            .contains("格式错误"));

        for value in ["enc:v0:abc", "enc:v1:not-base64!", "enc:v1:AA"] {
            assert!(decrypt("valid-name", value).is_err(), "{value}");
        }
        std::fs::remove_file(&path).unwrap();
        set_credential(&config, "valid-name", "fixture-value").unwrap();
        unset_credential(&config, "valid-name").unwrap();
        assert_eq!(get_credential(&config, "valid-name").unwrap(), None);
    }

    #[test]
    fn plaintext_entry_is_migrated_when_read() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        std::fs::write(
            credentials_file_path(&config),
            "format_version = 1\n[credentials]\nlegacy = 'plain-key'\n",
        )
        .unwrap();
        assert_eq!(
            get_credential(&config, "legacy").unwrap().as_deref(),
            Some("plain-key")
        );
        let stored = std::fs::read_to_string(credentials_file_path(&config)).unwrap();
        assert!(stored.contains("enc:v1:"));
        assert!(!stored.contains("plain-key"));
    }

    #[test]
    fn legacy_credential_document_is_upgraded_as_a_whole() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        std::fs::write(
            credentials_file_path(&config),
            "[credentials]\none = 'secret-one'\ntwo = 'secret-two'\n",
        )
        .unwrap();

        assert_eq!(upgrade_credentials(&config).unwrap(), 2);
        let stored = std::fs::read_to_string(credentials_file_path(&config)).unwrap();
        assert!(stored.contains("format_version = 1"));
        assert_eq!(stored.matches("enc:v1:").count(), 2);
        assert!(!stored.contains("secret-one"));
        let once = stored.clone();
        assert_eq!(upgrade_credentials(&config).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(credentials_file_path(&config)).unwrap(),
            once
        );
    }

    #[test]
    fn future_credential_document_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        let future = "format_version = 2\n[credentials]\none = 'value'\n";
        std::fs::write(credentials_file_path(&config), future).unwrap();
        assert!(upgrade_credentials(&config).is_err());
        assert_eq!(
            std::fs::read_to_string(credentials_file_path(&config)).unwrap(),
            future
        );
    }

    #[test]
    fn name_bound_ciphertext_cannot_be_reused() {
        let directory = tempfile::tempdir().unwrap();
        let config = config_path(&directory);
        set_credential(&config, "one", "secret").unwrap();
        let mut document = read_document(&credentials_file_path(&config)).unwrap();
        let value = document.credentials.get("one").unwrap().clone();
        document.credentials.insert("two".into(), value);
        write_document(&credentials_file_path(&config), &document).unwrap();
        assert!(get_credential(&config, "two").is_err());
    }
}

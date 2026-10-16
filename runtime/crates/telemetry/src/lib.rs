use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("日志写入失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("日志序列化失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("诊断包目标非法: {0}")]
    InvalidDestination(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPolicy {
    pub retention_days: u64,
    pub required: bool,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            retention_days: 7,
            required: false,
        }
    }
}

pub struct FileLogger {
    directory: PathBuf,
    policy: LogPolicy,
}

pub struct DiagnosticPayload<'a> {
    pub journal: &'a Value,
    pub storage_health: &'a Value,
}

impl FileLogger {
    pub fn open(directory: impl Into<PathBuf>, policy: LogPolicy) -> Result<Self, TelemetryError> {
        let directory = directory.into();
        if let Err(error) = std::fs::create_dir_all(&directory) {
            if policy.required {
                return Err(error.into());
            }
            eprintln!("rivo: telemetry disabled: {error}");
        }
        Ok(Self { directory, policy })
    }

    pub fn record_at(&self, now: SystemTime, event: &Value) -> Result<bool, TelemetryError> {
        self.record_at_with_keys(now, event, &[])
    }

    /// Writes a log event after replacing the explicitly known provider keys.
    /// The empty-key form preserves the legacy environment-name fallback.
    pub fn record_at_with_keys(
        &self,
        now: SystemTime,
        event: &Value,
        api_keys: &[String],
    ) -> Result<bool, TelemetryError> {
        let path = self
            .directory
            .join(format!("rivo-{}.jsonl", day_number(now)));
        let sanitized = if api_keys.is_empty() {
            redact(event)
        } else {
            redact_api_keys(event, api_keys)
        };
        match append_json_value(&path, &sanitized) {
            Ok(()) => {
                rotate_logs_at(&self.directory, now, self.policy.retention_days)?;
                Ok(true)
            }
            Err(error) if self.policy.required => Err(error),
            Err(error) => {
                eprintln!("rivo: telemetry write skipped: {error}");
                Ok(false)
            }
        }
    }
}

pub fn rotate_logs_at(
    directory: &Path,
    now: SystemTime,
    retention_days: u64,
) -> Result<(), TelemetryError> {
    let today = day_number(now);
    let keep_after = today.saturating_sub(retention_days.saturating_sub(1));
    if !directory.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(day) = name
            .strip_prefix("rivo-")
            .and_then(|value| value.strip_suffix(".jsonl"))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        if day < keep_after {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

pub fn write_diagnostic_bundle(
    destination: &Path,
    version: &str,
    platform: &str,
    config_summary: &Value,
    log_files: &[PathBuf],
) -> Result<(), TelemetryError> {
    write_complete_diagnostic_bundle(
        destination,
        version,
        platform,
        config_summary,
        log_files,
        DiagnosticPayload {
            journal: &Value::Null,
            storage_health: &Value::Null,
        },
        &[],
    )
}

/// 生成可供离线排障的完整导出。业务内容保持原样，仅替换调用方明确给出的
/// API Key；journal 由 storage 的一致数据模型生成。
pub fn write_complete_diagnostic_bundle(
    destination: &Path,
    version: &str,
    platform: &str,
    config_summary: &Value,
    log_files: &[PathBuf],
    payload: DiagnosticPayload<'_>,
    api_keys: &[String],
) -> Result<(), TelemetryError> {
    if destination.file_name().is_none() {
        return Err(TelemetryError::InvalidDestination(
            destination.display().to_string(),
        ));
    }
    let api_keys = merged_api_keys(api_keys);
    let mut logs = Vec::new();
    for path in log_files {
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("rivo.db"))
        {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            logs.push(serde_json::json!({"path": path.file_name().and_then(|value| value.to_str()).unwrap_or_default(), "content": content.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).map(|value| redact_api_keys(&value, &api_keys)).collect::<Vec<_>>() }));
        }
    }
    let journal = redact_api_keys(payload.journal, &api_keys);
    use sha2::{Digest, Sha256};
    let journal_bytes = serde_json::to_vec(&journal)?;
    let checksum = hex::encode(Sha256::digest(&journal_bytes));
    let bundle = serde_json::json!({
        "manifest": {
            "version": version, "platform": platform,
            "journal_sha256": checksum,
            "redaction": "known_api_keys_masked_v1",
            "contains_full_business_content": true,
        },
        "config": redact_api_keys(config_summary, &api_keys),
        "logs": logs,
        "journal": journal,
        "storage_health": redact_api_keys(payload.storage_health, &api_keys),
        "database_included": false,
    });
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    let temporary = destination.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, destination)?;
    Ok(())
}

fn merged_api_keys(explicit: &[String]) -> Vec<String> {
    let mut keys = explicit.to_vec();
    for key in std::env::vars().filter_map(|(name, value)| {
        (name.to_ascii_uppercase().contains("API_KEY") && !value.is_empty()).then_some(value)
    }) {
        if !keys.iter().any(|known| known == &key) {
            keys.push(key);
        }
    }
    keys
}

fn day_number(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / 86_400
}

/// 仅隐藏 API Key。本项目的本机诊断明确保留 prompt、对话、工具及命令
/// 内容；调用者应传入已知 key 值，而不是依赖字段名猜测敏感信息。
pub fn redact_api_keys(value: &Value, api_keys: &[String]) -> Value {
    match value {
        Value::Object(map) => {
            let mut output = Map::new();
            for (key, value) in map {
                output.insert(key.clone(), redact_api_keys(value, api_keys));
            }
            Value::Object(output)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_api_keys(value, api_keys))
                .collect(),
        ),
        Value::String(text) => {
            let mut sanitized = text.clone();
            for key in api_keys.iter().filter(|key| !key.is_empty()) {
                sanitized = sanitized.replace(key, &mask_api_key(key));
            }
            Value::String(sanitized)
        }
        _ => value.clone(),
    }
}

pub fn mask_api_key(value: &str) -> String {
    let count = value.chars().count();
    let (head, tail) = if count > 12 { (6, 4) } else { (2, 2) };
    if count <= head + tail {
        return "*".repeat(count);
    }
    let prefix = value.chars().take(head).collect::<String>();
    let suffix = value.chars().skip(count - tail).collect::<String>();
    format!("{prefix}{}{}", "*".repeat(count - head - tail), suffix)
}

pub fn redact(value: &Value) -> Value {
    let keys = std::env::vars()
        .filter_map(|(name, value)| {
            (name.to_ascii_uppercase().contains("API_KEY") && !value.is_empty()).then_some(value)
        })
        .collect::<Vec<_>>();
    redact_api_keys(value, &keys)
}

pub fn append_json(path: impl AsRef<Path>, event: &Value) -> Result<(), TelemetryError> {
    let sanitized = redact(event);
    append_json_value(path, &sanitized)
}

fn append_json_value(path: impl AsRef<Path>, sanitized: &Value) -> Result<(), TelemetryError> {
    let mut line = serde_json::to_vec(&sanitized)?;
    line.push(b'\n');
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_known_api_keys_are_masked() {
        std::env::set_var("RIVO_TEST_API_KEY", "canary-secret");
        let v =
            redact(&serde_json::json!({"api_key":"canary-secret", "message":"keep token text"}));
        assert_eq!(v["api_key"], "canary***cret");
        assert_eq!(v["message"], "keep token text");
        assert_eq!(redact(&serde_json::json!("canary-secret")), "canary***cret");
        std::env::remove_var("RIVO_TEST_API_KEY");
    }

    #[test]
    fn short_known_keys_are_not_left_in_plaintext() {
        let value = redact_api_keys(&serde_json::json!("abc"), &["abc".into()]);
        assert_eq!(value, "***");
    }

    #[test]
    fn logger_rotates_after_seven_days_and_diagnostic_excludes_database() {
        std::env::set_var("RIVO_DIAGNOSTIC_TEST_API_KEY", "canary-secret");
        let directory = tempfile::tempdir().unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(20 * 86_400);
        let old = directory.path().join(format!("rivo-{}.jsonl", 1));
        std::fs::write(&old, "{}\n").unwrap();
        let logger = FileLogger::open(directory.path(), LogPolicy::default()).unwrap();
        logger
            .record_at(now, &serde_json::json!({"api_key":"canary","ok":true}))
            .unwrap();
        assert!(!old.exists());
        let database = directory.path().join("rivo.db");
        std::fs::write(&database, "secret database").unwrap();
        let bundle = directory.path().join("diagnostic.json");
        write_complete_diagnostic_bundle(
            &bundle,
            "0.1.0",
            "test",
            &serde_json::json!({"api_key":"canary-secret"}),
            &[
                database,
                directory.path().join(format!("rivo-{}.jsonl", 20)),
            ],
            DiagnosticPayload {
                journal: &serde_json::json!({"journal_secret":"canary-secret"}),
                storage_health: &serde_json::json!({"storage_secret":"canary-secret"}),
            },
            &[],
        )
        .unwrap();
        let text = std::fs::read_to_string(bundle).unwrap();
        assert!(!text.contains("canary-secret"));
        assert!(text.contains("canary***cret"));
        assert!(!text.contains("secret database"));
        assert!(text.contains("database_included"));
        std::env::remove_var("RIVO_DIAGNOSTIC_TEST_API_KEY");
    }
}

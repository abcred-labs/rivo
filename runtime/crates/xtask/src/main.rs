use base64::Engine;
use futures::StreamExt;
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey;
use regex::Regex;
use rivo_core::{Message, Role, ToolDefinition};
use rivo_llm::{
    create_client, ProviderConfig, ProviderKind, ProviderRequest, ProviderStreamEvent,
    ResponseFormat, ToolChoice,
};
use rivo_session::{
    provider_connection_fingerprint, AuthConfig, FileConfig, ModelCatalogEntry,
    ModelDiscoveryCache, ProviderConfig as SessionProviderConfig, VendorCatalogEntry,
};
use rivo_test_support::TestEnvironment;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::{format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime};

const CRITICAL_PACKAGES: &[&str] = &[
    "rivo",
    "rivo-llm",
    "rivo-agent",
    "rivo-session",
    "rivo-policy",
    "rivo-tools",
    "rivo-storage",
];
const COVERAGE_TARGET: &str = "75";
const COVERAGE_FLOOR: f64 = 50.0;
const RELEASE_EVIDENCE_MAX_AGE_DAYS: i64 = 30;

fn coverage_passes(percent: f64) -> bool {
    percent > COVERAGE_FLOOR
}

#[tokio::main]
async fn main() -> ExitCode {
    let root = std::env::current_dir().expect("无法确定 runtime 目录");
    let repo = root.parent().unwrap_or(&root).to_path_buf();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let success = match args.first().map(String::as_str) {
        Some("check") if args.len() == 1 => run_check(&root, &repo),
        Some("coverage") if args.len() == 1 => run_coverage(&root, &repo),
        Some("external-test") => match parse_external_test_args(&args[1..]) {
            Ok(category) => run_external_test(&root, category).await,
            Err(error) => {
                eprintln!("{error}");
                print_usage();
                false
            }
        },
        Some("a2a-verify-protocol") if args.len() == 1 => run_a2a_verify_protocol(&repo),
        Some("dependency-direction") if args.len() == 1 => run_dependency_direction_check(&repo),
        Some("preview-gate") if args.len() == 1 => run_preview_gate(&repo),
        Some("release-gate") if args.len() == 1 => run_release_gate(&repo),
        Some("release-test") => match parse_release_args(&args[1..]) {
            Ok(options) => run_release_test(&root, &repo, options).await,
            Err(error) => {
                eprintln!("{error}");
                print_usage();
                false
            }
        },
        Some("live-test") => match parse_live_test_args(&args[1..]) {
            Ok(options) => run_live_test(&root, options).await,
            Err(error) => {
                eprintln!("{error}");
                print_usage();
                false
            }
        },
        _ => {
            print_usage();
            return ExitCode::from(2);
        }
    };
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_usage() {
    eprintln!("用法:");
    eprintln!("  cargo xtask check");
    eprintln!("  cargo xtask coverage");
    eprintln!("  cargo xtask external-test --category <tui-pty|cdp|oauth|a2a-local|all>");
    eprintln!("  cargo xtask a2a-verify-protocol");
    eprintln!("  cargo xtask dependency-direction");
    eprintln!("  cargo xtask preview-gate");
    eprintln!("  cargo xtask release-gate");
    eprintln!("  cargo xtask release-test --live-config <path> --report-dir <path>");
    eprintln!("  cargo xtask live-test --live-config <path> --target <id> --case <case-id>");
    eprintln!("    live 用例会将使用环境变量引用的脱敏 curl 同步输出到终端和报告");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExternalTestCategory {
    TuiPty,
    Cdp,
    Oauth,
    A2aLocal,
    All,
}

fn parse_external_test_args(args: &[String]) -> Result<ExternalTestCategory, String> {
    if args.len() != 2 || args[0] != "--category" {
        return Err("external-test 用法: --category <tui-pty|cdp|oauth|a2a-local|all>".into());
    }
    match args[1].as_str() {
        "tui-pty" => Ok(ExternalTestCategory::TuiPty),
        "cdp" => Ok(ExternalTestCategory::Cdp),
        "oauth" => Ok(ExternalTestCategory::Oauth),
        "a2a-local" => Ok(ExternalTestCategory::A2aLocal),
        "all" => Ok(ExternalTestCategory::All),
        value => Err(format!("未知 external-test category: {value}")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OptionalTestStatus {
    Passed,
    Skipped,
    Failed,
}

impl OptionalTestStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

async fn run_external_test(root: &Path, category: ExternalTestCategory) -> bool {
    let categories = match category {
        ExternalTestCategory::All => vec![
            ExternalTestCategory::TuiPty,
            ExternalTestCategory::Cdp,
            ExternalTestCategory::Oauth,
            ExternalTestCategory::A2aLocal,
        ],
        category => vec![category],
    };
    let mut failed = false;
    for category in categories {
        let (name, prerequisite) = match category {
            ExternalTestCategory::TuiPty => (
                "tui-pty",
                if !cfg!(windows) {
                    Some("仅 Windows 支持 ConPTY")
                } else {
                    None
                },
            ),
            ExternalTestCategory::Cdp => (
                "cdp",
                match reqwest::Client::new()
                    .get("http://127.0.0.1:9222/json/version")
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => None,
                    _ => Some("未发现 127.0.0.1:9222 专用 CDP 浏览器"),
                },
            ),
            ExternalTestCategory::Oauth => (
                "oauth",
                [
                    "RIVO_REMOTE_MCP_SMOKE",
                    "RIVO_REMOTE_MCP_NAME",
                    "RIVO_REMOTE_MCP_URL",
                    "RIVO_REMOTE_MCP_TOOLSET",
                ]
                .iter()
                .find_map(|key| {
                    let value = std::env::var(key).ok();
                    let missing = match *key {
                        "RIVO_REMOTE_MCP_SMOKE" => value.as_deref() != Some("1"),
                        _ => value.as_deref().is_none_or(|value| value.trim().is_empty()),
                    };
                    missing.then_some(*key)
                })
                .map(|key| match key {
                    "RIVO_REMOTE_MCP_SMOKE" => "未设置 RIVO_REMOTE_MCP_SMOKE=1",
                    _ => "OAuth 外部测试缺少必要环境变量",
                }),
            ),
            ExternalTestCategory::A2aLocal => (
                "a2a-local",
                if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
                    None
                } else {
                    Some("仅支持 Windows、Linux 和 macOS 本机多进程测试")
                },
            ),
            ExternalTestCategory::All => unreachable!(),
        };
        if let Some(reason) = prerequisite {
            let status = OptionalTestStatus::Skipped;
            eprintln!(
                "[xtask] external-test {name}: {} ({reason})",
                status.as_str()
            );
            continue;
        }
        let args: Vec<&str> = match category {
            ExternalTestCategory::TuiPty => vec![
                "test",
                "-p",
                "rivo",
                "--test",
                "tui_pty",
                "--",
                "--ignored",
                "--nocapture",
            ],
            ExternalTestCategory::Cdp => vec![
                "test",
                "-p",
                "rivo-tools",
                "live_cdp_browser_enforces_interaction_safety_contract",
                "--",
                "--ignored",
                "--nocapture",
            ],
            ExternalTestCategory::Oauth => vec![
                "test",
                "-p",
                "rivo",
                "--test",
                "cli_black_box",
                "e2e_remote_mcp_oauth_readonly_smoke",
                "--",
                "--ignored",
                "--nocapture",
            ],
            ExternalTestCategory::A2aLocal => vec![
                "test",
                "-p",
                "rivo",
                "--test",
                "a2a_local_multiprocess",
                "--",
                "--ignored",
                "--nocapture",
            ],
            ExternalTestCategory::All => unreachable!(),
        };
        let passed = if category == ExternalTestCategory::A2aLocal {
            run_a2a_local_external_test(root, &args)
        } else {
            run("cargo", &args, root)
        };
        let status = if passed {
            OptionalTestStatus::Passed
        } else {
            OptionalTestStatus::Failed
        };
        eprintln!("[xtask] external-test {name}: {}", status.as_str());
        failed |= status == OptionalTestStatus::Failed;
    }
    !failed
}

fn run_a2a_local_external_test(root: &Path, args: &[&str]) -> bool {
    let repo = root.parent().unwrap_or(root);
    let Some(commit) = command_text("git", &["rev-parse", "HEAD"], repo) else {
        eprintln!("[xtask] 无法确定 A2A 本机报告的候选提交");
        return false;
    };
    let Some(status) = command_text("git", &["status", "--porcelain"], repo) else {
        eprintln!("[xtask] 无法检查 A2A 本机报告的工作树状态");
        return false;
    };
    if !status.is_empty() {
        eprintln!("[xtask] A2A 本机报告必须对应干净工作树");
        return false;
    }
    let report_dir = repo.join("artifacts").join("test-reports");
    if let Err(error) = std::fs::create_dir_all(&report_dir) {
        eprintln!("[xtask] 无法创建 A2A 本机报告目录: {error}");
        return false;
    }
    let short_commit = commit.chars().take(12).collect::<String>();
    let report_path = report_dir.join(format!("a2a-local-{}-{short_commit}.md", unix_timestamp()));
    eprintln!("[xtask] cargo {}", args.join(" "));
    let succeeded = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .env("RIVO_A2A_LOCAL_REPORT_PATH", &report_path)
        .env("RIVO_A2A_LOCAL_CANDIDATE_COMMIT", &commit)
        .status()
        .map(|status| status.success())
        .unwrap_or_else(|error| {
            eprintln!("[xtask] 无法启动 cargo: {error}");
            false
        });
    if succeeded && report_path.is_file() {
        eprintln!("[xtask] A2A 本机报告: {}", report_path.display());
        true
    } else if succeeded {
        eprintln!("[xtask] A2A 本机测试未生成脱敏报告");
        false
    } else {
        false
    }
}

const A2A_PROTOCOL_DIRECTORY: &str = "runtime/crates/a2a/protocol/v1.0.1";
const A2A_PROTO_SHA256: &str = "e195bf96ab630c69797851970203e1b2b6b19528f2e9803b7d904b91a5104016";
const BIP39_ENGLISH_SHA256: &str =
    "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda";

fn run_a2a_verify_protocol(repo: &Path) -> bool {
    let directory = repo.join(A2A_PROTOCOL_DIRECTORY);
    let manifest_path = directory.join("protocol-manifest.toml");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) => {
            eprintln!(
                "[xtask] 无法读取 A2A 协议清单 {}: {error}",
                manifest_path.display()
            );
            return false;
        }
    };
    let manifest: toml::Value = match toml::from_str(&manifest) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[xtask] A2A 协议清单不是有效 TOML: {error}");
            return false;
        }
    };
    let expected = [
        ("upstream.repository", "https://github.com/a2aproject/A2A"),
        ("upstream.tag", "v1.0.1"),
        (
            "upstream.commit",
            "3303592588e388e62e0f69f701af531d2f4e3991",
        ),
        ("upstream.sha256", A2A_PROTO_SHA256),
        ("generator.protoc", "29.3"),
        ("generator.prost_build", "0.13.5"),
        ("sas_wordlist.repository", "https://github.com/bitcoin/bips"),
        (
            "sas_wordlist.commit",
            "9783d61f1b9c81231581fee026c8e8cb9499d265",
        ),
        ("sas_wordlist.sha256", BIP39_ENGLISH_SHA256),
    ];
    for (path, value) in expected {
        if toml_string_at(&manifest, path) != Some(value) {
            eprintln!("[xtask] A2A 协议清单字段 {path} 不匹配冻结值");
            return false;
        }
    }
    let facade_path = directory.join("schema-facade.toml");
    let facade = match std::fs::read_to_string(&facade_path)
        .ok()
        .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
    {
        Some(value) => value,
        None => {
            eprintln!("[xtask] A2A schema facade 生成配置缺失或不是有效 TOML");
            return false;
        }
    };
    for (path, value) in [
        ("facade.crate", "rivo-a2a"),
        ("facade.module", "a2a.v1"),
        ("facade.output", "src/generated/a2a_v1.rs"),
        ("prost_build.protoc", "29.3"),
        ("prost_build.prost_build", "0.13.5"),
        (
            "prost_build.file_descriptor_set",
            "src/generated/a2a_v1.descriptor.bin",
        ),
        (
            "protobuf_dependencies.googleapis.module",
            "buf.build/googleapis/googleapis",
        ),
        (
            "protobuf_dependencies.googleapis.commit",
            "004180b77378443887d3b55cabc00384",
        ),
        (
            "protobuf_dependencies.googleapis.digest",
            "b5:e8f475fe3330f31f5fd86ac689093bcd274e19611a09db91f41d637cb9197881ce89882b94d13a58738e53c91c6e4bae7dc1feba85f590164c975a89e25115dc",
        ),
        ("protobuf_dependencies.googleapis.buf_lock", "buf.lock"),
        (
            "protobuf_dependencies.googleapis.export_dir",
            "third_party/googleapis",
        ),
    ] {
        if toml_string_at(&facade, path) != Some(value) {
            eprintln!("[xtask] A2A schema facade 配置字段 {path} 不匹配冻结值");
            return false;
        }
    }
    let includes = facade
        .get("prost_build")
        .and_then(toml::Value::as_table)
        .and_then(|section| section.get("includes"))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()
        });
    if includes.as_deref() != Some(&[".", "third_party/googleapis"]) {
        eprintln!("[xtask] A2A schema facade 必须只使用冻结的本地与 googleapis include");
        return false;
    }
    let buf_lock_path = directory.join("buf.lock");
    let buf_lock = std::fs::read_to_string(&buf_lock_path).unwrap_or_default();
    for required in [
        "name: buf.build/googleapis/googleapis",
        "commit: 004180b77378443887d3b55cabc00384",
        "digest: b5:e8f475fe3330f31f5fd86ac689093bcd274e19611a09db91f41d637cb9197881ce89882b94d13a58738e53c91c6e4bae7dc1feba85f590164c975a89e25115dc",
    ] {
        if !buf_lock.contains(required) {
            eprintln!("[xtask] A2A googleapis buf.lock 缺失冻结字段: {required}");
            return false;
        }
    }
    let proto = directory.join("a2a.proto");
    let wordlist = directory.join("bip39-english.txt");
    let license = directory.join("LICENSE-A2A");
    if !license.is_file() {
        eprintln!("[xtask] A2A 上游许可证缺失: {}", license.display());
        return false;
    }
    if sha256_file(&proto).as_deref() != Some(A2A_PROTO_SHA256) {
        eprintln!("[xtask] a2a.proto 缺失或 SHA-256 不匹配");
        return false;
    }
    if sha256_file(&wordlist).as_deref() != Some(BIP39_ENGLISH_SHA256) {
        eprintln!("[xtask] BIP-39 英文词表缺失或 SHA-256 不匹配");
        return false;
    }
    let word_count = match std::fs::read_to_string(&wordlist) {
        Ok(content) => content
            .lines()
            .filter(|word| !word.trim().is_empty())
            .count(),
        Err(_) => 0,
    };
    if word_count != 2048 {
        eprintln!("[xtask] BIP-39 英文词表必须恰有 2,048 个词，当前为 {word_count}");
        return false;
    }
    for fixture in [
        "fixtures/agent-card.json",
        "fixtures/jsonrpc-send-message.json",
        "fixtures/a2a-delegate.valid.json",
        "fixtures/a2a-delegate.invalid-artifact-name.json",
        "fixtures/pairing-request.valid.json",
        "fixtures/pairing-request.invalid-extra.json",
        "fixtures/pairing-response.valid.json",
        "fixtures/claim-request.valid.json",
        "fixtures/claim-request.invalid-padding.json",
        "fixtures/claim-response.valid.json",
        "fixtures/sas-vector.json",
        "fixtures/claim-proof-vector.json",
        "fixtures/claim-proof-vector.invalid-signature.json",
    ] {
        let fixture_path = directory.join(fixture);
        match std::fs::read_to_string(&fixture_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        {
            Some(_) => {}
            None => {
                eprintln!(
                    "[xtask] A2A fixture 缺失或不是有效 JSON: {}",
                    fixture_path.display()
                );
                return false;
            }
        }
    }
    for schema in [
        "schemas/a2a-delegate.schema.json",
        "schemas/pairing-request.schema.json",
        "schemas/pairing-response.schema.json",
        "schemas/claim-request.schema.json",
        "schemas/claim-response.schema.json",
    ] {
        let schema_path = directory.join(schema);
        let valid_schema = std::fs::read_to_string(&schema_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .is_some_and(|value| {
                value["$schema"] == "https://json-schema.org/draft/2020-12/schema"
                    && value["type"] == "object"
                    && value["additionalProperties"] == false
            });
        if !valid_schema {
            eprintln!(
                "[xtask] A2A schema 缺失或不是受限的 Draft 2020-12 object: {}",
                schema_path.display()
            );
            return false;
        }
    }
    for (fixture, expected_valid) in [
        ("fixtures/a2a-delegate.valid.json", true),
        ("fixtures/a2a-delegate.invalid-artifact-name.json", false),
        ("fixtures/pairing-request.valid.json", true),
        ("fixtures/pairing-request.invalid-extra.json", false),
        ("fixtures/pairing-response.valid.json", true),
        ("fixtures/claim-request.valid.json", true),
        ("fixtures/claim-request.invalid-padding.json", false),
        ("fixtures/claim-response.valid.json", true),
    ] {
        let fixture_path = directory.join(fixture);
        let result = std::fs::read_to_string(&fixture_path)
            .map_err(|error| error.to_string())
            .and_then(|content| {
                serde_json::from_str::<serde_json::Value>(&content)
                    .map_err(|error| error.to_string())
            })
            .and_then(|value| validate_a2a_fixture(fixture, &value));
        if result.is_ok() != expected_valid {
            let detail = result
                .as_ref()
                .err()
                .map_or("fixture 被意外接受", String::as_str);
            eprintln!(
                "[xtask] A2A fixture 语义与正反例期望不符: {} ({})",
                fixture_path.display(),
                detail
            );
            return false;
        }
    }
    if let Err(error) = verify_sas_vector(&directory) {
        eprintln!("[xtask] SAS 向量校验失败: {error}");
        return false;
    }
    if let Err(error) = verify_claim_proof_vector(&directory) {
        eprintln!("[xtask] claim proof 向量校验失败: {error}");
        return false;
    }
    if verify_claim_proof_vector_fixture(
        &directory,
        "fixtures/claim-proof-vector.invalid-signature.json",
    )
    .is_ok()
    {
        eprintln!("[xtask] 损坏的 claim proof 向量被意外接受");
        return false;
    }
    let sse_path = directory.join("fixtures/task-sse.ndjson");
    let sse_valid = std::fs::read_to_string(&sse_path)
        .ok()
        .is_some_and(|content| {
            !content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .is_empty()
                && content
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        });
    if !sse_valid {
        eprintln!("[xtask] A2A SSE fixture 缺失或不是有效 NDJSON");
        return false;
    }
    eprintln!("[xtask] A2A v1.0.1 协议输入、词表与 fixture 校验通过");
    true
}

fn toml_string_at<'a>(value: &'a toml::Value, path: &str) -> Option<&'a str> {
    path.split('.')
        .try_fold(value, |current, segment| current.get(segment))
        .and_then(toml::Value::as_str)
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_a2a_fixture(name: &str, value: &serde_json::Value) -> Result<(), String> {
    match name {
        "fixtures/a2a-delegate.valid.json" | "fixtures/a2a-delegate.invalid-artifact-name.json" => {
            validate_a2a_delegate(value)
        }
        "fixtures/pairing-request.valid.json" | "fixtures/pairing-request.invalid-extra.json" => {
            validate_pairing_request(value)
        }
        "fixtures/pairing-response.valid.json" => validate_pairing_response(value),
        "fixtures/claim-request.valid.json" | "fixtures/claim-request.invalid-padding.json" => {
            validate_claim_request(value)
        }
        "fixtures/claim-response.valid.json" => validate_claim_response(value),
        _ => Err("未注册的 A2A fixture".into()),
    }
}

fn exact_object<'a>(
    value: &'a serde_json::Value,
    required: &[&str],
    optional: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "必须为 object".to_owned())?;
    if required.iter().any(|key| !object.contains_key(*key)) {
        return Err("缺少必填字段".into());
    }
    if object
        .keys()
        .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err("包含未声明字段".into());
    }
    Ok(object)
}

fn bounded_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    min_bytes: usize,
    max_bytes: usize,
) -> Result<&'a str, String> {
    let value = object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{key} 必须为 string"))?;
    if !(min_bytes..=max_bytes).contains(&value.len()) {
        return Err(format!("{key} UTF-8 bytes 超出范围"));
    }
    Ok(value)
}

fn validate_a2a_delegate(value: &serde_json::Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &["peer_id", "skill_id", "message"],
        &["stream", "artifacts"],
    )?;
    bounded_string(object, "peer_id", 1, 128)?;
    bounded_string(object, "skill_id", 1, 128)?;
    bounded_string(object, "message", 0, 65_536)?;
    if object
        .get("stream")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("stream 必须为 boolean".into());
    }
    let Some(artifacts) = object.get("artifacts") else {
        return Ok(());
    };
    let artifacts = artifacts
        .as_array()
        .ok_or_else(|| "artifacts 必须为 array".to_owned())?;
    if artifacts.len() > 16 {
        return Err("artifacts 不能超过 16 项".into());
    }
    for artifact in artifacts {
        let artifact = exact_object(artifact, &["kind", "content"], &["name"])?;
        let kind = bounded_string(artifact, "kind", 1, 4)?;
        if !matches!(kind, "text" | "json") {
            return Err("Artifact kind 必须为 text 或 json".into());
        }
        let content = bounded_string(artifact, "content", 0, 65_536)?;
        if kind == "json" {
            serde_json::from_str::<serde_json::Value>(content)
                .map_err(|_| "json Artifact content 必须是 JSON 文本".to_owned())?;
        }
        if let Some(name) = artifact.get("name") {
            let name = name
                .as_str()
                .ok_or_else(|| "Artifact name 必须为 string".to_owned())?;
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Err("Artifact name 不符合 ASCII 文件名限制".into());
            }
        }
    }
    Ok(())
}

fn validate_pairing_request(value: &serde_json::Value) -> Result<(), String> {
    let object = exact_object(value, &["version", "csr_pem"], &[])?;
    require_lan_version(object)?;
    bounded_string(object, "csr_pem", 1, 16_384)?;
    Ok(())
}

fn validate_pairing_response(value: &serde_json::Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &[
            "version",
            "request_id",
            "server_spki_sha256",
            "client_public_key_sha256",
            "expires_at",
            "sas_words",
            "claim_token",
        ],
        &[],
    )?;
    require_lan_version(object)?;
    bounded_string(object, "request_id", 1, 256)?;
    require_sha256(object, "server_spki_sha256")?;
    require_sha256(object, "client_public_key_sha256")?;
    bounded_string(object, "expires_at", 1, 64)?;
    require_base64url_32(object, "claim_token")?;
    let words = object["sas_words"]
        .as_array()
        .filter(|words| words.len() == 6)
        .ok_or_else(|| "sas_words 必须恰有六个词".to_owned())?;
    if words.iter().any(|word| {
        !word.as_str().is_some_and(|word| {
            !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase())
        })
    }) {
        return Err("sas_words 必须为小写 BIP-39 词".into());
    }
    Ok(())
}

fn validate_claim_request(value: &serde_json::Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &["version", "request_id", "claim_token", "claim_proof"],
        &[],
    )?;
    require_lan_version(object)?;
    bounded_string(object, "request_id", 1, 256)?;
    require_base64url_32(object, "claim_token")?;
    let proof = bounded_string(object, "claim_proof", 8, 16_384)?;
    if !proof
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("claim_proof 必须是无 padding base64url".into());
    }
    Ok(())
}

fn validate_claim_response(value: &serde_json::Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &[
            "version",
            "request_id",
            "peer_id",
            "certificate_chain_pem",
            "expires_at",
        ],
        &[],
    )?;
    require_lan_version(object)?;
    bounded_string(object, "request_id", 1, 256)?;
    bounded_string(object, "peer_id", 1, 128)?;
    bounded_string(object, "certificate_chain_pem", 1, 32_768)?;
    bounded_string(object, "expires_at", 1, 64)?;
    Ok(())
}

fn require_lan_version(object: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    if object.get("version").and_then(serde_json::Value::as_str) == Some("rivo-a2a-lan-v1") {
        Ok(())
    } else {
        Err("version 必须为 rivo-a2a-lan-v1".into())
    }
}

fn require_sha256(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<(), String> {
    let value = bounded_string(object, key, 64, 64)?;
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        .then_some(())
        .ok_or_else(|| format!("{key} 必须是小写 SHA-256"))
}

fn require_base64url_32(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<(), String> {
    let value = bounded_string(object, key, 43, 43)?;
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .then_some(())
        .ok_or_else(|| format!("{key} 必须是无 padding 32-byte base64url"))
}

fn verify_sas_vector(directory: &Path) -> Result<(), String> {
    let fixture = read_protocol_fixture(directory, "fixtures/sas-vector.json")?;
    let object = exact_object(
        &fixture,
        &[
            "request_id",
            "server_spki_sha256",
            "client_public_key_sha256",
            "sha256",
            "sas_words",
        ],
        &[],
    )?;
    let request_id = bounded_string(object, "request_id", 1, 256)?;
    let server_spki = decode_hex_32(bounded_string(object, "server_spki_sha256", 64, 64)?)?;
    let client_public = decode_hex_32(bounded_string(object, "client_public_key_sha256", 64, 64)?)?;
    let mut input = b"rivo-a2a-lan-v1/sas\0".to_vec();
    input.extend_from_slice(
        &(u32::try_from(request_id.len()).map_err(|_| "request_id 太长")?).to_be_bytes(),
    );
    input.extend_from_slice(request_id.as_bytes());
    input.extend_from_slice(&server_spki);
    input.extend_from_slice(&client_public);
    let digest = Sha256::digest(input);
    let expected_digest = bounded_string(object, "sha256", 64, 64)?;
    if format!("{digest:x}") != expected_digest {
        return Err("SHA-256 不匹配".into());
    }
    let wordlist = std::fs::read_to_string(directory.join("bip39-english.txt"))
        .map_err(|error| error.to_string())?;
    let words = wordlist.lines().collect::<Vec<_>>();
    let requested_words = object["sas_words"]
        .as_array()
        .filter(|items| items.len() == 6)
        .ok_or_else(|| "sas_words 必须恰有六项".to_owned())?;
    let prefix = digest
        .iter()
        .take(9)
        .fold(0_u128, |value, byte| (value << 8) | u128::from(*byte))
        >> 6;
    for (position, word) in requested_words.iter().enumerate() {
        let index = ((prefix >> ((5 - position) * 11)) & 0x7ff) as usize;
        if word.as_str() != words.get(index).copied() {
            return Err(format!("第 {} 个 SAS 词不匹配", position + 1));
        }
    }
    Ok(())
}

fn verify_claim_proof_vector(directory: &Path) -> Result<(), String> {
    verify_claim_proof_vector_fixture(directory, "fixtures/claim-proof-vector.json")
}

fn verify_claim_proof_vector_fixture(directory: &Path, fixture_name: &str) -> Result<(), String> {
    let fixture = read_protocol_fixture(directory, fixture_name)?;
    let object = exact_object(
        &fixture,
        &[
            "request_id",
            "claim_token",
            "sha256",
            "csr_p256_spki_der_base64url",
            "claim_proof",
        ],
        &[],
    )?;
    let request_id = bounded_string(object, "request_id", 1, 256)?;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bounded_string(object, "claim_token", 43, 43)?)
        .map_err(|_| "claim_token 不是 base64url")?;
    if token.len() != 32 {
        return Err("claim_token 必须解码为 32 bytes".into());
    }
    let mut input = b"rivo-a2a-lan-v1/claim-proof\0".to_vec();
    input.extend_from_slice(
        &(u32::try_from(request_id.len()).map_err(|_| "request_id 太长")?).to_be_bytes(),
    );
    input.extend_from_slice(request_id.as_bytes());
    input.extend_from_slice(&token);
    let digest = Sha256::digest(input);
    if format!("{digest:x}") != bounded_string(object, "sha256", 64, 64)? {
        return Err("claim proof prehash 不匹配".into());
    }
    let spki = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bounded_string(
            object,
            "csr_p256_spki_der_base64url",
            1,
            1024,
        )?)
        .map_err(|_| "SPKI 不是 canonical base64url")?;
    let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bounded_string(object, "claim_proof", 8, 16_384)?)
        .map_err(|_| "claim_proof 不是 canonical base64url")?;
    let key = VerifyingKey::from_public_key_der(&spki).map_err(|_| "SPKI 不是 P-256 public key")?;
    let signature = Signature::from_der(&proof).map_err(|_| "claim_proof 不是 ASN.1 DER")?;
    key.verify_prehash(&digest, &signature)
        .map_err(|_| "claim_proof 验签失败".to_owned())
}

fn read_protocol_fixture(directory: &Path, fixture: &str) -> Result<serde_json::Value, String> {
    let path = directory.join(fixture);
    let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content).map_err(|error| error.to_string())
}

fn decode_hex_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("SHA-256 必须为 64 个十六进制字符".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, target) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *target = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| "SHA-256 不是十六进制")?;
    }
    Ok(bytes)
}

fn run(program: &str, args: &[&str], cwd: &Path) -> bool {
    eprintln!("[xtask] {program} {}", args.join(" "));
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map(|status| status.success())
        .unwrap_or_else(|error| {
            eprintln!("[xtask] 无法启动 {program}: {error}");
            false
        })
}

fn output(program: &str, args: &[&str], cwd: &Path) -> Option<Output> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
}

fn run_check(root: &Path, repo: &Path) -> bool {
    let checks: Vec<(&str, Vec<&str>, &Path)> = vec![
        ("cargo", vec!["fmt", "--all", "--", "--check"], root),
        (
            "cargo",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            root,
        ),
        (
            "cargo",
            vec![
                "build",
                "--workspace",
                "--exclude",
                "xtask",
                "--all-targets",
                "--locked",
            ],
            root,
        ),
        (
            "cargo",
            vec![
                "test",
                "--workspace",
                "--exclude",
                "xtask",
                "--all-targets",
                "--locked",
            ],
            root,
        ),
        ("git", vec!["diff", "--check"], repo),
    ];
    for (program, args, cwd) in checks {
        if !run(program, &args, cwd) {
            return false;
        }
    }
    run_msrv_check(root)
        && run_a2a_verify_protocol(repo)
        && run_dependency_direction_check(repo)
        && run_repository_checks(repo)
}

/// Enforces the frozen production dependency boundary without treating the CLI
/// composition root as a frontend. The TUI can only obtain turn-profile types
/// through `rivo-session`; test-only dependencies remain deliberately allowed.
fn run_dependency_direction_check(repo: &Path) -> bool {
    let manifest_path = repo.join("runtime/crates/tui/Cargo.toml");
    let manifest = match std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
    {
        Some(manifest) => manifest,
        None => {
            eprintln!(
                "[xtask] 无法读取或解析 TUI 依赖清单: {}",
                manifest_path.display()
            );
            return false;
        }
    };
    let Some(dependencies) = manifest.get("dependencies").and_then(toml::Value::as_table) else {
        eprintln!("[xtask] TUI 缺少 [dependencies] 段");
        return false;
    };
    for forbidden in ["rivo-llm", "rivo-policy", "rivo-tools", "rivo-storage"] {
        if dependencies.contains_key(forbidden) {
            eprintln!(
                "[xtask] TUI 生产依赖不得直接引用 {forbidden}；请通过 rivo-session 暴露契约类型"
            );
            return false;
        }
    }
    if !dependencies.contains_key("rivo-session") {
        eprintln!("[xtask] TUI 必须通过 rivo-session 消费前端契约");
        return false;
    }

    let terminal_runtime = repo.join("runtime/crates/cli/src/terminal_runtime.rs");
    let source = match std::fs::read_to_string(&terminal_runtime) {
        Ok(source) => source,
        Err(error) => {
            eprintln!(
                "[xtask] 无法读取 CLI terminal runtime {}: {error}",
                terminal_runtime.display()
            );
            return false;
        }
    };
    for forbidden in [
        "rivo_llm::",
        "rivo_policy::",
        "rivo_tools::",
        "rivo_storage::",
    ] {
        if source.contains(forbidden) {
            eprintln!("[xtask] CLI terminal runtime 不得绕过 Session 直接引用 {forbidden}");
            return false;
        }
    }
    true
}

fn run_msrv_check(root: &Path) -> bool {
    let rustup_available =
        output("rustup", &["--version"], root).is_some_and(|result| result.status.success());
    if rustup_available {
        return run(
            "rustup",
            &[
                "run",
                "1.88.0",
                "cargo",
                "check",
                "--workspace",
                "--exclude",
                "xtask",
                "--all-targets",
                "--locked",
            ],
            root,
        );
    }
    let rustc_version =
        command_text("rustc", &["--version"], root).unwrap_or_else(|| "unknown".into());
    eprintln!("[xtask] rustup 不可用；MSRV 检查使用当前工具链: {rustc_version}");
    run(
        "cargo",
        &[
            "check",
            "--workspace",
            "--exclude",
            "xtask",
            "--all-targets",
            "--locked",
        ],
        root,
    )
}

#[derive(Clone, Debug)]
struct PythonCommand {
    program: String,
    prefix: Vec<String>,
}

fn find_python(repo: &Path) -> Option<PythonCommand> {
    if let Some(program) = std::env::var_os("RIVO_PYTHON") {
        let command = PythonCommand {
            program: program.to_string_lossy().into_owned(),
            prefix: Vec::new(),
        };
        if python_works(&command, repo) {
            return Some(command);
        }
    }
    for program in ["python3", "python"] {
        let command = PythonCommand {
            program: program.into(),
            prefix: Vec::new(),
        };
        if python_works(&command, repo) {
            return Some(command);
        }
    }
    let launcher = PythonCommand {
        program: "py".into(),
        prefix: vec!["-3".into()],
    };
    python_works(&launcher, repo).then_some(launcher)
}

fn python_works(command: &PythonCommand, cwd: &Path) -> bool {
    Command::new(&command.program)
        .args(&command.prefix)
        .arg("--version")
        .current_dir(cwd)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_python(command: &PythonCommand, args: &[&str], cwd: &Path) -> bool {
    let mut displayed = command.prefix.clone();
    displayed.extend(args.iter().map(|value| (*value).to_owned()));
    eprintln!("[xtask] {} {}", command.program, displayed.join(" "));
    Command::new(&command.program)
        .args(&command.prefix)
        .args(args)
        .current_dir(cwd)
        .status()
        .is_ok_and(|status| status.success())
}

fn run_repository_checks(repo: &Path) -> bool {
    let Some(python) = find_python(repo) else {
        eprintln!("[xtask] 未找到 Python；可设置 RIVO_PYTHON 指定解释器");
        return false;
    };
    run_python(&python, &["scripts/check_markdown_links.py"], repo)
}

fn run_coverage(root: &Path, repo: &Path) -> bool {
    if !run("cargo", &["llvm-cov", "--version"], root) {
        eprintln!("[xtask] 缺少 cargo-llvm-cov；请先安装后重试");
        return false;
    }
    // Use cargo-llvm-cov's native test orchestration so the test binaries and
    // merged profiles always come from the same target directory. This also
    // instruments CLI black-box child processes through the inherited profile
    // environment.
    if !run("cargo", &["llvm-cov", "clean", "--profraw-only"], root)
        || !run(
            "cargo",
            &[
                "llvm-cov",
                "--workspace",
                "--all-targets",
                "--locked",
                "--no-report",
            ],
            root,
        )
    {
        return false;
    }
    let report_dir = repo.join("artifacts").join("coverage");
    if let Err(error) = std::fs::create_dir_all(&report_dir) {
        eprintln!("[xtask] 无法创建覆盖率目录: {error}");
        return false;
    }
    let html_dir = report_dir.join("html");
    let lcov = report_dir.join("lcov.info");
    let json = report_dir.join("coverage.json");
    let html_path = html_dir.to_string_lossy().into_owned();
    let lcov_path = lcov.to_string_lossy().into_owned();
    let json_path = json.to_string_lossy().into_owned();
    // Coverage is advisory. Report generation failures still fail the command,
    // but coverage percentages never block the gate.
    if !run(
        "cargo",
        &[
            "llvm-cov",
            "report",
            "--html",
            "--output-dir",
            &html_path,
            "--ignore-filename-regex",
            "test-support",
        ],
        root,
    ) || !run(
        "cargo",
        &[
            "llvm-cov",
            "report",
            "--lcov",
            "--output-path",
            &lcov_path,
            "--ignore-filename-regex",
            "test-support",
        ],
        root,
    ) || !run(
        "cargo",
        &[
            "llvm-cov",
            "report",
            "--json",
            "--summary-only",
            "--output-path",
            &json_path,
            "--ignore-filename-regex",
            "test-support",
        ],
        root,
    ) {
        return false;
    }
    report_source_line_coverage(&lcov, repo, None);
    for package in CRITICAL_PACKAGES {
        report_source_line_coverage(&lcov, repo, Some(package));
    }
    eprintln!(
        "[xtask] workspace/关键包覆盖率超过 {COVERAGE_FLOOR:.0}% 才算通过；\
         不超过该值仅警告，{COVERAGE_TARGET}% 为长期目标"
    );
    report_diff_coverage(repo, &lcov);
    true
}

#[cfg(test)]
fn parse_coverage_environment(output: &str) -> Result<Vec<(OsString, OsString)>, String> {
    let mut environment = Vec::new();
    for line in output
        .lines()
        .filter_map(|line| line.strip_prefix("export "))
    {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("无法解析 coverage 环境变量: {line}"))?;
        let value = value.trim();
        let value = if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if key.is_empty() {
            return Err("coverage 环境变量名称不能为空".into());
        }
        environment.push((OsString::from(key), OsString::from(value)));
    }
    if environment.is_empty() {
        return Err("cargo llvm-cov show-env 未返回环境变量".into());
    }
    Ok(environment)
}

fn coverage_base(repo: &Path) -> Option<String> {
    if let Ok(value) = std::env::var("RIVO_COVERAGE_BASE") {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    if let Ok(value) = std::env::var("GITHUB_BASE_REF") {
        if !value.trim().is_empty() {
            return Some(format!("origin/{value}"));
        }
    }
    if output("git", &["rev-parse", "--verify", "origin/main"], repo)
        .is_some_and(|result| result.status.success())
    {
        return command_text("git", &["merge-base", "HEAD", "origin/main"], repo);
    }
    output("git", &["rev-parse", "--verify", "HEAD^"], repo)
        .is_some_and(|result| result.status.success())
        .then(|| "HEAD^".to_owned())
}

fn report_diff_coverage(repo: &Path, lcov: &Path) {
    let base = coverage_base(repo);
    if base.is_none() {
        eprintln!("[xtask] warning: 无可用提交基线，跳过 diff coverage 计算");
    }
    let Some(changed) = coverage_changed_lines(repo, base.as_deref()) else {
        eprintln!("[xtask] warning: 无法读取 coverage diff，继续执行");
        return;
    };
    if changed.is_empty() {
        eprintln!("[xtask] 没有变更的 Rust 行，diff coverage 无需检查");
        return;
    }
    let coverage = match parse_lcov(lcov, repo) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[xtask] warning: 无法解析 LCOV，跳过 diff coverage: {error}");
            return;
        }
    };
    let mut executable = 0_usize;
    let mut covered = 0_usize;
    for (path, lines) in changed {
        let normalized = normalize_path(&path);
        let Some(file_coverage) = coverage.get(&normalized) else {
            continue;
        };
        for line in lines {
            if let Some(count) = file_coverage.get(&line) {
                executable += 1;
                covered += usize::from(*count > 0);
            }
        }
    }
    if executable == 0 {
        eprintln!("[xtask] 变更中没有可执行 Rust 行，diff coverage 无需检查");
        return;
    }
    let percent = covered as f64 * 100.0 / executable as f64;
    eprintln!("[xtask] diff coverage: {covered}/{executable} = {percent:.2}%");
    if !coverage_passes(percent) {
        eprintln!(
            "[xtask] warning: diff coverage 不超过 {COVERAGE_FLOOR:.0}%（{percent:.2}%），\
             不阻断门禁"
        );
    }
}

/// 收集候选提交、暂存区与工作树中的 Rust 改动。
///
/// `base...HEAD` 覆盖 CI 中已经提交的候选改动；后两项覆盖本地尚未
/// 提交或尚未暂存的改动。三者必须合并并去重，否则本地 `coverage` 会
/// 错把存在未覆盖改动的工作树判为通过。
fn coverage_changed_lines(
    repo: &Path,
    base: Option<&str>,
) -> Option<BTreeMap<String, BTreeSet<u64>>> {
    let staged = command_text(
        "git",
        &["diff", "--cached", "--unified=0", "--", "*.rs"],
        repo,
    )?;
    let working_tree = command_text("git", &["diff", "--unified=0", "--", "*.rs"], repo)?;

    let mut changed = BTreeMap::new();
    if let Some(base) = base {
        let committed_range = format!("{base}...HEAD");
        let committed = command_text(
            "git",
            &["diff", "--unified=0", &committed_range, "--", "*.rs"],
            repo,
        )?;
        merge_changed_lines(&mut changed, parse_changed_lines(&committed));
    }
    merge_changed_lines(&mut changed, parse_changed_lines(&staged));
    merge_changed_lines(&mut changed, parse_changed_lines(&working_tree));
    Some(changed)
}

fn merge_changed_lines(
    target: &mut BTreeMap<String, BTreeSet<u64>>,
    source: BTreeMap<String, BTreeSet<u64>>,
) {
    for (path, lines) in source {
        target.entry(path).or_default().extend(lines);
    }
}

fn parse_changed_lines(diff: &str) -> BTreeMap<String, BTreeSet<u64>> {
    let hunk = Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@").expect("valid regex");
    let mut current = None::<String>;
    let mut changed = BTreeMap::<String, BTreeSet<u64>>::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current = Some(path.to_owned());
            continue;
        }
        let Some(capture) = hunk.captures(line) else {
            continue;
        };
        let Some(path) = current.as_ref() else {
            continue;
        };
        let start = capture[1].parse::<u64>().unwrap_or_default();
        let count = capture
            .get(2)
            .and_then(|value| value.as_str().parse::<u64>().ok())
            .unwrap_or(1);
        changed
            .entry(normalize_path(path))
            .or_default()
            .extend(start..start.saturating_add(count));
    }
    changed
}

fn parse_lcov(
    path: &Path,
    repo: &Path,
) -> Result<BTreeMap<String, BTreeMap<u64, u64>>, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let mut current = None::<String>;
    let mut coverage = BTreeMap::<String, BTreeMap<u64, u64>>::new();
    for line in content.lines() {
        if let Some(source) = line.strip_prefix("SF:") {
            let source = PathBuf::from(source);
            let relative = source.strip_prefix(repo).unwrap_or(&source);
            current = Some(normalize_path(&relative.to_string_lossy()));
        } else if let Some(data) = line.strip_prefix("DA:") {
            let Some(file) = current.as_ref() else {
                continue;
            };
            if let Some((line, count)) = data.split_once(',') {
                if let (Ok(line), Ok(count)) = (line.parse::<u64>(), count.parse::<u64>()) {
                    coverage
                        .entry(file.clone())
                        .or_default()
                        .insert(line, count);
                }
            }
        }
    }
    Ok(coverage)
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_owned()
}

fn report_source_line_coverage(lcov: &Path, repo: &Path, package: Option<&str>) {
    let label = package.unwrap_or("workspace");
    let coverage = match source_line_coverage(lcov, repo, package) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[xtask] warning: 无法计算 {label} 源代码行覆盖率: {error}");
            return;
        }
    };
    if !coverage_passes(coverage) {
        eprintln!(
            "[xtask] warning: {label} 源代码行覆盖率为 {coverage:.2}%，\
             不超过 {COVERAGE_FLOOR:.0}%，不阻断门禁"
        );
    } else {
        eprintln!("[xtask] {label} 源代码行覆盖率为 {coverage:.2}%，超过 {COVERAGE_FLOOR:.0}%");
    }
}

fn source_line_coverage(lcov: &Path, repo: &Path, package: Option<&str>) -> Result<f64, String> {
    let coverage = match parse_lcov(lcov, repo) {
        Ok(coverage) => coverage,
        Err(error) => {
            return Err(error.to_string());
        }
    };
    let package_dir = package.and_then(package_source_directory);
    let mut total = 0u64;
    let mut covered = 0u64;
    for (file, lines) in coverage {
        if file.starts_with("runtime/crates/test-support/")
            || package_dir
                .is_some_and(|directory| !file.starts_with(&format!("runtime/crates/{directory}/")))
        {
            continue;
        }
        total += lines.len() as u64;
        covered += lines.values().filter(|count| **count > 0).count() as u64;
    }
    if total == 0 {
        return Err("LCOV 没有可计数的源代码行".into());
    }
    let percent = covered as f64 * 100.0 / total as f64;
    Ok(percent)
}

fn package_source_directory(package: &str) -> Option<&'static str> {
    match package {
        "rivo" => Some("cli"),
        "rivo-llm" => Some("llm"),
        "rivo-agent" => Some("agent"),
        "rivo-session" => Some("session"),
        "rivo-policy" => Some("policy"),
        "rivo-tools" => Some("tools"),
        "rivo-storage" => Some("storage"),
        _ => None,
    }
}

#[derive(Debug)]
struct ReleaseOptions {
    live_config: PathBuf,
    report_dir: PathBuf,
}

#[derive(Debug)]
struct LiveTestOptions {
    live_config: PathBuf,
    target: String,
    case_id: String,
}

const LIVE_CASES: &[&str] = &[
    "LIVE-TEXT-001",
    "LIVE-STREAM-001",
    "LIVE-TOOL-001",
    "LIVE-STRUCTURED-001",
    "LIVE-CLI-001",
    "LIVE-CLI-TOOL-001",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseState {
    schema_version: u32,
    #[serde(default)]
    candidate_commit: Option<String>,
    #[serde(default)]
    verified_at: Option<String>,
    requirements: Vec<ReleaseRequirement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseRequirement {
    id: String,
    status: String,
    evidence: String,
    #[serde(default)]
    evidence_files: Vec<String>,
}

fn state_allows_gate(repo: &Path, file_name: &str, label: &str) -> Result<(), String> {
    let path = repo.join("docs").join("status").join(file_name);
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 {label} 状态 {}: {error}", path.display()))?;
    let state: ReleaseState = serde_json::from_str(&content)
        .map_err(|error| format!("{label} 状态不是有效 JSON: {error}"))?;
    let is_release = file_name == "release-state.json";
    let expected_schema = if is_release { 2 } else { 1 };
    if state.schema_version != expected_schema || state.requirements.is_empty() {
        return Err(format!(
            "{label} 状态必须使用 schema_version={expected_schema} 并列出至少一个 requirement"
        ));
    }
    if is_release {
        let candidate_commit = state
            .candidate_commit
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{label} 状态必须具有 candidate_commit"))?;
        let verified_at = state
            .verified_at
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{label} 状态必须具有 verified_at"))?;
        let current_commit = command_text("git", &["rev-parse", "HEAD"], repo)
            .ok_or_else(|| format!("{label} 无法读取当前候选提交"))?;
        let parent_commit = command_text("git", &["rev-parse", "HEAD^"], repo);
        if candidate_commit != current_commit && parent_commit.as_deref() != Some(candidate_commit)
        {
            return Err(format!(
                "{label} candidate_commit {candidate_commit} 与当前提交 {current_commit} 或其父提交不一致"
            ));
        }
        let verified_at = OffsetDateTime::parse(verified_at, &Rfc3339)
            .map_err(|error| format!("{label} verified_at 不是 RFC3339 UTC 时间: {error}"))?;
        let now = OffsetDateTime::now_utc();
        if verified_at > now {
            return Err(format!("{label} verified_at 不能晚于当前时间"));
        }
        if now - verified_at > TimeDuration::days(RELEASE_EVIDENCE_MAX_AGE_DAYS) {
            return Err(format!(
                "{label} 证据已过期，verified_at 超过 {RELEASE_EVIDENCE_MAX_AGE_DAYS} 天"
            ));
        }
        if has_relevant_worktree_changes(
            &command_text(
                "git",
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--",
                    ":!config.env",
                ],
                repo,
            )
            .ok_or_else(|| format!("{label} 无法读取工作树状态"))?,
        ) {
            return Err(format!("{label} 要求工作树干净"));
        }
    }
    let mut blocked = Vec::new();
    for requirement in &state.requirements {
        if requirement.id.trim().is_empty() || requirement.evidence.trim().is_empty() {
            return Err(format!(
                "{label} 状态中的 requirement 必须具有 id 和 evidence"
            ));
        }
        if is_release && requirement.status == "completed" {
            if requirement.evidence_files.is_empty() {
                return Err(format!(
                    "{label} 已完成 requirement {} 必须具有 evidence_files",
                    requirement.id
                ));
            }
            for file in &requirement.evidence_files {
                let path = repo.join(file);
                if !path.is_file() {
                    return Err(format!(
                        "{label} requirement {} 的证据文件不存在: {}",
                        requirement.id,
                        path.display()
                    ));
                }
            }
        }
        if requirement.status != "completed" {
            blocked.push(format!("{} ({})", requirement.id, requirement.status));
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{label} 状态存在未完成要求: {}",
            blocked.join(", ")
        ))
    }
}

fn release_state_allows_release(repo: &Path) -> Result<(), String> {
    state_allows_gate(repo, "release-state.json", "release")
}

fn preview_state_allows_preview(repo: &Path) -> Result<(), String> {
    state_allows_gate(repo, "preview-state.json", "preview")
}

fn run_preview_gate(repo: &Path) -> bool {
    match preview_state_allows_preview(repo) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("[xtask] 本地预览门禁拒绝: {error}");
            false
        }
    }
}

/// Release publication is the only operation that consumes the immutable
/// release-state file. Evidence collection must remain runnable while a
/// requirement is blocked so that it can produce the report needed to clear
/// that requirement.
fn run_release_gate(repo: &Path) -> bool {
    match release_state_allows_release(repo) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("[xtask] 发布门禁拒绝: {error}");
            false
        }
    }
}

fn parse_release_args(args: &[String]) -> Result<ReleaseOptions, String> {
    let mut live_config = None;
    let mut report_dir = None;
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{} 缺少值", args[index]))?;
        match args[index].as_str() {
            "--live-config" => live_config = Some(PathBuf::from(value)),
            "--report-dir" => report_dir = Some(PathBuf::from(value)),
            other => return Err(format!("未知 release-test 参数: {other}")),
        }
        index += 2;
    }
    Ok(ReleaseOptions {
        live_config: live_config.ok_or_else(|| "缺少 --live-config".to_owned())?,
        report_dir: report_dir.ok_or_else(|| "缺少 --report-dir".to_owned())?,
    })
}

fn parse_live_test_args(args: &[String]) -> Result<LiveTestOptions, String> {
    let mut live_config = None;
    let mut target = None;
    let mut case_id = None;
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{} 缺少值", args[index]))?;
        match args[index].as_str() {
            "--live-config" => live_config = Some(PathBuf::from(value)),
            "--target" => target = Some(value.to_owned()),
            "--case" => case_id = Some(value.to_owned()),
            other => return Err(format!("未知 live-test 参数: {other}")),
        }
        index += 2;
    }
    let case_id = case_id.ok_or_else(|| "缺少 --case".to_owned())?;
    if !LIVE_CASES.contains(&case_id.as_str()) {
        return Err(format!("未知 live case: {case_id}"));
    }
    Ok(LiveTestOptions {
        live_config: live_config.ok_or_else(|| "缺少 --live-config".to_owned())?,
        target: target.ok_or_else(|| "缺少 --target".to_owned())?,
        case_id,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveConfig {
    targets: Vec<LiveTarget>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveTarget {
    id: String,
    protocol: ProviderKind,
    model: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    credential_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReleaseReport {
    generated_at_unix: u64,
    commit: String,
    version: String,
    rustc: String,
    dirty: bool,
    success: bool,
    cases: Vec<CaseResult>,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    case_id: String,
    target: Option<String>,
    protocol: Option<String>,
    model: Option<String>,
    duration_ms: u128,
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    curl: Option<String>,
}

async fn run_release_test(root: &Path, repo: &Path, options: ReleaseOptions) -> bool {
    let mut report = ReleaseReport {
        generated_at_unix: unix_timestamp(),
        commit: command_text("git", &["rev-parse", "HEAD"], repo)
            .unwrap_or_else(|| "unknown".into()),
        version: env!("CARGO_PKG_VERSION").into(),
        rustc: command_text("rustc", &["--version"], root).unwrap_or_else(|| "unknown".into()),
        dirty: command_text(
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
            repo,
        )
        .is_none_or(|status| has_relevant_worktree_changes(&status)),
        success: false,
        cases: Vec::new(),
    };
    let config = match read_live_config(&options.live_config) {
        Ok(config) => config,
        Err(error) => {
            report
                .cases
                .push(failed_case("REL-CONFIG-001", Duration::ZERO, error));
            write_release_report(&options.report_dir, &report);
            return false;
        }
    };
    if !record_sync(&mut report, "REL-CREDENTIALS-001", || {
        config
            .targets
            .iter()
            .try_for_each(|target| resolve_live_api_key(target).map(|_| ()))
    }) {
        write_release_report(&options.report_dir, &report);
        return false;
    }
    if report.dirty {
        report.cases.push(failed_case(
            "REL-WORKTREE-001",
            Duration::ZERO,
            "工作树不干净；Release 报告必须对应已提交内容".into(),
        ));
    }
    let check_ok = record_sync(&mut report, "REL-CHECK-001", || {
        run_check(root, repo)
            .then_some(())
            .ok_or_else(|| "确定性质量门禁失败".into())
    });
    let coverage_ok = record_sync(&mut report, "REL-COVERAGE-001", || {
        run_coverage(root, repo)
            .then_some(())
            .ok_or_else(|| "覆盖率门禁失败".into())
    });
    let build_ok = record_sync(&mut report, "REL-BUILD-001", || {
        run(
            "cargo",
            &["build", "--release", "--locked", "-p", "rivo"],
            root,
        )
        .then_some(())
        .ok_or_else(|| "release 二进制构建失败".into())
    });
    if !build_ok {
        write_release_report(&options.report_dir, &report);
        return false;
    }
    let binary =
        root.join("target")
            .join("release")
            .join(if cfg!(windows) { "rivo.exe" } else { "rivo" });
    for target in &config.targets {
        run_live_target(&mut report, target, &binary, None).await;
    }
    if !check_ok || !coverage_ok {
        eprintln!("[xtask] 已完成 live 证据采集，但确定性或覆盖率门禁未通过");
    }
    report.success = report.cases.iter().all(|case| case.status == "passed");
    write_release_report(&options.report_dir, &report);
    report.success
}

async fn run_live_test(root: &Path, options: LiveTestOptions) -> bool {
    let content = match std::fs::read_to_string(&options.live_config) {
        Ok(content) => content,
        Err(error) => {
            eprintln!("live-test 配置读取失败: {error}");
            return false;
        }
    };
    let config: LiveConfig = match toml::from_str(&content) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("live-test 配置无效: {error}");
            return false;
        }
    };
    let matching_targets = config
        .targets
        .iter()
        .filter(|target| target.id == options.target)
        .collect::<Vec<_>>();
    if matching_targets.len() > 1 {
        eprintln!("live-test target id 重复: {}", options.target);
        return false;
    }
    let Some(target) = matching_targets.into_iter().next() else {
        eprintln!("live-test target 不存在: {}", options.target);
        return false;
    };
    if let Err(error) = validate_live_target(target) {
        eprintln!("live-test target 无效: {error}");
        return false;
    }
    let binary =
        root.join("target")
            .join("release")
            .join(if cfg!(windows) { "rivo.exe" } else { "rivo" });
    if !binary.is_file() {
        eprintln!(
            "找不到 release CLI: {}；请先运行 cargo build --release -p rivo",
            binary.display()
        );
        return false;
    }
    let mut report = ReleaseReport {
        generated_at_unix: unix_timestamp(),
        commit: String::new(),
        version: env!("CARGO_PKG_VERSION").into(),
        rustc: String::new(),
        dirty: false,
        success: false,
        cases: Vec::new(),
    };
    eprintln!(
        "[xtask] live-test target={} protocol={} model={} case={}",
        target.id, target.protocol, target.model, options.case_id
    );
    run_live_target(&mut report, target, &binary, Some(&options.case_id)).await;
    let result = report
        .cases
        .iter()
        .find(|case| case.case_id == options.case_id);
    match result {
        Some(case) => {
            println!(
                "{} target={} protocol={} model={} duration_ms={} status={} message={}",
                case.case_id,
                target.id,
                target.protocol,
                target.model,
                case.duration_ms,
                case.status,
                case.message
            );
            case.status == "passed"
        }
        None if report.cases.iter().any(|case| case.status == "failed") => {
            let failure = report
                .cases
                .iter()
                .find(|case| case.status == "failed")
                .expect("failed case exists");
            println!(
                "{} target={} protocol={} model={} duration_ms={} status=failed message={}",
                options.case_id,
                target.id,
                target.protocol,
                target.model,
                failure.duration_ms,
                failure.message
            );
            false
        }
        None => {
            eprintln!("live-test 未产生 case 结果: {}", options.case_id);
            false
        }
    }
}

fn has_relevant_worktree_changes(status: &str) -> bool {
    status.lines().any(|line| {
        let path = line.get(3..).unwrap_or(line).trim();
        let paths = path.split(" -> ");
        paths.into_iter().any(|path| {
            let path = normalize_path(path);
            path != "config.env"
        })
    })
}

fn read_live_config(path: &Path) -> Result<LiveConfig, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("无法读取 live config {}: {error}", path.display()))?;
    let config: LiveConfig = toml::from_str(&content).map_err(|error| error.to_string())?;
    let mut counts = BTreeMap::<String, usize>::new();
    for target in &config.targets {
        *counts.entry(target.protocol.to_string()).or_default() += 1;
    }
    let required_protocol_counts = [
        ("openai_chat_completions", 1),
        ("openai_responses", 1),
        ("claude_messages", 1),
    ];
    let has_exact_protocols = config.targets.len() == 3
        && required_protocol_counts
            .iter()
            .all(|(protocol, count)| counts.get(*protocol) == Some(count));
    if !has_exact_protocols {
        let protocols = counts
            .iter()
            .map(|(protocol, count)| format!("{protocol}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "live config 必须包含 3 个 target，协议计数必须为 openai_chat_completions=1、openai_responses=1、claude_messages=1；当前 target 数为 {}，协议计数为 [{}]",
            config.targets.len(), protocols
        ));
    }
    for target in &config.targets {
        validate_live_target(target)?;
    }
    Ok(config)
}

fn validate_live_target(target: &LiveTarget) -> Result<(), String> {
    if target.id.trim().is_empty() || target.model.trim().is_empty() {
        return Err("live target 的 id 和 model 不能为空".into());
    }
    match (&target.api_key_env, &target.credential_name) {
        (Some(environment), None) if !environment.trim().is_empty() => Ok(()),
        (None, Some(credential)) if !credential.trim().is_empty() => Ok(()),
        _ => Err("每个 live target 必须且只能设置 api_key_env 或 credential_name".into()),
    }
}

/// Resolves a live-test key without ever putting its value in the parsed config
/// or release report.  Local credentials remain opt-in through an explicit
/// `credential_name` in the ignored live-test configuration.
fn resolve_live_api_key(target: &LiveTarget) -> Result<(String, String), String> {
    if let Some(environment) = target.api_key_env.as_deref() {
        return std::env::var(environment)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| (value, environment.to_owned()))
            .ok_or_else(|| format!("环境变量未设置: {environment}"));
    }
    let config_path = std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(rivo_session::config_file_path)
        .ok_or_else(|| "无法确定本地配置文件路径".to_owned())?;
    resolve_live_credential_at(target, &config_path)
}

fn resolve_live_credential_at(
    target: &LiveTarget,
    config_path: &Path,
) -> Result<(String, String), String> {
    let credential = target
        .credential_name
        .as_deref()
        .ok_or_else(|| "live target 缺少 Credential 名称".to_owned())?;
    let key = rivo_session::get_credential(config_path, credential)
        .map_err(|error| format!("无法读取本地 Credential {credential}: {error}"))?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("本地 Credential 不存在或为空: {credential}"))?;
    // The child binary needs an environment variable, but this process-local
    // name never appears in the user configuration or release report.
    Ok((key, "RIVO_RELEASE_LOCAL_CREDENTIAL".into()))
}

async fn run_live_target(
    report: &mut ReleaseReport,
    target: &LiveTarget,
    binary: &Path,
    only_case: Option<&str>,
) {
    let (api_key, runtime_api_key_env) = match resolve_live_api_key(target) {
        Ok(value) => value,
        Err(message) => {
            push_live_failure(report, target, "LIVE-CONFIG-001", Duration::ZERO, message);
            return;
        }
    };
    let base_url = target
        .base_url
        .clone()
        .unwrap_or_else(|| target.protocol.default_base_url().into());
    let client = match create_client(
        target.protocol,
        ProviderConfig {
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            timeout_seconds: 60,
            max_stream_event_bytes: 1024 * 1024,
        },
    ) {
        Ok(client) => client,
        Err(error) => {
            push_live_failure(
                report,
                target,
                "LIVE-CLIENT-001",
                Duration::ZERO,
                redact_message(&error.to_string(), &api_key),
            );
            return;
        }
    };

    if only_case.is_none_or(|case| case == "LIVE-TEXT-001") {
        record_live_async(report, target, "LIVE-TEXT-001", &api_key, async {
            let response = client
                .complete(ProviderRequest::text(
                    target.model.clone(),
                    "Reply with the exact token RIVO_LIVE_OK.",
                ))
                .await
                .map_err(|error| error.to_string())?;
            (!response.message.text_content().trim().is_empty())
                .then_some(())
                .ok_or_else(|| "complete 返回空文本".into())
        })
        .await;
    }

    if only_case.is_none_or(|case| case == "LIVE-STREAM-001") {
        record_live_async(report, target, "LIVE-STREAM-001", &api_key, async {
            let mut stream = client.stream(ProviderRequest::text(
                target.model.clone(),
                "Reply with the exact token RIVO_LIVE_STREAM_OK.",
            ));
            let mut text = String::new();
            let mut done = false;
            while let Some(event) = stream.next().await {
                match event.map_err(|error| error.to_string())? {
                    ProviderStreamEvent::TextDelta { delta } => text.push_str(&delta),
                    ProviderStreamEvent::Done { .. } => done = true,
                    _ => {}
                }
            }
            (!text.trim().is_empty() && done)
                .then_some(())
                .ok_or_else(|| "stream 缺少文本或 Done 终态".into())
        })
        .await;
    }

    if only_case.is_none_or(|case| case == "LIVE-TOOL-001") {
        record_live_async(report, target, "LIVE-TOOL-001", &api_key, async {
        let response = client
            .complete(tool_request(&target.model))
            .await
            .map_err(|error| error.to_string())?;
        response
            .message
            .content
            .iter()
            .any(|part| matches!(part, rivo_core::ContentPart::ToolUse { name, .. } if name == "echo"))
            .then_some(())
            .ok_or_else(|| "未返回 echo tool call".into())
    })
    .await;
    }

    if only_case.is_none_or(|case| case == "LIVE-STRUCTURED-001") {
        record_live_async(report, target, "LIVE-STRUCTURED-001", &api_key, async {
            let structured = structured_request(&target.model);
            match client.complete(structured).await {
                Ok(response) => {
                    if response
                        .structured_output
                        .as_ref()
                        .is_some_and(|value| value.get("status").is_some())
                    {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
            Err("structured output 缺少 status".into())
        })
        .await;
    }

    if only_case.is_none_or(|case| case == "LIVE-CLI-001") {
        record_live_sync(report, target, "LIVE-CLI-001", &api_key, || {
            let environment = TestEnvironment::new().map_err(|error| error.to_string())?;
            write_live_cli_config(&environment, target, &base_url)?;
            let workspace = environment.workspace().display().to_string();
            let output = environment
                .command(binary)
                .args([
                    "--model".to_owned(),
                    target.model.clone(),
                    "--provider-id".to_owned(),
                    "live-provider".to_owned(),
                    "--api-key-env".to_owned(),
                    runtime_api_key_env.clone(),
                    "--workspace".to_owned(),
                    workspace,
                    "--prompt".to_owned(),
                    "Reply with the exact token RIVO_LIVE_CLI_OK.".to_owned(),
                ])
                .env(&runtime_api_key_env, api_key.clone())
                .run()
                .map_err(|error| error.to_string())?;
            if output.status != 0 || !output.stdout.contains("RIVO_LIVE_CLI_OK") {
                return Err(format!(
                    "CLI live answer failed: status={} stdout={} stderr={}",
                    output.status, output.stdout, output.stderr
                ));
            }
            Ok(())
        });
    }

    if only_case.is_none_or(|case| case == "LIVE-CLI-TOOL-001") {
        record_live_sync(report, target, "LIVE-CLI-TOOL-001", &api_key, || {
            let environment = TestEnvironment::new().map_err(|error| error.to_string())?;
            write_live_cli_config(&environment, target, &base_url)?;
            let nonce = format!("RIVO_FILE_{}", unix_timestamp());
            std::fs::write(environment.workspace().join("live-fixture.txt"), &nonce)
                .map_err(|error| error.to_string())?;
            let workspace = environment.workspace().display().to_string();
            let output = environment
            .command(binary)
            .args([
                "--model".to_owned(),
                target.model.clone(),
                "--provider-id".to_owned(),
                "live-provider".to_owned(),
                "--api-key-env".to_owned(),
                runtime_api_key_env.clone(),
                "--profile".to_owned(),
                "research".to_owned(),
                "--workspace".to_owned(),
                workspace,
                "--prompt".to_owned(),
                "Use the read_file tool to read live-fixture.txt, then include its exact content in the final answer.".to_owned(),
            ])
            .env(&runtime_api_key_env, api_key.clone())
            .timeout(Duration::from_secs(120))
            .run()
            .map_err(|error| error.to_string())?;
            if output.status != 0 || !output.stdout.contains(&nonce) {
                return Err(format!(
                    "CLI file tool failed: status={} stdout={} stderr={}",
                    output.status, output.stdout, output.stderr
                ));
            }
            Ok(())
        });
    }
}

fn write_live_cli_config(
    environment: &TestEnvironment,
    target: &LiveTarget,
    base_url: &str,
) -> Result<(), String> {
    let mut config = FileConfig::default();
    let builtin_model = rivo_session::read_default_catalog()
        .map_err(|error| error.to_string())?
        .models
        .get(&target.model)
        .cloned();
    let vendor = builtin_model
        .as_ref()
        .map(|model| model.vendor.as_str())
        .unwrap_or("live");
    if builtin_model.is_none() {
        config.vendors.insert(
            "live".into(),
            VendorCatalogEntry {
                display_name: "Live test".into(),
            },
        );
    }
    // 协议只由模型决定：内置模型若默认首选协议与 target 不同，写入本地覆盖以固定协议。
    match &builtin_model {
        None => {
            config.models.insert(
                target.model.clone(),
                ModelCatalogEntry {
                    vendor: vendor.into(),
                    display_name: target.model.clone(),
                    supported_protocols: vec![target.protocol],
                    supported_reasoning_efforts: BTreeSet::new(),
                    enabled: true,
                    priority: 0,
                },
            );
        }
        Some(model) if model.supported_protocols.first() != Some(&target.protocol) => {
            let mut model = model.clone();
            model.supported_protocols = vec![target.protocol];
            config.models.insert(target.model.clone(), model);
        }
        Some(_) => {}
    }
    let provider = SessionProviderConfig {
        base_url: base_url.into(),
        auth: AuthConfig { credential: None },
        enabled: true,
        priority: 0,
        discovery: ModelDiscoveryCache {
            config_fingerprint: None,
            supported: Some(true),
            models: vec![target.model.clone()],
            last_checked_at: Some(unix_timestamp()),
            last_error: None,
        },
    };
    let fingerprint = provider_connection_fingerprint("live-provider", &provider);
    let mut provider = provider;
    provider.discovery.config_fingerprint = Some(fingerprint);
    config.providers.insert("live-provider".into(), provider);
    config.defaults.vendor = Some(vendor.into());
    config.defaults.model = Some(target.model.clone());
    config.defaults.provider_id = Some("live-provider".into());
    config.defaults.workspace = Some(environment.workspace().to_path_buf());
    let serialized = toml::to_string_pretty(&config).map_err(|error| error.to_string())?;
    std::fs::write(environment.config_path(), serialized).map_err(|error| error.to_string())
}

fn tool_request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.into(),
        system: None,
        messages: vec![Message::text(
            Role::User,
            "Call the echo tool with value 'live-smoke'. Do not answer with plain text.",
        )],
        tools: vec![ToolDefinition {
            name: "echo".into(),
            description: "Returns the supplied value".into(),
            parameters: serde_json::json!({
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"],
                "additionalProperties":false
            }),
        }],
        tool_choice: ToolChoice::Required,
        response_format: None,
        max_output_tokens: Some(256),
        temperature: Some(0.0),
        reasoning_effort: Default::default(),
        metadata: BTreeMap::new(),
    }
}

fn structured_request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.into(),
        system: None,
        messages: vec![Message::text(
            Role::User,
            "Return only this exact JSON object, with no markdown or explanation: {\"status\":\"ok\"}",
        )],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        response_format: Some(ResponseFormat::JsonSchema {
            name: "release_result".into(),
            schema: serde_json::json!({
                "type":"object",
                "properties":{"status":{"type":"string","enum":["ok"]}},
                "required":["status"],
                "additionalProperties":false
            }),
            strict: true,
        }),
        max_output_tokens: Some(256),
        temperature: Some(0.0),
        reasoning_effort: Default::default(),
        metadata: BTreeMap::new(),
    }
}

async fn record_live_async<F>(
    report: &mut ReleaseReport,
    target: &LiveTarget,
    case_id: &str,
    api_key: &str,
    future: F,
) where
    F: std::future::Future<Output = Result<(), String>>,
{
    let started = Instant::now();
    let result = future.await;
    push_live_result(report, target, case_id, started.elapsed(), result, api_key);
}

fn record_live_sync<F>(
    report: &mut ReleaseReport,
    target: &LiveTarget,
    case_id: &str,
    api_key: &str,
    operation: F,
) where
    F: FnOnce() -> Result<(), String>,
{
    let started = Instant::now();
    let result = operation();
    push_live_result(report, target, case_id, started.elapsed(), result, api_key);
}

fn push_live_result(
    report: &mut ReleaseReport,
    target: &LiveTarget,
    case_id: &str,
    duration: Duration,
    result: Result<(), String>,
    api_key: &str,
) {
    let curl = live_curl(target, case_id, api_key);
    let (status, message) = match result {
        Ok(()) => ("passed", "ok".into()),
        Err(error) => ("failed", redact_message(&error, api_key)),
    };
    report.cases.push(CaseResult {
        case_id: case_id.into(),
        target: Some(target.id.clone()),
        protocol: Some(target.protocol.to_string()),
        model: Some(target.model.clone()),
        duration_ms: duration.as_millis(),
        status: status.into(),
        message: truncate(&message, 4096),
        curl: Some(curl.clone()),
    });
    eprintln!("[xtask] {case_id} curl: {curl}");
}

fn push_live_failure(
    report: &mut ReleaseReport,
    target: &LiveTarget,
    case_id: &str,
    duration: Duration,
    message: String,
) {
    report.cases.push(CaseResult {
        case_id: case_id.into(),
        target: Some(target.id.clone()),
        protocol: Some(target.protocol.to_string()),
        model: Some(target.model.clone()),
        duration_ms: duration.as_millis(),
        status: "failed".into(),
        message: truncate(&message, 4096),
        curl: None,
    });
}

fn record_sync<F>(report: &mut ReleaseReport, case_id: &str, operation: F) -> bool
where
    F: FnOnce() -> Result<(), String>,
{
    let started = Instant::now();
    let result = operation();
    let success = result.is_ok();
    report.cases.push(match result {
        Ok(()) => CaseResult {
            case_id: case_id.into(),
            target: None,
            protocol: None,
            model: None,
            duration_ms: started.elapsed().as_millis(),
            status: "passed".into(),
            message: "ok".into(),
            curl: None,
        },
        Err(error) => failed_case(case_id, started.elapsed(), error),
    });
    success
}

fn failed_case(case_id: &str, duration: Duration, message: String) -> CaseResult {
    CaseResult {
        case_id: case_id.into(),
        target: None,
        protocol: None,
        model: None,
        duration_ms: duration.as_millis(),
        status: "failed".into(),
        message: truncate(&message, 4096),
        curl: None,
    }
}

fn write_release_report(directory: &Path, report: &ReleaseReport) {
    if let Err(error) = std::fs::create_dir_all(directory) {
        eprintln!("[xtask] 无法创建报告目录: {error}");
        return;
    }
    let short_commit = report.commit.chars().take(12).collect::<String>();
    let stem = format!("{}-{short_commit}", report.generated_at_unix);
    let json_path = directory.join(format!("{stem}.json"));
    let markdown_path = directory.join(format!("{stem}.md"));
    if let Ok(json) = serde_json::to_vec_pretty(report) {
        if let Err(error) = std::fs::write(&json_path, json) {
            eprintln!("[xtask] 写入 JSON 报告失败: {error}");
        }
    }
    let mut markdown = format!(
            "# rivo Release 测试报告\n\n- Commit: `{}`\n- Version: `{}`\n- Rust: `{}`\n- Dirty: `{}`\n- Result: `{}`\n\n| Case | Target | Protocol | Model | Duration ms | Status | Message | Curl |\n| --- | --- | --- | --- | ---: | --- | --- | --- |\n",
        report.commit, report.version, report.rustc, report.dirty, report.success
    );
    for case in &report.cases {
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            markdown_cell(&case.case_id),
            markdown_cell(case.target.as_deref().unwrap_or("-")),
            markdown_cell(case.protocol.as_deref().unwrap_or("-")),
            markdown_cell(case.model.as_deref().unwrap_or("-")),
            case.duration_ms,
            markdown_cell(&case.status),
            markdown_cell(&case.message),
            markdown_cell(case.curl.as_deref().unwrap_or("-")),
        ));
    }
    if let Err(error) = std::fs::write(&markdown_path, markdown) {
        eprintln!("[xtask] 写入 Markdown 报告失败: {error}");
    }
    eprintln!("[xtask] Release 报告: {}", markdown_path.display());
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn redact_message(message: &str, api_key: &str) -> String {
    let masked = mask_api_key(api_key);
    truncate(&message.replace(api_key, &masked), 4096)
}

fn mask_api_key(api_key: &str) -> String {
    let _ = api_key;
    "<REDACTED>".into()
}

fn live_curl(target: &LiveTarget, case_id: &str, api_key: &str) -> String {
    let base = target
        .base_url
        .as_deref()
        .unwrap_or_else(|| target.protocol.default_base_url());
    let stream = case_id.contains("STREAM");
    let tool_case = case_id.contains("TOOL");
    let structured_case = case_id.contains("STRUCTURED");
    let (url, headers, body) = match target.protocol {
        ProviderKind::OpenAiChatCompletions => (
            format!("{}/chat/completions", base.trim_end_matches('/')),
            "-H \"Authorization: Bearer ${RIVO_API_KEY}\"",
            curl_body(
                target,
                stream,
                tool_case,
                structured_case,
                case_id.contains("CACHE"),
            ),
        ),
        ProviderKind::OpenAiResponses => (
            format!("{}/responses", base.trim_end_matches('/')),
            "-H \"Authorization: Bearer ${RIVO_API_KEY}\"",
            curl_body(
                target,
                stream,
                tool_case,
                structured_case,
                case_id.contains("CACHE"),
            ),
        ),
        ProviderKind::ClaudeMessages => (
            format!(
                "{}/messages",
                base.trim_end_matches('/').to_owned()
                    + if base.trim_end_matches('/').ends_with("/v1") {
                        ""
                    } else {
                        "/v1"
                    }
            ),
            "-H \"x-api-key: ${RIVO_API_KEY}\" -H 'anthropic-version: 2023-06-01'",
            curl_body(
                target,
                stream,
                tool_case,
                structured_case,
                case_id.contains("CACHE"),
            ),
        ),
    };
    format!(
        "curl --fail-with-body -sS '{}' {} -H 'Content-Type: application/json' --data '{}' # api_key={}",
        shell_single_quote(&url),
        headers,
        shell_single_quote(&body),
        mask_api_key(api_key)
    )
}

fn curl_body(
    target: &LiveTarget,
    stream: bool,
    tool_case: bool,
    structured_case: bool,
    cache_case: bool,
) -> String {
    let mut body = match target.protocol {
        ProviderKind::OpenAiChatCompletions => json!({
            "model": target.model,
            "messages": [{"role":"user", "content":"Reply with OK only."}],
            "stream": stream,
        }),
        ProviderKind::OpenAiResponses => json!({
            "model": target.model,
            "input": "Reply with OK only.",
            "stream": stream,
        }),
        ProviderKind::ClaudeMessages => json!({
            "model": target.model,
            "max_tokens": 16,
            "messages": [{"role":"user", "content":"Reply with OK only."}],
        }),
    };
    if target.protocol == ProviderKind::ClaudeMessages && cache_case {
        body["system"] = json!([{
            "type": "text",
            "text": "Stable Rivo caching instructions.",
            "cache_control": {"type": "ephemeral"}
        }]);
    }
    if tool_case {
        body["tools"] = match target.protocol {
            ProviderKind::OpenAiChatCompletions => {
                json!([{"type":"function","function":{"name":"echo","parameters":{"type":"object"}}}])
            }
            ProviderKind::OpenAiResponses => {
                json!([{"type":"function","name":"echo","parameters":{"type":"object"}}])
            }
            ProviderKind::ClaudeMessages => {
                json!([{"name":"echo","input_schema":{"type":"object"}}])
            }
        };
        body["tool_choice"] = match target.protocol {
            ProviderKind::ClaudeMessages => json!({"type":"tool","name":"echo"}),
            _ => json!("required"),
        };
    }
    if structured_case {
        let structured_prompt =
            "Return only this exact JSON object, with no markdown or explanation: {\"status\":\"ok\"}";
        match target.protocol {
            ProviderKind::OpenAiChatCompletions => {
                body["messages"][0]["content"] = json!(structured_prompt);
            }
            ProviderKind::OpenAiResponses => {
                body["input"] = json!(structured_prompt);
            }
            ProviderKind::ClaudeMessages => {
                body["messages"][0]["content"] = json!(structured_prompt);
            }
        }
        body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"release_result","schema":{"type":"object"},"strict":true}});
        if target.protocol == ProviderKind::OpenAiResponses {
            body.as_object_mut().unwrap().remove("response_format");
            body["text"] = json!({"format":{"type":"json_schema","name":"release_result","schema":{"type":"object"},"strict":true}});
        }
        if target.protocol == ProviderKind::ClaudeMessages {
            body.as_object_mut().unwrap().remove("response_format");
            body["tools"] = json!([{"name":"release_result","input_schema":{"type":"object"}}]);
            body["tool_choice"] = json!({"type":"tool","name":"release_result"});
        }
    }
    serde_json::to_string(&body).expect("curl body is serializable")
}

#[cfg(test)]
fn structured_text_has_status(parts: &[rivo_core::ContentPart]) -> bool {
    parts.iter().any(|part| match part {
        rivo_core::ContentPart::Text { text } => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .is_some_and(|value| value.get("status").is_some()),
        _ => false,
    })
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &value[..end])
}

fn command_text(program: &str, args: &[&str], cwd: &Path) -> Option<String> {
    // `current_dir` is normally sufficient, but Git for Windows can resolve
    // a temporary repository differently when invoked from a test process.
    // Pin the repository explicitly for Git commands so release-state checks
    // behave identically for the real checkout and temporary test repos.
    let result = if program == "git" {
        let cwd_text = cwd.to_string_lossy().into_owned();
        let mut command_args = vec!["-C", cwd_text.as_str()];
        command_args.extend_from_slice(args);
        Command::new(program).args(&command_args).output().ok()?
    } else {
        output(program, args, cwd)?
    };
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_arguments_require_both_paths() {
        assert!(parse_release_args(&[]).is_err());
        let parsed = parse_release_args(&[
            "--live-config".into(),
            "live.toml".into(),
            "--report-dir".into(),
            "reports".into(),
        ])
        .unwrap();
        assert_eq!(parsed.live_config, PathBuf::from("live.toml"));
        assert_eq!(parsed.report_dir, PathBuf::from("reports"));
    }

    #[test]
    fn external_test_arguments_require_known_category() {
        assert_eq!(
            parse_external_test_args(&["--category".into(), "tui-pty".into()]),
            Ok(ExternalTestCategory::TuiPty)
        );
        assert_eq!(
            parse_external_test_args(&["--category".into(), "all".into()]),
            Ok(ExternalTestCategory::All)
        );
        assert!(parse_external_test_args(&[]).is_err());
        assert!(parse_external_test_args(&["--category".into(), "unknown".into()]).is_err());
    }

    #[test]
    fn optional_status_is_explicitly_three_state() {
        assert_ne!(OptionalTestStatus::Passed, OptionalTestStatus::Skipped);
        assert_ne!(OptionalTestStatus::Skipped, OptionalTestStatus::Failed);
    }

    #[test]
    fn coverage_environment_parser_preserves_profile_template_and_wrapper_flags() {
        let environment = parse_coverage_environment(
            "export LLVM_PROFILE_FILE='C:/work/runtime-%p-%32m.profraw'\n\
             export RUSTC_WRAPPER='C:/Users/test/cargo-llvm-cov.exe'\n\
             export __CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS='-C\u{1f}instrument-coverage\u{1f}--cfg=coverage'\n",
        )
        .unwrap();
        assert!(environment.iter().any(|(key, value)| {
            key == "LLVM_PROFILE_FILE" && value == "C:/work/runtime-%p-%32m.profraw"
        }));
        assert!(environment.iter().any(|(key, value)| {
            key == "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS"
                && value.to_string_lossy().contains("instrument-coverage")
        }));
    }

    #[test]
    fn frozen_a2a_vectors_verify_cryptographically() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace repository root");
        let directory = repo.join(A2A_PROTOCOL_DIRECTORY);
        verify_sas_vector(&directory).unwrap();
        verify_claim_proof_vector(&directory).unwrap();
        assert!(verify_claim_proof_vector_fixture(
            &directory,
            "fixtures/claim-proof-vector.invalid-signature.json"
        )
        .is_err());
    }

    #[test]
    fn frozen_a2a_protocol_input_passes_full_verification() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace repository root");
        assert!(run_a2a_verify_protocol(repo));
    }

    #[test]
    fn dependency_direction_check_requires_session_and_rejects_tui_bypass() {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path();
        let tui = repo.join("runtime/crates/tui");
        let cli = repo.join("runtime/crates/cli/src");
        std::fs::create_dir_all(&tui).unwrap();
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(
            tui.join("Cargo.toml"),
            "[dependencies]\nrivo-session = { path = \"../session\" }\n",
        )
        .unwrap();
        std::fs::write(
            cli.join("terminal_runtime.rs"),
            "use rivo_session::Session;\n",
        )
        .unwrap();
        assert!(run_dependency_direction_check(repo));

        std::fs::write(
            tui.join("Cargo.toml"),
            "[dependencies]\nrivo-session = { path = \"../session\" }\nrivo-policy = { path = \"../policy\" }\n",
        )
        .unwrap();
        assert!(!run_dependency_direction_check(repo));
    }

    #[test]
    fn a2a_fixture_validators_reject_untrusted_shapes() {
        assert!(exact_object(&serde_json::json!([]), &[], &[]).is_err());
        assert!(exact_object(&serde_json::json!({ "unexpected": true }), &[], &[]).is_err());
        let empty = serde_json::json!({});
        assert!(bounded_string(empty.as_object().unwrap(), "value", 1, 2).is_err());
        assert!(validate_a2a_fixture("unknown.json", &serde_json::json!({})).is_err());
        assert!(validate_a2a_delegate(&serde_json::json!({
            "peer_id": "peer",
            "skill_id": "skill",
            "message": "message",
            "stream": "not-a-boolean",
        }))
        .is_err());
        assert!(validate_a2a_delegate(&serde_json::json!({
            "peer_id": "peer",
            "skill_id": "skill",
            "message": "message",
            "artifacts": "not-an-array",
        }))
        .is_err());
        let invalid_hash = serde_json::json!({ "hash": "g".repeat(64) });
        assert!(require_sha256(invalid_hash.as_object().unwrap(), "hash").is_err());
        let invalid_token = serde_json::json!({ "token": "=".repeat(43) });
        assert!(require_base64url_32(invalid_token.as_object().unwrap(), "token").is_err());
        let invalid_version = serde_json::json!({ "version": "unsupported" });
        assert!(require_lan_version(invalid_version.as_object().unwrap()).is_err());
    }

    #[test]
    fn changed_line_parser_tracks_added_hunks() {
        let changed = parse_changed_lines(
            "+++ b/runtime/crates/cli/src/main.rs\n@@ -10,0 +11,2 @@\n+one\n+two\n",
        );
        assert_eq!(
            changed["runtime/crates/cli/src/main.rs"],
            BTreeSet::from([11, 12])
        );
    }

    #[test]
    fn changed_line_merge_deduplicates_overlapping_hunks() {
        let mut changed = parse_changed_lines(
            "+++ b/runtime/crates/cli/src/main.rs\n@@ -10,0 +11,2 @@\n+one\n+two\n",
        );
        merge_changed_lines(
            &mut changed,
            parse_changed_lines(
                "+++ b/runtime/crates/cli/src/main.rs\n@@ -11,0 +12,2 @@\n+two\n+three\n",
            ),
        );
        assert_eq!(
            changed["runtime/crates/cli/src/main.rs"],
            BTreeSet::from([11, 12, 13])
        );
    }

    #[test]
    fn coverage_changed_lines_includes_committed_staged_and_worktree_changes() {
        fn git(directory: &Path, args: &[&str]) {
            let result = Command::new("git")
                .args(args)
                .current_dir(directory)
                .output()
                .expect("git must be available for xtask tests");
            assert!(
                result.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }

        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        git(
            directory.path(),
            &["config", "user.email", "xtask@example.test"],
        );
        git(directory.path(), &["config", "user.name", "xtask test"]);
        let source = directory.path().join("lib.rs");
        std::fs::write(&source, "pub fn baseline() -> u8 {\n    1\n}\n").unwrap();
        git(directory.path(), &["add", "lib.rs"]);
        git(directory.path(), &["commit", "-m", "baseline"]);

        std::fs::write(
            &source,
            "pub fn baseline() -> u8 {\n    1\n}\n\npub fn committed_marker() -> u8 {\n    2\n}\n",
        )
        .unwrap();
        git(directory.path(), &["add", "lib.rs"]);
        git(directory.path(), &["commit", "-m", "committed change"]);

        let staged = directory.path().join("staged.rs");
        std::fs::write(&staged, "pub fn staged_marker() -> u8 {\n    3\n}\n").unwrap();
        git(directory.path(), &["add", "staged.rs"]);

        std::fs::write(
            &source,
            "pub fn baseline() -> u8 {\n    1\n}\n\npub fn committed_marker() -> u8 {\n    2\n}\n\npub fn worktree_marker() -> u8 {\n    4\n}\n",
        )
        .unwrap();

        let changed = coverage_changed_lines(directory.path(), Some("HEAD^")).unwrap();
        assert!(changed.get("lib.rs").is_some_and(|lines| lines.len() >= 6));
        assert_eq!(changed["staged.rs"], BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn coverage_changed_lines_checks_local_changes_without_a_commit_baseline() {
        fn git(directory: &Path, args: &[&str]) {
            let result = Command::new("git")
                .args(args)
                .current_dir(directory)
                .output()
                .expect("git must be available for xtask tests");
            assert!(
                result.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }

        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        git(
            directory.path(),
            &["config", "user.email", "xtask@example.test"],
        );
        git(directory.path(), &["config", "user.name", "xtask test"]);
        let source = directory.path().join("lib.rs");
        std::fs::write(&source, "pub fn baseline() -> u8 {\n    1\n}\n").unwrap();
        git(directory.path(), &["add", "lib.rs"]);
        git(directory.path(), &["commit", "-m", "baseline"]);

        let staged = directory.path().join("staged.rs");
        std::fs::write(&staged, "pub fn staged_marker() -> u8 {\n    3\n}\n").unwrap();
        git(directory.path(), &["add", "staged.rs"]);
        std::fs::write(
            &source,
            "pub fn baseline() -> u8 {\n    1\n}\n\npub fn worktree_marker() -> u8 {\n    4\n}\n",
        )
        .unwrap();

        let changed = coverage_changed_lines(directory.path(), None).unwrap();
        assert!(changed.get("lib.rs").is_some_and(|lines| !lines.is_empty()));
        assert_eq!(changed["staged.rs"], BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn report_messages_are_redacted_and_bounded() {
        let value = redact_message(&format!("secret {}", "x".repeat(5000)), "secret");
        assert!(!value.contains("secret"));
        assert!(value.len() < 4200);
    }

    #[test]
    fn live_test_args_require_target_case_and_known_case() {
        let args = vec![
            "--live-config".into(),
            "live-test.toml".into(),
            "--target".into(),
            "local-responses".into(),
            "--case".into(),
            "LIVE-CLI-TOOL-001".into(),
        ];
        let options = parse_live_test_args(&args).unwrap();
        assert_eq!(options.target, "local-responses");
        assert_eq!(options.case_id, "LIVE-CLI-TOOL-001");
        assert!(parse_live_test_args(&args[..4]).is_err());
        let mut invalid = args;
        *invalid.last_mut().unwrap() = "LIVE-UNKNOWN-001".into();
        assert!(parse_live_test_args(&invalid).is_err());
    }

    #[test]
    fn live_target_rejects_duplicate_credential_sources() {
        let target = LiveTarget {
            id: "target".into(),
            protocol: ProviderKind::OpenAiResponses,
            model: "model".into(),
            base_url: None,
            api_key_env: Some("KEY".into()),
            credential_name: Some("credential".into()),
        };
        assert!(validate_live_target(&target).is_err());
    }

    #[test]
    fn live_cli_config_uses_schema_five_catalog_and_protocol_fingerprint() {
        let environment = TestEnvironment::new().unwrap();
        let target = LiveTarget {
            id: "live-responses".into(),
            protocol: ProviderKind::OpenAiResponses,
            model: "live-model".into(),
            base_url: Some("https://example.test/v1".into()),
            api_key_env: Some("LIVE_KEY".into()),
            credential_name: None,
        };
        write_live_cli_config(&environment, &target, target.base_url.as_deref().unwrap()).unwrap();

        let contents = std::fs::read_to_string(environment.config_path()).unwrap();
        let config: FileConfig = toml::from_str(&contents).unwrap();
        assert_eq!(config.version, rivo_session::CONFIG_VERSION);
        assert_eq!(config.defaults.vendor.as_deref(), Some("live"));
        assert_eq!(config.defaults.model.as_deref(), Some("live-model"));
        assert_eq!(
            config.defaults.provider_id.as_deref(),
            Some("live-provider")
        );
        assert_eq!(
            config.models["live-model"].supported_protocols,
            vec![ProviderKind::OpenAiResponses]
        );
        let provider = &config.providers["live-provider"];
        assert_eq!(provider.discovery.models, vec!["live-model"]);
        assert_eq!(provider.discovery.supported, Some(true));
        assert_eq!(
            provider.discovery.config_fingerprint,
            Some(provider_connection_fingerprint("live-provider", provider))
        );
    }

    #[test]
    fn live_cli_config_registers_builtin_model_for_isolated_cli_smoke() {
        let environment = TestEnvironment::new().unwrap();
        let target = LiveTarget {
            id: "openai-responses".into(),
            protocol: ProviderKind::OpenAiResponses,
            model: "gpt-5".into(),
            base_url: Some("https://example.test/v1".into()),
            api_key_env: Some("OPENAI_KEY".into()),
            credential_name: None,
        };
        write_live_cli_config(&environment, &target, target.base_url.as_deref().unwrap()).unwrap();

        let contents = std::fs::read_to_string(environment.config_path()).unwrap();
        let config: FileConfig = toml::from_str(&contents).unwrap();
        assert_eq!(config.defaults.vendor.as_deref(), Some("openai"));
        // gpt-5 默认首选 Responses，无需本地覆盖。
        assert!(!config.models.contains_key("gpt-5"));

        let chat = LiveTarget {
            id: "openai-chat".into(),
            protocol: ProviderKind::OpenAiChatCompletions,
            ..target
        };
        write_live_cli_config(&environment, &chat, chat.base_url.as_deref().unwrap()).unwrap();
        let contents = std::fs::read_to_string(environment.config_path()).unwrap();
        let config: FileConfig = toml::from_str(&contents).unwrap();
        assert_eq!(
            config.models["gpt-5"].supported_protocols,
            vec![ProviderKind::OpenAiChatCompletions]
        );
        assert_eq!(config.models["gpt-5"].vendor, "openai");
    }

    #[test]
    fn worktree_filter_ignores_only_local_configuration() {
        assert!(!has_relevant_worktree_changes(" M config.env\n"));
        assert!(has_relevant_worktree_changes(
            " M runtime/crates/llm/src/provider.rs\n"
        ));
        assert!(has_relevant_worktree_changes("?? docs/notes.md\n"));
    }

    #[test]
    fn api_key_mask_never_preserves_key_material() {
        assert_eq!(mask_api_key("short-key"), "<REDACTED>");
        assert_eq!(mask_api_key("abcd1234567890wxyz"), "<REDACTED>");
        assert_eq!(mask_api_key("密钥前缀1234567890后缀"), "<REDACTED>");
        let key = "abcd1234567890wxyz";
        let message = redact_message(&format!("Authorization={key}"), key);
        assert_eq!(message, "Authorization=<REDACTED>");
        assert!(!message.contains(key));
    }

    #[test]
    fn live_curl_is_protocol_specific_and_never_contains_a_key() {
        let cases = [
            (
                ProviderKind::OpenAiChatCompletions,
                "/v1/chat/completions",
                "Authorization: Bearer",
            ),
            (
                ProviderKind::OpenAiResponses,
                "/v1/responses",
                "Authorization: Bearer",
            ),
            (ProviderKind::ClaudeMessages, "/v1/messages", "x-api-key"),
        ];
        for (protocol, path, header) in cases {
            let target = LiveTarget {
                id: "test".into(),
                protocol,
                model: "model".into(),
                base_url: Some("https://example.test/v1".into()),
                api_key_env: Some("TEST_KEY".into()),
                credential_name: None,
            };
            let curl = live_curl(&target, "LIVE-TEXT-001", "abcd1234567890wxyz");
            assert!(curl.contains(path), "{protocol:?}: {curl}");
            assert!(curl.contains(header), "{protocol:?}: {curl}");
            assert!(curl.contains("${RIVO_API_KEY}"));
            assert!(!curl.contains("TEST_KEY"));
            assert!(!curl.contains("secret"));
            assert!(curl.contains("api_key=<REDACTED>"));
            assert!(!curl.contains("abcd1234567890wxyz"));
        }
        let target = LiveTarget {
            id: "test".into(),
            protocol: ProviderKind::OpenAiResponses,
            model: "test-model".into(),
            base_url: Some("https://example.test/v1".into()),
            api_key_env: None,
            credential_name: None,
        };
        let structured = live_curl(&target, "LIVE-STRUCTURED-001", "abcd1234567890wxyz");
        assert!(structured.contains("\"text\":"));
        assert!(structured.contains("\"format\":"));
        for protocol in [
            ProviderKind::OpenAiChatCompletions,
            ProviderKind::ClaudeMessages,
        ] {
            let target = LiveTarget {
                id: "test".into(),
                protocol,
                model: "test-model".into(),
                base_url: Some("https://example.test/v1".into()),
                api_key_env: Some("TEST_KEY".into()),
                credential_name: None,
            };
            let structured = live_curl(&target, "LIVE-STRUCTURED-001", "abcd1234567890wxyz");
            assert!(structured.contains("status"));
        }
        let tool = live_curl(&target, "LIVE-TOOL-001", "abcd1234567890wxyz");
        assert!(tool.contains("\"tools\":"));
        let claude_target = LiveTarget {
            id: "claude".into(),
            protocol: ProviderKind::ClaudeMessages,
            model: "claude-test".into(),
            base_url: Some("https://example.test/v1".into()),
            api_key_env: Some("TEST_KEY".into()),
            credential_name: None,
        };
        let cache = live_curl(&claude_target, "LIVE-CACHE-001", "abcd1234567890wxyz");
        assert!(cache.contains("cache_control"));
        let regular = live_curl(&claude_target, "LIVE-TEXT-001", "abcd1234567890wxyz");
        assert!(!regular.contains("cache_control"));
    }

    #[test]
    fn structured_text_fallback_requires_json_status() {
        assert!(structured_text_has_status(&[
            rivo_core::ContentPart::Text {
                text: r#"{"status":"ok"}"#.into(),
            },
        ]));
        assert!(!structured_text_has_status(&[
            rivo_core::ContentPart::Text { text: "OK".into() },
        ]));
        assert!(!structured_text_has_status(&[]));
    }

    #[test]
    fn source_line_coverage_maps_only_release_packages() {
        assert_eq!(package_source_directory("rivo"), Some("cli"));
        assert_eq!(package_source_directory("rivo-llm"), Some("llm"));
        assert_eq!(package_source_directory("unknown"), None);
    }

    #[test]
    fn coverage_threshold_is_strictly_above_fifty_percent() {
        assert!(coverage_passes(50.01));
        assert!(!coverage_passes(50.0));
        assert!(!coverage_passes(49.99));
    }

    #[test]
    fn source_line_coverage_reads_deduplicated_lcov_lines() {
        let directory = tempfile::tempdir().unwrap();
        let lcov = directory.path().join("lcov.info");
        std::fs::write(
            &lcov,
            "SF:runtime/crates/llm/src/lib.rs\nDA:1,1\nDA:2,0\nDA:2,3\nend_of_record\n",
        )
        .unwrap();
        assert_eq!(
            source_line_coverage(&lcov, directory.path(), Some("rivo-llm"),).unwrap(),
            100.0
        );
        assert!(source_line_coverage(&lcov, directory.path(), Some("rivo")).is_err());
        assert!(source_line_coverage(
            &directory.path().join("missing.info"),
            directory.path(),
            None,
        )
        .is_err());
    }

    #[test]
    fn release_state_requires_candidate_and_evidence_files() {
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("docs/status");
        std::fs::create_dir_all(&status).unwrap();
        std::fs::write(directory.path().join("report.md"), "report\n").unwrap();
        std::fs::write(
            status.join("release-state.json"),
            r#"{"schema_version":2,"candidate_commit":"test-commit","verified_at":"2026-08-31T00:00:00Z","requirements":[{"id":"REL-1","status":"completed","evidence":"manual acceptance completed","evidence_files":["report.md"]}]}"#,
        )
        .unwrap();
        assert!(release_state_allows_release(directory.path()).is_err());
    }

    #[test]
    fn release_state_still_blocks_non_completed_requirements() {
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("docs/status");
        std::fs::create_dir_all(&status).unwrap();
        std::fs::write(
            status.join("release-state.json"),
            r#"{"schema_version":2,"candidate_commit":"test-commit","verified_at":"2026-08-31T00:00:00Z","requirements":[{"id":"REL-1","status":"blocked","evidence":"manual acceptance pending","evidence_files":[]}]}"#,
        )
        .unwrap();
        assert!(release_state_allows_release(directory.path()).is_err());
    }

    #[test]
    fn local_preview_live_config_requires_three_targets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"

[[targets]]
id = "responses"
protocol = "openai_responses"
model = "responses-model"
api_key_env = "RESPONSES_KEY"

[[targets]]
id = "claude"
protocol = "claude_messages"
model = "claude-model"
api_key_env = "CLAUDE_KEY"

"#,
        )
        .unwrap();
        let config = read_live_config(&path).unwrap();
        assert_eq!(config.targets.len(), 3);
    }

    #[test]
    fn live_config_accepts_openai_and_claude() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"

[[targets]]
id = "responses-openai"
protocol = "openai_responses"
model = "responses-model"
api_key_env = "RESPONSES_KEY"

[[targets]]
id = "claude"
protocol = "claude_messages"
model = "claude-model"
api_key_env = "CLAUDE_KEY"
"#,
        )
        .unwrap();
        let config = read_live_config(&path).unwrap();
        assert_eq!(config.targets.len(), 3);
    }

    #[test]
    fn live_config_rejects_only_openai_protocols() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"

[[targets]]
id = "responses"
protocol = "openai_responses"
model = "responses-model"
api_key_env = "RESPONSES_KEY"
"#,
        )
        .unwrap();
        let error = read_live_config(&path).unwrap_err();
        assert!(error.contains("claude_messages"));
        assert!(error.contains("target 数为 2"));
    }

    #[test]
    fn live_config_rejects_missing_or_duplicate_protocols() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat-one"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"

[[targets]]
id = "chat-two"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY_2"
"#,
        )
        .unwrap();
        let error = read_live_config(&path).unwrap_err();
        assert!(error.contains("openai_chat_completions=1、openai_responses=1、claude_messages=1"));
        assert!(error.contains("openai_chat_completions=2"));
    }

    #[test]
    fn live_config_accepts_all_required_protocols() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"

[[targets]]
id = "responses"
protocol = "openai_responses"
model = "responses-model"
api_key_env = "RESPONSES_KEY"

[[targets]]
id = "claude"
protocol = "claude_messages"
model = "claude-model"
api_key_env = "CLAUDE_KEY"

"#,
        )
        .unwrap();
        let config = read_live_config(&path).unwrap();
        assert_eq!(config.targets.len(), 3);
    }

    #[test]
    fn live_test_example_has_the_supported_three_target_template() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace repository root");
        let config = read_live_config(&repo.join("live-test.example.toml")).unwrap();
        assert_eq!(config.targets.len(), 3);
        assert_eq!(
            config
                .targets
                .iter()
                .filter(|target| target.protocol.to_string() == "openai_responses")
                .count(),
            1
        );
        assert_eq!(
            config
                .targets
                .iter()
                .filter(|target| target.protocol.to_string() == "claude_messages")
                .count(),
            1
        );
        assert_eq!(
            config
                .targets
                .iter()
                .map(|target| target.protocol.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "openai_chat_completions".to_owned(),
                "openai_responses".to_owned(),
                "claude_messages".to_owned(),
            ])
        );
    }

    #[test]
    fn live_config_accepts_a_local_credential_reference() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
credential_name = "LOCAL_CHAT"

[[targets]]
id = "responses"
protocol = "openai_responses"
model = "responses-model"
credential_name = "LOCAL_RESPONSES"

[[targets]]
id = "claude"
protocol = "claude_messages"
model = "claude-model"
credential_name = "LOCAL_CLAUDE"

"#,
        )
        .unwrap();
        let config = read_live_config(&path).unwrap();
        assert_eq!(
            config.targets[0].credential_name.as_deref(),
            Some("LOCAL_CHAT")
        );
        assert!(config.targets[0].api_key_env.is_none());
    }

    #[test]
    fn live_config_rejects_ambiguous_key_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.toml");
        std::fs::write(
            &path,
            r#"
[[targets]]
id = "chat"
protocol = "openai_chat_completions"
model = "chat-model"
api_key_env = "CHAT_KEY"
credential_name = "LOCAL_CHAT"

[[targets]]
id = "responses"
protocol = "openai_responses"
model = "responses-model"
api_key_env = "RESPONSES_KEY"

[[targets]]
id = "claude"
protocol = "claude_messages"
model = "claude-model"
api_key_env = "CLAUDE_KEY"

"#,
        )
        .unwrap();
        assert!(read_live_config(&path).is_err());
    }

    #[test]
    fn local_credential_source_is_decrypted_without_an_environment_variable() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        rivo_session::set_credential(&config, "LOCAL_TEST_KEY", "test-key-123456").unwrap();
        let target = LiveTarget {
            id: "chat".into(),
            protocol: ProviderKind::OpenAiChatCompletions,
            model: "test-model".into(),
            base_url: None,
            api_key_env: None,
            credential_name: Some("LOCAL_TEST_KEY".into()),
        };
        let (key, environment) = resolve_live_credential_at(&target, &config).unwrap();
        assert_eq!(key, "test-key-123456");
        assert_eq!(environment, "RIVO_RELEASE_LOCAL_CREDENTIAL");
    }

    #[test]
    fn release_state_requires_all_requirements_to_be_completed() {
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("docs/status");
        std::fs::create_dir_all(&status).unwrap();
        std::fs::write(directory.path().join("report.md"), "report\n").unwrap();
        std::fs::write(
            status.join("release-state.json"),
            r#"{"schema_version":2,"candidate_commit":"test-commit","verified_at":"2026-08-31T00:00:00Z","requirements":[{"id":"REL-1","status":"blocked","evidence":"report","evidence_files":[]}]}"#,
        )
        .unwrap();
        assert!(release_state_allows_release(directory.path()).is_err());
        assert!(!run_release_gate(directory.path()));

        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "test@example.invalid"][..],
            &["config", "user.name", "xtask-test"][..],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success());
        }
        assert!(Command::new("git")
            .args(["add", "report.md"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-qm", "candidate"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        let candidate = command_text("git", &["rev-parse", "HEAD"], directory.path()).unwrap();
        let verified_at = (OffsetDateTime::now_utc() - TimeDuration::days(1))
            .format(&Rfc3339)
            .unwrap();
        std::fs::write(
            status.join("release-state.json"),
            format!(
                r#"{{"schema_version":2,"candidate_commit":"{candidate}","verified_at":"{verified_at}","requirements":[{{"id":"REL-1","status":"completed","evidence":"report","evidence_files":["report.md"]}}]}}"#
            ),
        )
        .unwrap();
        assert!(Command::new("git")
            .args(["add", "docs/status/release-state.json"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-qm", "release metadata"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        assert!(release_state_allows_release(directory.path()).is_ok());
        assert!(run_release_gate(directory.path()));
    }

    #[test]
    fn preview_state_is_independent_from_release_state() {
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("docs/status");
        std::fs::create_dir_all(&status).unwrap();
        std::fs::write(
            status.join("preview-state.json"),
            r#"{"schema_version":1,"requirements":[{"id":"PREVIEW-1","status":"completed","evidence":"report"}]}"#,
        )
        .unwrap();
        std::fs::write(
            status.join("release-state.json"),
            r#"{"schema_version":2,"candidate_commit":"test-commit","verified_at":"2026-08-31T00:00:00Z","requirements":[{"id":"REL-1","status":"blocked","evidence":"report","evidence_files":[]}]}"#,
        )
        .unwrap();
        assert!(preview_state_allows_preview(directory.path()).is_ok());
        assert!(run_preview_gate(directory.path()));
        assert!(release_state_allows_release(directory.path()).is_err());
    }
}

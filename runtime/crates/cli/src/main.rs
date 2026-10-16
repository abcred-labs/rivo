use anyhow::Context;
use async_trait::async_trait;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use fluent_bundle::FluentArgs;
use futures::StreamExt;
use p256::pkcs8::EncodePrivateKey;
use p256::SecretKey;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rivo_a2a::{
    begin_pairing, build_instance_control_jsonrpc_request, claim_pairing,
    create_client_pairing_identity, discover_lan_candidates, fetch_agent_card, psk_peer_id,
    A2aDelegationRequest, A2aIdentityStore, A2aPeerId, A2aTaskClient, A2aTaskId, A2aTransport,
    A2aTrustSnapshot, A2aTrustState, JsonRpcRequest, KeyringA2aIdentityStore, KeyringMtlsTransport,
    PskTransport, MAX_MDNS_TTL_SECONDS, SPEC_REVISION, WIRE_PROTOCOL_VERSION,
};
use rivo_a2a_server::{
    is_allowed_bind_address, server_spki_sha256, A2aHttpsListener, A2aServer, A2aServerFileConfig,
    ApprovalBroker, ClientCertificateIssuer, InboundSessionDriver, InboundSessionEvent,
    InboundSessionProjector, InboundSessionRef, InboundSessionStart, InstanceControlHandler,
    KeyringClientCertificateIssuer, KeyringServerMaterialStore, LocalApproval, LocalApprovalQueue,
    LocalApprovalRequest, PairingRegistry, PairingState, ServerError, ServerStateStore,
};
use rivo_i18n::{LocalLanguageManager, LocaleId, ReloadableLanguageCatalog};
use rivo_instance_control::{
    host_capabilities, registry_path, request_named_pipe, ControlRequest, ControlScope,
    InstanceRegistry, LanControlEnvelope, SkillRegistry,
};
use rivo_llm::{ProviderKind, ReasoningEffort};
use rivo_policy::{CapabilityProfile, PolicyProfile};
use rivo_session::{
    commit_model_routing_change, commit_model_routing_draft, config_file_path,
    delete_model_routing_config, discover_provider_models, merged_model_catalog,
    merged_vendor_catalog, model_catalog_sources, model_providers, plan_model_routing_change,
    plan_model_routing_delete, provider_models, read_default_catalog, read_file_config,
    read_model_routing_draft, refresh_provider_models, resolve_available_route,
    upgrade_file_config, validate_model_routing_config, vendor_catalog_sources, AgentEvent,
    ApprovalScope, AuthConfig, ConfigOverrides, FileConfig, Intent, ModelCatalogEntry,
    ModelDiscoveryCache, ModelSelection, ProviderConfig as RoutingProviderConfig,
    ProviderSelection, ResolvedConfig, Session, SessionEvent, TurnOptions, VendorCatalogEntry,
};
use rivo_tools::mcp::{
    McpClient, McpConnectOptions, McpServerConfig, McpToolRisk, McpToolsetConfig, McpTransport,
};
use rivo_tools::oauth::{McpHttpAuth, OAuthConfig};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static CONTROL_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
use std::time::Duration;

mod app_server;
mod browser_extension_smoke;
mod diagnostics;
mod instance;
mod language;
mod output;
#[cfg(test)]
#[path = "release_version.rs"]
mod release_version;
mod sessions;
mod terminal_runtime;
mod ui;
mod upgrade;
mod workspace;
use output::{
    parse_query_format, print_query, resolve_query_format, QueryOutput, QueryOutputFormat, Table,
};
use sessions::run_sessions_command;
#[cfg(test)]
use sessions::{run_sessions_command_at, run_sessions_command_at_with_terminal};
use terminal_runtime::TerminalRuntime;

/// `rivo run` 的稳定参数接口。
#[derive(Parser, Debug)]
#[command(name = "rivo run", version = env!("RIVO_VERSION_DISPLAY"), about = "运行一个 Rivo 任务")]
struct RunCli {
    /// UI 与默认模型回复语言，必须是 canonical BCP 47 locale。
    #[arg(long = "language")]
    language: Option<String>,
    /// 模型发布方的规范 ID。
    #[arg(long)]
    vendor: Option<String>,
    /// 仅本次运行读取 API key 的环境变量名。
    #[arg(long)]
    api_key_env: Option<String>,
    /// 已配置的 Provider 连接 ID。
    #[arg(long)]
    provider_id: Option<String>,
    /// 自动选择 Provider，覆盖配置中的默认 Provider。
    #[arg(long, conflicts_with = "provider_id")]
    auto_provider: bool,

    /// 精确模型 ID；覆盖配置中的默认模型。
    #[arg(long)]
    model: Option<String>,

    /// 能力模式：general、research、plan、coding 或 review；mcp 为旧配置兼容别名。
    #[arg(long, value_parser = parse_capability)]
    profile: Option<CapabilityProfile>,

    /// 安全档位：strict、medium 或 full_control（workspace 为兼容别名）。
    #[arg(long, value_parser = parse_policy)]
    policy: Option<PolicyProfile>,

    /// 推理强度：default、low、medium 或 high；仅作用于当前 Turn。
    #[arg(long, value_parser = parse_reasoning_effort)]
    reasoning_effort: Option<ReasoningEffort>,

    /// 当前任务的工作区根目录。
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// 仅为当前 Turn 显式启用 Web 工具。
    #[arg(long)]
    network: bool,

    /// 仅为当前 Turn 显式启用本机 CDP 浏览器工具。
    #[arg(long)]
    browser: bool,

    /// 仅为当前 Turn 显式启用 Windows Computer Use 工具。
    #[arg(long)]
    computer: bool,

    /// 仅为 coding run 显式启用受确认保护的原始 Shell 工具。
    #[arg(long)]
    shell: bool,

    /// 覆盖配置中的本机 Chrome/Edge CDP 地址。
    #[arg(long)]
    browser_cdp_url: Option<String>,

    /// 仅为当前 Turn 显式启用配置 allowlist 中的 MCP server；可重复指定。
    #[arg(long = "mcp-server")]
    mcp_servers: Vec<String>,

    /// 为当前 Turn 选择 MCP toolset，格式为 <server>/<toolset>；可重复指定。
    #[arg(long = "mcp-toolset", value_parser = parse_mcp_toolset)]
    mcp_toolsets: Vec<String>,

    /// 为当前 Turn 选择已完成本机配对的 A2A peer；可重复指定。
    #[arg(long = "a2a-peer")]
    a2a_peers: Vec<String>,

    /// 在当前 Turn 记录并显示追踪摘要。
    #[arg(long)]
    trace: bool,

    /// 关闭默认可见的内部子代理委派工具。
    #[arg(long)]
    no_subagents: bool,

    /// 同时运行的内部子代理数；默认 5。
    #[arg(long)]
    max_subagents: Option<usize>,

    /// 关闭当前 Turn 的持久记忆召回。
    #[arg(long)]
    no_memory_recall: bool,

    /// 从标准输入读取 UTF-8 任务；不能与 --prompt 同时使用。
    #[arg(long, conflicts_with = "prompt")]
    stdin: bool,

    /// 本次运行的任务描述或输入文本。
    #[arg(long, required_unless_present = "stdin")]
    prompt: Option<String>,

    /// Import a local PNG/JPEG/WebP image as a user attachment; repeat up to eight times.
    #[arg(long = "image")]
    image: Vec<PathBuf>,

    /// 输出格式：text 供人阅读；jsonl 供脚本稳定消费。
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// 授权策略。未指定时，TTY 使用 prompt，其他环境使用 deny。
    #[arg(long, value_enum)]
    approval_mode: Option<ApprovalMode>,

    /// 审批清单 JSON；只在 approval-mode=manifest 时允许变更工具。
    #[arg(long, requires = "approval_mode")]
    approval_manifest: Option<PathBuf>,
}

#[derive(Debug, Parser)]
#[command(
    name = "rivo config",
    about = "管理 Rivo Vendor、Model、Provider 和 Credential",
    disable_help_flag = true,
    disable_help_subcommand = true,
    after_help = "Examples:\n  rivo config show\n  rivo config provider list\n  rivo config model list"
)]
struct ConfigCli {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
    /// 显示帮助。
    #[arg(
        short = 'h',
        long = "help",
        action = clap::ArgAction::Help,
        global = true
    )]
    help: Option<bool>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 显示配置文件路径。
    Path,
    /// 显示脱敏后的有效配置。
    Show,
    /// 验证配置和模型目录。
    Validate,
    /// 设置配置项。
    Set(ConfigValueArgs),
    /// 清除配置项；部分集合项需要 VALUE 指定目标。
    Unset(ConfigValueArgs),
    /// 管理本地 Credential。
    Credentials {
        #[command(subcommand)]
        command: Option<CredentialsCommand>,
    },
    /// 预览 Vendor/Model 到实际 Provider 的确定性路由，不发起调用。
    Route {
        #[arg(long)]
        vendor: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        provider_id: Option<String>,
        /// 查询输出格式：text（默认）、table 或 json。
        #[arg(long, value_enum)]
        format: Option<QueryOutputFormat>,
    },
    /// 管理本地 Vendor 目录（list|show|set|remove）。
    Vendor {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 管理规范 Model 目录（list|show|set|remove）。
    Model {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 管理实际 Provider 连接（list|show|set|remove）。
    Provider {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Args)]
struct ConfigValueArgs {
    /// 配置项名称。
    key: String,
    /// 配置值；unset mcp.server 时用于指定 server 名称。
    value: Option<String>,
}

#[derive(Debug, Subcommand)]
enum CredentialsCommand {
    /// 显示 Credential 文件路径。
    Path,
    /// 列出 Credential 名称和保存状态，不显示密钥正文。
    Show,
    /// 交互式保存 Credential，或从环境变量读取。
    Set {
        /// Credential 名称。
        name: String,
        /// 从指定环境变量读取 API key。
        #[arg(long, value_name = "ENV")]
        from_env: Option<String>,
    },
    /// 删除 Credential。
    Unset {
        /// Credential 名称。
        name: String,
    },
    /// 将旧版明文 Credential 迁移为加密存储。
    Migrate,
}

impl ConfigCommand {
    fn into_args(self) -> Vec<String> {
        match self {
            Self::Path => vec!["path".into()],
            Self::Show => vec!["show".into()],
            Self::Validate => vec!["validate".into()],
            Self::Set(args) => config_value_args("set", args),
            Self::Unset(args) => config_value_args("unset", args),
            Self::Credentials { command } => credentials_args(command),
            Self::Route {
                vendor,
                model,
                provider_id,
                format,
            } => {
                let mut values = vec!["route".into(), "--vendor".into(), vendor];
                if let Some(model) = model {
                    values.extend(["--model".into(), model]);
                }
                if let Some(provider_id) = provider_id {
                    values.extend(["--provider-id".into(), provider_id]);
                }
                if let Some(format) = format {
                    values.extend([
                        "--format".into(),
                        format.to_possible_value().unwrap().get_name().into(),
                    ]);
                }
                values
            }
            Self::Vendor { mut args } => {
                let mut values = vec!["vendor".into()];
                values.append(&mut args);
                values
            }
            Self::Model { mut args } => {
                let mut values = vec!["model".into()];
                values.append(&mut args);
                values
            }
            Self::Provider { mut args } => {
                let mut values = vec!["provider".into()];
                values.append(&mut args);
                values
            }
        }
    }
}

fn config_value_args(command: &str, args: ConfigValueArgs) -> Vec<String> {
    let mut values = vec![command.to_owned(), args.key];
    values.extend(args.value);
    values
}

fn credentials_args(command: Option<CredentialsCommand>) -> Vec<String> {
    let mut values = vec!["credentials".into()];
    match command.unwrap_or(CredentialsCommand::Show) {
        CredentialsCommand::Path => values.push("path".into()),
        CredentialsCommand::Show => values.push("show".into()),
        CredentialsCommand::Set { name, from_env } => {
            values.extend(["set".into(), name]);
            if let Some(environment) = from_env {
                values.extend(["--from-env".into(), environment]);
            }
        }
        CredentialsCommand::Unset { name } => values.extend(["unset".into(), name]),
        CredentialsCommand::Migrate => values.push("migrate".into()),
    }
    values
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Jsonl,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum ApprovalMode {
    Prompt,
    Deny,
    Manifest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalManifest {
    version: u32,
    workspace: String,
    rules: Vec<ApprovalRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalRule {
    tool_name: String,
    arguments_sha256: String,
    scope: ApprovalScope,
    #[serde(default = "default_max_calls")]
    max_calls: u32,
}

fn default_max_calls() -> u32 {
    1
}

struct ManifestApprovals {
    rules: Vec<ApprovalRule>,
    used: Vec<u32>,
}

impl ManifestApprovals {
    fn load(path: &std::path::Path, workspace: &std::path::Path) -> anyhow::Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("无法读取审批清单: {}", path.display()))?;
        let manifest: ApprovalManifest = serde_json::from_slice(&bytes)
            .context("审批清单必须是符合 approval-manifest-v1 的 JSON")?;
        if manifest.version != 1 {
            anyhow::bail!("不支持的审批清单版本: {}", manifest.version);
        }
        for rule in &manifest.rules {
            if rule.tool_name.trim().is_empty()
                || rule.max_calls == 0
                || rule.arguments_sha256.len() != 64
                || !rule
                    .arguments_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                anyhow::bail!(
                    "审批清单规则无效：tool_name、arguments_sha256 和 max_calls 必须有效"
                );
            }
            if rule.scope != ApprovalScope::Once {
                anyhow::bail!("CLI 审批清单仅支持 scope=once");
            }
        }
        let declared = std::path::PathBuf::from(&manifest.workspace)
            .canonicalize()
            .with_context(|| format!("审批清单 workspace 不存在: {}", manifest.workspace))?;
        let actual = workspace.canonicalize().with_context(|| {
            format!(
                "当前 workspace 不存在，无法校验审批清单: {}",
                workspace.display()
            )
        })?;
        if declared != actual {
            anyhow::bail!(
                "审批清单 workspace 与当前任务不匹配: {} != {}",
                declared.display(),
                actual.display()
            );
        }
        let used = vec![0; manifest.rules.len()];
        Ok(Self {
            rules: manifest.rules,
            used,
        })
    }

    fn allows(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
        scope: ApprovalScope,
    ) -> bool {
        let arguments_sha256 = canonical_json_sha256(arguments);
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.tool_name == tool_name
                && rule
                    .arguments_sha256
                    .eq_ignore_ascii_case(&arguments_sha256)
                && rule.scope == scope
                && self.used[index] < rule.max_calls
            {
                self.used[index] += 1;
                return true;
            }
        }
        false
    }
}

fn canonical_json_sha256(value: &serde_json::Value) -> String {
    fn write_value(value: &serde_json::Value, output: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                output.push('{');
                let mut values = map.iter().collect::<Vec<_>>();
                values.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (key, value)) in values.into_iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key).expect("JSON key serializes"));
                    output.push(':');
                    write_value(value, output);
                }
                output.push('}');
            }
            serde_json::Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write_value(value, output);
                }
                output.push(']');
            }
            _ => output.push_str(&serde_json::to_string(value).expect("JSON value serializes")),
        }
    }
    let mut canonical = String::new();
    write_value(value, &mut canonical);
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // rustls 0.23 requires an explicit process-level provider when both
    // provider backends are available in the workspace.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let exit_code = match run_program().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rivo: {error:#}");
            2
        }
    };
    // Returning normally is semantically equivalent to `process::exit` for
    // this binary, while still allowing runtime instrumentation (including
    // LLVM coverage used by the black-box CLI tests) to flush at shutdown.
    std::process::ExitCode::from(exit_code as u8)
}

async fn run_program() -> anyhow::Result<i32> {
    run_program_with_args(std::env::args().collect()).await
}

async fn run_program_with_args(args: Vec<String>) -> anyhow::Result<i32> {
    let help_locale = cli_help_locale(&args)?;
    ui::set_locale(help_locale.clone());
    let mut dispatch_args = args.clone();
    if dispatch_args.get(1).is_some_and(|arg| arg == "--language") {
        if dispatch_args.len() < 3 {
            anyhow::bail!("--language requires a canonical BCP 47 locale");
        }
        dispatch_args.drain(1..=2);
    } else if dispatch_args
        .get(1)
        .is_some_and(|arg| arg.starts_with("--language="))
    {
        dispatch_args.remove(1);
    }
    match dispatch_args.get(1).map(String::as_str) {
        None => {
            print_root_help(help_locale.as_deref());
            Ok(0)
        }
        Some("app-server") => {
            app_server::run(dispatch_args.into_iter().skip(2).collect()).await?;
            Ok(0)
        }
        Some("browser-extension-host") => {
            if dispatch_args.len() > 2
                && matches!(
                    dispatch_args.get(2).map(String::as_str),
                    Some("--help" | "-h")
                )
            {
                println!("Usage: rivo browser-extension-host\n\nRun the Chrome/Edge Native Messaging host and local named-pipe broker.");
                Ok(0)
            } else if dispatch_args.len() != 2 {
                anyhow::bail!("Usage: rivo browser-extension-host")
            } else {
                rivo_tools::browser_extension_host::run()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                Ok(0)
            }
        }
        Some("browser-extension-smoke") => {
            browser_extension_smoke::run(command_args(&dispatch_args, 2)).await
        }
        Some("browser-extension-seo-smoke") => {
            browser_extension_smoke::run_seo_smoke(command_args(&dispatch_args, 2)).await
        }
        Some("tui")
            if ui::has_explicit_locale()
                && dispatch_args
                    .iter()
                    .any(|arg| arg == "--help" || arg == "-h") =>
        {
            println!("{}", ui::text("cli-tui-help", "Open the interactive terminal workspace.\n\nUsage: rivo tui [OPTIONS]\n\nOptions: --workspace <PATH>  Workspace for this TUI session; -h, --help  Show help"));
            Ok(0)
        }
        Some("tui")
            if dispatch_args.len() == 3
                && matches!(
                    dispatch_args.get(2).map(String::as_str),
                    Some("--help" | "-h")
                ) =>
        {
            println!("{}", cli_text(help_locale.as_deref(), "cli-tui-help", "Open the interactive terminal workspace.\n\nUsage: rivo tui [OPTIONS]\n\nOptions:\n      --workspace <PATH>  Workspace for this TUI session\n  -h, --help              Show help\n\nExamples:\n  rivo tui\n  rivo tui --workspace C:\\path\\to\\project"));
            Ok(0)
        }
        Some("tui") => {
            let workspace = parse_tui_workspace(&dispatch_args)?;
            run_tui(workspace).await?;
            Ok(0)
        }
        Some("config") => run_config_command(command_args(&dispatch_args, 2)).await,
        Some("language") => language::run(command_args(&dispatch_args, 2)).await,
        Some("mcp") => {
            run_mcp_command(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("a2a") => {
            run_a2a_command(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("instance") => {
            instance::run(command_args(&dispatch_args, 2))?;
            Ok(0)
        }
        Some("sessions") => {
            run_sessions_command(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("workspace") => {
            workspace::run_workspace_command(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("diagnostics") => {
            diagnostics::run(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("upgrade") => {
            run_upgrade_command(command_args(&dispatch_args, 2)).await?;
            Ok(0)
        }
        Some("run")
            if ui::has_explicit_locale()
                && dispatch_args
                    .iter()
                    .any(|arg| arg == "--help" || arg == "-h") =>
        {
            println!("{}", ui::text("cli-run-help", "Run a Rivo task.\n\nUsage: rivo run --prompt <TEXT> [OPTIONS]\n       rivo run --stdin [OPTIONS]\n\nOptions: --language <BCP47> --vendor <ID> --model <ID> --provider-id <ID> --format <text|jsonl> --approval-mode <prompt|deny|manifest>\n\nTasks must be supplied with --prompt or --stdin."));
            Ok(0)
        }
        Some("run") if dispatch_args.len() == 2 => {
            print_run_help();
            Ok(0)
        }
        Some("run") => run_cli(dispatch_args.into_iter().skip(1).collect()).await,
        Some("--help" | "-h") => {
            print_root_help(help_locale.as_deref());
            Ok(0)
        }
        Some(option) if option.starts_with('-') => run_cli(dispatch_args).await,
        Some(command) => anyhow::bail!(
            "未知命令: {command}\n\nUsage:\n  rivo <COMMAND>\n\n运行 `rivo --help` 查看可用命令。"
        ),
    }
}

fn command_args(args: &[String], start: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = start;
    while index < args.len() {
        if args[index] == "--language" {
            index += 2;
            continue;
        }
        if args[index].starts_with("--language=") {
            index += 1;
            continue;
        }
        result.push(args[index].clone());
        index += 1;
    }
    result
}

fn parse_tui_workspace(args: &[String]) -> anyhow::Result<Option<PathBuf>> {
    let usage = "Usage: rivo tui [--workspace <PATH>]";
    match args.get(2..) {
        Some([]) => Ok(None),
        Some([option, workspace]) if option == "--workspace" && !workspace.is_empty() => {
            Ok(Some(PathBuf::from(workspace)))
        }
        Some([option]) => option
            .strip_prefix("--workspace=")
            .filter(|workspace| !workspace.is_empty())
            .map(PathBuf::from)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!(usage)),
        _ => anyhow::bail!(usage),
    }
}

fn print_root_help(locale: Option<&str>) {
    println!("{}", cli_text(locale, "cli-root-help", "Rivo — General Agent CLI\n\nUsage:\n  rivo <COMMAND>\n  rivo run --prompt <TEXT> [OPTIONS]\n  rivo run --stdin [OPTIONS]\n\nCommands:\n  run          Run a task\n  tui          Open the interactive terminal workspace\n  config       Manage Vendor, Model, Provider and Credential\n  language     Manage local UI language files\n  sessions     View, resume or delete local sessions\n  mcp          Manage configured MCP servers\n  a2a          Manage A2A peers and the local A2A server\n  instance     Inspect the local Rivo instance\n  diagnostics  Create a redacted diagnostics bundle\n  upgrade      Check for platform updates (installation is manual)\n  app-server   Start the local JSON-RPC bridge\n\nOptions:\n  -h, --help     Show help\n  -V, --version  Show version\n\nCommon task options: --vendor, --model, --provider-id, --auto-provider, --format and --approval-mode.\nTasks must be supplied explicitly with --prompt or --stdin. Run `rivo <COMMAND> --help` for command help."));
}

fn cli_help_locale(args: &[String]) -> anyhow::Result<Option<String>> {
    let mut locale = None;
    for (index, arg) in args.iter().enumerate() {
        if arg == "--language" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--language requires a canonical BCP 47 locale"))?;
            locale = Some(
                LocaleId::parse(value)
                    .map_err(|error| anyhow::anyhow!("invalid locale: {error}"))?
                    .to_string(),
            );
        } else if let Some(value) = arg.strip_prefix("--language=") {
            locale = Some(
                LocaleId::parse(value)
                    .map_err(|error| anyhow::anyhow!("invalid locale: {error}"))?
                    .to_string(),
            );
        }
    }
    Ok(locale)
}

fn cli_text(locale: Option<&str>, id: &str, fallback: &str) -> String {
    let locale = locale
        .and_then(|value| LocaleId::parse(value).ok())
        .unwrap_or_else(|| LocaleId::parse("en-US").expect("built-in locale"));
    LocalLanguageManager::from_environment()
        .ok()
        .and_then(|manager| ReloadableLanguageCatalog::load(&manager, &locale).ok())
        .and_then(|catalog| catalog.catalog().format(id, None))
        .unwrap_or_else(|| fallback.to_owned())
}

fn print_run_help() {
    RunCli::command().print_help().expect("stdout is writable");
    println!();
}

async fn run_upgrade_command(args: Vec<String>) -> anyhow::Result<()> {
    if matches!(args.as_slice(), [help] if matches!(help.as_str(), "--help" | "-h")) {
        println!(
            "{}",
            ui::text(
                "cli-upgrade-help",
                "Check the latest archive for this platform. Installation is manual in the V1 CLI release.\n\nUsage: rivo upgrade [OPTIONS]\n\nOptions:\n      --check  Compare versions without downloading or installing\n  -h, --help   Show help",
            )
        );
        return Ok(());
    }
    let check_only = match args.as_slice() {
        [] => false,
        [check] if check == "--check" => true,
        _ => anyhow::bail!("Usage: rivo upgrade [--check]"),
    };
    if !check_only {
        anyhow::bail!("V1 CLI archive releases do not support automatic installation; download the matching archive, verify SHA256SUMS, and replace the rivo binary manually");
    }
    upgrade::run().await
}

/// A panic in a TUI reducer, renderer, or input path must not strand the user
/// in raw mode or the alternate screen. The hook is process-wide, so install
/// it once before either of the two TUI screens first enters raw mode.
fn install_tui_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show
            );
            previous(info);
        }));
    });
}

async fn run_cli(args: Vec<String>) -> anyhow::Result<i32> {
    let cli = RunCli::parse_from(args);
    let text = read_task_input(&cli)?;
    let startup_args = serde_json::json!({
        "vendor": cli.vendor.clone(),
        "model": cli.model.clone(),
        "provider_id": cli.provider_id.clone(),
        "auto_provider": cli.auto_provider,
        "profile": format!("{:?}", cli.profile),
        "policy": format!("{:?}", cli.policy),
        "workspace": cli.workspace.clone(),
        "network": cli.network,
        "browser": cli.browser,
        "shell": cli.shell,
        "mcp_servers": cli.mcp_servers.clone(),
        "mcp_toolsets": cli.mcp_toolsets.clone(),
        "api_key_env_set": cli.api_key_env.is_some(),
        "language": cli.language.clone(),
    });
    if cli.max_subagents == Some(0) {
        anyhow::bail!("--max-subagents 必须是正整数");
    }
    if cli.approval_mode == Some(ApprovalMode::Manifest) && cli.approval_manifest.is_none() {
        anyhow::bail!("approval-mode=manifest 必须指定 --approval-manifest <path>");
    }
    if cli.approval_mode != Some(ApprovalMode::Manifest) && cli.approval_manifest.is_some() {
        anyhow::bail!("--approval-manifest 只能与 --approval-mode manifest 一起使用");
    }
    let config = ResolvedConfig::load(ConfigOverrides {
        vendor: cli.vendor,
        api_key_env: cli.api_key_env,
        provider_id: cli.provider_id,
        auto_provider: cli.auto_provider,
        model: cli.model,
        capability_profile: cli.profile,
        policy_profile: cli.policy,
        workspace: cli.workspace,
        browser_cdp_url: cli.browser_cdp_url,
        locale: cli.language,
    })?;
    init_telemetry(&config, "run", &startup_args)?;
    let known_api_keys = config.known_api_keys();
    let session = match Session::connect(config.clone()) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            record_startup_failure(&config, "run", &error)?;
            return Err(session_startup_error(error));
        }
    };
    let media_refs = cli
        .image
        .iter()
        .map(|path| {
            rivo_session::import_media_asset(session.session_id(), path)
                .map_err(|error| anyhow::anyhow!(error))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mcp_toolsets = collect_mcp_toolsets(&cli.mcp_toolsets)?;
    let mut mcp_servers = cli.mcp_servers;
    for server in mcp_toolsets.keys() {
        if !mcp_servers.contains(server) {
            mcp_servers.push(server.clone());
        }
    }
    let options = TurnOptions {
        locale: config.locale.clone(),
        capability_profile: config.capability_profile,
        policy_profile: config.policy_profile,
        reasoning_effort: cli.reasoning_effort.unwrap_or_default(),
        network: cli.network,
        browser: cli.browser,
        computer: cli.computer,
        shell: cli.shell,
        mcp_servers,
        mcp_toolsets,
        trace: cli.trace,
        memory_recall: !cli.no_memory_recall,
        subagents_enabled: config.subagents_enabled && !cli.no_subagents,
        max_subagents: cli.max_subagents.unwrap_or(config.max_subagents),
        a2a_peers: cli.a2a_peers.into_iter().collect(),
    };
    let approval_mode = cli.approval_mode.unwrap_or_else(|| {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            ApprovalMode::Prompt
        } else {
            ApprovalMode::Deny
        }
    });
    let manifest = match (&cli.approval_manifest, approval_mode) {
        (Some(path), ApprovalMode::Manifest) => {
            Some(ManifestApprovals::load(path, &config.workspace)?)
        }
        _ => None,
    };
    execute_headless_turn(
        session,
        text,
        options,
        HeadlessRuntime {
            format: cli.format,
            approval_mode,
            manifest,
            known_api_keys,
            media_refs,
        },
    )
    .await
}

fn read_task_input(cli: &RunCli) -> anyhow::Result<String> {
    if cli.stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        if text.trim().is_empty() {
            anyhow::bail!("--stdin 没有收到非空 UTF-8 任务");
        }
        return Ok(text);
    }

    let prompt = cli
        .prompt
        .as_deref()
        .expect("clap requires --prompt or --stdin");
    if prompt.trim().is_empty() {
        anyhow::bail!("--prompt 必须提供非空任务文本");
    }
    Ok(prompt.to_owned())
}

struct HeadlessRuntime {
    format: OutputFormat,
    approval_mode: ApprovalMode,
    manifest: Option<ManifestApprovals>,
    known_api_keys: Vec<String>,
    media_refs: Vec<rivo_core::MediaRef>,
}

async fn execute_headless_turn(
    session: Arc<Session>,
    text: String,
    options: TurnOptions,
    runtime: HeadlessRuntime,
) -> anyhow::Result<i32> {
    let HeadlessRuntime {
        format,
        approval_mode,
        mut manifest,
        known_api_keys,
        media_refs,
    } = runtime;
    let (sender, mut events) = session.clone().channel();
    sender
        .send(Intent::SetTurnOptions {
            options: options.clone(),
        })
        .await
        .context("设置 Turn options 失败")?;
    sender
        .send(if media_refs.is_empty() {
            Intent::SubmitTask { text }
        } else {
            Intent::SubmitTaskWithMedia { text, media_refs }
        })
        .await
        .context("会话执行失败")?;
    let mut exit_code = 0;
    let mut approval_denied = false;
    let mut terminal_seen = false;
    let mut last_metadata = None;
    let mut tool_arguments = HashMap::<String, serde_json::Value>::new();
    while let Some(SessionEvent { metadata, event }) = events.next().await {
        last_metadata = Some(metadata.clone());
        if format == OutputFormat::Jsonl {
            emit_jsonl_event(&metadata, &event, &known_api_keys);
        }
        let terminal = matches!(
            &event,
            AgentEvent::FinalAnswer { .. } | AgentEvent::Stopped { .. }
        );
        if options.trace {
            if let AgentEvent::SubagentConfirmationRequired {
                agent_id,
                task_id,
                tool_name,
                ..
            } = &event
            {
                eprintln!(
                    "rivo: subagent confirmation requested agent={agent_id:?} task={task_id:?} tool={tool_name}"
                );
            }
        }
        match event {
            AgentEvent::ModelDelta { text } => {
                if format == OutputFormat::Text {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
            }
            AgentEvent::FinalAnswer { .. } if format == OutputFormat::Text => {
                if !approval_denied {
                    exit_code = 0;
                }
                println!()
            }
            AgentEvent::FinalAnswer { .. } => {
                if !approval_denied {
                    exit_code = 0;
                }
            }
            AgentEvent::MediaReady { result, .. } => {
                if format == OutputFormat::Text {
                    match &result.delivery {
                        rivo_core::MediaDelivery::LocalPath { path } => {
                            eprintln!(
                                "{}",
                                ui::text_with(
                                    "cli-run-media-ready-path",
                                    &format!("rivo: media ready: {path}"),
                                    &[("path", path.clone())],
                                )
                            );
                        }
                        rivo_core::MediaDelivery::SignedUrl { url, .. } => {
                            eprintln!(
                                "{}",
                                ui::text_with(
                                    "cli-run-media-ready-url",
                                    &format!("rivo: media ready: {url}"),
                                    &[("url", url.clone())],
                                )
                            );
                        }
                    }
                }
            }
            AgentEvent::MediaStarted { operation, .. } => {
                if options.trace {
                    eprintln!(
                        "{}",
                        ui::text_with(
                            "cli-run-media-started",
                            &format!("rivo: media started: {operation:?}"),
                            &[("operation", format!("{operation:?}"))],
                        )
                    );
                }
            }
            AgentEvent::MediaFailed { message, .. } => {
                eprintln!(
                    "{}",
                    ui::text_with(
                        "cli-run-media-failed",
                        &format!("rivo: media failed: {message}"),
                        &[("message", message.clone())],
                    )
                );
                exit_code = 1;
            }
            AgentEvent::Error { message } => {
                eprintln!("rivo: {message}");
                exit_code = 1;
            }
            AgentEvent::Failure { message, details } => {
                eprintln!(
                    "rivo: {} [{}:{}] {}",
                    message,
                    format!("{:?}", details.stage).to_lowercase(),
                    details.code,
                    details.cause
                );
                exit_code = 1;
            }
            AgentEvent::BudgetUpdated {
                used_tokens,
                context_limit_tokens,
                steps,
                max_steps,
            } => {
                if options.trace {
                    eprintln!(
                        "rivo: budget tokens={used_tokens}/{context_limit_tokens} steps={steps}/{max_steps}"
                    );
                }
            }
            AgentEvent::ContextCompressed {
                omitted_start,
                omitted_end,
            } => {
                if options.trace {
                    eprintln!("rivo: context compressed omitted={omitted_start}..{omitted_end}");
                }
            }
            AgentEvent::SubagentQueued {
                agent_id, task_id, ..
            } => {
                if options.trace {
                    eprintln!("rivo: subagent queued agent={agent_id:?} task={task_id:?}");
                }
            }
            AgentEvent::SubagentStarted { agent_id, task_id } => {
                if options.trace {
                    eprintln!("rivo: subagent started agent={agent_id:?} task={task_id:?}");
                }
            }
            AgentEvent::SubagentCompleted { result } => {
                if options.trace {
                    eprintln!(
                        "rivo: subagent completed agent={:?} task={:?} state={:?} steps={}",
                        result.agent_id, result.task_id, result.state, result.usage.steps
                    );
                }
            }
            AgentEvent::ToolRequested {
                call_id, arguments, ..
            } => {
                tool_arguments.insert(call_id, arguments);
            }
            AgentEvent::ConfirmationRequired {
                request_id,
                call_id,
                tool_name,
                ..
            }
            | AgentEvent::SubagentConfirmationRequired {
                request_id,
                call_id,
                tool_name,
                ..
            } => {
                let arguments = tool_arguments.get(&call_id).cloned().unwrap_or_default();
                let allowed = match approval_mode {
                    ApprovalMode::Prompt
                        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() =>
                    {
                        prompt_for_confirmation(&tool_name)?
                    }
                    ApprovalMode::Prompt => {
                        eprintln!(
                            "{}",
                            ui::text(
                                "cli-run-approval-unavailable",
                                "rivo: approval prompt is unavailable without a TTY; request denied",
                            )
                        );
                        false
                    }
                    ApprovalMode::Deny => false,
                    ApprovalMode::Manifest => manifest.as_mut().is_some_and(|manifest| {
                        manifest.allows(&tool_name, &arguments, ApprovalScope::Once)
                    }),
                };
                if !allowed {
                    eprintln!(
                        "{}",
                        ui::text_with(
                            "cli-run-approval-denied",
                            &format!(
                                "rivo: approval denied for {tool_name}; use TTY prompt or an exact approval manifest rule"
                            ),
                            &[("tool", tool_name.clone())],
                        )
                    );
                    exit_code = 3;
                    approval_denied = true;
                }
                sender
                    .send(Intent::Confirm {
                        request_id,
                        allow: allowed,
                        reason: (!allowed)
                            .then_some("CLI approval policy denied this request".into()),
                        scope: ApprovalScope::Once,
                    })
                    .await?;
            }
            AgentEvent::InteractionRequired { request } => {
                eprintln!(
                    "{}",
                    ui::text_with(
                        "cli-run-interaction-noninteractive",
                        &format!(
                            "rivo: non-interactive mode cannot answer interaction {}",
                            request.request_id
                        ),
                        &[("id", request.request_id.clone())],
                    )
                );
                sender
                    .send(Intent::RespondInteraction {
                        request_id: request.request_id,
                        value: "cancel".into(),
                    })
                    .await?;
                exit_code = 1;
            }
            AgentEvent::ModelSelected { selection, route } => {
                if options.trace {
                    eprintln!(
                        "rivo: vendor={} model={} provider={}",
                        selection.vendor, selection.model, route.provider_id
                    );
                }
            }
            AgentEvent::SessionSettingsApplied { .. } | AgentEvent::SessionLocaleApplied { .. } => {
            }
            AgentEvent::PromptPrepared {
                revision,
                instruction_hashes,
                instructions_truncated,
            } => {
                if options.trace {
                    eprintln!(
                        "rivo: prompt revision={revision} instructions={} truncated={instructions_truncated}",
                        instruction_hashes.len()
                    );
                }
            }
            AgentEvent::Started { .. }
            | AgentEvent::ToolObservation { .. }
            | AgentEvent::ConfirmationResolved { .. }
            | AgentEvent::SubagentActivity { .. }
            | AgentEvent::MemoryCandidate { .. }
            | AgentEvent::MemoryRecalled { .. }
            | AgentEvent::MemoryInjected { .. }
            | AgentEvent::MemoryConfirmed { .. }
            | AgentEvent::MemoryRejected { .. }
            | AgentEvent::MemoryDeleted { .. }
            | AgentEvent::RetryAvailable { .. }
            | AgentEvent::GoalUpdated { .. }
            | AgentEvent::GoalContinuationScheduled { .. }
            | AgentEvent::GoalAudit { .. }
            | AgentEvent::GoalCompleted { .. }
            | AgentEvent::GoalBlocked { .. }
            | AgentEvent::PlanCreated { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::PlanApprovalRequired { .. }
            | AgentEvent::PlanApproved { .. }
            | AgentEvent::PlanRejected { .. }
            | AgentEvent::PlanExecutionStarted { .. }
            | AgentEvent::PlanStepStarted { .. }
            | AgentEvent::PlanStepCompleted { .. }
            | AgentEvent::PlanStepBlocked { .. }
            | AgentEvent::PlanCompleted { .. } => {}
            AgentEvent::Stopped { reason } => {
                if !approval_denied {
                    exit_code = exit_code.max(1);
                }
                if format == OutputFormat::Text {
                    eprintln!(
                        "{}",
                        ui::text_with(
                            "cli-run-stopped",
                            &format!("rivo: run stopped: {reason}"),
                            &[("reason", reason.clone())],
                        )
                    );
                }
            }
        }
        if terminal {
            terminal_seen = true;
            break;
        }
    }
    if !terminal_seen {
        eprintln!(
            "{}",
            ui::text(
                "cli-run-stream-ended",
                "rivo: session event stream ended without a terminal event",
            )
        );
        exit_code = exit_code.max(1);
        // A correctly functioning Session always emits its own terminal event.
        // Preserve the JSONL consumer contract if a driver/channel failure
        // nevertheless closes the stream after a run has identified itself.
        if format == OutputFormat::Jsonl {
            if let Some(metadata) = last_metadata.map(stream_end_terminal_metadata) {
                emit_jsonl_event(
                    &metadata,
                    &AgentEvent::Stopped {
                        reason: "session event stream ended unexpectedly".into(),
                    },
                    &known_api_keys,
                );
            }
        }
    }
    drop(events);
    drop(sender);
    if let Err(error) = session.shutdown_async().await {
        eprintln!(
            "{}",
            ui::text_with(
                "cli-run-shutdown-failed",
                &format!("rivo: session shutdown failed: {error}"),
                &[("error", error.to_string())],
            )
        );
    }
    Ok(exit_code)
}

fn stream_end_terminal_metadata(
    mut metadata: rivo_session::EventMetadata,
) -> rivo_session::EventMetadata {
    metadata.sequence = metadata.sequence.saturating_add(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    metadata.timestamp_utc_ms = metadata.timestamp_utc_ms.max(now);
    metadata
}

fn prompt_for_confirmation(tool_name: &str) -> anyhow::Result<bool> {
    eprint!("rivo: allow {tool_name} once? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn emit_jsonl_event(
    metadata: &rivo_session::EventMetadata,
    event: &AgentEvent,
    known_api_keys: &[String],
) {
    let event = public_jsonl_event(event);
    let value = serde_json::json!({
        "schema_version": 1,
        "session_id": metadata.session_id,
        "turn_id": metadata.turn_id,
        "sequence": metadata.sequence,
        "timestamp_utc_ms": metadata.timestamp_utc_ms,
        "event": event,
    });
    let sanitized = rivo_telemetry::redact_api_keys(&value, known_api_keys);
    println!("{}", sanitized);
}

/// JSONL intentionally projects tool events rather than serializing tool arguments or bodies.
/// Those values can contain user data and are available locally through the bounded session UI.
fn public_jsonl_event(event: &AgentEvent) -> serde_json::Value {
    match event {
        AgentEvent::ToolRequested {
            call_id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "tool_requested",
            "call_id": call_id,
            "name": name,
            "arguments_sha256": canonical_json_sha256(arguments),
        }),
        AgentEvent::ToolObservation {
            call_id,
            name,
            result,
        } => serde_json::json!({
            "type": "tool_observation",
            "call_id": call_id,
            "name": name,
            "truncated": result.get("truncated").and_then(serde_json::Value::as_bool).unwrap_or(false),
            "has_evidence": result.get("evidence").is_some(),
        }),
        AgentEvent::MediaReady { call_id, result } => serde_json::json!({
            "type": "media_ready",
            "call_id": call_id,
            "asset": result.asset,
            "delivery": result.delivery,
        }),
        AgentEvent::MediaStarted { call_id, operation } => serde_json::json!({
            "type": "media_started",
            "call_id": call_id,
            "operation": operation,
        }),
        AgentEvent::MediaFailed {
            call_id,
            operation,
            message,
        } => serde_json::json!({
            "type": "media_failed",
            "call_id": call_id,
            "operation": operation,
            "message": message,
        }),
        AgentEvent::SubagentQueued {
            agent_id,
            task_id,
            call_id,
            label,
        } => serde_json::json!({
            "type": "subagent_queued",
            "agent_id": agent_id,
            "task_id": task_id,
            "call_id": call_id,
            "label": label,
        }),
        AgentEvent::SubagentActivity {
            agent_id,
            task_id,
            activity,
        } => {
            // 与工具事件一致，JSONL 不输出子代理工具的错误正文。
            let mut value = serde_json::json!({
                "type": "subagent_activity",
                "agent_id": agent_id,
                "task_id": task_id,
            });
            match activity {
                rivo_session::SubagentToolActivity::Requested { call_id, name } => {
                    value["kind"] = "requested".into();
                    value["call_id"] = call_id.as_str().into();
                    value["name"] = name.as_str().into();
                }
                rivo_session::SubagentToolActivity::Observed {
                    call_id,
                    name,
                    failed,
                    ..
                } => {
                    value["kind"] = "observed".into();
                    value["call_id"] = call_id.as_str().into();
                    value["name"] = name.as_str().into();
                    value["failed"] = (*failed).into();
                }
                rivo_session::SubagentToolActivity::ConfirmationResolved {
                    call_id,
                    tool_name,
                    allowed,
                    ..
                } => {
                    value["kind"] = "confirmation_resolved".into();
                    value["call_id"] = call_id.as_str().into();
                    value["name"] = tool_name.as_str().into();
                    value["allowed"] = (*allowed).into();
                }
            }
            value
        }
        AgentEvent::SubagentStarted { agent_id, task_id } => serde_json::json!({
            "type": "subagent_started",
            "agent_id": agent_id,
            "task_id": task_id,
        }),
        AgentEvent::SubagentCompleted { result } => serde_json::json!({
            "type": "subagent_completed",
            "agent_id": result.agent_id,
            "task_id": result.task_id,
            "state": result.state,
            "truncated": result.truncated,
            "usage": result.usage,
            "evidence_count": result.evidence.len(),
        }),
        AgentEvent::SubagentConfirmationRequired {
            agent_id,
            task_id,
            request_id,
            call_id,
            tool_name,
            session_scope_available,
            ..
        } => serde_json::json!({
            "type": "subagent_confirmation_required",
            "agent_id": agent_id,
            "task_id": task_id,
            "request_id": request_id,
            "call_id": call_id,
            "tool_name": tool_name,
            "session_scope_available": session_scope_available,
        }),
        _ => serde_json::to_value(event).unwrap_or_else(
            |error| serde_json::json!({"type":"serialization_error","message":error.to_string()}),
        ),
    }
}

async fn run_mcp_command(args: Vec<String>) -> anyhow::Result<()> {
    if print_mcp_help(&args) {
        return Ok(());
    }
    let mut args = args;
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let format = if matches!(operation, "list" | "show" | "inspect") {
        parse_query_format(&mut args)?
    } else {
        None
    };
    match args.as_slice() {
        [command] if command == "list" => {
            let config = ResolvedConfig::load(ConfigOverrides::default())?;
            let format = resolve_query_format(format, config.query_format);
            let mut text = String::new();
            let mut table = Table::new(["Name", "Enabled", "Capabilities", "Toolsets"]);
            let mut json = Vec::new();
            for server in &config.mcp_servers {
                let toolsets = if server.toolsets.is_empty() {
                    "legacy-all-tools".to_owned()
                } else {
                    server
                        .toolsets
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let capabilities = server
                    .allowed_capabilities
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",");
                text.push_str(&format!(
                    "{}\tenabled={}\tcapabilities={}\ttoolsets={}\n",
                    server.name, server.enabled, capabilities, toolsets
                ));
                table.push_row(vec![
                    server.name.clone(),
                    server.enabled.to_string(),
                    capabilities.clone(),
                    toolsets.clone(),
                ]);
                json.push(serde_json::json!({"name": server.name, "enabled": server.enabled, "capabilities": capabilities, "toolsets": toolsets}));
            }
            print_query(
                format,
                QueryOutput::new(text, table, serde_json::Value::Array(json)),
            )?;
            Ok(())
        }
        [command, name] if command == "show" => {
            let config = ResolvedConfig::load(ConfigOverrides::default())?;
            let format = resolve_query_format(format, config.query_format);
            let server = config
                .mcp_servers
                .iter()
                .find(|server| server.name == *name)
                .context(ui::explicit_text("cli-mcp-not-found", "MCP server 不存在"))?;
            let lines = mcp_server_details(server)?;
            if format == QueryOutputFormat::Text {
                for line in lines {
                    println!("{line}");
                }
            } else {
                let mut table = Table::new([
                    ui::text("cli-output-property", "Property"),
                    ui::text("cli-output-value", "Value"),
                ]);
                let mut json = serde_json::Map::new();
                for line in lines {
                    let (key, value) = line.split_once('=').context(ui::explicit_text(
                        "cli-mcp-config-invalid",
                        "MCP 配置输出无效",
                    ))?;
                    table.push_row(vec![key.to_owned(), value.to_owned()]);
                    json.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
                }
                print_query(
                    format,
                    QueryOutput::new(String::new(), table, serde_json::Value::Object(json)),
                )?;
            }
            Ok(())
        }
        [command, name] if command == "login" => {
            let config = ResolvedConfig::load(ConfigOverrides::default())?;
            let server = config
                .mcp_servers
                .iter()
                .find(|server| server.name == *name)
                .context(ui::explicit_text("cli-mcp-not-found", "MCP server 不存在"))?;
            let (url, oauth_config) = match &server.transport {
                McpTransport::StreamableHttp {
                    url,
                    auth: McpHttpAuth::OAuth(oauth_config),
                    ..
                } => (url, oauth_config),
                _ => anyhow::bail!(ui::explicit_text(
                    "cli-mcp-login-oauth-only",
                    "rivo mcp login 仅适用于配置了 --oauth 的 http server",
                )),
            };
            let store = rivo_tools::oauth::store::KeyringStore::new();
            rivo_tools::oauth::interactive_login(url, oauth_config, &store, &server.name).await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-mcp-login-success",
                    &format!(
                        "MCP server {name} login succeeded; credentials stored in the system keychain"
                    ),
                    &[("name", name.clone())],
                )
            );
            Ok(())
        }
        [command, name] if matches!(command.as_str(), "test" | "inspect") => {
            let config = ResolvedConfig::load(ConfigOverrides::default())?;
            let format = if command == "inspect" {
                resolve_query_format(format, config.query_format)
            } else {
                QueryOutputFormat::Text
            };
            let server = config
                .mcp_servers
                .iter()
                .find(|server| server.name == *name)
                .context(ui::explicit_text("cli-mcp-not-found", "MCP server 不存在"))?;
            if !server.enabled {
                anyhow::bail!(ui::text_with(
                    "cli-mcp-disabled",
                    &format!("MCP server 未启用: {name}"),
                    &[("name", name.clone())],
                ));
            }
            // 交互式命令：提供 keychain 存储与浏览器登录（仅当 server 配置 auto_login 时才会拉起）。
            let store = rivo_tools::oauth::store::KeyringStore::new();
            let login = rivo_tools::oauth::BrowserLogin;
            let mut client = McpClient::connect_with_options(
                server,
                McpConnectOptions {
                    oauth_store: Some(&store),
                    oauth_login: Some(&login),
                    ..McpConnectOptions::default()
                },
            )
            .await?;
            client.initialize().await?;
            let tools = if server.allowed_capabilities.contains("tools") {
                client.list_tools().await?
            } else {
                Vec::new()
            };
            let resources = if server.allowed_capabilities.contains("resources") {
                client.list_resources().await?
            } else {
                Vec::new()
            };
            let prompts = if server.allowed_capabilities.contains("prompts") {
                client.list_prompts().await?
            } else {
                Vec::new()
            };
            if command == "test" || format == QueryOutputFormat::Text {
                println!(
                    "server={name} status=ready tools={} resources={} prompts={}",
                    tools.len(),
                    resources.len(),
                    prompts.len()
                );
                for tool in &tools {
                    println!(
                        "tool={}\t{}",
                        tool.name,
                        tool.description.as_deref().unwrap_or_default()
                    );
                }
                for resource in &resources {
                    println!(
                        "resource={}\t{}",
                        resource.uri,
                        resource.name.as_deref().unwrap_or_default()
                    );
                }
                for prompt in &prompts {
                    println!(
                        "prompt={}\t{}",
                        prompt.name,
                        prompt.description.as_deref().unwrap_or_default()
                    );
                }
            } else {
                let mut table = Table::new([
                    ui::text("cli-output-type", "Type"),
                    ui::text("cli-output-name", "Name"),
                    ui::text("cli-output-description", "Description"),
                ]);
                let mut tool_rows = Vec::new();
                let mut resource_rows = Vec::new();
                let mut prompt_rows = Vec::new();
                for tool in &tools {
                    let description =
                        query_output_summary(tool.description.as_deref().unwrap_or_default());
                    table.push_row(vec!["tool".into(), tool.name.clone(), description.clone()]);
                    tool_rows
                        .push(serde_json::json!({"name": tool.name, "description": description}));
                }
                for resource in &resources {
                    let description =
                        query_output_summary(resource.name.as_deref().unwrap_or_default());
                    table.push_row(vec![
                        "resource".into(),
                        resource.uri.clone(),
                        description.clone(),
                    ]);
                    resource_rows
                        .push(serde_json::json!({"uri": resource.uri, "name": description}));
                }
                for prompt in &prompts {
                    let description =
                        query_output_summary(prompt.description.as_deref().unwrap_or_default());
                    table.push_row(vec![
                        "prompt".into(),
                        prompt.name.clone(),
                        description.clone(),
                    ]);
                    prompt_rows
                        .push(serde_json::json!({"name": prompt.name, "description": description}));
                }
                print_query(
                    format,
                    QueryOutput::new(
                        String::new(),
                        table,
                        serde_json::json!({"server": name, "status": "ready", "tools": tool_rows, "resources": resource_rows, "prompts": prompt_rows}),
                    ),
                )?;
            }
            client.shutdown().await;
            Ok(())
        }
        [action, name] if matches!(action.as_str(), "enable" | "disable" | "remove") => {
            update_mcp_config(|file| {
                let index = file
                    .mcp
                    .servers
                    .iter()
                    .position(|server| server.name == *name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(ui::text_with(
                            "cli-mcp-not-found",
                            &format!("MCP server 不存在: {name}"),
                            &[("name", name.clone())],
                        ))
                    })?;
                if action == "remove" {
                    file.mcp.servers.remove(index);
                } else {
                    file.mcp.servers[index].enabled = action == "enable";
                }
                Ok(())
            })?;
            println!(
                "{}",
                ui::text_with(
                    "cli-mcp-state-changed",
                    &format!("MCP server {name} {action}d"),
                    &[("name", name.clone()), ("action", action.clone())],
                )
            );
            Ok(())
        }
        [command, kind, name, target, rest @ ..] if command == "install" => {
            let server = parse_mcp_install(kind, name, target, rest)?;
            let server_name = server.name.clone();
            let needs_login = matches!(
                &server.transport,
                McpTransport::StreamableHttp {
                    auth: McpHttpAuth::OAuth(_),
                    ..
                }
            );
            // OAuth server 首次安装通常尚未登录：实时发现失败时先落配置，引导用户登录。
            let discovery = discover_mcp_server(&server).await;
            match discovery {
                Ok((tools, resources, prompts)) => {
                    persist_mcp_server(server)?;
                    println!(
                        "{}",
                        ui::text_with(
                            "cli-mcp-install-ready",
                            &format!("installed MCP server {server_name}; status=ready tools={} resources={} prompts={}", tools.len(), resources, prompts),
                            &[("name", server_name.clone()), ("tools", tools.len().to_string()), ("resources", resources.to_string()), ("prompts", prompts.to_string())],
                        )
                    );
                }
                Err(_) if needs_login => {
                    persist_mcp_server(server)?;
                    println!(
                        "{}",
                        ui::text_with(
                            "cli-mcp-install-oauth-pending",
                            &format!("installed MCP server {server_name}（OAuth 未登录，已跳过实时发现）；请运行 `rivo mcp login {server_name}`，随后 `rivo mcp inspect {server_name}` 验证"),
                            &[("name", server_name.clone())],
                        )
                    );
                }
                Err(error) => return Err(error),
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-usage",
                "用法: rivo mcp list|show|inspect <server>|install|enable|disable|remove",
            )
        ),
    }
}

fn print_mcp_help(args: &[String]) -> bool {
    let help = match args {
        [help] if matches!(help.as_str(), "--help" | "-h") => "管理已配置的 MCP server。\n\nUsage: rivo mcp <COMMAND>\n\nCommands:\n  list      列出 MCP server\n  show      显示脱敏配置\n  inspect   连接并发现远程能力\n  login     执行 OAuth 交互式登录\n  install   安装并验证 MCP server\n  enable    启用 MCP server\n  disable   禁用 MCP server\n  remove    删除 MCP server\n\nOptions:\n  -h, --help  显示帮助",
        [command, help] if matches!(help.as_str(), "--help" | "-h") => match command.as_str() {
            "list" => "列出已配置的 MCP server。\n\nUsage:\n  rivo mcp list",
            "show" => "显示 MCP server 的脱敏配置。\n\nUsage:\n  rivo mcp show <NAME>",
            "inspect" => "连接 MCP server 并发现 tools、resources 和 prompts。\n\nUsage:\n  rivo mcp inspect <NAME>",
            "login" => "为 HTTP MCP server 执行 OAuth 登录。\n\nUsage:\n  rivo mcp login <NAME>",
            "enable" => "启用 MCP server。\n\nUsage:\n  rivo mcp enable <NAME>",
            "disable" => "禁用 MCP server。\n\nUsage:\n  rivo mcp disable <NAME>",
            "remove" => "删除 MCP server。\n\nUsage:\n  rivo mcp remove <NAME>",
            "install" => "安装并验证 MCP server。\n\nUsage:\n  rivo mcp install <http|stdio> <NAME> <TARGET> [OPTIONS]\n\nCommands:\n  http   安装 Streamable HTTP server\n  stdio  安装本机 stdio server",
            _ => return false,
        },
        [install, kind, help]
            if install == "install" && matches!(help.as_str(), "--help" | "-h") =>
        {
            match kind.as_str() {
                "http" => "安装 Streamable HTTP MCP server。\n\nUsage:\n  rivo mcp install http <NAME> <HTTPS_URL> [OPTIONS]\n\nOptions:\n  --capability <LIST>     允许的能力\n  --toolset <SPEC>        命名 toolset；可重复\n  --header-env <H>=<ENV>  从环境变量读取 HTTP header\n  --bearer-env <ENV>      从环境变量读取 Bearer token\n  --oauth                 启用 OAuth\n  --allow-loopback        允许 loopback URL\n  --allow-sampling        允许受控 sampling",
                "stdio" => "安装本机 stdio MCP server。\n\nUsage:\n  rivo mcp install stdio <NAME> <PROGRAM> [OPTIONS]\n\nOptions:\n  --arg <ARG>           进程参数；可重复\n  --cwd <PATH>          进程工作目录\n  --env <KEY>=<ENV>     从环境变量传入进程环境\n  --capability <LIST>   允许的能力\n  --toolset <SPEC>      命名 toolset；可重复\n  --allow-sampling      允许受控 sampling",
                _ => return false,
            }
        }
        _ => return false,
    };
    let id = match args {
        [help] if matches!(help.as_str(), "--help" | "-h") => "cli-mcp-root-help",
        [command, help] if matches!(help.as_str(), "--help" | "-h") => match &command[..] {
            "list" => "cli-mcp-list-help",
            "show" => "cli-mcp-show-help",
            "inspect" => "cli-mcp-inspect-help",
            "login" => "cli-mcp-login-help",
            "enable" => "cli-mcp-enable-help",
            "disable" => "cli-mcp-disable-help",
            "remove" => "cli-mcp-remove-help",
            "install" => "cli-mcp-install-help",
            _ => return false,
        },
        [install, kind, help]
            if install == "install" && matches!(help.as_str(), "--help" | "-h") =>
        {
            match &kind[..] {
                "http" => "cli-mcp-install-http-help",
                "stdio" => "cli-mcp-install-stdio-help",
                _ => return false,
            }
        }
        _ => return false,
    };
    println!("{}", ui::text(id, help));
    true
}

/// Produces the safe, credential-reference-only representation used by
/// `rivo mcp show`; keeping it pure makes every transport/auth combination
/// testable without connecting to an MCP server or OS keychain.
fn mcp_server_details(server: &McpServerConfig) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![
        format!("name={}", server.name),
        format!("enabled={}", server.enabled),
        format!("allow_sampling={}", server.allow_sampling),
        format!(
            "capabilities={}",
            server
                .allowed_capabilities
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ),
    ];
    for (name, toolset) in &server.toolsets {
        lines.push(format!(
            "toolset.{name}=risk:{},tools:{}",
            serde_json::to_value(toolset.risk)?
                .as_str()
                .unwrap_or("unknown"),
            toolset.tools.iter().cloned().collect::<Vec<_>>().join(",")
        ));
    }
    match &server.transport {
        McpTransport::Stdio { program, env, .. } => {
            lines.push(format!("transport=stdio program={program}"));
            if !env.is_empty() {
                lines.push(format!(
                    "env={}",
                    env.iter()
                        .map(|(key, source)| format!("{key}<-{source}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
        }
        McpTransport::StreamableHttp {
            url,
            auth,
            allow_loopback,
            ..
        } => {
            lines.push(format!(
                "transport=http url={url} allow_loopback={allow_loopback}"
            ));
            lines.push(match auth {
                McpHttpAuth::None => "auth=none".into(),
                McpHttpAuth::Bearer { token_env } => format!("auth=bearer token_env={token_env}"),
                McpHttpAuth::OAuth(config) => format!(
                    "auth=oauth client_id={} token_env={} scopes={} auto_login={}",
                    config.client_id.as_deref().unwrap_or("<dynamic>"),
                    config.token_env.as_deref().unwrap_or("<unset>"),
                    config.scopes.join(","),
                    config.auto_login
                ),
            });
        }
    }
    Ok(lines)
}

fn query_output_summary(value: &str) -> String {
    const LIMIT: usize = 160;
    let normalized = value.replace(['\n', '\r', '\t'], " ");
    if normalized.chars().count() <= LIMIT {
        normalized
    } else {
        format!("{}...", normalized.chars().take(LIMIT).collect::<String>())
    }
}

fn persist_mcp_server(server: McpServerConfig) -> anyhow::Result<()> {
    update_mcp_config(|file| {
        if let Some(existing) = file
            .mcp
            .servers
            .iter_mut()
            .find(|existing| existing.name == server.name)
        {
            *existing = server;
        } else {
            file.mcp.servers.push(server);
        }
        Ok(())
    })
}

async fn discover_mcp_server(
    server: &McpServerConfig,
) -> anyhow::Result<(Vec<String>, usize, usize)> {
    // 复用 keychain 中已有的 OAuth 会话（若有）；不在发现阶段强制拉起浏览器登录。
    let store = rivo_tools::oauth::store::KeyringStore::new();
    let mut client = McpClient::connect_with_options(
        server,
        McpConnectOptions {
            oauth_store: Some(&store),
            ..McpConnectOptions::default()
        },
    )
    .await?;
    client.initialize().await?;
    let tools = if server.allowed_capabilities.contains("tools") {
        client.list_tools().await?
    } else {
        Vec::new()
    };
    let resources = if server.allowed_capabilities.contains("resources") {
        client.list_resources().await?.len()
    } else {
        0
    };
    let prompts = if server.allowed_capabilities.contains("prompts") {
        client.list_prompts().await?.len()
    } else {
        0
    };
    let names = tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    for (toolset_name, toolset) in &server.toolsets {
        if let Some(missing) = toolset.tools.iter().find(|name| !names.contains(*name)) {
            client.shutdown().await;
            anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-mcp-toolset-missing-tool",
                    &format!(
                        "MCP server {} 的 toolset {} 引用了未发现的工具 {}",
                        server.name, toolset_name, missing
                    ),
                    &[
                        ("server", server.name.clone()),
                        ("toolset", toolset_name.clone()),
                        ("tool", missing.clone())
                    ]
                )
            );
        }
    }
    client.shutdown().await;
    Ok((
        tools.into_iter().map(|tool| tool.name).collect(),
        resources,
        prompts,
    ))
}

fn update_mcp_config(
    update: impl FnOnce(&mut FileConfig) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let path = std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(config_file_path)
        .ok_or_else(|| {
            anyhow::anyhow!(ui::explicit_text(
                "cli-config-path-unresolved",
                "无法确定配置文件路径"
            ))
        })?;
    let draft = read_model_routing_draft(&path)?;
    let mut file = draft.config;
    update(&mut file)?;
    commit_model_routing_draft(&path, &draft.revision, &file)?;
    Ok(())
}

fn parse_mcp_install(
    kind: &str,
    name: &str,
    target: &str,
    values: &[String],
) -> anyhow::Result<McpServerConfig> {
    if name.trim().is_empty() || name.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-name-invalid",
                "MCP server 名称不能为空且不能包含空白字符"
            )
        );
    }
    let mut capabilities = BTreeSet::new();
    let mut toolsets = BTreeMap::new();
    let mut header_env = BTreeMap::new();
    let mut stdio_args = Vec::new();
    let mut stdio_env = BTreeMap::new();
    let mut cwd = None;
    let mut bearer_env: Option<String> = None;
    let mut oauth_enabled = false;
    let mut oauth_client_id: Option<String> = None;
    let mut oauth_client_name: Option<String> = None;
    let mut oauth_token_env: Option<String> = None;
    let mut oauth_scopes: Vec<String> = Vec::new();
    let mut oauth_redirect_port: u16 = 0;
    let mut oauth_auto_login = false;
    let mut allow_loopback = false;
    let mut allow_sampling = false;
    let mut index = 0;
    while index < values.len() {
        let flag = values[index].as_str();
        // 无值布尔 flag 先行处理。
        if flag == "--oauth" {
            if kind != "http" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-mcp-oauth-http-only", "--oauth 仅适用于 http MCP")
                );
            }
            oauth_enabled = true;
            index += 1;
            continue;
        }
        if flag == "--allow-loopback" {
            if kind != "http" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-mcp-loopback-http-only",
                        "--allow-loopback 仅适用于 http MCP"
                    )
                );
            }
            allow_loopback = true;
            index += 1;
            continue;
        }
        if flag == "--auto-login" {
            if kind != "http" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-mcp-auto-login-http-only",
                        "--auto-login 仅适用于 http MCP"
                    )
                );
            }
            oauth_enabled = true;
            oauth_auto_login = true;
            index += 1;
            continue;
        }
        if flag == "--allow-sampling" {
            allow_sampling = true;
            index += 1;
            continue;
        }
        let value = values.get(index + 1).ok_or_else(|| {
            anyhow::anyhow!(ui::text_with(
                "cli-mcp-flag-value-missing",
                &format!("{flag} 缺少值"),
                &[("flag", flag.to_owned())]
            ))
        })?;
        match flag {
            "--capability" => {
                for capability in value.split(',').filter(|value| !value.is_empty()) {
                    if !matches!(capability, "tools" | "resources" | "prompts") {
                        anyhow::bail!(
                            "{}",
                            ui::text_with(
                                "cli-mcp-capability-unknown",
                                &format!("未知 MCP capability: {capability}"),
                                &[("capability", capability.to_owned())]
                            )
                        );
                    }
                    capabilities.insert(capability.to_owned());
                }
            }
            "--toolset" => {
                let mut fields = value.splitn(3, ':');
                let toolset_name = fields.next().unwrap_or_default();
                let risk = match fields.next().unwrap_or_default() {
                    "read_only" => McpToolRisk::ReadOnly,
                    "action" => McpToolRisk::Action,
                    "unknown" => McpToolRisk::Unknown,
                    _ => anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-mcp-toolset-risk-invalid",
                            "toolset 风险必须是 read_only、action 或 unknown"
                        )
                    ),
                };
                let tools = fields
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter(|tool| !tool.trim().is_empty())
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>();
                if toolset_name.trim().is_empty() || tools.is_empty() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-mcp-toolset-format-invalid",
                            "toolset 格式为 <name>:<risk>:<tool,tool>"
                        )
                    );
                }
                toolsets.insert(toolset_name.to_owned(), McpToolsetConfig { tools, risk });
            }
            "--header-env" => {
                let (header, env) = value.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!(ui::explicit_text(
                        "cli-mcp-header-env-format-invalid",
                        "--header-env 格式为 <Header>=<ENV>"
                    ))
                })?;
                if header.trim().is_empty() || env.trim().is_empty() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-mcp-header-env-format-invalid",
                            "--header-env 格式为 <Header>=<ENV>"
                        )
                    );
                }
                header_env.insert(header.to_owned(), env.to_owned());
            }
            "--arg" if kind == "stdio" => stdio_args.push(value.clone()),
            "--cwd" if kind == "stdio" => cwd = Some(PathBuf::from(value)),
            "--env" if kind == "stdio" => {
                let (key, source) = value.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!(ui::explicit_text(
                        "cli-mcp-env-format-invalid",
                        "--env 格式为 <KEY>=<ENV>"
                    ))
                })?;
                if key.trim().is_empty() || source.trim().is_empty() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text("cli-mcp-env-format-invalid", "--env 格式为 <KEY>=<ENV>")
                    );
                }
                stdio_env.insert(key.to_owned(), source.to_owned());
            }
            "--bearer-env" if kind == "http" => {
                if value.trim().is_empty() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-mcp-bearer-env-missing",
                            "--bearer-env 需要环境变量名"
                        )
                    );
                }
                bearer_env = Some(value.clone());
            }
            "--oauth-client-id" if kind == "http" => {
                oauth_enabled = true;
                oauth_client_id = Some(value.clone());
            }
            "--oauth-client-name" if kind == "http" => {
                oauth_enabled = true;
                oauth_client_name = Some(value.clone());
            }
            "--oauth-token-env" if kind == "http" => {
                oauth_enabled = true;
                if value.trim().is_empty() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-mcp-oauth-token-env-missing",
                            "--oauth-token-env 需要环境变量名"
                        )
                    );
                }
                oauth_token_env = Some(value.clone());
            }
            "--oauth-scope" if kind == "http" => {
                oauth_enabled = true;
                for scope in value.split(',').filter(|scope| !scope.trim().is_empty()) {
                    oauth_scopes.push(scope.to_owned());
                }
            }
            "--oauth-redirect-port" if kind == "http" => {
                oauth_enabled = true;
                oauth_redirect_port = value.parse().map_err(|_| {
                    anyhow::anyhow!(ui::explicit_text(
                        "cli-mcp-oauth-port-invalid",
                        "--oauth-redirect-port 必须是端口号"
                    ))
                })?;
            }
            _ => anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-mcp-install-flag-unsupported",
                    &format!("不支持的 MCP install 参数: {flag}"),
                    &[("flag", flag.to_owned())]
                )
            ),
        }
        index += 2;
    }
    if capabilities.is_empty() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-capability-required",
                "必须至少声明一个 --capability"
            )
        );
    }
    if capabilities.contains("tools") && toolsets.is_empty() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-toolset-required",
                "声明 tools capability 时必须配置至少一个精确 --toolset"
            )
        );
    }
    let http_auth = if bearer_env.is_some() && oauth_enabled {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-auth-conflict",
                "--bearer-env 与 --oauth 不能同时使用"
            )
        );
    } else if let Some(token_env) = bearer_env {
        McpHttpAuth::Bearer { token_env }
    } else if oauth_enabled {
        McpHttpAuth::OAuth(OAuthConfig {
            client_id: oauth_client_id,
            scopes: oauth_scopes,
            client_name: oauth_client_name,
            redirect_port: oauth_redirect_port,
            token_env: oauth_token_env,
            auto_login: oauth_auto_login,
        })
    } else {
        McpHttpAuth::None
    };
    let transport = match kind {
        "http" => McpTransport::StreamableHttp {
            url: target.to_owned(),
            header_env,
            auth: http_auth,
            allow_loopback,
        },
        "stdio" => {
            if !header_env.is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-mcp-stdio-header-env-unsupported",
                        "stdio MCP 不支持 --header-env；请由进程环境提供凭据"
                    )
                );
            }
            McpTransport::Stdio {
                program: target.to_owned(),
                args: stdio_args,
                cwd,
                env: stdio_env,
            }
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-mcp-transport-invalid",
                "MCP install transport 必须是 http 或 stdio"
            )
        ),
    };
    Ok(McpServerConfig {
        name: name.to_owned(),
        enabled: true,
        transport,
        allowed_capabilities: capabilities,
        toolsets,
        allow_sampling,
    })
}

async fn run_config_command(args: Vec<String>) -> anyhow::Result<i32> {
    if print_config_help(&args) {
        return Ok(0);
    }
    let command =
        match ConfigCli::try_parse_from(std::iter::once("rivo config".to_owned()).chain(args)) {
            Ok(cli) => cli.command.unwrap_or(ConfigCommand::Show),
            Err(error) => {
                let exit_code = error.exit_code();
                error.print().context("无法输出 config 命令帮助")?;
                return Ok(exit_code);
            }
        };
    let path = std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(config_file_path)
        .ok_or_else(|| {
            anyhow::anyhow!(ui::explicit_text(
                "cli-config-path-unresolved",
                "无法确定配置文件路径",
            ))
        })?;
    run_config_command_at(&path, command.into_args()).await?;
    Ok(0)
}

fn print_config_help(args: &[String]) -> bool {
    let (id, fallback) = match args {
        [help] if matches!(help.as_str(), "--help" | "-h") => (
            "cli-config-help",
            "Manage Rivo Vendor, Model, Provider and Credential.\n\nUsage: rivo config [COMMAND]\n\nCommands: path, show, validate, set, unset, credentials, route, vendor, model, provider\n\nOptions: -h, --help  Show help",
        ),
        [command, help] if matches!(help.as_str(), "--help" | "-h") => match command.as_str() {
            "path" => (
                "cli-config-path-help",
                "Show the configuration file path.\n\nUsage: rivo config path\n\nOptions: -h, --help  Show help",
            ),
            "show" => (
                "cli-config-show-help",
                "Show the redacted effective configuration.\n\nUsage: rivo config show\n\nOptions: -h, --help  Show help",
            ),
            "validate" => (
                "cli-config-validate-help",
                "Validate configuration and the model catalog.\n\nUsage: rivo config validate\n\nOptions: -h, --help  Show help",
            ),
            "set" => (
                "cli-config-set-help",
                "Set a configuration value.\n\nUsage: rivo config set <KEY> [VALUE]\n\nOptions: -h, --help  Show help",
            ),
            "unset" => (
                "cli-config-unset-help",
                "Unset a configuration value.\n\nUsage: rivo config unset <KEY> [VALUE]\n\nOptions: -h, --help  Show help",
            ),
            "credentials" => (
                "cli-config-credentials-help",
                "Manage local credentials.\n\nUsage: rivo config credentials [COMMAND]\n\nCommands: path, show, set, unset, migrate\n\nOptions: -h, --help  Show help",
            ),
            "route" => (
                "cli-config-route-help",
                "Preview deterministic Vendor/Model routing without making a request.\n\nUsage: rivo config route --vendor <VENDOR> [--model <MODEL>]\n\nOptions: -h, --help  Show help",
            ),
            "vendor" => (
                "cli-config-vendor-help",
                "Manage the local Vendor catalog.\n\nUsage: rivo config vendor <list|show|set|remove> [ARGS]\n\nOptions: -h, --help  Show help",
            ),
            "model" => (
                "cli-config-model-help",
                "Manage the canonical Model catalog.\n\nUsage: rivo config model <list|show|set|remove> [ARGS]\n\nOptions: -h, --help  Show help",
            ),
            "provider" => (
                "cli-config-provider-help",
                "Manage Provider connections.\n\nUsage: rivo config provider <list|show|set|remove> [ARGS]\n\nOptions: -h, --help  Show help",
            ),
            _ => return false,
        },
        [credentials, command, help]
            if credentials == "credentials" && matches!(help.as_str(), "--help" | "-h") =>
        {
            match command.as_str() {
                "path" => (
                    "cli-config-credentials-path-help",
                    "Show the credential file path.\n\nUsage: rivo config credentials path\n\nOptions: -h, --help  Show help",
                ),
                "show" => (
                    "cli-config-credentials-show-help",
                    "List credential names and saved state without showing secrets.\n\nUsage: rivo config credentials show\n\nOptions: -h, --help  Show help",
                ),
                "set" => (
                    "cli-config-credentials-set-help",
                    "Save a credential interactively or from an environment variable.\n\nUsage: rivo config credentials set <NAME> [--from-env <ENV>]\n\nOptions: -h, --help  Show help",
                ),
                "unset" => (
                    "cli-config-credentials-unset-help",
                    "Delete a credential.\n\nUsage: rivo config credentials unset <NAME>\n\nOptions: -h, --help  Show help",
                ),
                "migrate" => (
                    "cli-config-credentials-migrate-help",
                    "Migrate legacy plaintext credentials to encrypted storage.\n\nUsage: rivo config credentials migrate\n\nOptions: -h, --help  Show help",
                ),
                _ => return false,
            }
        }
        _ => return false,
    };
    println!("{}", ui::text(id, fallback));
    true
}

fn print_key_value_query(
    format: QueryOutputFormat,
    values: Vec<(&str, String)>,
) -> anyhow::Result<()> {
    print_query_values(format, None, values)
}

fn print_config_detail_query(
    format: QueryOutputFormat,
    text: String,
    values: Vec<(&str, String)>,
) -> anyhow::Result<()> {
    print_query_values(format, Some(text), values)
}

fn print_query_values(
    format: QueryOutputFormat,
    text_override: Option<String>,
    values: Vec<(&str, String)>,
) -> anyhow::Result<()> {
    let mut table = Table::new([
        ui::text("cli-output-field", "Field"),
        ui::text("cli-output-value", "Value"),
    ]);
    let mut json = serde_json::Map::new();
    let use_key_value_text = text_override.is_none();
    let mut text = text_override.unwrap_or_default();
    for (key, value) in values {
        if use_key_value_text {
            text.push_str(&format!("{key}={value}\n"));
        }
        table.push_row(vec![key.to_owned(), value.clone()]);
        json.insert(key.into(), serde_json::Value::String(value));
    }
    print_query(
        format,
        QueryOutput::new(text, table, serde_json::Value::Object(json)),
    )
}

/// Executes a config command against an explicit path so deterministic tests
/// do not need to mutate the process-wide `RIVO_CONFIG` environment.
async fn run_config_command_at(path: &std::path::Path, args: Vec<String>) -> anyhow::Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("show");
    match command {
        "path" => println!("{}", path.display()),
        "show" => {
            let draft = read_model_routing_draft(path)?;
            println!("{}", toml::to_string_pretty(&draft.config)?);
        }
        "validate" => {
            // Validation of a saved document must not require a successful
            // discovery. Discovery freshness is an execution precondition,
            // not a prerequisite for inspecting or repairing configuration.
            let file = read_file_config(path)?;
            validate_model_routing_config(&file)?;
            println!("{}", ui::text("cli-config-valid", "valid"));
        }
        "route" => {
            let mut route_args = args.to_vec();
            let format = parse_query_format(&mut route_args)?;
            let vendor = route_args
                .iter()
                .position(|value| value == "--vendor")
                .and_then(|index| route_args.get(index + 1))
                .context(ui::explicit_text(
                    "cli-config-route-usage",
                    "用法: rivo config route --vendor <vendor> [--model <model>]",
                ))?;
            let model = route_args
                .iter()
                .position(|value| value == "--model")
                .and_then(|index| route_args.get(index + 1));
            let provider_id = route_args
                .iter()
                .position(|value| value == "--provider-id")
                .and_then(|index| route_args.get(index + 1));
            let file = read_file_config(path)?;
            let format = resolve_query_format(format, file.defaults.query_format);
            let route = resolve_available_route(
                &file,
                Some(vendor),
                model.map(String::as_str),
                provider_id.map(String::as_str),
            )?;
            print_key_value_query(
                format,
                vec![
                    ("vendor", route.vendor),
                    ("model", route.model),
                    ("provider", route.provider_id),
                    ("protocol", route.protocol.to_string()),
                    ("base_url", route.base_url),
                    ("priority", route.priority.to_string()),
                ],
            )?;
        }
        "credentials" => run_credentials_command(path, &args[1..])?,
        "vendor" | "model" | "provider" => {
            run_model_routing_config_command(path, command, &args[1..]).await?
        }
        "set" | "unset" => {
            upgrade_file_config(path)?;
            let draft = read_model_routing_draft(path)?;
            let mut file = draft.config;
            let key = args
                .get(1)
                .ok_or_else(|| {
                    anyhow::anyhow!(ui::text_with(
                        "cli-config-set-usage",
                        &format!("用法: rivo config {command} <key> [value]"),
                        &[("command", command.to_owned())],
                    ))
                })?;
            let value = args.get(2);
            let set = command == "set";
            match key.as_str() {
                "locale" => {
                    file.defaults.locale = if set {
                        let value = value.context(ui::explicit_text(
                            "cli-config-locale-missing",
                            "缺少 locale",
                        ))?;
                        rivo_i18n::LocaleId::parse(value)
                            .map_err(|error| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-locale-invalid",
                                    &format!("locale 无效: {error}"),
                                    &[("error", error.to_string())],
                                ))
                            })?
                            .to_string()
                    } else {
                        "en-US".into()
                    };
                }
                "query_format" => {
                    file.defaults.query_format = if set {
                        Some(
                            QueryOutputFormat::from_str(
                                value.context(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "query_format".into())],
                                ))?,
                                false,
                            )
                            .map_err(|error| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-invalid",
                                    &error,
                                    &[("key", "query_format".into()), ("error", error.clone())],
                                ))
                            })?
                                .into(),
                        )
                    } else {
                        None
                    };
                }
                "vendor" | "model" | "provider_id" => {
                    let target = match key.as_str() {
                        "vendor" => &mut file.defaults.vendor,
                        "model" => &mut file.defaults.model,
                        _ => &mut file.defaults.provider_id,
                    };
                    *target = if set {
                        Some(
                            value
                                .context(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", key.clone())],
                                ))?
                                .clone(),
                        )
                    } else {
                        None
                    };
                }
                "profile" => {
                    file.defaults.capability = if set {
                        Some(
                            value
                                .ok_or_else(|| {
                                    anyhow::anyhow!(ui::text_with(
                                        "cli-config-value-missing",
                                        "缺少 {$key} 的值",
                                        &[("key", "profile".into())],
                                    ))
                                })?
                                .parse()
                                .map_err(|e: String| anyhow::anyhow!(e))?,
                        )
                    } else {
                        None
                    }
                }
                "policy" => {
                    file.defaults.policy = if set {
                        Some(
                            value
                                .ok_or_else(|| {
                                    anyhow::anyhow!(ui::text_with(
                                        "cli-config-value-missing",
                                        "缺少 {$key} 的值",
                                        &[("key", "policy".into())],
                                    ))
                                })?
                                .parse()
                                .map_err(|e: String| anyhow::anyhow!(e))?,
                        )
                    } else {
                        None
                    }
                }
                "workspace" => {
                    file.defaults.workspace = if set {
                        Some(PathBuf::from(
                            value.ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "workspace".into())],
                                ))
                            })?,
                        ))
                    } else {
                        None
                    }
                }
                "budget.max_steps" => {
                    file.defaults.budget.max_steps = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "max_steps".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::BudgetConfig::default().max_steps
                    }
                }
                "budget.max_retries" => {
                    file.defaults.budget.max_retries = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "max_retries".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::BudgetConfig::default().max_retries
                    }
                }
                "budget.max_duration_seconds" => {
                    file.defaults.budget.max_duration_seconds = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "max_duration_seconds".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::BudgetConfig::default().max_duration_seconds
                    }
                }
                "budget.context_limit_tokens" => {
                    file.defaults.budget.context_limit_tokens = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "context_limit_tokens".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::BudgetConfig::default().context_limit_tokens
                    }
                }
                "budget.max_observation_bytes" => {
                    file.defaults.budget.max_observation_bytes = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "max_observation_bytes".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::BudgetConfig::default().max_observation_bytes
                    }
                }
                "logging.required" => {
                    file.defaults.logging.required = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "required".into())],
                                ))
                            })
                            .and_then(|value| value.parse().map_err(Into::into))?
                    } else {
                        false
                    }
                }
                "logging.retention_days" => {
                    file.defaults.logging.retention_days = if set {
                        value
                            .ok_or_else(|| {
                                anyhow::anyhow!(ui::text_with(
                                    "cli-config-value-missing",
                                    "缺少 {$key} 的值",
                                    &[("key", "retention_days".into())],
                                ))
                            })?
                            .parse()?
                    } else {
                        rivo_session::LoggingConfig::default().retention_days
                    }
                }
                "mcp.server" => {
                    if set {
                        let raw = value.ok_or_else(|| {
                            anyhow::anyhow!(ui::explicit_text(
                                "cli-config-mcp-json-missing",
                                "缺少 MCP server JSON",
                            ))
                        })?;
                        let server: rivo_session::McpServerConfig =
                            serde_json::from_str(raw).context(ui::explicit_text(
                                "cli-config-mcp-json-invalid",
                                "MCP server JSON 无效",
                            ))?;
                        if let Some(existing) = file
                            .mcp
                            .servers
                            .iter_mut()
                            .find(|existing| existing.name == server.name)
                        {
                            *existing = server;
                        } else {
                            file.mcp.servers.push(server);
                        }
                    } else {
                        let name = value.ok_or_else(|| {
                            anyhow::anyhow!(ui::explicit_text(
                                "cli-config-mcp-name-missing",
                                "缺少 MCP server name",
                            ))
                        })?;
                        file.mcp.servers.retain(|server| server.name != *name);
                    }
                }
                _ => anyhow::bail!(
                    "{}",
                    ui::text_with(
                        "cli-config-unsupported-field",
                        &format!("不支持的配置字段: {key}"),
                        &[("key", key.clone())],
                    )
                ),
            }
            commit_model_routing_draft(path, &draft.revision, &file)?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-updated",
                    &format!("updated {}", path.display()),
                    &[("path", path.display().to_string())],
                )
            );
        }
        _ => {
            return Err(anyhow::anyhow!(ui::explicit_text(
                "cli-config-usage",
                "用法: rivo config path|show|validate|set|unset|vendor|model|provider|credentials|route"
            )))
        }
    }
    Ok(())
}

fn prompt_masked_password(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    rpassword::read_password_with_config(
        rpassword::ConfigBuilder::new()
            .password_feedback_mask('*')
            .build(),
    )
}

fn run_credentials_command(path: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!(
            "{}",
            ui::text(
                "cli-config-credentials-help",
                "Usage: rivo config credentials path|show|set <name>|unset <name>|migrate"
            )
        );
        return Ok(());
    }
    match args.first().map(String::as_str).unwrap_or("show") {
        "path" => println!("{}", rivo_session::credentials_file_path(path).display()),
        "show" => {
            for (name, status) in rivo_session::credential_names(path)? {
                let status = match status {
                    rivo_session::CredentialStatus::Encrypted => "encrypted",
                    rivo_session::CredentialStatus::Plaintext => "plaintext (will migrate on use)",
                };
                println!("{name}={status}");
            }
        }
        "set" => {
            let name = args.get(1).context(ui::explicit_text(
                "cli-config-credentials-set-usage",
                "用法: rivo config credentials set <环境变量名>",
            ))?;
            let value = if args.get(2).map(String::as_str) == Some("--from-env") {
                let environment = args.get(3).context(ui::explicit_text(
                    "cli-config-credentials-from-env-usage",
                    "用法: rivo config credentials set <name> --from-env <environment-variable>",
                ))?;
                if args.get(4).is_some() {
                    anyhow::bail!("{}", ui::explicit_text("cli-config-credentials-from-env-usage", "用法: rivo config credentials set <name> --from-env <environment-variable>"));
                }
                std::env::var(environment).with_context(|| {
                    ui::text_with(
                        "cli-config-env-missing",
                        &format!("环境变量未设置: {environment}"),
                        &[("name", environment.clone())],
                    )
                })?
            } else {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!("{}", ui::explicit_text("cli-config-credentials-noninteractive", "无法读取非交互式输入中的 API key；自动化调用请使用 --from-env <environment-variable>"));
                }
                let value = prompt_masked_password("API key: ")?;
                let confirmation = prompt_masked_password("Confirm API key: ")?;
                if value != confirmation {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-config-credentials-mismatch",
                            "两次输入的 API key 不一致"
                        )
                    );
                }
                value
            };
            if value.trim().is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-config-credentials-empty", "API key 不能为空")
                );
            }
            rivo_session::set_credential(path, name, &value)?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-credential-saved",
                    &format!("credential saved for {name}"),
                    &[("name", name.clone())],
                )
            );
        }
        "unset" => {
            let name = args.get(1).context(ui::explicit_text(
                "cli-config-credentials-unset-usage",
                "用法: rivo config credentials unset <环境变量名>",
            ))?;
            let draft = read_model_routing_draft(path)?;
            let references = draft
                .config
                .providers
                .iter()
                .filter(|(_, provider)| provider.auth.credential.as_deref() == Some(name))
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>();
            if !references.is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::text_with(
                        "cli-config-credentials-in-use",
                        &format!(
                            "Credential {name} 仍被 Provider 引用: {}；请先修正这些 Provider",
                            references.join(", ")
                        ),
                        &[("name", name.clone()), ("providers", references.join(", "))]
                    )
                );
            }
            rivo_session::unset_credential(path, name)?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-credential-removed",
                    &format!("credential removed for {name}"),
                    &[("name", name.clone())],
                )
            );
        }
        "migrate" => {
            let count = rivo_session::migrate_credentials(path)?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-credentials-migrated",
                    &format!("migrated {count} credential(s)"),
                    &[("count", count.to_string())],
                )
            );
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-config-credentials-usage",
                "用法: rivo config credentials path|show|set|unset|migrate"
            )
        ),
    }
    Ok(())
}

async fn run_model_routing_config_command(
    path: &std::path::Path,
    kind: &str,
    args: &[String],
) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let format = if matches!(operation, "list" | "show") {
        parse_query_format(&mut args)?
    } else {
        None
    };
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let is_read_only = matches!(operation, "list" | "show" | "providers")
        || (kind == "provider"
            && operation == "models"
            && matches!(
                args.get(1).map(String::as_str).unwrap_or("list"),
                "list" | "remote"
            ));
    if !is_read_only {
        upgrade_file_config(path)?;
    }
    let draft = read_model_routing_draft(path)?;
    let format = if matches!(operation, "list" | "show") {
        resolve_query_format(format, draft.config.defaults.query_format)
    } else {
        QueryOutputFormat::Text
    };
    if kind == "model" && operation == "providers" {
        let model_id = args.get(1).context(ui::explicit_text(
            "cli-config-model-providers-usage",
            "用法: rivo config model providers <model-id>",
        ))?;
        let models = merged_model_catalog(&draft.config)?;
        let model = models.get(model_id).with_context(|| {
            ui::text_with(
                "cli-config-model-not-found",
                &format!("Model 不存在: {model_id}"),
                &[("id", model_id.clone())],
            )
        })?;
        let providers = model_providers(&draft.config, &model.vendor, model_id)?;
        if providers.is_empty() {
            println!(
                "{}",
                ui::text_with(
                    "cli-config-no-provider",
                    &format!(
                        "no available provider for model={model_id} vendor={}",
                        model.vendor
                    ),
                    &[
                        ("model", model_id.clone()),
                        ("vendor", model.vendor.clone())
                    ],
                )
            );
            return Ok(());
        }
        for route in providers {
            println!(
                "{}\tavailable\tpriority={}",
                route.provider_id, route.priority
            );
        }
        return Ok(());
    }
    if kind == "provider" && matches!(operation, "models") {
        let models_operation = args.get(1).map(String::as_str).unwrap_or("list");
        let provider_id = args.get(2).context(ui::explicit_text(
            "cli-config-provider-models-usage",
            "用法: rivo config provider models list|remote|refresh <provider-id>",
        ))?;
        let provider = draft.config.providers.get(provider_id).with_context(|| {
            ui::text_with(
                "cli-config-provider-not-found",
                &format!("Provider 不存在: {provider_id}"),
                &[("id", provider_id.clone())],
            )
        })?;
        match models_operation {
            "list" => {
                println!("provider={provider_id}");
                println!(
                    "supported={}",
                    provider
                        .discovery
                        .supported
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".into())
                );
                println!(
                    "last_checked_at={}",
                    provider
                        .discovery
                        .last_checked_at
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "never".into())
                );
                if let Some(error) = &provider.discovery.last_error {
                    println!("last_error={error}");
                }
                for model in provider_models(&draft.config, provider_id)? {
                    println!("{}\t{}", model.model, model.vendor);
                }
            }
            "refresh" => {
                let updated = refresh_provider_models(path, &draft.revision, provider_id).await?;
                let discovery = &updated.config.providers[provider_id].discovery;
                if discovery.supported != Some(true) || discovery.last_error.is_some() {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-config-discovery-failed",
                            "模型发现失败；请检查 Provider 连接与认证引用",
                        )
                    );
                }
                println!("supported=true");
                print_discovered_models(discovery);
            }
            "remote" => {
                for model in discover_provider_models(path, provider_id).await? {
                    println!("{model}");
                }
            }
            _ => {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-config-provider-models-usage",
                        "用法: rivo config provider models list|remote|refresh <provider-id>"
                    )
                )
            }
        }
        return Ok(());
    }
    match operation {
        "list" => match kind {
            "vendor" => {
                let sources = vendor_catalog_sources(&draft.config)?;
                let mut table = Table::new(["ID", "Display name", "Source"]);
                let mut json = Vec::new();
                let mut text = String::new();
                for (id, value) in merged_vendor_catalog(&draft.config)? {
                    let source = format!("{:?}", sources[&id]);
                    text.push_str(&format!("{id}\t{}\tsource={source}\n", value.display_name));
                    table.push_row([id.clone(), value.display_name.clone(), source.clone()]);
                    json.push(serde_json::json!({"id": id, "display_name": value.display_name, "source": source}));
                }
                print_query(
                    format,
                    QueryOutput::new(text, table, serde_json::Value::Array(json)),
                )?;
            }
            "model" => {
                let filters = parse_model_list_filters(&mut args)?;
                if let Some(vendor) = &filters.vendor {
                    if !merged_vendor_catalog(&draft.config)?.contains_key(vendor) {
                        anyhow::bail!(
                            "{}",
                            ui::text_with(
                                "cli-config-vendor-not-found",
                                &format!("Vendor 不存在: {vendor}"),
                                &[("id", vendor.clone())]
                            )
                        );
                    }
                }
                let sources = model_catalog_sources(&draft.config)?;
                let mut table = Table::new([
                    "ID",
                    "Vendor",
                    "Display name",
                    "Protocols",
                    "Enabled",
                    "Priority",
                    "Source",
                ]);
                let mut json = Vec::new();
                let mut text = String::new();
                for (id, value) in merged_model_catalog(&draft.config)? {
                    if filters
                        .vendor
                        .as_deref()
                        .is_some_and(|vendor| value.vendor != vendor)
                        || !filters.state.includes(value.enabled)
                    {
                        continue;
                    }
                    let source = format!("{:?}", sources[&id]);
                    text.push_str(&format!(
                        "{id}\t{}\t{}\t{}\t{}\t{}\tsource={source}\n",
                        value.vendor,
                        value.display_name,
                        format_supported_protocols(&value.supported_protocols),
                        value.enabled,
                        value.priority
                    ));
                    table.push_row([
                        id.clone(),
                        value.vendor.clone(),
                        value.display_name.clone(),
                        format_supported_protocols(&value.supported_protocols),
                        value.enabled.to_string(),
                        value.priority.to_string(),
                        source.clone(),
                    ]);
                    json.push(serde_json::json!({"id": id, "vendor": value.vendor, "display_name": value.display_name, "supported_protocols": value.supported_protocols.iter().map(ToString::to_string).collect::<Vec<_>>(), "enabled": value.enabled, "priority": value.priority, "source": source}));
                }
                print_query(
                    format,
                    QueryOutput::new(text, table, serde_json::Value::Array(json)),
                )?;
            }
            "provider" => {
                let mut table = Table::new(["ID", "Base URL", "Priority", "Enabled"]);
                let mut json = Vec::new();
                let mut text = String::new();
                for (id, value) in &draft.config.providers {
                    text.push_str(&format!(
                        "{id}\t{}\t{}\t{}\n",
                        value.base_url, value.priority, value.enabled
                    ));
                    table.push_row([
                        id.clone(),
                        value.base_url.clone(),
                        value.priority.to_string(),
                        value.enabled.to_string(),
                    ]);
                    json.push(serde_json::json!({"id": id, "base_url": value.base_url, "priority": value.priority, "enabled": value.enabled}));
                }
                print_query(
                    format,
                    QueryOutput::new(text, table, serde_json::Value::Array(json)),
                )?;
            }
            _ => unreachable!(),
        },
        "show" => {
            let id = args.get(1).context(ui::explicit_text(
                "cli-config-show-id-missing",
                "show 需要 ID",
            ))?;
            match kind {
                "vendor" => {
                    let value = merged_vendor_catalog(&draft.config)?
                        .get(id)
                        .with_context(|| {
                            ui::text_with(
                                "cli-config-vendor-not-found",
                                &format!("Vendor 不存在: {id}"),
                                &[("id", id.clone())],
                            )
                        })?
                        .clone();
                    print_config_detail_query(
                        format,
                        toml::to_string_pretty(&value)?,
                        vec![("id", id.clone()), ("display_name", value.display_name)],
                    )?;
                }
                "model" => {
                    let value = merged_model_catalog(&draft.config)?
                        .get(id)
                        .with_context(|| {
                            ui::text_with(
                                "cli-config-model-not-found",
                                &format!("Model 不存在: {id}"),
                                &[("id", id.clone())],
                            )
                        })?
                        .clone();
                    print_config_detail_query(
                        format,
                        toml::to_string_pretty(&value)?,
                        vec![
                            ("id", id.clone()),
                            ("vendor", value.vendor),
                            ("display_name", value.display_name),
                            (
                                "supported_protocols",
                                format_supported_protocols(&value.supported_protocols),
                            ),
                            ("enabled", value.enabled.to_string()),
                            ("priority", value.priority.to_string()),
                        ],
                    )?;
                }
                "provider" => {
                    let value = draft.config.providers.get(id).with_context(|| {
                        ui::text_with(
                            "cli-config-provider-not-found",
                            &format!("Provider 不存在: {id}"),
                            &[("id", id.clone())],
                        )
                    })?;
                    print_config_detail_query(
                        format,
                        toml::to_string_pretty(value)?,
                        vec![
                            ("id", id.clone()),
                            ("base_url", value.base_url.clone()),
                            ("priority", value.priority.to_string()),
                            ("enabled", value.enabled.to_string()),
                        ],
                    )?;
                }
                _ => unreachable!(),
            }
        }
        "set" => {
            let id = args.get(1).context("set 需要 ID")?.clone();
            let provider_id = id.clone();
            let values = routing_flags(&args[2..])?;
            let allowed: &[&str] = match kind {
                "vendor" => &["display-name"],
                "model" => &[
                    "vendor",
                    "display-name",
                    "protocols",
                    "enable-protocol",
                    "disable-protocol",
                    "reasoning",
                    "priority",
                ],
                "provider" => &[
                    "base-url",
                    "credential-name",
                    "enabled",
                    "priority",
                    "confirm",
                    "refresh",
                ],
                _ => anyhow::bail!("未知配置对象"),
            };
            if kind == "provider" && values.contains_key("protocol") {
                anyhow::bail!(
                    "Provider 不再携带协议：协议由模型决定；请使用 `config model set <id> --enable-protocol/--disable-protocol/--protocols` 调整模型协议"
                );
            }
            for key in values.keys() {
                if !allowed.contains(&key.as_str()) {
                    anyhow::bail!("未知参数: --{key}");
                }
            }
            let mut config = draft.config;
            match kind {
                "vendor" => {
                    config.vendors.insert(
                        id,
                        VendorCatalogEntry {
                            display_name: required_flag(&values, "display-name")?.into(),
                        },
                    );
                }
                "model" => {
                    // 本地条目优先，否则以内置条目为基础，以便覆盖内置模型的协议列表。
                    let merged = merged_model_catalog(&config)?;
                    let previous = merged.get(&id);
                    let mut supported_protocols = values
                        .get("protocols")
                        .map(|value| parse_supported_protocols(value))
                        .transpose()?
                        .or_else(|| previous.map(|model| model.supported_protocols.clone()))
                        .unwrap_or_default();
                    if let Some(value) = values.get("enable-protocol") {
                        for protocol in parse_supported_protocols(value)? {
                            if !supported_protocols.contains(&protocol) {
                                supported_protocols.push(protocol);
                            }
                        }
                    }
                    if let Some(value) = values.get("disable-protocol") {
                        let disabled = parse_supported_protocols(value)?;
                        supported_protocols.retain(|protocol| !disabled.contains(protocol));
                        if supported_protocols.is_empty() {
                            anyhow::bail!("不能禁用模型的全部协议；模型至少需要一个支持协议");
                        }
                    }
                    if supported_protocols.is_empty() {
                        anyhow::bail!("缺少 --protocols；模型必须显式声明至少一个支持协议");
                    }
                    let supported_reasoning_efforts = values
                        .get("reasoning")
                        .map(|value| {
                            parse_supported_reasoning_efforts(value).map_err(anyhow::Error::msg)
                        })
                        .transpose()?
                        .or_else(|| previous.map(|model| model.supported_reasoning_efforts.clone()))
                        .unwrap_or_default();
                    let vendor = match values.get("vendor") {
                        Some(vendor) => vendor.clone(),
                        None => previous
                            .map(|model| model.vendor.clone())
                            .context("缺少 --vendor")?,
                    };
                    let display_name = match values.get("display-name") {
                        Some(name) => name.clone(),
                        None => previous
                            .map(|model| model.display_name.clone())
                            .context("缺少 --display-name")?,
                    };
                    let priority = if values.contains_key("priority") {
                        optional_priority(&values, "priority")?
                    } else {
                        previous.map(|model| model.priority).unwrap_or_default()
                    };
                    let enabled = previous.is_none_or(|model| model.enabled);
                    config.models.insert(
                        id,
                        ModelCatalogEntry {
                            vendor,
                            display_name,
                            supported_protocols,
                            supported_reasoning_efforts,
                            enabled,
                            priority,
                        },
                    );
                }
                "provider" => {
                    if values.contains_key("refresh")
                        && values.get("refresh") != Some(&"true".into())
                    {
                        anyhow::bail!("--refresh 不接受值；请直接使用 --refresh");
                    }
                    let previous = config.providers.get(&id);
                    let base_url = values
                        .get("base-url")
                        .cloned()
                        .or_else(|| previous.map(|provider| provider.base_url.clone()))
                        .context("缺少 --base-url")?;
                    let auth = if values.contains_key("credential-name") {
                        let credential_name = required_flag(&values, "credential-name")?;
                        if !rivo_session::credential_exists(path, credential_name)? {
                            anyhow::bail!(
                                "Credential {credential_name} 不存在，请先执行 `config credentials set {credential_name}`"
                            );
                        }
                        AuthConfig {
                            credential: Some(credential_name.into()),
                        }
                    } else {
                        previous
                            .map(|provider| provider.auth.clone())
                            .unwrap_or_default()
                    };
                    let enabled = optional_bool(
                        &values,
                        "enabled",
                        previous.is_none_or(|provider| provider.enabled),
                    )?;
                    let priority = if values.contains_key("priority") {
                        optional_priority(&values, "priority")?
                    } else {
                        previous
                            .map(|provider| provider.priority)
                            .unwrap_or_default()
                    };
                    let discovery = previous
                        .map(|provider| provider.discovery.clone())
                        .unwrap_or_default();
                    config.providers.insert(
                        id,
                        RoutingProviderConfig {
                            base_url,
                            auth,
                            enabled,
                            priority,
                            discovery,
                        },
                    );
                }
                _ => unreachable!(),
            }
            let plan = plan_model_routing_change(path, &draft.revision, &config)?;
            if values
                .get("confirm")
                .is_some_and(|token| token != &plan.confirmation_token)
            {
                anyhow::bail!("确认计划已变化；请重新读取并确认");
            }
            if !plan.provider_impacts.is_empty() && !values.contains_key("confirm") {
                println!("{}", serde_json::to_string_pretty(&plan)?);
                anyhow::bail!("连接变更需要 --confirm <confirmation_token> 确认");
            }
            let committed = commit_model_routing_change(
                path,
                &draft.revision,
                &config,
                values.get("confirm").map(String::as_str),
            )?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-saved-revision",
                    &format!("saved revision={}", committed.revision),
                    &[("revision", committed.revision.to_string())],
                )
            );
            if kind == "provider" {
                if values.contains_key("refresh") {
                    let refreshed =
                        refresh_provider_models(path, &committed.revision, &provider_id).await?;
                    let discovery = &refreshed.config.providers[&provider_id].discovery;
                    if discovery.supported != Some(true) || discovery.last_error.is_some() {
                        let error = discovery.last_error.clone().unwrap_or_else(|| {
                            ui::text("cli-config-model-discovery-failed", "Provider 模型发现失败")
                        });
                        anyhow::bail!("{error}");
                    }
                    println!(
                        "{}",
                        ui::text_with(
                            "cli-config-discovery-refreshed",
                            &format!("模型发现已刷新: {provider_id}"),
                            &[("provider", provider_id.clone())],
                        )
                    );
                    print_discovered_models(discovery);
                } else {
                    println!(
                        "{}",
                        ui::text_with(
                            "cli-config-discovery-invalidated",
                            &format!("模型发现已失效；请执行 `config provider models refresh {provider_id}`"),
                            &[("provider", provider_id.clone())],
                        )
                    );
                }
            }
        }
        "enable" | "disable" if kind == "model" => {
            let enabled = operation == "enable";
            let ids = args.get(1..).unwrap_or_default();
            if ids.is_empty() {
                anyhow::bail!("用法: rivo config model {operation} <model-id>...");
            }
            let builtin = read_default_catalog()?.models;
            let models = merged_model_catalog(&draft.config)?;
            let mut config = draft.config;
            let mut changed = false;
            for id in BTreeSet::<String>::from_iter(ids.iter().cloned()) {
                let Some(mut entry) = models.get(&id).cloned() else {
                    println!("model={id}\tmissing");
                    continue;
                };
                entry.enabled = enabled;
                let previous = config.models.get(&id).cloned();
                if builtin.get(&id) == Some(&entry) {
                    config.models.remove(&id);
                } else {
                    config.models.insert(id.clone(), entry);
                }
                changed |= previous != config.models.get(&id).cloned();
                println!(
                    "{}",
                    ui::text_with(
                        "cli-config-model-state",
                        &format!(
                            "model={id}\t{}",
                            if enabled { "enabled" } else { "disabled" }
                        ),
                        &[
                            ("id", id.clone()),
                            (
                                "state",
                                if enabled {
                                    "enabled".into()
                                } else {
                                    "disabled".into()
                                }
                            )
                        ],
                    )
                );
            }
            if changed {
                let committed = commit_model_routing_change(path, &draft.revision, &config, None)?;
                println!(
                    "{}",
                    ui::text_with(
                        "cli-config-saved-revision",
                        &format!("saved revision={}", committed.revision),
                        &[("revision", committed.revision.to_string())],
                    )
                );
            }
        }
        "remove" => {
            let id = args.get(1).context("remove 需要 ID")?;
            let plan = plan_model_routing_delete(path, kind, id)?;
            let force = match args.get(2..).unwrap_or_default() {
                [] => false,
                [flag] if flag == "--yes" => true,
                _ => anyhow::bail!("用法: rivo config {kind} remove <id> [--yes]"),
            };
            if !force {
                print_model_routing_delete_plan(&plan)?;
                confirm_model_routing_delete(kind, id)?;
            }
            let committed = delete_model_routing_config(path, &plan, &plan.confirmation_token)?;
            println!(
                "{}",
                ui::text_with(
                    "cli-config-removed-revision",
                    &format!("removed revision={}", committed.revision),
                    &[("revision", committed.revision.to_string())],
                )
            );
        }
        _ => anyhow::bail!(
            "用法: rivo config {kind} list|show <id>|set <id> ...|remove <id> [--yes]"
        ),
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ModelListState {
    All,
    Enabled,
    Disabled,
}

impl ModelListState {
    fn includes(self, enabled: bool) -> bool {
        match self {
            Self::All => true,
            Self::Enabled => enabled,
            Self::Disabled => !enabled,
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "all" => Ok(Self::All),
            "enabled" => Ok(Self::Enabled),
            "disabled" => Ok(Self::Disabled),
            _ => anyhow::bail!("--state 必须是 enabled、disabled 或 all"),
        }
    }
}

struct ModelListFilters {
    vendor: Option<String>,
    state: ModelListState,
}

fn parse_model_list_filters(args: &mut Vec<String>) -> anyhow::Result<ModelListFilters> {
    let mut vendor = None;
    let mut state = ModelListState::All;
    let mut state_seen = false;
    while args.len() > 1 {
        let index = 1;
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--"))
            .with_context(|| format!("{} 缺少值", args[index]))?
            .clone();
        match args[index].as_str() {
            "--vendor" => {
                if vendor.replace(value).is_some() {
                    anyhow::bail!("配置参数重复: --vendor");
                }
            }
            "--state" => {
                if state_seen {
                    anyhow::bail!("配置参数重复: --state");
                }
                state = ModelListState::parse(&value)?;
                state_seen = true;
            }
            flag => anyhow::bail!("未知参数: {flag}"),
        }
        args.drain(index..=index + 1);
    }
    Ok(ModelListFilters { vendor, state })
}

fn print_discovered_models(discovery: &ModelDiscoveryCache) {
    if discovery.models.is_empty() {
        println!(
            "{}",
            ui::explicit_text(
                "cli-config-no-compatible-models",
                "模型发现成功，但没有发现模型目录中的模型",
            )
        );
        return;
    }
    for model in &discovery.models {
        println!("{model}");
    }
}

fn print_model_routing_delete_plan(
    plan: &rivo_session::ModelRoutingDeletePlan,
) -> anyhow::Result<()> {
    let mut display_plan = serde_json::to_value(plan)?;
    display_plan
        .as_object_mut()
        .expect("model routing delete plan serializes as an object")
        .remove("confirmation_token");
    println!("{}", serde_json::to_string_pretty(&display_plan)?);
    Ok(())
}

fn confirm_model_routing_delete(kind: &str, id: &str) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        anyhow::bail!("删除需要交互确认；自动化调用请显式传入 --yes");
    }
    eprint!("确认删除 {kind} {id}？此操作不可恢复。 [y/N] ");
    std::io::stderr().flush()?;
    // Use terminal key events instead of a line-buffered read so Ctrl+C is a
    // deterministic cancellation on ConPTY as well as on a native terminal.
    // Restore the caller's raw-mode state even when event reading fails.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode()?;
    }
    let answer = (|| -> anyhow::Result<Option<char>> {
        loop {
            match crossterm::event::read()? {
                crossterm::event::Event::Key(key)
                    if matches!(
                        key.kind,
                        crossterm::event::KeyEventKind::Press
                            | crossterm::event::KeyEventKind::Repeat
                    ) =>
                {
                    if key.code == crossterm::event::KeyCode::Enter {
                        return Ok(None);
                    }
                    if key.code == crossterm::event::KeyCode::Char('c')
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        return Ok(None);
                    }
                    if let crossterm::event::KeyCode::Char(character) = key.code {
                        return Ok(Some(character));
                    }
                }
                _ => {}
            }
        }
    })();
    let restore = if !was_raw {
        crossterm::terminal::disable_raw_mode()
    } else {
        Ok(())
    };
    restore?;
    let answer = answer?;
    if !answer.is_some_and(|value| is_affirmative_confirmation(&value.to_string())) {
        anyhow::bail!("已取消删除 {kind} {id}");
    }
    Ok(())
}

fn is_affirmative_confirmation(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn routing_flags(values: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut index = 0;
    while index < values.len() {
        let flag = values[index]
            .strip_prefix("--")
            .context("配置参数必须以 -- 开头")?;
        if flag == "refresh" {
            if let Some(value) = values.get(index + 1) {
                if value == "--refresh" {
                    anyhow::bail!("配置参数重复: --refresh");
                }
                if !value.starts_with("--") {
                    anyhow::bail!("--refresh 不接受值；请直接使用 --refresh");
                }
            }
            result.insert(flag.into(), "true".into());
            index += 1;
            continue;
        }
        let value = values.get(index + 1).context("配置参数缺少值")?;
        if result.insert(flag.into(), value.clone()).is_some() {
            anyhow::bail!("配置参数重复: --{flag}");
        }
        index += 2;
    }
    Ok(result)
}

fn required_flag<'a>(values: &'a BTreeMap<String, String>, name: &str) -> anyhow::Result<&'a str> {
    values
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .context(format!("缺少 --{name}"))
}

fn optional_priority(values: &BTreeMap<String, String>, name: &str) -> anyhow::Result<u16> {
    let value = values
        .get(name)
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(0);
    if value > 1000 {
        anyhow::bail!("--{name} 必须在 0..=1000");
    }
    Ok(value)
}

/// 按书写顺序解析协议列表；顺序即偏好，重复项被拒绝。
fn parse_supported_protocols(value: &str) -> anyhow::Result<Vec<ProviderKind>> {
    let mut protocols = Vec::new();
    for value in value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let protocol = value
            .parse::<ProviderKind>()
            .map_err(|error| anyhow::anyhow!(error))?;
        if protocols.contains(&protocol) {
            anyhow::bail!("协议重复: {protocol}");
        }
        protocols.push(protocol);
    }
    if protocols.is_empty() {
        anyhow::bail!("协议列表至少需要一个协议")
    }
    Ok(protocols)
}

fn format_supported_protocols(protocols: &[ProviderKind]) -> String {
    protocols
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn optional_bool(
    values: &BTreeMap<String, String>,
    name: &str,
    default: bool,
) -> anyhow::Result<bool> {
    Ok(values
        .get(name)
        .map(|value| value.parse::<bool>())
        .transpose()?
        .unwrap_or(default))
}

fn history_database_path() -> anyhow::Result<PathBuf> {
    let root = rivo_session::data_root_dir().context("无法确定用户数据目录")?;
    Ok(root.join("rivo").join("rivo.db"))
}

async fn delete_history_session_async(session_id: String) -> anyhow::Result<()> {
    let path = history_database_path()?;
    tokio::task::spawn_blocking(move || {
        let store = rivo_storage::Store::open(path)?;
        store.delete_session(&session_id)?;
        rivo_session::remove_media_assets(&session_id);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("本地会话存储 worker 已中断")?
}

/// `rivo a2a serve` 的 Session 反向适配器。它只从 owner 已加载的普通配置与
/// `a2a-server.toml` 中的固定 alias 构造独立 Session；远端请求不能影响其中任何
/// 一个配置值，也无法取得 `IntentSender`。
struct CliInboundSessionDriver {
    base_config: ResolvedConfig,
    workspace_aliases: BTreeMap<String, PathBuf>,
    active: Arc<
        tokio::sync::Mutex<BTreeMap<InboundSessionRef, (rivo_session::IntentSender, A2aTaskId)>>,
    >,
    approval_broker: Option<Arc<LocalApprovalQueue>>,
}

impl CliInboundSessionDriver {
    fn new(
        base_config: ResolvedConfig,
        workspace_aliases: BTreeMap<String, PathBuf>,
        approval_broker: Option<Arc<LocalApprovalQueue>>,
    ) -> Self {
        Self {
            base_config,
            workspace_aliases,
            active: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            // 未显式装配时仍 fail-closed；A2A 网络永远没有 approval route。
            approval_broker,
        }
    }

    fn fixed_config(&self, request: &InboundSessionStart) -> Result<ResolvedConfig, ServerError> {
        let workspace = self
            .workspace_aliases
            .get(&request.profile.workspace_alias)
            .cloned()
            .ok_or(ServerError::InvalidRequest)?;
        let capability = CapabilityProfile::from_str(&request.profile.capability_profile)
            .map_err(|_| ServerError::InvalidRequest)?;
        let policy = PolicyProfile::from_str(&request.profile.policy_ceiling)
            .map_err(|_| ServerError::InvalidRequest)?;
        // ServiceProfile validation has already limited policy to strict/medium and
        // research → strict; retain this defensive check at the Session boundary.
        if matches!(policy, PolicyProfile::FullControl)
            || (matches!(capability, CapabilityProfile::Research)
                && !matches!(policy, PolicyProfile::Strict))
        {
            return Err(ServerError::InvalidRequest);
        }
        let mut config = self.base_config.clone();
        config.workspace = workspace;
        config.capability_profile = capability;
        config.policy_profile = policy;
        config.budget.max_duration_seconds = config
            .budget
            .max_duration_seconds
            .min(request.profile.max_duration_seconds);
        Ok(config)
    }
}

#[async_trait]
impl InboundSessionDriver for CliInboundSessionDriver {
    async fn start(
        &self,
        request: InboundSessionStart,
        projector: Arc<dyn InboundSessionProjector>,
    ) -> Result<(), ServerError> {
        let config = self.fixed_config(&request)?;
        let session =
            Arc::new(Session::connect(config.clone()).map_err(|_| ServerError::InvalidRequest)?);
        let (sender, mut events) = session.channel();
        self.active.lock().await.insert(
            request.local_session_ref.clone(),
            (sender.clone(), request.task_id.clone()),
        );
        // `Session` 已创建且本机关联已建立后才把 task 标为 WORKING。这样即便
        // provider 在发出 Started 前初始化失败，FAILED 也能从合法状态转换，且
        // 远端不会看到“已接受但尚未映射 Session”的长期 SUBMITTED task。
        if projector
            .project(
                &request.peer_id,
                &request.task_id,
                &request.local_session_ref,
                InboundSessionEvent::Working,
            )
            .await
            .is_err()
        {
            self.active.lock().await.remove(&request.local_session_ref);
            return Err(ServerError::InvalidRequest);
        }
        let active = self.active.clone();
        let broker = self.approval_broker.clone();
        tokio::spawn(async move {
            let options = TurnOptions {
                locale: config.locale.clone(),
                capability_profile: config.capability_profile,
                policy_profile: config.policy_profile,
                reasoning_effort: Default::default(),
                network: request.profile.network_enabled,
                browser: request.profile.browser_enabled,
                computer: false,
                shell: false,
                mcp_servers: request.profile.mcp_servers.clone(),
                mcp_toolsets: request.profile.mcp_toolsets.clone(),
                // 远端 task 不继承本机 A2A peer、记忆或内部子代理选择；它们都不是
                // ServiceProfile 允许远端调用的 capability。
                a2a_peers: Default::default(),
                memory_recall: false,
                trace: false,
                subagents_enabled: false,
                max_subagents: 1,
            };
            // Local smoke only: hold a mapped task long enough to exercise the
            // cancellation boundary. Normal runs never set this variable.
            if let Ok(value) = std::env::var("RIVO_A2A_SMOKE_HOLD_MS") {
                if let Ok(milliseconds) = value.parse::<u64>() {
                    tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
                }
            }
            if request.profile.capability_profile == "coding"
                && std::env::var("RIVO_A2A_SMOKE_REQUIRE_APPROVAL").as_deref() == Ok("1")
            {
                let local_request = LocalApprovalRequest {
                    task_id: request.task_id.clone(),
                    peer_id: request.peer_id.clone(),
                    skill_id: request.profile.skill_id.clone(),
                    turn_id: format!("a2a-smoke-{}", request.task_id.0),
                    request_id: format!("approval-{}", request.task_id.0),
                    call_id: format!("approval-{}", request.task_id.0),
                    tool_name: "smoke.coding".into(),
                    canonical_args_sha256: hex::encode(Sha256::digest(b"{}")),
                    arguments_summary: "本机 smoke coding 审批".into(),
                    targets: Vec::new(),
                    program: None,
                    side_effects: vec!["workspace_write".into()],
                    redacted_summary: "等待服务端本机授权".into(),
                    expires_unix: unix_now()
                        .saturating_add(request.profile.approval_timeout_seconds),
                };
                if projector
                    .project(
                        &request.peer_id,
                        &request.task_id,
                        &request.local_session_ref,
                        InboundSessionEvent::LocalApprovalRequired,
                    )
                    .await
                    .is_err()
                {
                    active.lock().await.remove(&request.local_session_ref);
                    return;
                }
                let decision = match broker.clone() {
                    Some(queue) => tokio::task::spawn_blocking(move || {
                        queue.request_local_approval(&local_request)
                    })
                    .await
                    .unwrap_or(LocalApproval::Unavailable),
                    None => LocalApproval::Unavailable,
                };
                if !matches!(decision, LocalApproval::ApprovedOnce) {
                    let _ = projector
                        .project(
                            &request.peer_id,
                            &request.task_id,
                            &request.local_session_ref,
                            InboundSessionEvent::Rejected,
                        )
                        .await;
                    active.lock().await.remove(&request.local_session_ref);
                    return;
                }
            }
            if sender
                .send(Intent::SetTurnOptions { options })
                .await
                .is_err()
                || sender
                    .send(Intent::SubmitTask {
                        text: request.message.clone(),
                    })
                    .await
                    .is_err()
            {
                let _ = projector
                    .project(
                        &request.peer_id,
                        &request.task_id,
                        &request.local_session_ref,
                        InboundSessionEvent::Failed,
                    )
                    .await;
                active.lock().await.remove(&request.local_session_ref);
                return;
            }

            let mut tool_arguments = HashMap::<String, serde_json::Value>::new();
            while let Some(SessionEvent { event, .. }) = events.next().await {
                let projection = match event {
                    AgentEvent::Started { .. } => Some(InboundSessionEvent::Working),
                    AgentEvent::ToolRequested {
                        call_id, arguments, ..
                    } => {
                        tool_arguments.insert(call_id, arguments);
                        None
                    }
                    AgentEvent::ConfirmationRequired {
                        request_id,
                        call_id,
                        turn_id,
                        tool_name,
                        arguments_summary,
                        targets,
                        program,
                        side_effects,
                        ..
                    }
                    | AgentEvent::SubagentConfirmationRequired {
                        request_id,
                        call_id,
                        turn_id,
                        tool_name,
                        arguments_summary,
                        targets,
                        program,
                        side_effects,
                        ..
                    } => {
                        let _ = projector
                            .project(
                                &request.peer_id,
                                &request.task_id,
                                &request.local_session_ref,
                                InboundSessionEvent::LocalApprovalRequired,
                            )
                            .await;
                        // 仅本机 broker 可以产生 Confirm；远端 JSON-RPC 不存在对应
                        // 路由。缺 broker、超时或拒绝都发送一次性 deny 并结束 task。
                        // Session 保留原始工具 JSON 以执行；queue 只持久化其 canonical
                        // SHA-256，因此本机批准无法被换绑到不同参数。缺少 hash 时不能
                        // 安全形成一次性许可，直接 fail-closed。
                        let arguments_sha256 = tool_arguments
                            .get(&call_id)
                            .and_then(|arguments| rivo_a2a::canonical_json_sha256(arguments).ok());
                        let decision = match (broker.clone(), arguments_sha256) {
                            (Some(queue), Some(arguments_sha256)) => {
                                let local_request = LocalApprovalRequest {
                                    task_id: request.task_id.clone(),
                                    peer_id: request.peer_id.clone(),
                                    skill_id: request.profile.skill_id.clone(),
                                    turn_id,
                                    request_id: request_id.clone(),
                                    call_id,
                                    tool_name,
                                    canonical_args_sha256: arguments_sha256,
                                    arguments_summary,
                                    targets,
                                    program,
                                    side_effects,
                                    redacted_summary: "等待服务端本机授权".into(),
                                    expires_unix: unix_now()
                                        .saturating_add(request.profile.approval_timeout_seconds),
                                };
                                tokio::task::spawn_blocking(move || {
                                    queue.request_local_approval(&local_request)
                                })
                                .await
                                .unwrap_or(LocalApproval::Unavailable)
                            }
                            _ => LocalApproval::Unavailable,
                        };
                        match decision {
                            LocalApproval::ApprovedOnce => {
                                if sender
                                    .send(Intent::Confirm {
                                        request_id,
                                        allow: true,
                                        reason: None,
                                        scope: ApprovalScope::Once,
                                    })
                                    .await
                                    .is_ok()
                                {
                                    Some(InboundSessionEvent::Working)
                                } else {
                                    Some(InboundSessionEvent::Failed)
                                }
                            }
                            LocalApproval::Rejected
                            | LocalApproval::TimedOut
                            | LocalApproval::Unavailable => {
                                let _ = sender
                                    .send(Intent::Confirm {
                                        request_id,
                                        allow: false,
                                        reason: None,
                                        scope: ApprovalScope::Once,
                                    })
                                    .await;
                                Some(InboundSessionEvent::Rejected)
                            }
                        }
                    }
                    AgentEvent::FinalAnswer { text, .. } => {
                        Some(InboundSessionEvent::Completed { text })
                    }
                    AgentEvent::Error { .. } | AgentEvent::Failure { .. } => {
                        Some(InboundSessionEvent::Failed)
                    }
                    AgentEvent::Stopped { .. } => Some(InboundSessionEvent::Canceled),
                    _ => None,
                };
                if let Some(event) = projection {
                    let terminal = matches!(
                        event,
                        InboundSessionEvent::Completed { .. }
                            | InboundSessionEvent::Failed
                            | InboundSessionEvent::Rejected
                            | InboundSessionEvent::Canceled
                    );
                    let _ = projector
                        .project(
                            &request.peer_id,
                            &request.task_id,
                            &request.local_session_ref,
                            event,
                        )
                        .await;
                    if terminal {
                        break;
                    }
                }
            }
            active.lock().await.remove(&request.local_session_ref);
        });
        Ok(())
    }

    async fn cancel(&self, local_session: &InboundSessionRef) -> Result<(), ServerError> {
        let (sender, task_id) = self
            .active
            .lock()
            .await
            .get(local_session)
            .cloned()
            .ok_or(ServerError::TaskNotFound)?;
        if let Some(queue) = &self.approval_broker {
            // 取消先使已排队/刚批准但尚未消费的许可失效，再投递 Session cancel。
            // 若本机队列不可写，保留 task 为 working，避免报告已取消但副作用仍可执行。
            queue.invalidate_task(&task_id)?;
        }
        sender
            .send(Intent::CancelTurn)
            .await
            .map_err(|_| ServerError::InvalidRequest)
    }
}

struct CliInstanceControlHandler {
    registry: InstanceRegistry,
}

#[async_trait]
impl InstanceControlHandler for CliInstanceControlHandler {
    async fn handle(
        &self,
        _peer: &A2aPeerId,
        envelope: LanControlEnvelope,
    ) -> Result<serde_json::Value, ServerError> {
        let instance = self
            .registry
            .load()
            .map_err(|_| ServerError::InvalidRequest)?
            .into_iter()
            .find(|instance| instance.instance_id == envelope.target_instance_id)
            .ok_or(ServerError::InvalidRequest)?;
        let mut params = envelope.params;
        if let Some(object) = params.as_object_mut() {
            object.insert(
                "idempotency_key".into(),
                serde_json::Value::String(envelope.control.idempotency_key),
            );
            object.insert(
                "confirmation".into(),
                serde_json::Value::Bool(envelope.control.confirmation),
            );
        }
        let response = request_named_pipe(
            &instance.endpoint,
            &serde_json::json!({
                "jsonrpc":"2.0",
                "id": envelope.request_id,
                "method": envelope.method,
                "params": params,
            }),
            Duration::from_secs(10),
        )
        .await
        .map_err(|_| ServerError::InvalidRequest)?;
        if response.get("error").is_some() {
            return Err(ServerError::InvalidRequest);
        }
        Ok(response.get("result").cloned().unwrap_or(response))
    }
}

async fn run_a2a_command(args: Vec<String>) -> anyhow::Result<()> {
    if print_a2a_help(&args) {
        return Ok(());
    }
    let path = history_database_path()?;
    run_a2a_command_at(&path, args).await
}

async fn with_a2a_store<T, F>(path: &std::path::Path, operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&rivo_storage::Store) -> anyhow::Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let store = rivo_storage::Store::open(path)?;
        operation(&store)
    })
    .await
    .context(ui::explicit_text(
        "cli-a2a-storage-worker-interrupted",
        "A2A 本地存储 worker 已中断",
    ))?
}

fn provision_a2a_test_materials(args: &[String]) -> anyhow::Result<()> {
    let yes = args.iter().any(|value| value == "--yes");
    let bind = args.windows(2).find(|pair| pair[0] == "--bind").map(|pair| pair[1].clone())
        .context(ui::explicit_text("cli-a2a-provision-usage", "用法: rivo a2a provision --yes --bind <private-ip:port> --server-identity-ref a2a-test-... --issuer-ref a2a-test-..."))?;
    let server_ref = args
        .windows(2)
        .find(|pair| pair[0] == "--server-identity-ref")
        .map(|pair| pair[1].clone())
        .context(ui::explicit_text(
            "cli-a2a-server-identity-missing",
            "缺少 --server-identity-ref",
        ))?;
    let issuer_ref = args
        .windows(2)
        .find(|pair| pair[0] == "--issuer-ref")
        .map(|pair| pair[1].clone())
        .context(ui::explicit_text(
            "cli-a2a-issuer-missing",
            "缺少 --issuer-ref",
        ))?;
    if !yes {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-provision-confirm-required",
                "provision 需要显式 --yes"
            )
        )
    }
    if !server_ref.starts_with("a2a-test-") || !issuer_ref.starts_with("a2a-test-") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-test-identity-required",
                "provision 只允许 a2a-test- 前缀 identity"
            )
        )
    }
    let address = bind
        .rsplit_once(':')
        .map(|(ip, _)| ip)
        .context(ui::explicit_text(
            "cli-a2a-bind-format-invalid",
            "bind 必须为 IP:port",
        ))?;
    let ip: std::net::IpAddr = address.parse().context(ui::explicit_text(
        "cli-a2a-bind-format-invalid",
        "bind 必须为 IP:port",
    ))?;
    if !is_allowed_bind_address(
        ip,
        std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1"),
    ) {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-bind-address-invalid",
                "bind 必须为 RFC1918/ULA 地址；仅 RIVO_A2A_LOCAL_TEST=1 可使用 loopback"
            )
        )
    }

    let store = KeyringServerMaterialStore;
    if store.load(&server_ref).is_ok() || store.load_client_issuer(&issuer_ref).is_ok() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-test-identity-exists",
                "测试 identity 已存在；为避免覆盖，请换用新的引用"
            )
        )
    }
    let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut server_params = CertificateParams::new(vec![address.to_owned()])?;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let server_cert = server_params.self_signed(&server_key)?;
    let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let material = rivo_a2a_server::ServerTlsMaterial {
        certificate_chain: vec![server_cert.der().clone()],
        private_key: tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
            server_key.serialize_der(),
        )
        .into(),
        client_ca_certificates: vec![ca_cert.der().clone()],
    };
    let spki = server_spki_sha256(&material).context(ui::explicit_text(
        "cli-a2a-spki-compute-failed",
        "无法从生成的 Server 证书计算 SPKI",
    ))?;
    store
        .save(&server_ref, &material)
        .context(ui::explicit_text(
            "cli-a2a-server-keychain-write-failed",
            "无法写入 Server TLS bundle 到 OS keychain",
        ))?;
    let issuer = rivo_a2a_server::ClientIssuerMaterial {
        certificate: ca_cert.der().clone(),
        private_key: tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
            ca_key.serialize_der(),
        )
        .into(),
    };
    if let Err(error) = store.save_client_issuer(&issuer_ref, &issuer) {
        let _ = store.delete(&server_ref);
        return Err(anyhow::Error::new(error).context(ui::explicit_text(
            "cli-a2a-client-ca-write-failed",
            "无法写入 profile Client CA 到 OS keychain",
        )));
    }
    println!(
        "provisioned server_identity_ref={} issuer_ref={} bind={} spki_sha256={}",
        server_ref, issuer_ref, bind, spki
    );
    Ok(())
}

/// PSK transport deliberately keeps its TLS leaf and unused compatibility CA
/// in process memory. Unlike the default mTLS path, no keychain material is
/// required or written on Linux/macOS test hosts.
fn ephemeral_psk_server_material(
    bind_ip: std::net::IpAddr,
    secret: &rivo_a2a::psk::PskSecret,
) -> anyhow::Result<(
    rivo_a2a_server::ServerTlsMaterial,
    rivo_a2a_server::ClientIssuerMaterial,
)> {
    // The leaf key is deterministically reconstructed from the in-memory PSK
    // and the private bind address. This keeps the SPKI pin stable across a
    // PSK-server restart while neither key material is persisted. A PSK holder
    // can already authenticate as either side, so this adds no new authority.
    let mut transcript = b"rivo-a2a-psk-tls-leaf-v1\0".to_vec();
    transcript.extend_from_slice(bind_ip.to_string().as_bytes());
    let seed = rivo_a2a::psk::derive_session_key(secret, &[0u8; 32], &[0u8; 32], &transcript);
    let server_secret = SecretKey::from_slice(seed.as_ref())
        .map_err(|_| anyhow::anyhow!("PSK TLS seed 无法生成 P-256 身份"))?;
    let server_key = KeyPair::try_from(server_secret.to_pkcs8_der()?.as_bytes())?;
    let mut server_params = CertificateParams::new(vec![bind_ip.to_string()])?;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let server_cert = server_params.self_signed(&server_key)?;
    let issuer_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut issuer_params = CertificateParams::default();
    issuer_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    issuer_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let issuer_cert = issuer_params.self_signed(&issuer_key)?;
    Ok((
        rivo_a2a_server::ServerTlsMaterial {
            certificate_chain: vec![server_cert.der().clone()],
            private_key: tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                server_key.serialize_der(),
            )
            .into(),
            client_ca_certificates: vec![issuer_cert.der().clone()],
        },
        rivo_a2a_server::ClientIssuerMaterial {
            certificate: issuer_cert.der().clone(),
            private_key: tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                issuer_key.serialize_der(),
            )
            .into(),
        },
    ))
}

fn cleanup_a2a_test_materials(args: &[String]) -> anyhow::Result<()> {
    if !args.iter().any(|value| value == "--yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-cleanup-confirm-required",
                "provision cleanup 需要显式 --yes"
            )
        )
    }
    let store = KeyringServerMaterialStore;
    for flag in ["--server-identity-ref", "--issuer-ref"] {
        let value = args
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .context(ui::text_with(
                "cli-a2a-flag-missing",
                &format!("缺少 {flag}"),
                &[("flag", flag.to_owned())],
            ))?;
        if !value.starts_with("a2a-test-") {
            anyhow::bail!(
                "{}",
                ui::explicit_text(
                    "cli-a2a-cleanup-identity-required",
                    "cleanup 只允许 a2a-test- 前缀 identity"
                )
            )
        }
        if flag == "--server-identity-ref" {
            let _ = store.delete(&value);
        } else {
            let _ = store.delete_client_issuer(&value);
        }
    }
    println!(
        "{}",
        ui::text(
            "cli-a2a-test-identities-cleaned",
            "test A2A identities cleaned"
        )
    );
    Ok(())
}

fn check_a2a_test_materials(args: &[String]) -> anyhow::Result<()> {
    let server_ref = args
        .windows(2)
        .find(|pair| pair[0] == "--server-identity-ref")
        .map(|pair| pair[1].clone())
        .context(ui::explicit_text(
            "cli-a2a-server-identity-missing",
            "缺少 --server-identity-ref",
        ))?;
    let issuer_ref = args
        .windows(2)
        .find(|pair| pair[0] == "--issuer-ref")
        .map(|pair| pair[1].clone())
        .context(ui::explicit_text(
            "cli-a2a-issuer-missing",
            "缺少 --issuer-ref",
        ))?;
    if !server_ref.starts_with("a2a-test-") || !issuer_ref.starts_with("a2a-test-") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-check-identity-required",
                "check 只允许 a2a-test- 前缀 identity"
            )
        )
    }
    let store = KeyringServerMaterialStore;
    let tls = store.load(&server_ref).context(ui::explicit_text(
        "cli-a2a-server-keychain-read-failed",
        "无法读取 Server v2 keychain 条目",
    ))?;
    let _issuer = store
        .load_client_issuer(&issuer_ref)
        .context(ui::explicit_text(
            "cli-a2a-client-ca-read-failed",
            "无法读取 Client CA v2 keychain 条目",
        ))?;
    let spki = server_spki_sha256(&tls).context(ui::explicit_text(
        "cli-a2a-spki-parse-failed",
        "Server 证书 SPKI 无法解析",
    ))?;
    println!(
        "keychain_ready server_identity_ref={} issuer_ref={} spki_sha256={}",
        server_ref, issuer_ref, spki
    );
    Ok(())
}

/// A2A 管理只作用于 Server/Client 所在的本机 SQLite 与 OS keychain。它不接受
/// endpoint、证书或 token 的远端覆盖输入；发现和配对另有显式流程。
async fn run_a2a_command_at(path: &std::path::Path, args: Vec<String>) -> anyhow::Result<()> {
    if print_a2a_help(&args) {
        return Ok(());
    }
    match args.first().map(String::as_str).unwrap_or("peers") {
        "provision" => {
            if args.get(1).map(String::as_str) == Some("cleanup") {
                cleanup_a2a_test_materials(&args)?;
            } else if args.get(1).map(String::as_str) == Some("check") {
                check_a2a_test_materials(&args)?;
            } else {
                provision_a2a_test_materials(&args)?;
            }
        }
        "discover" => {
            let timeout_seconds = parse_a2a_discovery_timeout(&args)?;
            let candidates = tokio::task::spawn_blocking(move || {
                discover_lan_candidates(std::time::Duration::from_secs(timeout_seconds))
            })
            .await
            .context(ui::explicit_text(
                "cli-a2a-discovery-interrupted",
                "A2A discovery task 已中断",
            ))??;
            for candidate in candidates {
                println!(
                    "{}	name={}	endpoint={}	card={}	spki_sha256={}	expires_unix={}",
                    candidate.candidate_id,
                    candidate.instance_name,
                    candidate.interface_url,
                    candidate.card_url,
                    candidate.spki_sha256,
                    candidate.expires_unix,
                );
            }
        }
        "pair" => {
            let psk_mode = parse_a2a_psk_flags(&args)?;
            let (identity_label, endpoint, spki_sha256) = parse_a2a_pair_args(&args)?;
            if psk_mode {
                let secret = read_a2a_psk_secret()?;
                let card = fetch_agent_card(&endpoint, &spki_sha256).await?;
                let peer_id = psk_peer_id(&endpoint, &card.canonical_sha256)?;
                eprintln!("PSK candidate peer_id={}", peer_id.0);
                let trust = A2aTrustSnapshot {
                    peer_id: peer_id.clone(), interface_url: endpoint.clone(), protocol_binding: "PSK".into(),
                    tenant: None, spki_sha256: spki_sha256.clone(), agent_card_id: card.id.clone(),
                    agent_card_jcs_sha256: card.canonical_sha256.clone(), allowed_skills: card.skills.clone(),
                    spec_revision: SPEC_REVISION.into(), wire_protocol_version: WIRE_PROTOCOL_VERSION.into(),
                    status: A2aTrustState::Trusted, approved_at: utc_now_rfc3339(),
                };
                let transport = PskTransport::new(&trust, secret)?;
                transport.request(&trust, JsonRpcRequest {
                    jsonrpc: "2.0".into(), id: "psk-bootstrap".into(), method: "tasks/get".into(),
                    params: serde_json::json!({"id": "psk-bootstrap"}),
                }).await.map_err(|error| anyhow::anyhow!("PSK bootstrap 被拒绝: {error}"))?;
                with_a2a_store(path, move |store| {
                    Ok(store.save_a2a_peer(&rivo_storage::StoredA2aPeer {
                        snapshot: trust,
                        identity_ref: None,
                    })?)
                })
                .await?;
                println!("paired peer_id={} transport=psk identity_label={identity_label}", peer_id.0);
                return Ok(());
            }
            // Server 在 claim 后才分配不透明 peer ID；此处的 label 仅命名本机
            // keychain identity，绝不能被当成远端身份或写入 trust snapshot。
            let identity_ref = format!("a2a-client-{identity_label}");
            let identities = KeyringA2aIdentityStore;
            let identity = create_client_pairing_identity(&identities, &identity_ref)?;
            let result = async {
                let card = fetch_agent_card(&endpoint, &spki_sha256).await?;
                let pairing = begin_pairing(&endpoint, &spki_sha256, &identity).await?;
                eprintln!(
                    "A2A 配对 request_id={} endpoint={} spki_sha256={} card_sha256={}\nSAS（请与对端本机显示内容逐词比对）：{}",
                    pairing.request_id.0,
                    endpoint,
                    spki_sha256,
                    card.canonical_sha256,
                    pairing.sas_words.join(" "),
                );
                confirm_a2a_sas()?;
                let claim = claim_pairing(&endpoint, &identities, &identity, &pairing).await?;
                let peer_id = claim.peer_id.clone();
                let stored_peer = rivo_storage::StoredA2aPeer {
                    snapshot: A2aTrustSnapshot {
                        peer_id: claim.peer_id, interface_url: endpoint, protocol_binding: "JSONRPC".into(),
                        tenant: None, spki_sha256, agent_card_id: card.id,
                        agent_card_jcs_sha256: card.canonical_sha256, allowed_skills: card.skills,
                        spec_revision: SPEC_REVISION.into(), wire_protocol_version: WIRE_PROTOCOL_VERSION.into(),
                        status: A2aTrustState::Trusted, approved_at: utc_now_rfc3339(),
                    }, identity_ref: Some(identity_ref),
                };
                with_a2a_store(path, move |store| Ok(store.save_a2a_peer(&stored_peer)?)).await?;
                Ok::<A2aPeerId, anyhow::Error>(peer_id)
            }.await;
            if result.is_err() { let _ = identities.delete(&identity.identity_ref); }
            let remote_peer_id = result?;
            println!("paired peer_id={} identity_label={identity_label}", remote_peer_id.0);
        }
        "peers" => {
            if !path.exists() {
                return Ok(());
            }
            let peers = with_a2a_store(path, |store| Ok(store.a2a_peers()?)).await?;
            for peer in peers {
                println!(
                    "{}\tstatus={:?}\tinterface_url={}\tspki_sha256={}\tcard_sha256={}\tskills={}\tidentity_present={}",
                    peer.snapshot.peer_id.0,
                    peer.snapshot.status,
                    peer.snapshot.interface_url,
                    peer.snapshot.spki_sha256,
                    peer.snapshot.agent_card_jcs_sha256,
                    peer.snapshot.allowed_skills.into_iter().collect::<Vec<_>>().join(","),
                    peer.identity_ref.is_some(),
                );
            }
        }
        "inspect" => {
            let peer_id = args
                .get(1)
                .context(ui::explicit_text(
                    "cli-a2a-inspect-usage",
                    "用法: rivo a2a inspect <peer-id>",
                ))?
                .clone();
            let peer = with_a2a_store(path, move |store| {
                Ok(store.a2a_peer(&A2aPeerId(peer_id))?)
            })
            .await?
                .context(ui::explicit_text(
                    "cli-a2a-peer-not-found",
                    "A2A peer 不存在",
                ))?;
            println!("peer_id={}", peer.snapshot.peer_id.0);
            println!("status={:?}", peer.snapshot.status);
            println!("interface_url={}", peer.snapshot.interface_url);
            println!("spki_sha256={}", peer.snapshot.spki_sha256);
            println!("agent_card_jcs_sha256={}", peer.snapshot.agent_card_jcs_sha256);
            println!("protocol_binding={}", peer.snapshot.protocol_binding);
            println!("spec_revision={}", peer.snapshot.spec_revision);
            println!("wire_protocol_version={}", peer.snapshot.wire_protocol_version);
            println!(
                "skills={}",
                peer.snapshot
                    .allowed_skills
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!("identity_present={}", peer.identity_ref.is_some());
        }
        "revoke" => {
            let (peer_id, force) = match args.as_slice() {
                [_, peer_id] => (peer_id, false),
                [_, peer_id, flag] if flag == "--yes" => (peer_id, true),
                _ => anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-revoke-usage",
                        "用法: rivo a2a revoke <peer-id> [--yes]",
                    )
                ),
            };
            confirm_history_deletion(&format!("撤销本机 A2A peer {peer_id}"), force)?;
            let peer_id = peer_id.clone();
            let revoked_peer_id = peer_id.clone();
            with_a2a_store(path, move |store| {
                revoke_a2a_peer_with_identity(
                    store,
                    &A2aPeerId(revoked_peer_id),
                    &KeyringA2aIdentityStore,
                )
            })
            .await?;
            println!("{}", ui::text_with("cli-a2a-peer-revoked", &format!("revoked {peer_id}"), &[("id", peer_id.clone())]));
        }
        "serve" => {
            let psk_mode = parse_a2a_psk_flags(&args)?;
            let psk_secret = if psk_mode { Some(read_a2a_psk_secret()?) } else { None };
            let (config_path, profile_name) = parse_a2a_serve_args(&args)?;
            let config = A2aServerFileConfig::load_at(&config_path)?;
            let profile = match profile_name {
                Some(name) => config.profile(&name)?,
                None if config.profiles.len() == 1 => &config.profiles[0],
                None => anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-profile-required",
                        "配置含多个 Server profile；请显式指定 `rivo a2a serve <server-profile> --config <path>`"
                    )
                ),
            };
            let state_path = server_state_path(&config_path, &config);
            let state = ServerStateStore::open(&state_path)?;
            // 先将无法安全恢复的 active task 固定为 FAILED，再恢复 terminal 映射。
            // 不向 Session driver 提供原始输入，因此重启路径没有重放入口。
            let failed = state.recover_unfinished(&profile.profile.name, unix_now())?;
            let expired_pairings = state.expire_pending_pairings_after_restart()?;
            let restored_tasks = state.load_tasks(&profile.profile.name)?;
            let restored_grants = state.load_peer_grants(&profile.profile.name)?;
            let (tls, issuer): (_, Arc<dyn ClientCertificateIssuer>) = if psk_mode {
                let secret = psk_secret
                    .as_ref()
                    .context(ui::explicit_text(
                        "cli-a2a-bootstrap-secret-missing",
                        "PSK Server 缺少仅内存的 bootstrap secret",
                    ))?;
                let (tls, issuer) = ephemeral_psk_server_material(profile.profile.bind.ip(), secret)?;
                (
                    tls,
                    Arc::new(KeyringClientCertificateIssuer::from_material(issuer)),
                )
            } else {
                let materials = KeyringServerMaterialStore;
                (
                    materials.load(&profile.identity_ref).context(ui::explicit_text(
                        "cli-a2a-server-tls-read-failed",
                        "无法从本机 keychain 读取 A2A Server TLS 身份；配置和 SQLite 不接受私钥",
                    ))?,
                    Arc::new(materials.load_client_issuer(&profile.profile.issuer_ref).context(ui::explicit_text(
                        "cli-a2a-client-ca-read-failed",
                        "无法从本机 keychain 读取 profile Client CA；请由本机 owner provisioning",
                    ))?),
                )
            };
            let spki = server_spki_sha256(&tls)?;
            let config_base = config_path.parent().unwrap_or_else(|| std::path::Path::new("."));
            let workspace_aliases = config.workspace_aliases.iter().map(|(alias, path)| {
                let workspace = config_base.join(path).canonicalize().map_err(|_| anyhow::anyhow!(ui::text_with(
                    "cli-a2a-workspace-alias-invalid",
                    &format!("A2A workspace alias 无法解析: {alias}"),
                    &[("alias", alias.clone())],
                )))?;
                Ok::<_, anyhow::Error>((alias.clone(), workspace))
            }).collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            // 普通 Rivo 配置只由 Server 所在本机 owner 读取。ServiceProfile
            // 随后会覆盖 workspace/capability/policy，并收紧 Turn options；远端
            // JSON-RPC 从未参与 ResolvedConfig 的构造。
            let session_config = ResolvedConfig::load(ConfigOverrides::default())?;
            // 使用同一台主机 state DB 的 owner-only queue；审批 CLI 必须另行在
            // 本机交互终端执行，listener 不暴露批准路由。
            let approval_queue = Arc::new(LocalApprovalQueue::open(state_path.clone())?);
            let session_driver = Arc::new(CliInboundSessionDriver::new(
                session_config,
                workspace_aliases,
                Some(approval_queue),
            ));
            let listener = if let Some(secret) = psk_secret {
                A2aHttpsListener::new_psk(
                    A2aServer::restore(profile.profile.clone(), restored_tasks)?,
                    PairingRegistry::from_peer_grants(restored_grants),
                    spki.clone(),
                    tls,
                    issuer,
                    secret,
                )?
            } else {
                A2aHttpsListener::new(
                    A2aServer::restore(profile.profile.clone(), restored_tasks)?,
                    PairingRegistry::from_peer_grants(restored_grants),
                    spki.clone(),
                    tls,
                    issuer,
                )?
            }
            .with_state_store(state)
            .with_session_driver(session_driver)
            .with_instance_control_handler(Arc::new(CliInstanceControlHandler {
                registry: InstanceRegistry::new(registry_path(
                    &std::env::var_os("RIVO_DATA_DIR")
                        .map(PathBuf::from)
                        .or_else(dirs::data_local_dir)
                        .context("无法确定实例数据目录")?,
                )),
            }));
            println!(
                "A2A Server profile={} bind={} transport={} spki_sha256={} recovered_failed_tasks={} expired_pending_pairings={}",
                profile.profile.name,
                profile.profile.bind,
                if psk_mode { "psk" } else { "mtls" },
                spki,
                failed,
                expired_pairings
            );
            Arc::new(listener).serve().await?;
        }
        "pairings" => {
            let (operation, request_id, config_path) = parse_a2a_pairings_args(&args)?;
            let config = A2aServerFileConfig::load_at(&config_path)?;
            let state = ServerStateStore::open(&server_state_path(&config_path, &config))?;
            match operation.as_str() {
                "list" => {
                    for pairing in state.pairing_summaries()? {
                        println!(
                            "request_id={} state={:?} client_public_key_sha256={} expires_unix={}",
                            pairing.request_id.0, pairing.state, pairing.client_public_key_sha256, pairing.expires_unix
                        );
                    }
                }
                "approve" | "reject" => {
                    let request_id = request_id.context("配对 request_id 不能为空")?;
                    confirm_a2a_pairing_decision(&operation, &request_id)?;
                    let next = if operation == "approve" { PairingState::Approved } else { PairingState::Rejected };
                    state.set_pairing_state(&rivo_a2a::A2aRequestId(request_id.clone()), next, unix_now())?;
                    println!("pairing {request_id} {operation}d");
                }
                "revoke" => {
                    let peer_id = request_id.context("peer_id 不能为空")?;
                    confirm_a2a_pairing_decision("revoke", &peer_id)?;
                    let profile_name = match config.profiles.as_slice() {
                        [profile] => profile.profile.name.clone(),
                        _ => anyhow::bail!("配置含多个 Server profile；无法确定 PeerGrant 所属 profile"),
                    };
                    if !state.revoke_peer_grant(&profile_name, &A2aPeerId(peer_id.clone()))? {
                        anyhow::bail!("PeerGrant 不存在或已经撤销");
                    }
                    println!("{}", ui::text_with("cli-a2a-peer-grant-revoked", &format!("peer grant {peer_id} revoked"), &[("id", peer_id.clone())]));
                }
                _ => unreachable!("argument parser only returns frozen operations"),
            }
        }
        "approvals" => {
            let (operation, approval_id, config_path) = parse_a2a_approvals_args(&args)?;
            ensure_local_a2a_owner_terminal()?;
            let config = A2aServerFileConfig::load_at(&config_path)?;
            let state = ServerStateStore::open(&server_state_path(&config_path, &config))?;
            match operation.as_str() {
                "list" => {
                    let approvals = state.pending_local_approvals(unix_now())?;
                    if approvals.is_empty() {
                        println!(
                            "{}",
                            ui::text(
                                "cli-a2a-no-pending-approvals",
                                "没有待本机裁决的 A2A 审批请求",
                            )
                        );
                    }
                    for approval in approvals {
                        println!(
                            "approval_id={} task_id={} peer_id={} skill_id={} turn_id={} request_id={} call_id={} tool={} arguments_sha256={} expires_unix={}\n  summary={}\n  targets={}\n  program={}\n  side_effects={}",
                            approval.approval_id,
                            approval.task_id.0,
                            approval.peer_id.0,
                            approval.skill_id,
                            approval.turn_id,
                            approval.request_id,
                            approval.call_id,
                            approval.tool_name,
                            approval.canonical_args_sha256,
                            approval.expires_unix,
                            approval.arguments_summary,
                            approval.targets.join(","),
                            approval.program.unwrap_or_else(|| "-".into()),
                            approval.side_effects.join(","),
                        );
                    }
                }
                "approve" | "reject" => {
                    let approval_id = approval_id.context("审批 approval_id 不能为空")?;
                    confirm_a2a_approval_decision(&operation, &approval_id)?;
                    state.decide_local_approval(&approval_id, operation == "approve", unix_now())?;
                    println!("{}", ui::text_with("cli-a2a-approval-decided", &format!("approval {approval_id} {operation}d once"), &[("id", approval_id.clone()), ("operation", operation.clone())]));
                }
                _ => unreachable!("argument parser only returns frozen operations"),
            }
        }
        "delegate" => {
            let (peer_id, skill_id, stream) = parse_a2a_delegate_args(&args)?;
            let mut message = String::new();
            std::io::stdin().read_to_string(&mut message)?;
            let psk_mode = args.windows(2).any(|pair| pair[0] == "--transport" && pair[1] == "psk");
            let psk_secret = if psk_mode {
                let (secret_text, body) = message.split_once('\n').context("PSK delegate stdin 第一行必须是 64 字符 secret")?;
                let bytes = hex::decode(secret_text.trim()).context("PSK secret 必须是 64 个 hex 字符")?;
                let secret: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("PSK secret 必须正好是 32 bytes"))?;
                message = body.to_string();
                Some(zeroize::Zeroizing::new(secret))
            } else { None };
            if message.trim().is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-empty-message",
                        "A2A 委派标准输入不能为空",
                    )
                );
            }
            let lookup_peer_id = peer_id.clone();
            let peer = with_a2a_store(path, move |store| {
                Ok(store.a2a_peer(&A2aPeerId(lookup_peer_id))?)
            })
            .await?
                .context(ui::explicit_text(
                    "cli-a2a-peer-not-paired",
                    "A2A peer 未配对或不存在",
                ))?;
            let peer = reconcile_a2a_peer_before_request(path, &peer).await?;
            if psk_mode && peer.snapshot.protocol_binding != "PSK" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-transport-mismatch",
                        "该 peer 不是 PSK transport",
                    )
                );
            }
            if !psk_mode && peer.snapshot.protocol_binding == "PSK" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-transport-mismatch",
                        "PSK peer 必须使用 --transport psk --secret-stdin",
                    )
                );
            }
            let request = A2aDelegationRequest {
                peer_id: peer.snapshot.peer_id.clone(),
                skill_id,
                message,
                stream,
                artifacts: Vec::new(),
            };
            let wire = rivo_a2a::build_delegation_jsonrpc_request(&peer.snapshot, &request)?;
            eprintln!(
                "将向 peer={} skill={} 外发 {} bytes（request_sha256={}）；已由 --yes 显式确认。",
                peer.snapshot.peer_id,
                request.skill_id,
                request.message.len(),
                wire.id,
            );
            let transport: Arc<dyn A2aTransport> = if let Some(secret) = psk_secret {
                Arc::new(PskTransport::new(&peer.snapshot, secret)?)
            } else {
                let identity_ref = peer.identity_ref.clone().context(ui::explicit_text(
                    "cli-a2a-identity-missing",
                    "A2A peer 缺少本机 mTLS 身份",
                ))?;
                Arc::new(KeyringMtlsTransport::new(Arc::new(KeyringA2aIdentityStore), [(peer.snapshot.peer_id.clone(), identity_ref)].into_iter().collect()))
            };
            let task = A2aTaskClient::new(transport)
                .send_delegation(&peer.snapshot, &request)
                .await?;
            let stored_task = rivo_storage::StoredA2aTask {
                peer_id: peer.snapshot.peer_id,
                task_id: task.id.clone(),
                request_sha256: wire.id,
                state: task.state,
                revision: task.revision,
            };
            with_a2a_store(path, move |store| Ok(store.save_a2a_task(&stored_task)?)).await?;
            println!(
                "task_id={} state={:?} revision={:?}",
                task.id.0, task.state, task.revision
            );
        }
        "control" => {
            let (peer_id, target_instance_id, method, confirmation) =
                parse_a2a_control_args(&args)?;
            let psk_mode = args.windows(2).any(|pair| pair[0] == "--transport" && pair[1] == "psk");
            let mut params_text = String::new();
            std::io::stdin().read_to_string(&mut params_text)?;
            let psk_secret = if psk_mode {
                let (secret_text, body) = params_text.split_once('\n').context("PSK control stdin 第一行必须是 64 字符 secret")?;
                let bytes = hex::decode(secret_text.trim()).context("PSK secret 必须是 64 个 hex 字符")?;
                let secret: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("PSK secret 必须正好是 32 bytes"))?;
                params_text = body.to_string();
                Some(zeroize::Zeroizing::new(secret))
            } else { None };
            let params: serde_json::Value = serde_json::from_str(&params_text)
                .context(ui::explicit_text(
                    "cli-a2a-control-json-invalid",
                    "A2A control 的标准输入必须是 JSON object",
                ))?;
            if !params.is_object() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-a2a-control-json-object",
                        "A2A control 参数必须是 JSON object",
                    )
                );
            }
            let lookup_peer_id = peer_id.clone();
            let peer = with_a2a_store(path, move |store| {
                Ok(store.a2a_peer(&A2aPeerId(lookup_peer_id))?)
            })
            .await?
                .context(ui::explicit_text(
                    "cli-a2a-peer-not-paired",
                    "A2A peer 未配对或不存在",
                ))?;
            if psk_mode && peer.snapshot.protocol_binding != "PSK" { anyhow::bail!("该 peer 不是 PSK transport"); }
            if !psk_mode && peer.snapshot.protocol_binding == "PSK" { anyhow::bail!("PSK peer 必须使用 --transport psk --secret-stdin"); }
            let now_ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u128 / 1_000_000;
            let sequence = CONTROL_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let seed = format!(
                "{}:{}:{}:{}:{}",
                peer_id,
                target_instance_id,
                std::process::id(),
                now_ms,
                sequence
            );
            let request_id = format!(
                "control-{}",
                &hex::encode(Sha256::digest(seed.as_bytes()))[..24]
            );
            let envelope = LanControlEnvelope {
                peer_id: peer.snapshot.peer_id.0.clone(),
                target_instance_id,
                method,
                params,
                request_id: request_id.clone(),
                nonce: format!("{}{}", request_id, "1234567890123456"),
                expires_at_unix_ms: now_ms.saturating_add(5 * 60 * 1000),
                control: ControlRequest {
                    scope: ControlScope::Lan,
                    confirmation,
                    idempotency_key: request_id.clone(),
                },
            };
            let wire = build_instance_control_jsonrpc_request(&peer.snapshot, &envelope)?;
            let transport: Box<dyn A2aTransport> = if let Some(secret) = psk_secret {
                Box::new(PskTransport::new(&peer.snapshot, secret)?)
            } else {
                let identity_ref = peer.identity_ref.clone().context(ui::explicit_text(
                    "cli-a2a-identity-missing",
                    "A2A peer 缺少本机 mTLS 身份",
                ))?;
                Box::new(KeyringMtlsTransport::new(Arc::new(KeyringA2aIdentityStore), [(peer.snapshot.peer_id.clone(), identity_ref)].into_iter().collect()))
            };
            let response = transport.request(&peer.snapshot, wire).await?.into_result()?;
            println!("{}", serde_json::to_string(&response)?);
        }
        "cancel" => {
            let (peer_id, task_id) = parse_a2a_cancel_args(&args)?;
            let psk_mode = args.windows(2).any(|pair| pair[0] == "--transport" && pair[1] == "psk");
            let task_id = A2aTaskId(task_id);
            let lookup_peer_id = peer_id.clone();
            let lookup_task_id = task_id.clone();
            let (peer, stored) = with_a2a_store(path, move |store| {
                let peer = store
                    .a2a_peer(&A2aPeerId(lookup_peer_id))?
                    .context(ui::explicit_text(
                        "cli-a2a-peer-not-paired",
                        "A2A peer 未配对或不存在",
                    ))?;
                let stored = store
                    .a2a_task(&peer.snapshot.peer_id, &lookup_task_id)?
                    .context("本机没有该 task 的持久化映射")?;
                Ok((peer, stored))
            })
            .await?;
            let peer = reconcile_a2a_peer_before_request(path, &peer).await?;
            let transport: Arc<dyn A2aTransport> = if psk_mode {
                if peer.snapshot.protocol_binding != "PSK" {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-mismatch",
                            "该 peer 不是 PSK transport",
                        )
                    );
                }
                Arc::new(PskTransport::new(&peer.snapshot, read_a2a_psk_secret()?)?)
            } else {
                let identity_ref = peer.identity_ref.clone().context(ui::explicit_text(
                    "cli-a2a-identity-missing",
                    "A2A peer 缺少本机 mTLS 身份",
                ))?;
                Arc::new(KeyringMtlsTransport::new(Arc::new(KeyringA2aIdentityStore), [(peer.snapshot.peer_id.clone(), identity_ref)].into_iter().collect()))
            };
            let task = A2aTaskClient::new(transport)
                .cancel_task(&peer.snapshot, &task_id)
                .await?;
            let stored_task = rivo_storage::StoredA2aTask {
                peer_id: peer.snapshot.peer_id,
                task_id: task.id.clone(),
                request_sha256: stored.request_sha256,
                state: task.state,
                revision: task.revision,
            };
            with_a2a_store(path, move |store| Ok(store.save_a2a_task(&stored_task)?)).await?;
            println!("task_id={} state={:?} revision={:?}", task.id.0, task.state, task.revision);
        }
        "recover" => {
            let (peer_id, task_id) = parse_a2a_recover_args(&args)?;
            let psk_mode = args.windows(2).any(|pair| pair[0] == "--transport" && pair[1] == "psk");
            let lookup_peer_id = peer_id.clone();
            let lookup_task_id = A2aTaskId(task_id);
            let (peer, stored) = with_a2a_store(path, move |store| {
                let peer = store
                    .a2a_peer(&A2aPeerId(lookup_peer_id))?
                    .context(ui::explicit_text(
                        "cli-a2a-peer-not-paired",
                        "A2A peer 未配对或不存在",
                    ))?;
                let stored = store
                    .a2a_task(&peer.snapshot.peer_id, &lookup_task_id)?
                    .context(ui::explicit_text(
                        "cli-a2a-task-not-found",
                        "A2A task 映射不存在；未知提交结果不得重新委派",
                    ))?;
                Ok((peer, stored))
            })
            .await?;
            let peer = reconcile_a2a_peer_before_request(path, &peer).await?;
            let transport: Arc<dyn A2aTransport> = if psk_mode {
                if peer.snapshot.protocol_binding != "PSK" {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-mismatch",
                            "该 peer 不是 PSK transport",
                        )
                    );
                }
                Arc::new(PskTransport::new(&peer.snapshot, read_a2a_psk_secret()?)?)
            } else {
                let identity_ref = peer.identity_ref.clone().context(ui::explicit_text(
                    "cli-a2a-identity-missing",
                    "A2A peer 缺少本机 mTLS 身份",
                ))?;
                Arc::new(KeyringMtlsTransport::new(Arc::new(KeyringA2aIdentityStore), [(peer.snapshot.peer_id.clone(), identity_ref)].into_iter().collect()))
            };
            let tasks = recover_a2a_task_to_storage(
                path,
                &peer,
                &stored,
                &A2aTaskClient::new(transport),
            )
            .await?;
            let last = tasks.last().context("A2A recover 未返回 task 状态")?;
            println!(
                "task_id={} state={:?} revision={:?} recovered_updates={}",
                last.task_id.0,
                last.state,
                last.revision,
                tasks.len()
            );
        }
        _ => anyhow::bail!("用法: rivo a2a discover [--timeout-seconds <1..=300>]|pair <local-identity-label> --endpoint <https-url> --spki-sha256 <sha256>|peers|inspect <peer-id>|revoke <peer-id> [--yes]|delegate <peer-id> <skill-id> --message-stdin --yes [--stream]|cancel <peer-id> <task-id> --yes|recover <peer-id> <task-id>|serve [<server-profile>] --config <owner-a2a-server.toml>|pairings list|approve|reject <request-id> --config <owner-a2a-server.toml>|approvals list|approve|reject <approval-id> --config <owner-a2a-server.toml>"),
    }
    Ok(())
}

fn print_a2a_help(args: &[String]) -> bool {
    let help_requested = args
        .last()
        .is_some_and(|value| matches!(value.as_str(), "--help" | "-h"));
    if !help_requested {
        return false;
    }
    let (id, help) = match args.first().map(String::as_str) {
        Some("--help" | "-h") => ("cli-a2a-help", "Manage A2A peers, delegation and the local A2A server.\n\nUsage: rivo a2a <COMMAND>\n\nCommands: discover, pair, peers, inspect, revoke, delegate, control, cancel, recover, serve, pairings, approvals, provision\n\nOptions: -h, --help  Show help"),
        Some("discover") => ("cli-a2a-discover-help", "Discover local A2A candidates.\n\nUsage:\n  rivo a2a discover [--timeout-seconds <1..=300>]"),
        Some("pair") => ("cli-a2a-pair-help", "Pair an A2A peer.\n\nUsage:\n  rivo a2a pair <LOCAL_IDENTITY_LABEL> --endpoint <HTTPS_URL> --spki-sha256 <SHA256> [--transport <mtls|psk>] [--secret-stdin]"),
        Some("peers") => ("cli-a2a-peers-help", "List paired A2A peers.\n\nUsage:\n  rivo a2a peers"),
        Some("inspect") => ("cli-a2a-inspect-help", "Show A2A peer details.\n\nUsage:\n  rivo a2a inspect <PEER_ID>"),
        Some("revoke") => ("cli-a2a-revoke-help", "Revoke an A2A peer.\n\nUsage:\n  rivo a2a revoke <PEER_ID> [--yes]"),
        Some("delegate") => ("cli-a2a-delegate-help", "Delegate a task to an A2A peer; the body is read only from stdin.\n\nUsage:\n  rivo a2a delegate <PEER_ID> <SKILL_ID> --message-stdin --yes [--stream] [--transport psk --secret-stdin]"),
        Some("control") => ("cli-a2a-control-help", "Call an approved instance-control operation on a peer.\n\nUsage:\n  rivo a2a control <PEER_ID> <TARGET_INSTANCE_ID> <METHOD> --params-stdin [--yes]"),
        Some("cancel") => ("cli-a2a-cancel-help", "Cancel a remote A2A task.\n\nUsage:\n  rivo a2a cancel <PEER_ID> <TASK_ID> --yes [--transport psk --secret-stdin]"),
        Some("recover") => ("cli-a2a-recover-help", "Recover a remote A2A task.\n\nUsage:\n  rivo a2a recover <PEER_ID> <TASK_ID> [--transport psk --secret-stdin]"),
        Some("serve") => ("cli-a2a-serve-help", "Start the local A2A server.\n\nUsage:\n  rivo a2a serve [<SERVER_PROFILE>] --config <OWNER_A2A_SERVER_TOML>"),
        Some("pairings") => ("cli-a2a-pairings-help", "Manage local A2A pairing requests.\n\nUsage:\n  rivo a2a pairings list --config <OWNER_A2A_SERVER_TOML>\n  rivo a2a pairings <approve|reject> <REQUEST_ID> --config <OWNER_A2A_SERVER_TOML>"),
        Some("approvals") => ("cli-a2a-approvals-help", "Manage local A2A approvals.\n\nUsage:\n  rivo a2a approvals list --config <OWNER_A2A_SERVER_TOML>\n  rivo a2a approvals <approve|reject> <APPROVAL_ID> --config <OWNER_A2A_SERVER_TOML>"),
        Some("provision") => ("cli-a2a-provision-help", "Manage local A2A test identity material.\n\nUsage:\n  rivo a2a provision --yes --bind <PRIVATE_IP:PORT> --server-identity-ref <REF> --issuer-ref <REF>\n  rivo a2a provision <check|cleanup> --server-identity-ref <REF> --issuer-ref <REF> [--yes]"),
        _ => return false,
    };
    println!("{}", ui::text(id, help));
    true
}

/// 将恢复快照及 SSE 的每个单调更新写回本机 SQLite。它绝不调用
/// `send_delegation`：持久映射是查询远端 task 的唯一授权依据。
async fn reconcile_a2a_peer_before_request(
    path: &std::path::Path,
    peer: &rivo_storage::StoredA2aPeer,
) -> anyhow::Result<rivo_storage::StoredA2aPeer> {
    let card =
        match fetch_agent_card(&peer.snapshot.interface_url, &peer.snapshot.spki_sha256).await {
            Ok(card) => card,
            Err(error) => {
                let peer_id = peer.snapshot.peer_id.clone();
                with_a2a_store(path, move |store| Ok(store.suspend_a2a_peer(&peer_id)?)).await?;
                return Err(anyhow::anyhow!("a2a_peer_suspended: {error}"));
            }
        };
    let peer_id = peer.snapshot.peer_id.clone();
    let protocol_binding = peer.snapshot.protocol_binding.clone();
    let tenant = peer.snapshot.tenant.clone();
    let spki_sha256 = peer.snapshot.spki_sha256.clone();
    let reconciled = with_a2a_store(path, move |store| {
        store
            .reconcile_a2a_peer(
                &peer_id,
                &card.url,
                // Reconciliation verifies the card and TLS pin, but must not rewrite
                // an explicitly paired PSK transport into the mTLS binding.
                &protocol_binding,
                tenant,
                &spki_sha256,
                &card.id,
                &card.canonical_sha256,
                card.skills,
                SPEC_REVISION,
                WIRE_PROTOCOL_VERSION,
            )
            .map_err(Into::into)
    })
    .await?;
    if reconciled.snapshot.status != A2aTrustState::Trusted {
        return Err(anyhow::anyhow!("a2a_agent_card_changed"));
    }
    Ok(reconciled)
}

async fn recover_a2a_task_to_storage(
    path: &std::path::Path,
    peer: &rivo_storage::StoredA2aPeer,
    stored: &rivo_storage::StoredA2aTask,
    client: &A2aTaskClient,
) -> anyhow::Result<Vec<rivo_storage::StoredA2aTask>> {
    if stored.peer_id != peer.snapshot.peer_id {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-task-peer-mismatch", "A2A task 映射 peer 不匹配",)
        );
    }
    if stored.state.terminal() {
        return Ok(vec![stored.clone()]);
    }
    let updates = client
        .recover_task(&peer.snapshot, &stored.task_id)
        .await
        .context(ui::explicit_text(
            "cli-a2a-recover-failed",
            "A2A task 恢复失败；未重发业务委派",
        ))?;
    let mut saved = Vec::with_capacity(updates.len());
    for task in updates {
        // A stale GetTask snapshot may race an already-persisted SSE update. Keep the
        // durable revision as the authority, but continue consuming later SSE events
        // instead of treating that harmless replay as a reason to resend business work.
        if matches!((stored.revision, task.revision), (Some(previous), Some(next)) if next < previous)
        {
            continue;
        }
        let next = rivo_storage::StoredA2aTask {
            peer_id: stored.peer_id.clone(),
            task_id: task.id,
            request_sha256: stored.request_sha256.clone(),
            state: task.state,
            revision: task.revision,
        };
        let saved_task = with_a2a_store(path, move |store| {
            store.save_a2a_task(&next)?;
            Ok(next)
        })
        .await?;
        saved.push(saved_task);
    }
    Ok(saved)
}

fn server_state_path(config_path: &std::path::Path, config: &A2aServerFileConfig) -> PathBuf {
    if config.state_db.is_absolute() {
        config.state_db.clone()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(&config.state_db)
    }
}

fn parse_a2a_serve_args(args: &[String]) -> anyhow::Result<(PathBuf, Option<String>)> {
    if args.first().map(String::as_str) != Some("serve") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-serve-usage",
                "用法: rivo a2a serve [<server-profile>] --config <owner-a2a-server.toml>",
            )
        );
    }
    let mut profile = None;
    let mut config = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                config = args.get(index).filter(|value| !value.is_empty()).cloned();
            }
            "--transport" => {
                index += 1;
                if !matches!(
                    args.get(index).map(String::as_str),
                    Some("mTLS" | "mtls" | "psk")
                ) {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-invalid",
                            "A2A transport 必须是 mtls 或 psk"
                        )
                    );
                }
            }
            "--secret-stdin" => {}
            value if !value.starts_with('-') && profile.is_none() => {
                profile = Some(value.to_string())
            }
            _ => anyhow::bail!(
                "{}",
                ui::explicit_text("cli-a2a-server-args-invalid", "A2A Server 参数无效")
            ),
        }
        index += 1;
    }
    let config = config.context(ui::explicit_text(
        "cli-a2a-config-missing",
        "A2A Server 配置路径不能为空",
    ))?;
    Ok((PathBuf::from(config), profile))
}

fn parse_a2a_psk_flags(args: &[String]) -> anyhow::Result<bool> {
    let transport = args
        .windows(2)
        .find(|pair| pair[0] == "--transport")
        .map(|pair| pair[1].as_str());
    let secret_stdin = args.iter().any(|value| value == "--secret-stdin");
    match transport {
        Some("psk") if secret_stdin => Ok(true),
        Some("psk") => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-psk-secret-required",
                "PSK transport 必须使用 --secret-stdin"
            )
        ),
        Some("mtls" | "mTLS") if secret_stdin => {
            anyhow::bail!(
                "{}",
                ui::explicit_text(
                    "cli-a2a-mtls-secret-forbidden",
                    "mTLS transport 不接受 --secret-stdin"
                )
            )
        }
        Some("mtls" | "mTLS") | None if !secret_stdin => Ok(false),
        Some(_) => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-transport-invalid",
                "A2A transport 必须是 mtls 或 psk"
            )
        ),
        None => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-psk-transport-required",
                "--secret-stdin 必须与 --transport psk 一起使用"
            )
        ),
    }
}

fn read_a2a_psk_secret() -> anyhow::Result<rivo_a2a::psk::PskSecret> {
    let mut encoded = String::new();
    std::io::stdin().read_to_string(&mut encoded)?;
    let bytes = hex::decode(encoded.trim()).context(ui::explicit_text(
        "cli-a2a-secret-invalid",
        "PSK secret 必须是 64 个 hex 字符",
    ))?;
    let secret: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!(ui::explicit_text(
            "cli-a2a-secret-length-invalid",
            "PSK secret 必须正好是 32 bytes"
        ))
    })?;
    if secret.iter().all(|byte| *byte == 0) {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-secret-zero", "PSK secret 不能全为零")
        );
    }
    Ok(zeroize::Zeroizing::new(secret))
}

fn parse_a2a_pairings_args(args: &[String]) -> anyhow::Result<(String, Option<String>, PathBuf)> {
    match args {
        [command, operation, flag, path]
            if command == "pairings" && operation == "list" && flag == "--config" && !path.is_empty() =>
                Ok((operation.clone(), None, PathBuf::from(path))),
        [command, operation, request_id, flag, path]
            if command == "pairings" && matches!(operation.as_str(), "approve" | "reject" | "revoke")
                && !request_id.is_empty() && request_id.len() <= 256 && flag == "--config" && !path.is_empty() =>
                Ok((operation.clone(), Some(request_id.clone()), PathBuf::from(path))),
        _ => anyhow::bail!("{}", ui::explicit_text("cli-a2a-pairings-usage", "用法: rivo a2a pairings list --config <owner-a2a-server.toml>|pairings approve|reject <request-id>|revoke <peer-id> --config <owner-a2a-server.toml>")),
    }
}

fn parse_a2a_approvals_args(args: &[String]) -> anyhow::Result<(String, Option<String>, PathBuf)> {
    match args {
        [command, operation, flag, path]
            if command == "approvals" && operation == "list" && flag == "--config" && !path.is_empty() =>
                Ok((operation.clone(), None, PathBuf::from(path))),
        [command, operation, approval_id, flag, path]
            if command == "approvals" && matches!(operation.as_str(), "approve" | "reject")
                && approval_id.len() == 73 && approval_id.starts_with("approval-")
                && approval_id[9..].bytes().all(|byte| byte.is_ascii_hexdigit())
                && flag == "--config" && !path.is_empty() =>
                Ok((operation.clone(), Some(approval_id.clone()), PathBuf::from(path))),
        _ => anyhow::bail!("{}", ui::explicit_text("cli-a2a-approvals-usage", "用法: rivo a2a approvals list --config <owner-a2a-server.toml>|approvals approve|reject <approval-id> --config <owner-a2a-server.toml>")),
    }
}

fn ensure_local_a2a_owner_terminal() -> anyhow::Result<()> {
    if local_a2a_test_confirmation_enabled(
        std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1"),
        std::env::var("RIVO_A2A_LOCAL_TEST_CONFIRM").as_deref() == Ok("1"),
    ) {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-owner-terminal-required",
                "A2A 审批只能在 Server 所在本机的交互式 owner 用户会话执行"
            )
        );
    }
    Ok(())
}

fn confirm_a2a_pairing_decision(operation: &str, request_id: &str) -> anyhow::Result<()> {
    ensure_local_a2a_owner_terminal()?;
    if local_a2a_test_confirmation_enabled(
        std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1"),
        std::env::var("RIVO_A2A_LOCAL_TEST_CONFIRM").as_deref() == Ok("1"),
    ) {
        return Ok(());
    }
    eprint!("确认本机 {operation} 配对请求 {request_id}？输入 yes: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-pairing-cancelled", "已取消 A2A 配对裁决")
        );
    }
    Ok(())
}

fn confirm_a2a_approval_decision(operation: &str, approval_id: &str) -> anyhow::Result<()> {
    eprint!("确认本机 {operation} 一次性审批 {approval_id}？输入 yes: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-approval-cancelled", "已取消 A2A 本机审批裁决")
        );
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_a2a_delegate_args(args: &[String]) -> anyhow::Result<(String, String, bool)> {
    let (peer_id, skill_id, flags) = match args {
        [command, peer_id, skill_id, flags @ ..] if command == "delegate" => {
            (peer_id, skill_id, flags)
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-delegate-usage",
                "用法: rivo a2a delegate <peer-id> <skill-id> --message-stdin --yes [--stream]",
            )
        ),
    };
    if peer_id.is_empty()
        || skill_id.is_empty()
        || peer_id.len() > 96
        || skill_id.len() > 128
        || !peer_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-peer-skill-invalid", "A2A peer_id 或 skill_id 无效")
        );
    }
    let mut seen = BTreeSet::<String>::new();
    let mut stream = false;
    let mut index = 0;
    while index < flags.len() {
        let flag = &flags[index];
        if !seen.insert(flag.clone()) {
            anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-a2a-duplicate-flag",
                    &format!("重复的 A2A delegate 参数: {flag}"),
                    &[("flag", flag.clone())]
                )
            );
        }
        match flag.as_str() {
            "--message-stdin" | "--yes" => {}
            "--stream" => stream = true,
            "--secret-stdin" => {}
            "--transport" => {
                index += 1;
                if !matches!(
                    flags.get(index).map(|value| value.as_str()),
                    Some("psk" | "mtls" | "mTLS")
                ) {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-invalid",
                            "A2A transport 必须是 mtls 或 psk"
                        )
                    );
                }
            }
            _ => anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-a2a-unknown-flag",
                    &format!("未知的 A2A delegate 参数: {flag}"),
                    &[("flag", flag.clone())]
                )
            ),
        }
        index += 1;
    }
    if !seen.contains("--message-stdin") || !seen.contains("--yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-delegate-confirm-required",
                "A2A 委派必须使用 --message-stdin 从标准输入读取正文，并以 --yes 显式确认数据外发"
            )
        );
    }
    Ok((peer_id.clone(), skill_id.clone(), stream))
}

fn parse_a2a_control_args(args: &[String]) -> anyhow::Result<(String, String, String, bool)> {
    let (peer_id, target_instance_id, method, flags) = match args {
        [command, peer_id, target_instance_id, method, flags @ ..] if command == "control" => {
            (peer_id, target_instance_id, method, flags)
        }
        _ => anyhow::bail!("{}", ui::explicit_text("cli-a2a-control-usage", "用法: rivo a2a control <peer-id> <target-instance-id> <method> --params-stdin [--yes]")),
    };
    let valid_id = |value: &str, max: usize| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !valid_id(peer_id, 96) || !valid_id(target_instance_id, 96) || method.len() > 64 {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-control-id-invalid",
                "A2A control 标识或 method 无效"
            )
        );
    }
    let allowed = SkillRegistry::load_defaults();
    let skill = allowed.get("process-control").unwrap();
    if !skill
        .read_operations
        .iter()
        .chain(skill.mutating_operations.iter())
        .any(|operation| operation == method)
    {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-control-method-forbidden",
                "process-control 不允许该 method"
            )
        );
    }
    let mut params_stdin = false;
    let mut confirmation = false;
    let mut seen = BTreeSet::<String>::new();
    let mut index = 0;
    while index < flags.len() {
        let flag = &flags[index];
        if !seen.insert(flag.clone()) {
            anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-a2a-duplicate-flag",
                    &format!("重复的 A2A control 参数: {flag}"),
                    &[("flag", flag.clone())]
                )
            );
        }
        match flag.as_str() {
            "--params-stdin" => params_stdin = true,
            "--yes" => confirmation = true,
            "--secret-stdin" => {}
            "--transport" => {
                index += 1;
                if !matches!(
                    flags.get(index).map(|value| value.as_str()),
                    Some("psk" | "mtls" | "mTLS")
                ) {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-invalid",
                            "A2A transport 必须是 mtls 或 psk"
                        )
                    );
                }
            }
            _ => anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-a2a-unknown-flag",
                    &format!("未知的 A2A control 参数: {flag}"),
                    &[("flag", flag.clone())]
                )
            ),
        }
        index += 1;
    }
    if !params_stdin {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-control-params-required",
                "A2A control 必须使用 --params-stdin"
            )
        );
    }
    if skill
        .mutating_operations
        .iter()
        .any(|operation| operation == method)
        && !confirmation
    {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-control-confirm-required",
                "变更型 A2A control 必须使用 --yes"
            )
        );
    }
    Ok((
        peer_id.clone(),
        target_instance_id.clone(),
        method.clone(),
        confirmation,
    ))
}

fn parse_a2a_recover_args(args: &[String]) -> anyhow::Result<(String, String)> {
    if args.len() < 3 || args.first().map(String::as_str) != Some("recover") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-recover-usage",
                "用法: rivo a2a recover <peer-id> <task-id> [--transport psk --secret-stdin]"
            )
        );
    }
    let valid = |value: &str, max: usize| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !valid(&args[1], 96) || !valid(&args[2], 128) {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-peer-task-invalid", "A2A peer_id 或 task_id 无效")
        );
    }
    let _ = parse_a2a_psk_flags(args)?;
    Ok((args[1].clone(), args[2].clone()))
}

fn parse_a2a_cancel_args(args: &[String]) -> anyhow::Result<(String, String)> {
    if args.len() < 4 || args.first().map(String::as_str) != Some("cancel") || args[3] != "--yes" {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-cancel-usage",
                "用法: rivo a2a cancel <peer-id> <task-id> --yes [--transport psk --secret-stdin]"
            )
        );
    }
    let valid = |value: &str, max: usize| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !valid(&args[1], 96) || !valid(&args[2], 128) {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-peer-task-invalid", "A2A peer_id 或 task_id 无效")
        );
    }
    let _ = parse_a2a_psk_flags(args)?;
    Ok((args[1].clone(), args[2].clone()))
}

fn parse_a2a_pair_args(args: &[String]) -> anyhow::Result<(String, String, String)> {
    if args.first().map(String::as_str) != Some("pair") {
        anyhow::bail!("{}", ui::explicit_text("cli-a2a-pair-usage", "用法: rivo a2a pair <local-identity-label> --endpoint <https-url> --spki-sha256 <sha256>"));
    }
    let peer_id = args
        .get(1)
        .filter(|value| !value.is_empty())
        .cloned()
        .context(ui::explicit_text(
            "cli-a2a-local-identity-missing",
            "本机 identity label 不能为空",
        ))?;
    if peer_id.len() > 96
        || !peer_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-identity-invalid", "A2A identity label 无效")
        );
    }
    let mut endpoint = None;
    let mut spki = None;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--endpoint" => {
                index += 1;
                endpoint = args.get(index).filter(|value| !value.is_empty()).cloned();
            }
            "--spki-sha256" => {
                index += 1;
                spki = args.get(index).filter(|value| !value.is_empty()).cloned();
            }
            "--transport" => {
                index += 1;
                if !matches!(
                    args.get(index).map(String::as_str),
                    Some("mtls" | "mTLS" | "psk")
                ) {
                    anyhow::bail!(
                        "{}",
                        ui::explicit_text(
                            "cli-a2a-transport-invalid",
                            "A2A transport 必须是 mtls 或 psk"
                        )
                    );
                }
            }
            "--secret-stdin" => {}
            _ => anyhow::bail!(
                "{}",
                ui::explicit_text("cli-a2a-pair-args-invalid", "A2A pair 参数无效")
            ),
        }
        index += 1;
    }
    Ok((
        peer_id,
        endpoint.context(ui::explicit_text(
            "cli-a2a-endpoint-missing",
            "A2A endpoint 不能为空",
        ))?,
        spki.context(ui::explicit_text(
            "cli-a2a-spki-missing",
            "A2A SPKI 不能为空",
        ))?,
    ))
}

fn confirm_a2a_sas() -> anyhow::Result<()> {
    if local_a2a_test_confirmation_enabled(
        std::env::var("RIVO_A2A_LOCAL_TEST").as_deref() == Ok("1"),
        std::env::var("RIVO_A2A_LOCAL_TEST_CONFIRM").as_deref() == Ok("1"),
    ) {
        if let Some(milliseconds) = std::env::var("RIVO_A2A_LOCAL_TEST_CONFIRM_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|milliseconds| *milliseconds <= 10_000)
        {
            std::thread::sleep(std::time::Duration::from_millis(milliseconds));
        }
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-a2a-sas-terminal-required",
                "A2A 配对需要本机交互式 SAS 确认"
            )
        );
    }
    eprint!("确认对端显示相同六词 SAS？输入 yes 领取证书: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-a2a-pair-cancelled", "已取消 A2A 配对")
        );
    }
    Ok(())
}

/// Automated confirmation exists only for the loopback-only integration
/// runner. Normal interactive pairings and owner decisions always retain their
/// terminal confirmation requirement.
fn local_a2a_test_confirmation_enabled(local_test: bool, auto_confirm: bool) -> bool {
    local_test && auto_confirm
}

fn utc_now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // RFC3339 格式由 unix 时间转换：此时间仅作批准审计字段，不参与安全决策。
    time::OffsetDateTime::from_unix_timestamp(seconds as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn parse_a2a_discovery_timeout(args: &[String]) -> anyhow::Result<u64> {
    match args {
        [command] if command == "discover" => Ok(5),
        [command, flag, value] if command == "discover" && flag == "--timeout-seconds" => {
            let timeout = value
                .parse::<u64>()
                .context("A2A discovery timeout 必须是整数秒")?;
            if !(1..=MAX_MDNS_TTL_SECONDS).contains(&timeout) {
                anyhow::bail!("A2A discovery timeout 必须在 1..={MAX_MDNS_TTL_SECONDS} 秒内");
            }
            Ok(timeout)
        }
        _ => anyhow::bail!(
            "用法: rivo a2a discover [--timeout-seconds <1..={MAX_MDNS_TTL_SECONDS}>]"
        ),
    }
}

fn revoke_a2a_peer_with_identity(
    store: &rivo_storage::Store,
    peer_id: &A2aPeerId,
    identities: &dyn A2aIdentityStore,
) -> anyhow::Result<()> {
    let peer = store.a2a_peer(peer_id)?.context("A2A peer 不存在")?;
    // 先撤销持久 grant 与活动 task，确保即使 keychain 清理因 OS 故障失败，
    // 本机也不会再向该 peer 发起调用。之后才尝试删除本机身份材料。
    store.revoke_a2a_peer(peer_id)?;
    if let Some(identity_ref) = peer.identity_ref {
        identities.delete(&identity_ref).map_err(|error| {
            anyhow::anyhow!(
                "peer 已撤销，但 OS keychain 身份清理失败；请在本机修复后重试 cleanup: {error}"
            )
        })?;
    }
    Ok(())
}

/// Destructive history operations must be acknowledged independently from any
/// task approval policy. `--yes` applies only to this one command; it never
/// authorizes tools or future commands.
fn confirm_history_deletion(action: &str, force: bool) -> anyhow::Result<()> {
    confirm_history_deletion_with_terminal(
        action,
        force,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    )
}

fn confirm_history_deletion_with_terminal(
    action: &str,
    force: bool,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
) -> anyhow::Result<()> {
    if force {
        return Ok(());
    }
    if !stdin_is_terminal || !stderr_is_terminal {
        anyhow::bail!("{action} 需要交互确认；自动化调用请只为本命令显式传入 --yes");
    }
    eprint!("确认{action}？此操作不可恢复。输入 yes 继续: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("已取消{action}");
    }
    Ok(())
}

fn telemetry_directory() -> Option<PathBuf> {
    rivo_session::data_root_dir().map(|root| root.join("rivo").join("logs"))
}

fn init_telemetry(
    config: &ResolvedConfig,
    command: &str,
    startup_args: &serde_json::Value,
) -> anyhow::Result<()> {
    if let Some(directory) = telemetry_directory() {
        let policy = rivo_telemetry::LogPolicy {
            retention_days: config.logging.retention_days,
            required: config.logging.required,
        };
        let logger = rivo_telemetry::FileLogger::open(directory, policy)?;
        logger.record_at_with_keys(
            std::time::SystemTime::now(),
            &serde_json::json!({
                "event":"cli_start",
                "version":env!("RIVO_RELEASE_VERSION"),
                "command": command,
                "arguments": startup_args,
                "config": rivo_session::redacted_config(config),
                "storage": rivo_storage::inspect_database(history_database_path()?),
                "terminal": terminal_runtime::detect_terminal_capabilities(),
            }),
            &config.known_api_keys(),
        )?;
    }
    Ok(())
}

fn record_startup_failure(
    config: &ResolvedConfig,
    command: &str,
    error: &rivo_session::SessionError,
) -> anyhow::Result<()> {
    if let Some(directory) = telemetry_directory() {
        let logger = rivo_telemetry::FileLogger::open(
            directory,
            rivo_telemetry::LogPolicy {
                retention_days: config.logging.retention_days,
                required: config.logging.required,
            },
        )?;
        logger.record_at_with_keys(
            std::time::SystemTime::now(),
            &serde_json::json!({
                "event":"startup_failure",
                "command": command,
                "error": error.to_string(),
                "storage": rivo_storage::inspect_database(history_database_path()?),
            }),
            &config.known_api_keys(),
        )?;
    }
    Ok(())
}

fn session_startup_error(error: rivo_session::SessionError) -> anyhow::Error {
    if let rivo_session::SessionError::Storage(rivo_storage::StorageError::UnsupportedSchema(
        version,
    )) = &error
    {
        let path = history_database_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        let logs = telemetry_directory()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unavailable>".into());
        let recovery = if *version == 17 {
            "执行 `rivo sessions migrate --yes` 自动创建并保留备份；也可先执行 `rivo sessions backup <target>`，再执行 `rivo sessions migrate --backup <target> --yes`"
        } else if *version == 14 {
            "先执行 `rivo sessions backup <target>`，再执行 `rivo sessions reset --backup <target> --yes`"
        } else {
            "先执行 `rivo sessions backup <target>`，再根据兼容手册选择受控恢复操作"
        };
        return anyhow::anyhow!(
            "不支持的 SQLite schema 版本: {version}（当前要求 {}）。数据库: {path}。日志: {logs}。{recovery}；不会自动迁移或清空。",
            rivo_storage::SCHEMA_VERSION
        );
    }
    error.into()
}

/*
fn run_diagnostics_command(args: Vec<String>) -> anyhow::Result<()> {
    if matches!(args.as_slice(), [help] | [_, help] if matches!(help.as_str(), "--help" | "-h")) {
        println!("创建脱敏诊断包。\n\nUsage: rivo diagnostics create <PATH> [--yes]\n\nArguments:\n  <PATH>  诊断包输出路径\n\nOptions:\n      --yes  跳过本命令的交互确认\n  -h, --help  显示帮助\n\nExamples:\n  rivo diagnostics create .\\rivo-diagnostics.zip --yes");
        return Ok(());
    }
    if args.first().map(String::as_str) != Some("create") {
        anyhow::bail!("用法: rivo diagnostics create <path> [--yes]");
    }
    let destination = PathBuf::from(
        args.get(1)
            .context("用法: rivo diagnostics create <path> [--yes]")?,
    );
    if args.len() > 3 || args.get(2).is_some_and(|value| value != "--yes") {
        anyhow::bail!("用法: rivo diagnostics create <path> [--yes]");
    }
    if args.get(2).map(String::as_str) != Some("--yes") {
        eprint!("诊断包可能包含脱敏后的用户业务内容。继续导出？ [y/N] ");
        std::io::Write::flush(&mut std::io::stderr())?;
        let mut confirmation = String::new();
        std::io::stdin().read_line(&mut confirmation)?;
        if !matches!(
            confirmation.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ) {
            anyhow::bail!("已取消诊断导出；自动化调用请显式传入 --yes");
        }
    }
    let config = ResolvedConfig::load(ConfigOverrides::default())?;
    let log_files = telemetry_directory()
        .and_then(|directory| std::fs::read_dir(directory).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    let summary = serde_json::to_value(redacted_config(&config))?;
    let history_path = history_database_path()?;
    // A diagnostic export must also work on a clean installation.  Do not
    // create an empty SQLite database merely to report that no history exists.
    let journal = if history_path.is_file() {
        rivo_storage::Store::open(history_path)?.full_diagnostic_journal()?
    } else {
        serde_json::json!({
            "sessions": [],
            "ui_audit_events": [],
            "diagnostics": [],
        })
    };
    let api_keys = config.known_api_keys();
    rivo_telemetry::write_complete_diagnostic_bundle(
        &destination,
        env!("RIVO_RELEASE_VERSION"),
        std::env::consts::OS,
        &summary,
        &log_files,
        &journal,
        &api_keys,
    )?;
    println!("created {}", destination.display());
    Ok(())
}
*/

async fn run_tui(workspace: Option<PathBuf>) -> anyhow::Result<()> {
    let capabilities = terminal_runtime::detect_terminal_capabilities();
    require_interactive_terminal(
        capabilities.interactive_input,
        capabilities.interactive_output,
        &ui::explicit_text(
            "cli-tui-terminal-required",
            "rivo tui 需要交互式终端；请在本地终端直接运行，自动化请使用 `rivo run --format jsonl`",
        ),
    )?;
    let startup_args = serde_json::json!({"workspace_override": workspace.clone()});
    let (config, selection) = run_tui_startup_configuration(workspace).await?;
    init_telemetry(&config, "tui", &startup_args)?;
    run_tui_workspace(config, selection).await
}

fn require_interactive_terminal(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    message: &str,
) -> anyhow::Result<()> {
    if !stdin_is_terminal || !stdout_is_terminal {
        anyhow::bail!(message.to_owned());
    }
    Ok(())
}

fn load_tui_startup_file(path: &std::path::Path) -> anyhow::Result<FileConfig> {
    upgrade_file_config(path)?;
    Ok(read_model_routing_draft(path)?.config)
}

async fn run_tui_startup_configuration(
    workspace: Option<PathBuf>,
) -> anyhow::Result<(ResolvedConfig, rivo_tui::StartupSelection)> {
    use crossterm::event::{Event, KeyEventKind};
    use ratatui::{backend::CrosstermBackend, Terminal};

    let path = std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(config_file_path)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let mut file = load_tui_startup_file(&path)?;
    if file.providers.is_empty() {
        let first_launch_workspace = workspace
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .context(ui::explicit_text(
                "cli-tui-current-directory-missing",
                "无法确定当前工作目录",
            ))?;
        run_tui_first_launch_setup(&path, &mut file, first_launch_workspace).await?;
    }
    let mut state = tui_startup_state(&file)?;
    install_tui_panic_hook();
    let mut runtime = terminal_runtime::CrosstermRuntime::new();
    runtime.enter()?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    loop {
        terminal.draw(|frame| rivo_tui::render(frame, &state))?;
        let Some(Event::Key(key)) = runtime.read_event(std::time::Duration::from_millis(100))?
        else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match handle_startup_key(&mut state, key) {
            StartupKeyAction::Continue => {}
            StartupKeyAction::Exit => anyhow::bail!(
                "{}",
                ui::text("cli-tui-startup-cancelled", "TUI startup cancelled")
            ),
            StartupKeyAction::Submit => {
                let startup = state.startup.as_mut().expect("startup state is present");
                match startup.submit() {
                    Ok(selection) => {
                        match commit_startup_defaults(&mut file, &selection, workspace.clone()) {
                            Ok(config) => return Ok((config, selection)),
                            Err(error) => {
                                startup.notice = ui::text_with(
                                    "cli-tui-startup-save-failed",
                                    &format!("无法保存启动配置: {error}"),
                                    &[("error", error.to_string())],
                                )
                            }
                        }
                    }
                    Err(error) => startup.notice = error,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupKeyAction {
    Continue,
    Submit,
    Exit,
}

fn handle_startup_key(
    state: &mut rivo_tui::AppState,
    key: crossterm::event::KeyEvent,
) -> StartupKeyAction {
    use crossterm::event::KeyCode;
    if is_global_interrupt(&key) {
        return StartupKeyAction::Exit;
    }
    let startup = state.startup.as_mut().expect("startup state is present");
    match key.code {
        KeyCode::Up => startup.move_selection(-1),
        KeyCode::Down => startup.move_selection(1),
        KeyCode::Left => {
            startup.change_current(false);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if startup.change_current(true) {
                return StartupKeyAction::Submit;
            }
        }
        KeyCode::Enter => {
            if startup.selected_row + 1 == startup.row_count() {
                return StartupKeyAction::Submit;
            }
            startup.change_current(true);
        }
        KeyCode::Esc | KeyCode::Char('q') => return StartupKeyAction::Exit,
        _ => {}
    }
    StartupKeyAction::Continue
}

fn tui_startup_state(file: &FileConfig) -> anyhow::Result<rivo_tui::AppState> {
    let enabled_mcp_servers = file
        .mcp
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    let startup = rivo_tui::StartupConfigState::new(
        file.clone(),
        enabled_mcp_servers,
        None,
        TurnOptions {
            capability_profile: file.defaults.capability.unwrap_or_default(),
            policy_profile: file.defaults.policy.unwrap_or_default(),
            locale: file.defaults.locale.clone(),
            reasoning_effort: Default::default(),
            network: file.defaults.network,
            browser: file.defaults.browser,
            computer: false,
            shell: false,
            mcp_servers: file.defaults.mcp_servers.clone(),
            mcp_toolsets: file.defaults.mcp_toolsets.clone(),
            memory_recall: file.defaults.memory_recall,
            trace: file.defaults.trace,
            subagents_enabled: file.defaults.subagents_enabled,
            max_subagents: file.defaults.max_subagents,
            a2a_peers: Default::default(),
        },
    )
    .map_err(anyhow::Error::msg)?;
    let mut state = rivo_tui::AppState {
        startup: Some(startup),
        ..rivo_tui::AppState::default()
    };
    state.normalize_history_selection();
    Ok(state)
}

#[derive(Debug, Clone)]
enum FirstLaunchCredential {
    Credential(String),
}

#[derive(Debug, Clone)]
struct FirstLaunchSetup {
    provider_id: String,
    base_url: String,
    workspace: PathBuf,
    credential: FirstLaunchCredential,
}

async fn run_tui_first_launch_setup(
    path: &std::path::Path,
    file: &mut FileConfig,
    workspace: PathBuf,
) -> anyhow::Result<()> {
    let capabilities = terminal_runtime::detect_terminal_capabilities();
    require_interactive_terminal(
        capabilities.interactive_input,
        capabilities.interactive_output,
        &ui::text(
            "cli-tui-first-launch-terminal-required",
            "No Provider is configured; run `rivo tui` in an interactive terminal or use `rivo config provider set ...`",
        ),
    )?;
    if !workspace.is_dir() {
        anyhow::bail!(
            "{}",
            ui::text_with(
                "cli-tui-first-launch-workspace-invalid",
                &format!("工作区不存在或不是目录: {}", workspace.display()),
                &[("path", workspace.display().to_string())],
            )
        );
    }
    let setup = first_launch_form(&workspace).await?;
    commit_first_launch_setup(path, file, &setup).await?;
    eprintln!(
        "{}",
        ui::text(
            "cli-tui-first-launch-provider-saved",
            "Provider saved and explicit discovery completed; choose a Vendor and Model next.",
        )
    );
    Ok(())
}

async fn first_launch_form(workspace: &std::path::Path) -> anyhow::Result<FirstLaunchSetup> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::{backend::CrosstermBackend, Terminal};
    let mut runtime = terminal_runtime::CrosstermRuntime::new();
    runtime.enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut values: [String; 4] = [
        "default".into(),
        "https://api.openai.com/v1".into(),
        "openai-api-key".into(),
        "no".into(),
    ];
    let labels = [
        ui::text("cli-tui-first-launch-provider-id", "Provider ID"),
        ui::text("cli-tui-first-launch-base-url", "Base URL"),
        ui::text(
            "cli-tui-first-launch-credential",
            "Existing Credential name",
        ),
        ui::text(
            "cli-tui-first-launch-discover",
            "Save and discover models now (yes/no)",
        ),
    ];
    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| {
            let area = frame.size();
            frame.render_widget(ratatui::widgets::Clear, area);
            let lines = std::iter::once(ui::text(
                "cli-tui-first-launch-title",
                "Rivo · First-launch setup",
            ))
            .chain(std::iter::once(ui::text_with(
                "cli-tui-first-launch-workspace",
                &format!("Workspace: {}", workspace.display()),
                &[("path", workspace.display().to_string())],
            )))
            .chain(labels.iter().enumerate().map(|(i, label)| {
                let value = values[i].clone();
                format!(
                    "{} {}: {}",
                    if i == selected { ">" } else { " " },
                    label,
                    value
                )
            }))
            .chain(std::iter::once("".into()))
            .chain(std::iter::once(ui::text(
                "cli-tui-first-launch-help",
                "Enter next/confirm final field · Tab/↑↓ switch · Esc cancel",
            )))
            .collect::<Vec<_>>();
            frame.render_widget(ratatui::widgets::Paragraph::new(lines.join("\n")), area);
        })?;
        let Some(Event::Key(key)) = runtime.read_event(std::time::Duration::from_millis(100))?
        else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            anyhow::bail!(
                "{}",
                ui::text(
                    "cli-tui-first-launch-cancelled",
                    "First-launch setup cancelled; configuration was not saved",
                )
            );
        }
        match key.code {
            KeyCode::Esc => anyhow::bail!(
                "{}",
                ui::text(
                    "cli-tui-first-launch-cancelled",
                    "First-launch setup cancelled; configuration was not saved",
                )
            ),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % labels.len(),
            KeyCode::BackTab => selected = selected.checked_sub(1).unwrap_or(labels.len() - 1),
            KeyCode::Backspace => {
                values[selected].pop();
            }
            KeyCode::Enter if selected == labels.len() - 1 => {
                if values[3] != "yes" {
                    anyhow::bail!(
                        "{}",
                        ui::text(
                            "cli-tui-first-launch-not-confirmed",
                            "Save and discovery were not confirmed; configuration was not saved",
                        )
                    );
                }
                return Ok(FirstLaunchSetup {
                    provider_id: values[0].clone(),
                    base_url: values[1].clone(),
                    workspace: workspace.to_path_buf(),
                    credential: FirstLaunchCredential::Credential(values[2].clone()),
                });
            }
            KeyCode::Enter => selected += 1,
            KeyCode::Char(ch) => values[selected].push(ch),
            _ => {}
        }
    }
}

async fn commit_first_launch_setup(
    path: &std::path::Path,
    file: &mut FileConfig,
    setup: &FirstLaunchSetup,
) -> anyhow::Result<()> {
    let draft = read_model_routing_draft(path)?;
    let candidate = build_first_launch_config(&draft.config, setup)?;
    let saved = commit_model_routing_draft(path, &draft.revision, &candidate)?;
    *file = saved.config;
    let refreshed = refresh_provider_models(path, &saved.revision, &setup.provider_id).await?;
    *file = refreshed.config;
    if file.providers[&setup.provider_id].discovery.supported != Some(true) {
        anyhow::bail!(
            "{}",
            ui::text(
                "cli-tui-first-launch-discovery-failed",
                "Provider saved, but model discovery failed; check the credential reference and run refresh explicitly",
            )
        );
    }
    Ok(())
}

#[cfg(test)]
fn normalize_first_launch_input(value: &str, default: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value == "\u{1b}" {
        anyhow::bail!(
            "{}",
            ui::text(
                "cli-tui-first-launch-cancelled",
                "First-launch setup cancelled; configuration was not saved",
            )
        );
    }
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn build_first_launch_config(
    file: &FileConfig,
    setup: &FirstLaunchSetup,
) -> anyhow::Result<FileConfig> {
    let mut candidate = file.clone();
    if candidate.providers.contains_key(&setup.provider_id) {
        anyhow::bail!(
            "{}",
            ui::text(
                "cli-tui-first-launch-provider-exists",
                "Provider already exists; update it through a controlled configuration change",
            )
        );
    }
    let auth = match &setup.credential {
        FirstLaunchCredential::Credential(name) => AuthConfig {
            credential: Some(name.clone()),
        },
    };
    candidate.providers.insert(
        setup.provider_id.clone(),
        RoutingProviderConfig {
            base_url: setup.base_url.clone(),
            auth,
            enabled: true,
            priority: 0,
            discovery: ModelDiscoveryCache::default(),
        },
    );
    candidate.defaults.workspace = Some(setup.workspace.clone());
    candidate.defaults.capability = Some(CapabilityProfile::General);
    candidate.defaults.policy = Some(PolicyProfile::Medium);
    validate_model_routing_config(&candidate)?;
    Ok(candidate)
}

fn commit_startup_defaults(
    file: &mut FileConfig,
    selection: &rivo_tui::StartupSelection,
    workspace: Option<PathBuf>,
) -> anyhow::Result<ResolvedConfig> {
    let path = std::env::var_os("RIVO_CONFIG")
        .map(PathBuf::from)
        .or_else(config_file_path)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    upgrade_file_config(&path)?;
    let draft = read_model_routing_draft(&path)?;
    let mut candidate = draft.config;
    apply_startup_defaults(&mut candidate, selection)?;
    let environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    let resolved = ResolvedConfig::resolve_at(
        candidate.clone(),
        &environment,
        startup_config_overrides(selection, workspace),
        path.to_path_buf(),
    )?;
    *file = commit_model_routing_draft(&path, &draft.revision, &candidate)?.config;
    Ok(resolved)
}

fn startup_config_overrides(
    selection: &rivo_tui::StartupSelection,
    workspace: Option<PathBuf>,
) -> ConfigOverrides {
    ConfigOverrides {
        vendor: Some(selection.selection.vendor.clone()),
        provider_id: selection
            .selection
            .provider
            .explicit_id()
            .map(str::to_owned),
        auto_provider: matches!(selection.selection.provider, ProviderSelection::Auto),
        model: Some(selection.selection.model.clone()),
        capability_profile: Some(selection.turn_options.capability_profile),
        policy_profile: Some(selection.turn_options.policy_profile),
        workspace,
        ..ConfigOverrides::default()
    }
}

fn apply_startup_defaults(
    file: &mut FileConfig,
    selection: &rivo_tui::StartupSelection,
) -> anyhow::Result<()> {
    resolve_available_route(
        file,
        Some(&selection.selection.vendor),
        Some(&selection.selection.model),
        selection.selection.provider.explicit_id(),
    )?;
    file.defaults.vendor = Some(selection.selection.vendor.clone());
    file.defaults.model = Some(selection.selection.model.clone());
    file.defaults.provider_id = selection
        .selection
        .provider
        .explicit_id()
        .map(str::to_owned);
    file.defaults.capability = Some(selection.turn_options.capability_profile);
    file.defaults.policy = Some(selection.turn_options.policy_profile);
    file.defaults.network = selection.turn_options.network;
    file.defaults.browser = selection.turn_options.browser;
    file.defaults.mcp_servers = selection.turn_options.mcp_servers.clone();
    file.defaults.mcp_toolsets = selection.turn_options.mcp_toolsets.clone();
    file.defaults.memory_recall = selection.turn_options.memory_recall;
    file.defaults.trace = selection.turn_options.trace;
    file.defaults.subagents_enabled = selection.turn_options.subagents_enabled;
    file.defaults.max_subagents = selection.turn_options.max_subagents;
    Ok(())
}

async fn run_tui_workspace(
    mut config: ResolvedConfig,
    selection: rivo_tui::StartupSelection,
) -> anyhow::Result<()> {
    use crossterm::event::{Event, KeyEventKind};
    use ratatui::{backend::CrosstermBackend, Terminal};
    let mut session = match Session::connect(config.clone()) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            record_startup_failure(&config, "tui", &error)?;
            return Err(session_startup_error(error));
        }
    };
    let (mut sender, mut events) = session.clone().channel();
    sender
        .send(Intent::SetTurnOptions {
            options: selection.turn_options.clone(),
        })
        .await
        .context("设置启动配置失败")?;
    install_tui_panic_hook();
    let mut runtime = terminal_runtime::CrosstermRuntime::new();
    runtime.enter()?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let workspace = session.workspace_async().await?;
    let history = session
        .history_scoped_async()
        .await?
        .into_iter()
        .map(|summary| rivo_tui::HistoryEntry {
            session_id: summary.session_id,
            created_at: summary.created_at,
            task_summary: summary.task_summary,
            expiration_reason: summary.expiration_reason,
            event_count: summary.event_count,
            message_count: summary.message_count,
            expired: summary.expired,
        })
        .collect();
    let available_a2a_peers = match history_database_path() {
        Ok(path) => with_a2a_store(&path, |store| {
            Ok(store
                .a2a_peers()?
                .into_iter()
                .filter(|peer| matches!(peer.snapshot.status, A2aTrustState::Trusted))
                .map(|peer| peer.snapshot.peer_id.0)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let available_mcp_toolsets = config
        .mcp_servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            (
                server.name.clone(),
                server.toolsets.keys().cloned().collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let sidecar_host_capabilities = host_capabilities();
    let mut state = rivo_tui::AppState {
        session_id: Some(session.session_id().to_owned()),
        workspace_name: workspace.as_ref().map(|value| value.display_name.clone()),
        workspace_root: Some(session.workspace_path().display().to_string()),
        sidecar_host_capabilities: Some(sidecar_host_capabilities),
        history,
        selection: Some(selection.selection.clone()),
        route: config.route.clone(),
        catalog_models: rivo_tui::catalog_models(&read_file_config(&config.config_path)?)
            .map_err(anyhow::Error::msg)?,
        available_mcp_servers: config
            .mcp_servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| server.name.clone())
            .collect(),
        available_a2a_peers,
        available_mcp_toolsets,
        model: Some(selection.selection.model.clone()),
        turn_options: selection.turn_options.clone(),
        ..rivo_tui::AppState::default()
    };
    let initial_locale = rivo_i18n::LocaleId::parse(&config.locale)
        .map_err(|error| anyhow::anyhow!("配置 locale 无效: {error}"))?;
    let mut language_runtime = TuiLanguageRuntime::from_environment_with_locale(initial_locale)
        .context("初始化 TUI 本地语言 catalog 失败")?;
    language_runtime.sync_state(&mut state);
    state.diagnostics = session
        .diagnostics_async(Some(session.session_id()), 128)
        .await?
        .into_iter()
        .map(|entry| rivo_tui::DiagnosticRecord {
            stage: entry.stage,
            code: entry.code,
            retryable: entry.retryable,
            recovery: "查看完整会话事件或重试".into(),
            message: entry.summary.clone(),
            cause: entry.summary,
        })
        .collect();
    let mut pending_events = rivo_tui::EventQueue::default();
    // Crossterm input must have exactly one reader. Creating a new blocking poll task in
    // every select iteration lets cancelled tasks consume keystrokes in the background.
    // Declared after the mode owner: every exit stops/joins input before mode restore.
    let mut input_reader = terminal_runtime::TerminalInputReader::new();
    let mut last_render = std::time::Instant::now();
    let mut needs_render = true;
    let mut immediate_render = true;
    loop {
        if needs_render
            && (immediate_render || last_render.elapsed() >= std::time::Duration::from_millis(33))
        {
            for event in pending_events.drain() {
                state.reduce(event);
            }
            terminal.draw(|frame| rivo_tui::render(frame, &state))?;
            last_render = std::time::Instant::now();
            needs_render = false;
            immediate_render = false;
        }
        tokio::select! {
            event = events.next() => {
                if let Some(event) = event {
                    pending_events.push(event.event);
                    if pending_events.has_urgent() {
                        for event in pending_events.drain() {
                            state.reduce(event);
                        }
                        immediate_render = true;
                    }
                    needs_render = true;
                } else {
                    state.notice = tui_text(&state, "error-session-stream-ended", "Session 事件流意外结束");
                    state.mode = Some(rivo_tui::UiMode::Error);
                    terminal.draw(|frame| rivo_tui::render(frame, &state))?;
                    break;
                }
            }
            input_event = input_reader.recv() => {
                let input_event = input_event?;
                if matches!(&input_event, Event::Resize(_, _)) {
                    needs_render = true;
                    immediate_render = true;
                }
                if let Event::Paste(text) = input_event {
                    match state.active_overlay() {
                        Some(rivo_tui::ActiveOverlay::CommandPalette) => {
                            state.push_command_input(&text);
                        }
                        None if state.focus == rivo_tui::FocusTarget::Input => {
                            state.push_input(&text);
                        }
                        _ => {}
                    }
                    needs_render = true;
                    immediate_render = true;
                    continue;
                }
                if let Event::Key(key) = input_event {
                    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) { continue; }
                    // The event stream and terminal input are selected concurrently. A
                    // confirmation frame may already be visible while its durable event
                    // is still queued; apply queued events before interpreting the key so
                    // a fast `n`/`y` cannot be consumed against stale overlay state.
                    for event in pending_events.drain() {
                        state.reduce(event);
                    }
                    if let Some(settings) = state.session_settings.as_mut() {
                        let columns = match terminal.size()?.width {
                            120.. => 4,
                            80..=119 => 2,
                            _ => 1,
                        };
                        settings.set_layout_columns(columns);
                    }
                    needs_render = true;
                    immediate_render = true;
                    match handle_tui_shortcut_with_runtime(
                        &mut state,
                        &sender,
                        Some(session.as_ref()),
                        Some(&mut language_runtime),
                        key,
                    )
                    .await {
                        Some(TuiKeyAction::Continue) | None => continue,
                        Some(TuiKeyAction::Exit) => break,
                        Some(TuiKeyAction::Rebind(TuiControl::Exit)) => break,
                        Some(TuiKeyAction::Rebind(control)) => {
                            rebind_tui_session(
                                control,
                                &mut config,
                                &mut session,
                                &mut sender,
                                &mut events,
                                &mut state,
                            )
                            .await?;
                            continue;
                        }
                    }
                }
            }
        }
    }
    input_reader.stop()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiControl {
    None,
    Exit,
    NewSession,
    ResumeSession(String),
    SwitchWorkspace(PathBuf),
}

struct TuiLanguageRuntime {
    manager: LocalLanguageManager,
    active: ReloadableLanguageCatalog,
}

// This is the code-owned UI catalog contract. External Fluent files may
// translate these IDs but cannot add behavior, commands, or protocol fields.
const TUI_MESSAGE_IDS: &[&str] = &[
    "phase-idle",
    "phase-running",
    "phase-waiting",
    "phase-cancelling",
    "phase-finished",
    "phase-error",
    "settings-title",
    "approval-title",
    "help-title",
    "common-ok",
    "common-cancel",
    "common-reload",
    "startup-title",
    "startup-tagline",
    "startup-selection-help",
    "startup-capability",
    "startup-policy",
    "startup-vendor",
    "startup-model",
    "startup-unselected",
    "startup-model-default",
    "startup-provider",
    "startup-network",
    "startup-browser",
    "startup-memory",
    "startup-trace",
    "startup-mcp",
    "startup-start",
    "startup-route-unresolved",
    "startup-notice",
    "sidebar-title",
    "drawer-title",
    "input-placeholder",
    "input-title",
    "command-palette-title",
    "settings-scope",
    "settings-status",
    "settings-hint",
    "workspace-empty",
    "help-keyhint",
    "palette-keyhint",
    "settings-keyhint",
    "workspace-keyhint",
    "confirm-keyhint",
    "interaction-keyhint",
    "diagnostic-keyhint",
    "default-keyhint",
    "notice-plan-approved",
    "notice-plan-updated",
    "notice-plan-rejected",
    "notice-plan-started",
    "notice-step-started",
    "notice-step-completed",
    "notice-step-blocked",
    "notice-plan-completed",
    "notice-goal-completed",
    "notice-goal-blocked",
    "notice-goal-updated",
    "notice-goal-continuation",
    "notice-goal-audit",
    "goal-audit-complete",
    "goal-audit-continue",
    "notice-tool-completed",
    "notice-tool-requested",
    "notice-media-ready",
    "notice-media-failed",
    "notice-media-started",
    "notice-context-compressed",
    "notice-subagent-queued",
    "notice-subagent-completed",
    "notice-subagent-confirmation",
    "notice-subagent-started",
    "notice-confirmation-required",
    "notice-interaction-required",
    "notice-memory-saved",
    "notice-memory-rejected",
    "notice-memory-deleted",
    "notice-retry-available",
    "notice-model-selected",
    "notice-prompt-prepared",
    "notice-session-settings-applied",
    "tool-summary-awaiting-policy",
    "tool-summary-needs-confirmation",
    "notice-memory-candidate",
    "notice-memory-recalled",
    "notice-memory-injected",
    "memory-candidate-actions",
    "diagnostic-recovery",
    "error-workspace-switch-blocked",
    "error-settings-blocked",
    "error-route-unresolved",
    "confirmation-no-side-effects",
    "confirmation-actions-session",
    "confirmation-actions-once",
    "memory-candidate-title",
    "label-source-agent",
    "label-task",
    "label-tool",
    "label-summary",
    "label-side-effects",
    "label-candidate",
    "label-summary-hash",
    "error-terminal-too-small",
    "label-minimum",
    "terminal-resize-hint",
    "label-errors",
    "settings-mode-browse",
    "settings-mode-edit-field",
    "settings-mode-edit-text",
    "settings-mode-multiselect",
    "workspace-title",
    "diagnostic-title",
    "label-stage",
    "label-error-code",
    "label-retryable",
    "label-recovery",
    "label-message",
    "label-full-cause",
    "label-freeform",
    "interaction-detail-keyhint",
    "interaction-title",
    "label-budget",
    "label-steps",
    "label-none",
    "label-targets",
    "label-program",
    "label-capability",
    "label-policy",
    "label-constraints",
    "label-read",
    "label-write",
    "label-network",
    "label-timeout",
    "label-output-limit",
    "label-resolved-provider",
    "label-persistent-instructions",
    "label-truncated",
    "sidebar-session",
    "sidebar-user-selection",
    "label-unselected",
    "sidebar-resolved-route",
    "sidebar-route-pending",
    "label-model",
    "label-runtime-status",
    "sidebar-policy",
    "label-capability-mode",
    "label-safety-policy",
    "sidebar-runtime",
    "label-browser",
    "label-off",
    "label-memory",
    "sidebar-budget",
    "conversation-collapsed",
    "settings-group-routing",
    "settings-group-runtime",
    "settings-group-context",
    "settings-group-agents",
    "settings-edit-number",
    "settings-no-options",
    "help-body",
    "language-switched",
    "error-language-runtime-missing",
    "error-language-switch-failed",
];

const TUI_DYNAMIC_MESSAGE_IDS: &[&str] = &[
    "startup-provider-explicit",
    "startup-route-preview",
    "notice-locale-pending-turn",
    "notice-settings-applying",
    "notice-cancel-in-progress",
    "notice-history-delete-cancelled",
    "error-no-retry-step",
    "error-history-expired-resume",
    "error-history-expired-delete",
    "notice-session-switched",
    "notice-diagnostics-cleared",
    "error-diagnostics-clear",
    "notice-no-persistent-grants",
    "notice-persistent-history-disabled",
    "notice-turn-cancel-first",
    "notice-workspace-empty",
    "error-workspace-read",
    "notice-session-not-ready",
    "notice-model-required",
    "error-model-invalid",
    "notice-no-retry",
    "notice-mcp-disabled",
    "notice-a2a-cleared",
    "notice-memory-next-turn",
    "error-subagents-positive",
    "notice-unknown-command-help",
    "notice-interaction-select-or-input",
    "notice-interaction-response-required",
    "notice-session-approval-unsupported",
    "notice-routing-draft-incompatible",
    "notice-language-catalog-status",
    "notice-tui-cancel-exit",
    "notice-error-filter",
    "notice-persistent-grants",
    "notice-revoke-grants",
    "notice-session-delete-confirm",
    "notice-session-deleted",
    "error-session-delete",
    "notice-history-help",
    "notice-provider-settings-open",
    "error-route-not-ready",
    "notice-provider-switching",
    "notice-model-settings-open",
    "notice-model-switching",
    "notice-network-state",
    "notice-browser-state",
    "notice-browser-status",
    "notice-mcp-server-list",
    "notice-mcp-server-selected",
    "notice-mcp-server-disabled",
    "error-a2a-selection",
    "notice-a2a-selected",
    "notice-memory-delete",
    "notice-subagents-state",
    "notice-subagents-max",
    "notice-subagents-status",
    "notice-trace-state",
    "error-session-stream-ended",
    "notice-memory-state",
];

#[cfg(test)]
const CLI_MESSAGE_IDS: &[&str] = &[
    "cli-root-help",
    "cli-tui-help",
    "cli-config-help",
    "cli-language-help",
    "cli-sessions-help",
    "cli-workspace-help",
    "cli-diagnostics-help",
    "cli-instance-help",
    "cli-mcp-help",
    "cli-a2a-help",
    "cli-run-help",
    "cli-instance-list-help",
    "cli-instance-endpoint-help",
    "cli-instance-skill-help",
    "cli-upgrade-help",
    "cli-sessions-path-help",
    "cli-sessions-backup-help",
    "cli-sessions-reset-help",
    "cli-sessions-migrate-help",
    "cli-sessions-list-help",
    "cli-sessions-show-help",
    "cli-sessions-resume-help",
    "cli-sessions-attach-help",
    "cli-sessions-delete-help",
    "cli-sessions-clear-help",
    "cli-app-server-help",
    "cli-mcp-root-help",
    "cli-mcp-list-help",
    "cli-mcp-show-help",
    "cli-mcp-inspect-help",
    "cli-mcp-login-help",
    "cli-mcp-enable-help",
    "cli-mcp-disable-help",
    "cli-mcp-remove-help",
    "cli-mcp-install-help",
    "cli-mcp-install-http-help",
    "cli-mcp-install-stdio-help",
    "cli-a2a-discover-help",
    "cli-a2a-pair-help",
    "cli-a2a-peers-help",
    "cli-a2a-inspect-help",
    "cli-a2a-revoke-help",
    "cli-a2a-delegate-help",
    "cli-a2a-control-help",
    "cli-a2a-cancel-help",
    "cli-a2a-recover-help",
    "cli-a2a-serve-help",
    "cli-a2a-pairings-help",
    "cli-a2a-approvals-help",
    "cli-a2a-provision-help",
    "cli-config-path-help",
    "cli-config-show-help",
    "cli-config-validate-help",
    "cli-config-set-help",
    "cli-config-unset-help",
    "cli-config-route-help",
    "cli-config-vendor-help",
    "cli-config-model-help",
    "cli-config-provider-help",
    "cli-config-credentials-path-help",
    "cli-config-credentials-show-help",
    "cli-config-credentials-set-help",
    "cli-config-credentials-unset-help",
    "cli-config-credentials-migrate-help",
    "cli-config-usage",
    "cli-a2a-serve-usage",
    "cli-a2a-transport-invalid",
    "cli-a2a-server-args-invalid",
    "cli-a2a-config-missing",
    "cli-a2a-psk-secret-required",
    "cli-a2a-mtls-secret-forbidden",
    "cli-a2a-psk-transport-required",
    "cli-a2a-secret-invalid",
    "cli-a2a-secret-length-invalid",
    "cli-a2a-secret-zero",
    "cli-a2a-pairings-usage",
    "cli-a2a-approvals-usage",
    "cli-a2a-owner-terminal-required",
    "cli-a2a-pairing-cancelled",
    "cli-a2a-approval-cancelled",
    "cli-a2a-delegate-usage",
    "cli-a2a-peer-skill-invalid",
    "cli-a2a-delegate-confirm-required",
    "cli-a2a-control-usage",
    "cli-a2a-control-id-invalid",
    "cli-a2a-control-method-forbidden",
    "cli-a2a-control-params-required",
    "cli-a2a-control-confirm-required",
    "cli-a2a-recover-usage",
    "cli-a2a-peer-task-invalid",
    "cli-a2a-cancel-usage",
    "cli-a2a-pair-usage",
    "cli-a2a-local-identity-missing",
    "cli-a2a-identity-invalid",
    "cli-a2a-pair-args-invalid",
    "cli-a2a-endpoint-missing",
    "cli-a2a-spki-missing",
    "cli-a2a-sas-terminal-required",
    "cli-a2a-pair-cancelled",
    "cli-a2a-profile-required",
    "cli-a2a-bootstrap-secret-missing",
    "cli-a2a-server-tls-read-failed",
];

impl TuiLanguageRuntime {
    fn from_environment_with_locale(
        locale: LocaleId,
    ) -> Result<Self, rivo_i18n::LocalLanguageError> {
        let manager = LocalLanguageManager::from_environment()?;
        Self::new(manager, locale)
    }

    fn new(
        manager: LocalLanguageManager,
        locale: LocaleId,
    ) -> Result<Self, rivo_i18n::LocalLanguageError> {
        let active = ReloadableLanguageCatalog::load(&manager, &locale)?;
        Ok(Self { manager, active })
    }

    fn sync_state(&self, state: &mut rivo_tui::AppState) {
        state.current_language = Some(self.active.catalog().locale().to_string());
        state.available_languages = self
            .active
            .discovery()
            .languages()
            .iter()
            .filter(|language| language.error.is_none())
            .filter_map(|language| language.locale.as_ref().map(ToString::to_string))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        state.messages.clear();
        for id in TUI_MESSAGE_IDS {
            if let Some(value) = self.active.catalog().format(id, None) {
                state.messages.insert((*id).to_owned(), value);
            }
        }
        let mut args = FluentArgs::new();
        for key in [
            "current",
            "available",
            "error",
            "filter",
            "grants",
            "count",
            "id",
            "provider",
            "models",
            "model",
            "enabled",
            "server",
            "peer",
            "max",
            "operation",
            "value",
            "vendor",
            "protocol",
            "priority",
        ] {
            args.set(key, format!("__RIVO_{key}__"));
        }
        for id in TUI_DYNAMIC_MESSAGE_IDS {
            if let Some(value) = self.active.catalog().format(id, Some(&args)) {
                state.messages.insert((*id).to_owned(), value);
            }
        }
    }

    fn switch_to(
        &mut self,
        state: &mut rivo_tui::AppState,
        locale: &str,
    ) -> Result<(), rivo_i18n::LocalLanguageError> {
        let locale = LocaleId::parse(locale)?;
        self.active.reload(&self.manager, &locale)?;
        self.sync_state(state);
        Ok(())
    }

    fn reload(
        &mut self,
        state: &mut rivo_tui::AppState,
    ) -> Result<(), rivo_i18n::LocalLanguageError> {
        let locale = self.active.catalog().locale().clone();
        self.active.reload(&self.manager, &locale)?;
        self.sync_state(state);
        Ok(())
    }
}

/// Resolve a TUI notice through the active immutable catalog. Dynamic values are
/// deliberately limited to code-owned placeholders; user/model/tool text is not
/// translated or interpreted as Fluent source.
fn tui_text(state: &rivo_tui::AppState, id: &str, fallback: &str) -> String {
    state
        .messages
        .get(id)
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

fn tui_text_with(
    state: &rivo_tui::AppState,
    id: &str,
    fallback: &str,
    values: &[(&str, String)],
) -> String {
    let mut text = tui_text(state, id, fallback);
    for (key, value) in values {
        text = text.replace(&format!("{{${key}}}"), value);
        text = text.replace(&format!("__RIVO_{key}__"), value);
    }
    text.chars()
        .filter(|character| *character != '\u{2068}' && *character != '\u{2069}')
        .collect()
}

/// Result of deterministic key dispatch before the terminal driver performs a
/// session rebind. Keeping this boundary terminal-free lets the same routing
/// rules be exercised without raw mode or a real PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiKeyAction {
    Continue,
    Exit,
    Rebind(TuiControl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayInputAction {
    Consumed,
    RunCommand(String),
    SelectWorkspace(PathBuf),
    RespondInteraction {
        request_id: String,
        value: String,
    },
    Confirm {
        request_id: String,
        allow: bool,
        reason: Option<String>,
        scope: ApprovalScope,
    },
    ConfirmMemory {
        candidate_id: String,
        body_sha256: String,
        allow: bool,
    },
    RetryStep(String),
}

#[cfg(test)]
async fn handle_tui_shortcut(
    state: &mut rivo_tui::AppState,
    sender: &rivo_session::IntentSender,
    session: Option<&Session>,
    key: crossterm::event::KeyEvent,
) -> Option<TuiKeyAction> {
    handle_tui_shortcut_with_runtime(state, sender, session, None, key).await
}

async fn handle_tui_shortcut_with_runtime(
    state: &mut rivo_tui::AppState,
    sender: &rivo_session::IntentSender,
    session: Option<&Session>,
    mut language_runtime: Option<&mut TuiLanguageRuntime>,
    key: crossterm::event::KeyEvent,
) -> Option<TuiKeyAction> {
    use crossterm::event::KeyCode;

    if is_global_interrupt(&key) {
        if matches!(
            state.mode,
            Some(rivo_tui::UiMode::Running | rivo_tui::UiMode::Confirming)
        ) {
            let _ = sender.send(Intent::CancelTurn).await;
            return Some(TuiKeyAction::Continue);
        }
        return Some(TuiKeyAction::Exit);
    }
    if state.active_overlay() == Some(rivo_tui::ActiveOverlay::SessionSettings)
        && key.code == KeyCode::Enter
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        if matches!(
            state.turn_phase,
            rivo_tui::TurnPhase::Running
                | rivo_tui::TurnPhase::WaitingConfirmation
                | rivo_tui::TurnPhase::Cancelling
        ) || state.last_retry_step_id.is_some()
        {
            state.notice = tui_text(
                state,
                "notice-locale-pending-turn",
                "当前 Turn 结束或 retry 完成后可应用",
            );
            return Some(TuiKeyAction::Continue);
        }
        if state
            .session_settings
            .as_ref()
            .is_some_and(|settings| settings.mode != rivo_tui::SessionSettingsMode::Browse)
        {
            commit_session_setting(state);
            if state
                .session_settings
                .as_ref()
                .is_some_and(|settings| settings.mode != rivo_tui::SessionSettingsMode::Browse)
            {
                return Some(TuiKeyAction::Continue);
            }
        }
        if let Some(settings) = state.session_settings.take() {
            let selection = settings.selection;
            let options = settings.options;
            let _ = sender
                .send(Intent::ApplySessionSettings { selection, options })
                .await;
            state.notice = tui_text(state, "notice-settings-applying", "正在应用 Session 设置");
        }
        return Some(TuiKeyAction::Continue);
    }
    if let Some(action) = dispatch_overlay_key(state, key.code) {
        match action {
            OverlayInputAction::Consumed => {}
            OverlayInputAction::SelectWorkspace(path) => {
                state.close_workspace_selector();
                return Some(TuiKeyAction::Rebind(TuiControl::SwitchWorkspace(path)));
            }
            OverlayInputAction::RunCommand(text) => {
                let control = handle_tui_command_with_language(
                    state,
                    sender,
                    session,
                    language_runtime.as_deref_mut(),
                    &text,
                )
                .await;
                return Some(TuiKeyAction::Rebind(control));
            }
            OverlayInputAction::RespondInteraction { request_id, value } => {
                let _ = sender
                    .send(Intent::RespondInteraction { request_id, value })
                    .await;
                state.interaction = None;
                state.focus = rivo_tui::FocusTarget::Input;
            }
            OverlayInputAction::Confirm {
                request_id,
                allow,
                reason,
                scope,
            } => {
                let _ = sender
                    .send(Intent::Confirm {
                        request_id,
                        allow,
                        reason,
                        scope,
                    })
                    .await;
            }
            OverlayInputAction::ConfirmMemory {
                candidate_id,
                body_sha256,
                allow,
            } => {
                let intent = if allow {
                    Intent::ConfirmMemory {
                        candidate_id,
                        body_sha256,
                    }
                } else {
                    Intent::RejectMemory { candidate_id }
                };
                let _ = sender.send(intent).await;
            }
            OverlayInputAction::RetryStep(step_id) => {
                let _ = sender.send(Intent::RetryStep { step_id }).await;
            }
        }
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::F(6) && key.modifiers.is_empty() {
        state.open_session_settings();
        return Some(TuiKeyAction::Continue);
    }
    if state.focus == rivo_tui::FocusTarget::Input {
        match key.code {
            KeyCode::Left => state.move_input_cursor(false),
            KeyCode::Right => state.move_input_cursor(true),
            KeyCode::Home => state.move_input_line_boundary(false),
            KeyCode::End => state.move_input_line_boundary(true),
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
        ) {
            return Some(TuiKeyAction::Continue);
        }
    }
    if key.code == KeyCode::Char('q') && key.modifiers.is_empty() && state.input.is_empty() {
        if matches!(
            state.mode,
            Some(rivo_tui::UiMode::Running | rivo_tui::UiMode::Confirming)
        ) {
            let _ = sender.send(Intent::CancelTurn).await;
            state.notice = tui_text(
                state,
                "notice-cancel-in-progress",
                "正在取消当前 Turn；再次按 q 退出",
            );
            return Some(TuiKeyAction::Continue);
        }
        return Some(TuiKeyAction::Exit);
    }
    if key.code == KeyCode::Char('/')
        && key.modifiers.is_empty()
        && state.focus == rivo_tui::FocusTarget::Input
        && state.input.is_empty()
    {
        state.toggle_command_palette();
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::F(1) {
        state.toggle_command_palette();
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::Char('?') && key.modifiers.is_empty() {
        state.toggle_help_overlay();
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::F(2) {
        state.sidebar = state.sidebar.next();
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::F(3) {
        state.sidebar = state.sidebar.previous();
        return Some(TuiKeyAction::Continue);
    }
    if key.code == KeyCode::F(4) {
        if let Some(call_id) = state
            .tools
            .get(state.selected_tool)
            .map(|tool| tool.call_id.clone())
        {
            state.toggle_tool_expanded(&call_id);
        }
        return Some(TuiKeyAction::Continue);
    }
    match key.code {
        KeyCode::PageUp => state.scroll_focused(-8),
        KeyCode::PageDown => state.scroll_focused(8),
        KeyCode::End => state.jump_to_bottom(),
        KeyCode::Left => state.scroll_horizontal(-4),
        KeyCode::Right => state.scroll_horizontal(4),
        KeyCode::Up => {
            if state.focus == rivo_tui::FocusTarget::Conversation {
                state.select_conversation(-1);
            } else if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::Errors
            {
                state.select_diagnostic(-1);
            } else if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::History
            {
                state.select_history(-1);
            } else {
                state.scroll_focused(-1);
            }
        }
        KeyCode::Down => {
            if state.focus == rivo_tui::FocusTarget::Conversation {
                state.select_conversation(1);
            } else if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::Errors
            {
                state.select_diagnostic(1);
            } else if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::History
            {
                state.select_history(1);
            } else {
                state.scroll_focused(1);
            }
        }
        KeyCode::Tab => state.cycle_focus(false),
        KeyCode::BackTab => state.cycle_focus(true),
        KeyCode::Esc => {
            if state.pending_history_action.take().is_some() {
                state.notice = tui_text(
                    state,
                    "notice-history-delete-cancelled",
                    "已取消历史删除确认",
                );
            } else if state.focus == rivo_tui::FocusTarget::Drawer {
                state.focus = rivo_tui::FocusTarget::Input;
            } else {
                state.clear_input();
            }
        }
        KeyCode::Char('r')
            if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::Errors =>
        {
            if let Some(step_id) = state.last_retry_step_id.clone() {
                let _ = sender.send(Intent::RetryStep { step_id }).await;
            } else {
                state.notice = tui_text(state, "error-no-retry-step", "当前错误没有可重试步骤");
            }
        }
        KeyCode::Enter
            if state.focus == rivo_tui::FocusTarget::Conversation && state.input.is_empty() =>
        {
            state.toggle_selected_conversation();
        }
        KeyCode::Enter
            if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::Errors =>
        {
            state.toggle_diagnostic_detail();
        }
        KeyCode::Enter
            if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::History =>
        {
            if let Some(id) = state.selected_history_session().map(str::to_owned) {
                let control = handle_tui_command_with_language(
                    state,
                    sender,
                    session,
                    language_runtime.as_deref_mut(),
                    &format!("/sessions resume {id}"),
                )
                .await;
                return Some(TuiKeyAction::Rebind(control));
            }
            state.notice = tui_text(
                state,
                "error-history-expired-resume",
                "所选历史会话已过期，无法恢复；请新建会话",
            );
        }
        KeyCode::Delete
            if state.focus == rivo_tui::FocusTarget::Drawer
                && state.sidebar == rivo_tui::SidebarView::History =>
        {
            if let Some(id) = state.selected_history_session().map(str::to_owned) {
                let _ = handle_tui_command_with_language(
                    state,
                    sender,
                    session,
                    language_runtime.as_deref_mut(),
                    &format!("/sessions delete {id}"),
                )
                .await;
            } else {
                state.notice = tui_text(
                    state,
                    "error-history-expired-delete",
                    "所选历史会话已过期，无法删除；请使用 CLI 查看留存状态",
                );
            }
        }
        KeyCode::Enter
            if (key.modifiers.is_empty()
                || key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL))
                && state.focus == rivo_tui::FocusTarget::Input =>
        {
            if !state.input.trim().is_empty() {
                let text = std::mem::take(&mut state.input);
                state.input_cursor = 0;
                if text.starts_with(':') || text.starts_with('/') {
                    let control = handle_tui_command_with_language(
                        state,
                        sender,
                        session,
                        language_runtime,
                        &text,
                    )
                    .await;
                    return Some(TuiKeyAction::Rebind(control));
                }
                state.append_user_prompt(text.clone());
                if let Some(session) = session {
                    let _ = session
                        .record_ui_audit_async(None, "submit", serde_json::json!({"text": text}))
                        .await;
                }
                let _ = sender.send(Intent::SubmitTask { text }).await;
            }
        }
        KeyCode::Enter
            if key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
                && state.focus == rivo_tui::FocusTarget::Input
                || matches!(key.code, KeyCode::Char('j'))
                    && key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                    && state.focus == rivo_tui::FocusTarget::Input =>
        {
            state.push_input("\n");
        }
        KeyCode::Backspace if state.focus == rivo_tui::FocusTarget::Input => {
            state.backspace_input();
        }
        KeyCode::Char(ch) if state.focus == rivo_tui::FocusTarget::Input => {
            state.push_input(&ch.to_string());
        }
        _ => return None,
    }
    Some(TuiKeyAction::Continue)
}

fn is_global_interrupt(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

fn session_setting_candidates(state: &rivo_tui::AppState, field: usize) -> Vec<String> {
    let Some(settings) = state.session_settings.as_ref() else {
        return Vec::new();
    };
    match field {
        0 => {
            let mut values = state
                .catalog_models
                .iter()
                .map(|m| m.vendor.clone())
                .collect::<Vec<_>>();
            values.sort();
            values.dedup();
            values
        }
        1 => state
            .catalog_models
            .iter()
            .filter(|m| m.vendor == settings.selection.vendor)
            .map(|m| m.id.clone())
            .collect(),
        2 => {
            let mut values = vec!["Auto".into()];
            if let Some(model) = state
                .catalog_models
                .iter()
                .find(|m| m.id == settings.selection.model)
            {
                values.extend(model.providers.iter().map(|p| p.id.clone()));
            }
            values
        }
        3 => {
            let mut values = vec![ReasoningEffort::ProviderDefault.as_str().into()];
            if let Some(model) = state
                .catalog_models
                .iter()
                .find(|m| m.id == settings.selection.model)
            {
                values.extend(
                    model
                        .supported_reasoning_efforts
                        .iter()
                        .map(|v| v.as_str().into()),
                );
            }
            values
        }
        4 => ["general", "research", "plan", "coding", "review"]
            .into_iter()
            .map(String::from)
            .collect(),
        5 => ["strict", "medium", "full_control"]
            .into_iter()
            .map(String::from)
            .collect(),
        6..=8 | 11..=13 => vec!["off".into(), "on".into()],
        _ => Vec::new(),
    }
}

fn session_multi_items(state: &rivo_tui::AppState, field: usize) -> Vec<String> {
    let Some(settings) = state.session_settings.as_ref() else {
        return Vec::new();
    };
    let mut items = if field == 9 {
        state.available_mcp_servers.clone()
    } else {
        state.available_a2a_peers.clone()
    };
    if field == 9 {
        items.extend(
            state
                .available_mcp_toolsets
                .iter()
                .flat_map(|(server, toolsets)| {
                    toolsets
                        .iter()
                        .map(move |toolset| format!("{server}/{toolset}"))
                }),
        );
        items.extend(settings.options.mcp_servers.iter().cloned());
        items.extend(
            settings
                .options
                .mcp_toolsets
                .iter()
                .flat_map(|(server, toolsets)| {
                    toolsets
                        .iter()
                        .map(move |toolset| format!("{server}/{toolset}"))
                }),
        );
    } else {
        items.extend(settings.options.a2a_peers.iter().cloned());
    }
    items.sort();
    items.dedup();
    items
}

fn selected_multi_values(
    settings: &rivo_tui::SessionSettingsState,
    field: usize,
) -> std::collections::BTreeSet<String> {
    if field == 9 {
        settings
            .options
            .mcp_servers
            .iter()
            .cloned()
            .chain(
                settings
                    .options
                    .mcp_toolsets
                    .iter()
                    .flat_map(|(server, toolsets)| {
                        toolsets
                            .iter()
                            .map(move |toolset| format!("{server}/{toolset}"))
                    }),
            )
            .collect()
    } else {
        settings.options.a2a_peers.clone()
    }
}

fn begin_session_setting_edit(state: &mut rivo_tui::AppState) {
    let Some(field) = state.session_settings.as_ref().map(|s| s.field) else {
        return;
    };
    if field == 9 || field == 10 {
        let items = session_multi_items(state, field);
        let selected = state
            .session_settings
            .as_ref()
            .map(|s| selected_multi_values(s, field))
            .unwrap_or_default();
        if let Some(settings) = state.session_settings.as_mut() {
            settings.begin_edit(rivo_tui::SessionSettingsMode::MultiSelect);
            settings.multi_items = items;
            settings.multi_selected = selected;
        }
    } else if field == 14 {
        if let Some(settings) = state.session_settings.as_mut() {
            settings.begin_edit(rivo_tui::SessionSettingsMode::EditText);
        }
    } else {
        let candidates = session_setting_candidates(state, field);
        let current = state
            .session_settings
            .as_ref()
            .map(|s| s.value(field))
            .unwrap_or_default();
        let index = candidates
            .iter()
            .position(|value| value == &current)
            .unwrap_or(0);
        if let Some(settings) = state.session_settings.as_mut() {
            settings.begin_edit(rivo_tui::SessionSettingsMode::EditField);
            settings.candidate_index = index;
        }
    }
}

fn cycle_session_setting_candidate(state: &mut rivo_tui::AppState, delta: i16) {
    let Some((field, index)) = state
        .session_settings
        .as_ref()
        .map(|s| (s.field, s.candidate_index))
    else {
        return;
    };
    let values = session_setting_candidates(state, field);
    if values.is_empty() {
        return;
    }
    let next = if delta < 0 {
        index.checked_sub(1).unwrap_or(values.len() - 1)
    } else {
        (index + 1) % values.len()
    };
    if let Some(settings) = state.session_settings.as_mut() {
        settings.candidate_index = next;
    }
}

fn move_session_multi(state: &mut rivo_tui::AppState, delta: i16) {
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    if settings.multi_items.is_empty() {
        return;
    }
    settings.candidate_index = if delta < 0 {
        settings
            .candidate_index
            .checked_sub(1)
            .unwrap_or(settings.multi_items.len() - 1)
    } else {
        (settings.candidate_index + 1) % settings.multi_items.len()
    };
}

fn toggle_session_multi(state: &mut rivo_tui::AppState) {
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    let Some(item) = settings.multi_items.get(settings.candidate_index).cloned() else {
        return;
    };
    if !settings.multi_selected.insert(item.clone()) {
        settings.multi_selected.remove(&item);
    }
}

fn insert_session_text(state: &mut rivo_tui::AppState, text: &str) {
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    let cursor = settings.edit_cursor.min(settings.edit_buffer.len());
    settings.edit_buffer.insert_str(cursor, text);
    settings.edit_cursor = cursor + text.len();
}

fn backspace_session_text(state: &mut rivo_tui::AppState) {
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    let cursor = settings.edit_cursor.min(settings.edit_buffer.len());
    if cursor == 0 {
        return;
    }
    let start = settings.edit_buffer[..cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
    settings.edit_buffer.replace_range(start..cursor, "");
    settings.edit_cursor = start;
}

fn move_session_text_cursor(state: &mut rivo_tui::AppState, right: bool) {
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    let cursor = settings.edit_cursor.min(settings.edit_buffer.len());
    settings.edit_cursor = if right {
        settings.edit_buffer[cursor..]
            .char_indices()
            .next()
            .map(|(i, ch)| cursor + i + ch.len_utf8())
            .unwrap_or(cursor)
    } else {
        settings.edit_buffer[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
}

fn commit_session_setting(state: &mut rivo_tui::AppState) {
    let Some((field, mode, candidate_index, buffer, selected)) =
        state.session_settings.as_ref().map(|s| {
            (
                s.field,
                s.mode,
                s.candidate_index,
                s.edit_buffer.clone(),
                s.multi_selected.clone(),
            )
        })
    else {
        return;
    };
    if mode == rivo_tui::SessionSettingsMode::MultiSelect {
        if let Some(settings) = state.session_settings.as_mut() {
            if field == 9 {
                let mut servers = Vec::new();
                let mut toolsets = BTreeMap::new();
                for item in selected {
                    if let Some((server, toolset)) = item.split_once('/') {
                        toolsets
                            .entry(server.to_owned())
                            .or_insert_with(BTreeSet::new)
                            .insert(toolset.to_owned());
                    } else {
                        servers.push(item);
                    }
                }
                settings.options.mcp_servers = servers;
                settings.options.mcp_toolsets = toolsets;
            } else {
                settings.options.a2a_peers = selected;
            }
            settings.cancel_edit();
        }
        return;
    }
    if mode == rivo_tui::SessionSettingsMode::EditText {
        match buffer.parse::<usize>() {
            Ok(value) if value > 0 => {
                if let Some(settings) = state.session_settings.as_mut() {
                    settings.options.max_subagents = value;
                    settings.cancel_edit();
                }
            }
            _ => {
                state.notice = tui_text(
                    state,
                    "error-subagents-positive",
                    "最大并发数必须是大于 0 的整数",
                )
            }
        }
        return;
    }
    let values = session_setting_candidates(state, field);
    let Some(value) = values.get(candidate_index).cloned() else {
        return;
    };
    let Some(settings) = state.session_settings.as_mut() else {
        return;
    };
    let mut compatibility_reset = false;
    match field {
        0 => {
            let changed = settings.selection.vendor != value;
            settings.selection.vendor = value.clone();
            if let Some(model) = state.catalog_models.iter().find(|m| m.vendor == value) {
                settings.selection.model = model.id.clone();
                settings.selection.provider = ProviderSelection::Auto;
                settings.options.reasoning_effort = ReasoningEffort::ProviderDefault;
                compatibility_reset = changed;
            }
        }
        1 => {
            settings.selection.model = value;
            if let Some(model) = state
                .catalog_models
                .iter()
                .find(|m| m.id == settings.selection.model)
            {
                let provider_ok = matches!(&settings.selection.provider, ProviderSelection::Auto) || model.providers.iter().any(|p| matches!(&settings.selection.provider, ProviderSelection::Explicit { provider_id } if provider_id == &p.id));
                if !provider_ok {
                    settings.selection.provider = ProviderSelection::Auto;
                    compatibility_reset = true;
                }
                if settings.options.reasoning_effort != ReasoningEffort::ProviderDefault
                    && !model
                        .supported_reasoning_efforts
                        .contains(&settings.options.reasoning_effort)
                {
                    settings.options.reasoning_effort = ReasoningEffort::ProviderDefault;
                    compatibility_reset = true;
                }
            }
        }
        2 => {
            settings.selection.provider = if value == "Auto" {
                ProviderSelection::Auto
            } else {
                ProviderSelection::Explicit { provider_id: value }
            }
        }
        3 => {
            settings.options.reasoning_effort = match value.as_str() {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                _ => ReasoningEffort::ProviderDefault,
            }
        }
        4 => {
            settings.options.capability_profile = match value.as_str() {
                "research" => CapabilityProfile::Research,
                "plan" => CapabilityProfile::Plan,
                "coding" => CapabilityProfile::Coding,
                "review" => CapabilityProfile::Review,
                _ => CapabilityProfile::General,
            }
        }
        5 => {
            settings.options.policy_profile = match value.as_str() {
                "strict" => PolicyProfile::Strict,
                "full_control" => PolicyProfile::FullControl,
                _ => PolicyProfile::Medium,
            }
        }
        6 => settings.options.network = value == "on",
        7 => settings.options.browser = value == "on",
        8 => settings.options.shell = value == "on",
        11 => settings.options.memory_recall = value == "on",
        12 => settings.options.trace = value == "on",
        13 => settings.options.subagents_enabled = value == "on",
        _ => {}
    }
    settings.cancel_edit();
    if compatibility_reset {
        state.notice = tui_text(
            state,
            "notice-routing-draft-incompatible",
            "模型变化使 Provider 或 reasoning 不兼容，已仅更新本地草稿",
        );
    }
}

fn toggle_browse_session_setting(state: &mut rivo_tui::AppState) {
    let field = state
        .session_settings
        .as_ref()
        .map(|s| s.field)
        .unwrap_or(usize::MAX);
    if matches!(field, 6..=8 | 11..=13) {
        let current = state
            .session_settings
            .as_ref()
            .map(|s| s.value(field) == "on")
            .unwrap_or(false);
        if let Some(settings) = state.session_settings.as_mut() {
            let value = !current;
            match field {
                6 => settings.options.network = value,
                7 => settings.options.browser = value,
                8 => settings.options.shell = value,
                11 => settings.options.memory_recall = value,
                12 => settings.options.trace = value,
                13 => settings.options.subagents_enabled = value,
                _ => {}
            }
        }
    }
}

fn handle_session_settings_key(state: &mut rivo_tui::AppState, key: crossterm::event::KeyEvent) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mode = state
        .session_settings
        .as_ref()
        .map(|s| s.mode)
        .unwrap_or_default();
    match mode {
        rivo_tui::SessionSettingsMode::Browse => match key.code {
            KeyCode::Up => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.move_spatial(rivo_tui::SessionSettingsDirection::Up);
                }
            }
            KeyCode::Down => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.move_spatial(rivo_tui::SessionSettingsDirection::Down);
                }
            }
            KeyCode::Left => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.move_spatial(rivo_tui::SessionSettingsDirection::Left);
                }
            }
            KeyCode::Right => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.move_spatial(rivo_tui::SessionSettingsDirection::Right);
                }
            }
            KeyCode::Tab => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.cycle_field(1);
                }
            }
            KeyCode::BackTab => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.cycle_field(-1);
                }
            }
            KeyCode::Char(' ') => toggle_browse_session_setting(state),
            KeyCode::Enter => begin_session_setting_edit(state),
            KeyCode::Esc => state.close_session_settings(),
            _ => {}
        },
        rivo_tui::SessionSettingsMode::EditField => match key.code {
            KeyCode::Up | KeyCode::Left => cycle_session_setting_candidate(state, -1),
            KeyCode::Down | KeyCode::Right => cycle_session_setting_candidate(state, 1),
            KeyCode::Enter => commit_session_setting(state),
            KeyCode::Esc => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.cancel_edit();
                }
            }
            _ => {}
        },
        rivo_tui::SessionSettingsMode::EditText => match key.code {
            KeyCode::Left => move_session_text_cursor(state, false),
            KeyCode::Right => move_session_text_cursor(state, true),
            KeyCode::Backspace => backspace_session_text(state),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                insert_session_text(state, &ch.to_string())
            }
            KeyCode::Enter => commit_session_setting(state),
            KeyCode::Esc => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.cancel_edit();
                }
            }
            _ => {}
        },
        rivo_tui::SessionSettingsMode::MultiSelect => match key.code {
            KeyCode::Up | KeyCode::Left => move_session_multi(state, -1),
            KeyCode::Down | KeyCode::Right => move_session_multi(state, 1),
            KeyCode::Char(' ') => toggle_session_multi(state),
            KeyCode::Enter => commit_session_setting(state),
            KeyCode::Esc => {
                if let Some(s) = state.session_settings.as_mut() {
                    s.cancel_edit();
                }
            }
            _ => {}
        },
    }
}

/// Routes a key to the topmost visible TUI layer. `None` means there is no
/// overlay and the focused pane may handle it; every visible overlay consumes
/// every other key so input cannot leak to the background.
fn dispatch_overlay_key(
    state: &mut rivo_tui::AppState,
    key: crossterm::event::KeyCode,
) -> Option<OverlayInputAction> {
    use crossterm::event::KeyCode;

    let overlay = state.active_overlay()?;
    let action = match overlay {
        rivo_tui::ActiveOverlay::Interaction => {
            let interaction = state
                .interaction
                .as_mut()
                .expect("active interaction overlay");
            let option_count = interaction.request.options.len();
            match key {
                KeyCode::PageUp => state.overlay_scroll = state.overlay_scroll.saturating_sub(8),
                KeyCode::PageDown => state.overlay_scroll = state.overlay_scroll.saturating_add(8),
                KeyCode::Up => interaction.selected = interaction.selected.saturating_sub(1),
                KeyCode::Down => {
                    if option_count > 0 {
                        interaction.selected = (interaction.selected + 1).min(option_count - 1);
                    }
                }
                KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                    let index = (ch as usize).saturating_sub('1' as usize);
                    if index < option_count {
                        interaction.selected = index;
                    }
                }
                KeyCode::Backspace if interaction.request.allow_freeform => {
                    interaction.freeform.pop();
                }
                KeyCode::Enter => {
                    let value = interaction
                        .request
                        .options
                        .get(interaction.selected)
                        .map(|option| option.id.clone())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| interaction.freeform.clone());
                    if value.trim().is_empty() {
                        state.notice = tui_text(
                            state,
                            "notice-interaction-select-or-input",
                            "请先选择选项或输入内容",
                        );
                    } else {
                        return Some(OverlayInputAction::RespondInteraction {
                            request_id: interaction.request.request_id.clone(),
                            value,
                        });
                    }
                }
                KeyCode::Esc => {
                    state.notice = tui_text(
                        state,
                        "notice-interaction-response-required",
                        "Interaction 仍需明确响应",
                    )
                }
                KeyCode::Char(ch) if interaction.request.allow_freeform => {
                    interaction.freeform.push(ch)
                }
                _ => {}
            }
            OverlayInputAction::Consumed
        }
        rivo_tui::ActiveOverlay::Confirmation => match key {
            KeyCode::PageUp => {
                state.overlay_scroll = state.overlay_scroll.saturating_sub(8);
                OverlayInputAction::Consumed
            }
            KeyCode::PageDown => {
                state.overlay_scroll = state.overlay_scroll.saturating_add(8);
                OverlayInputAction::Consumed
            }
            KeyCode::Up => {
                state.overlay_scroll = state.overlay_scroll.saturating_sub(1);
                OverlayInputAction::Consumed
            }
            KeyCode::Down => {
                state.overlay_scroll = state.overlay_scroll.saturating_add(1);
                OverlayInputAction::Consumed
            }
            KeyCode::Char('y') | KeyCode::Char('a') | KeyCode::Char('n') | KeyCode::Esc => {
                if key == KeyCode::Char('a') && !state.pending_session_scope_available {
                    state.notice = tui_text(
                        state,
                        "notice-session-approval-unsupported",
                        "当前确认不支持本会话授权；请使用 y 或 n",
                    );
                    return Some(OverlayInputAction::Consumed);
                }
                let allow = matches!(key, KeyCode::Char('y' | 'a'));
                let reason = (key == KeyCode::Esc).then(|| "Esc 拒绝确认".into());
                if let Some(request_id) = state.pending_request_id.clone() {
                    OverlayInputAction::Confirm {
                        request_id,
                        allow,
                        reason,
                        scope: if key == KeyCode::Char('a') {
                            ApprovalScope::Session
                        } else {
                            ApprovalScope::Once
                        },
                    }
                } else if let Some((candidate_id, body_sha256)) =
                    state.pending_memory_candidate.clone()
                {
                    OverlayInputAction::ConfirmMemory {
                        candidate_id,
                        body_sha256,
                        allow,
                    }
                } else {
                    OverlayInputAction::Consumed
                }
            }
            _ => OverlayInputAction::Consumed,
        },
        rivo_tui::ActiveOverlay::DiagnosticDetail => {
            match key {
                KeyCode::PageUp => state.overlay_scroll = state.overlay_scroll.saturating_sub(8),
                KeyCode::PageDown => state.overlay_scroll = state.overlay_scroll.saturating_add(8),
                KeyCode::Up => state.overlay_scroll = state.overlay_scroll.saturating_sub(1),
                KeyCode::Down => state.overlay_scroll = state.overlay_scroll.saturating_add(1),
                KeyCode::Enter | KeyCode::Esc => state.diagnostic_detail_open = false,
                KeyCode::Char('r') => {
                    if let Some(step_id) = state.last_retry_step_id.clone() {
                        return Some(OverlayInputAction::RetryStep(step_id));
                    }
                    state.notice = tui_text(state, "error-no-retry-step", "当前错误没有可重试步骤");
                }
                _ => {}
            }
            OverlayInputAction::Consumed
        }
        rivo_tui::ActiveOverlay::Help => {
            match key {
                KeyCode::PageUp => state.overlay_scroll = state.overlay_scroll.saturating_sub(8),
                KeyCode::PageDown => state.overlay_scroll = state.overlay_scroll.saturating_add(8),
                KeyCode::Up => state.overlay_scroll = state.overlay_scroll.saturating_sub(1),
                KeyCode::Down => state.overlay_scroll = state.overlay_scroll.saturating_add(1),
                KeyCode::Esc | KeyCode::Char('?') => state.help_overlay = false,
                _ => {}
            }
            OverlayInputAction::Consumed
        }
        rivo_tui::ActiveOverlay::CommandPalette => {
            match key {
                KeyCode::Up => state.move_command_selection(-1),
                KeyCode::Down => state.move_command_selection(1),
                KeyCode::Tab => state.complete_command(),
                KeyCode::Enter => {
                    let text = std::mem::take(&mut state.command_input);
                    state.toggle_command_palette();
                    if !text.trim().is_empty() {
                        return Some(OverlayInputAction::RunCommand(format!("/{text}")));
                    }
                }
                KeyCode::Esc => state.toggle_command_palette(),
                KeyCode::Backspace => state.backspace_command_input(),
                KeyCode::Char(ch) => state.push_command_input(&ch.to_string()),
                _ => {}
            }
            OverlayInputAction::Consumed
        }
        rivo_tui::ActiveOverlay::SessionSettings => {
            handle_session_settings_key(
                state,
                crossterm::event::KeyEvent::new(key, crossterm::event::KeyModifiers::NONE),
            );
            OverlayInputAction::Consumed
        }
        rivo_tui::ActiveOverlay::WorkspaceSelector => match key {
            KeyCode::Up => {
                if let Some(selector) = state.workspace_selector.as_mut() {
                    selector.move_selection(-1);
                }
                OverlayInputAction::Consumed
            }
            KeyCode::Down => {
                if let Some(selector) = state.workspace_selector.as_mut() {
                    selector.move_selection(1);
                }
                OverlayInputAction::Consumed
            }
            KeyCode::Enter => state
                .workspace_selector
                .as_ref()
                .and_then(|selector| selector.selected_workspace())
                .map(|workspace| {
                    OverlayInputAction::SelectWorkspace(PathBuf::from(&workspace.root_path))
                })
                .unwrap_or(OverlayInputAction::Consumed),
            KeyCode::Esc => {
                state.close_workspace_selector();
                OverlayInputAction::Consumed
            }
            _ => OverlayInputAction::Consumed,
        },
    };
    Some(action)
}

async fn rebind_tui_session(
    control: TuiControl,
    config: &mut ResolvedConfig,
    session: &mut Arc<Session>,
    sender: &mut rivo_session::IntentSender,
    events: &mut rivo_session::SessionEventStream,
    state: &mut rivo_tui::AppState,
) -> anyhow::Result<()> {
    let mut effective_config = if let Some(selection) = state.selection.clone() {
        config.with_model_selection(selection)?
    } else {
        config.clone()
    };
    effective_config.capability_profile = state.turn_options.capability_profile;
    effective_config.policy_profile = state.turn_options.policy_profile;
    let next = match control {
        TuiControl::None | TuiControl::Exit => return Ok(()),
        TuiControl::NewSession => Session::connect(effective_config)?,
        TuiControl::ResumeSession(id) => Session::resume(effective_config, id)?,
        TuiControl::SwitchWorkspace(path) => {
            effective_config.workspace = path;
            Session::connect(effective_config)?
        }
    };
    *session = Arc::new(next);
    let (next_sender, next_events) = session.clone().channel();
    *sender = next_sender;
    *events = next_events;
    state.history = session
        .history_scoped_async()
        .await?
        .into_iter()
        .map(|summary| rivo_tui::HistoryEntry {
            session_id: summary.session_id,
            created_at: summary.created_at,
            task_summary: summary.task_summary,
            expiration_reason: summary.expiration_reason,
            event_count: summary.event_count,
            message_count: summary.message_count,
            expired: summary.expired,
        })
        .collect();
    state.normalize_history_selection();
    state.answer.clear();
    state.conversation.clear();
    state.active_assistant_block = None;
    state.tools.clear();
    state.trace.clear();
    state.evidence.clear();
    for event in session.replay_events_async(session.session_id()).await? {
        state.reduce(event);
    }
    sender
        .send(Intent::SetTurnOptions {
            options: state.turn_options.clone(),
        })
        .await
        .context("恢复会话后设置 Turn options 失败")?;
    let messages = session.replay_messages_async(session.session_id()).await?;
    state.conversation.clear();
    state.answer.clear();
    state.active_assistant_block = None;
    for message in messages {
        state.conversation.push(rivo_tui::ConversationBlock {
            id: state.conversation.len(),
            role: message.role,
            text: message.body.clone(),
            collapsed: false,
        });
        if state
            .conversation
            .last()
            .is_some_and(|block| block.role == "assistant")
        {
            state.answer = message.body;
        }
    }
    state.selected_conversation = state.conversation.len().saturating_sub(1);
    state.follow_bottom = true;
    state.diagnostics = session
        .diagnostics_async(Some(session.session_id()), 128)
        .await?
        .into_iter()
        .map(|entry| rivo_tui::DiagnosticRecord {
            stage: entry.stage,
            code: entry.code,
            retryable: entry.retryable,
            recovery: "查看完整会话事件或重试".into(),
            message: entry.summary.clone(),
            cause: entry.summary,
        })
        .collect();
    let workspace = session.workspace_async().await?;
    state.workspace_name = workspace.as_ref().map(|value| value.display_name.clone());
    state.workspace_root = Some(session.workspace_path().display().to_string());
    config.workspace = session.workspace_path().to_path_buf();
    state.notice = tui_text_with(
        state,
        "notice-session-switched",
        &format!("已切换会话 {}", session.session_id()),
        &[("id", session.session_id().to_owned())],
    );
    state.session_id = Some(session.session_id().to_owned());
    state.turn_phase = rivo_tui::TurnPhase::Idle;
    Ok(())
}

#[cfg(test)]
async fn handle_tui_command(
    state: &mut rivo_tui::AppState,
    sender: &rivo_session::IntentSender,
    session: Option<&Session>,
    input: &str,
) -> TuiControl {
    handle_tui_command_with_language(state, sender, session, None, input).await
}

async fn handle_tui_command_with_language(
    state: &mut rivo_tui::AppState,
    sender: &rivo_session::IntentSender,
    session: Option<&Session>,
    mut language_runtime: Option<&mut TuiLanguageRuntime>,
    input: &str,
) -> TuiControl {
    if let Some(session) = session {
        let _ = session
            .record_ui_audit_async(None, "command", serde_json::json!({"input": input}))
            .await;
    }
    let normalized = if input.starts_with('/') && !input.starts_with("/model") {
        format!(":{}", &input[1..])
    } else {
        input.to_owned()
    };
    let parts = normalized.split_whitespace().collect::<Vec<_>>();
    if !matches!(parts.as_slice(), [":sessions", "delete", _]) {
        state.pending_history_action = None;
    }
    match parts.as_slice() {
        [":help"] => {
            state.notice = tui_text(state, "help-body", "/exit /cancel /retry /sessions /new /errors /permissions /language /settings /provider /model /network /browser /mcp /a2a /subagents /trace /memory").to_owned()
        }
        [":language"] => {
            state.notice = match state.current_language.as_deref() {
                Some(current) => tui_text_with(state, "notice-language-catalog-status", "当前 UI catalog={$current}；可选: {$available}", &[("current", current.to_owned()), ("available", state.available_languages.join(", "))]),
                None => tui_text(state, "error-language-runtime-missing", "当前 TUI 未初始化语言 catalog"),
            };
        }
        [":language", "reload"] => match language_runtime.as_mut() {
            Some(runtime) => match runtime.reload(state) {
                Ok(()) => {
                    let locale = state.current_language.as_deref().unwrap_or("en-US");
                    state.turn_options.locale = state
                        .current_language
                        .clone()
                        .unwrap_or_else(|| "en-US".into());
                    let _ = sender.try_send(Intent::SetSessionLocale {
                        locale: state.turn_options.locale.clone(),
                    });
                    state.notice = format!("{}={locale}", tui_text(state, "language-reloaded", "Language reloaded"));
                }
                Err(error) => state.notice = format!("{}: {error}", tui_text(state, "error-language-reload-failed", "Could not reload language")),
            },
            None => state.notice = tui_text(state, "error-language-runtime-missing", "Language runtime is unavailable").to_owned(),
        },
        [":language", locale] => match language_runtime.as_mut() {
            Some(runtime) => match runtime.switch_to(state, locale) {
                Ok(()) => {
                    state.turn_options.locale = state
                        .current_language
                        .clone()
                        .unwrap_or_else(|| "en-US".into());
                    let _ = sender.try_send(Intent::SetSessionLocale {
                        locale: state.turn_options.locale.clone(),
                    });
                    state.notice = format!("{}={locale}", tui_text(state, "language-switched", "Language switched"));
                }
                Err(error) => state.notice = format!("{}: {error}", tui_text(state, "error-language-switch-failed", "Could not switch language")),
            },
            None => state.notice = tui_text(state, "error-language-runtime-missing", "Language runtime is unavailable").to_owned(),
        },
        [":exit"] | [":quit"] => {
            if matches!(
                state.turn_phase,
                rivo_tui::TurnPhase::Running
                    | rivo_tui::TurnPhase::WaitingConfirmation
                    | rivo_tui::TurnPhase::Cancelling
            ) {
                let _ = sender.send(Intent::CancelTurn).await;
                state.notice = tui_text(state, "notice-tui-cancel-exit", "正在取消当前 Turn，请收到终态后再次执行 /exit 退出");
            } else {
                return TuiControl::Exit;
            }
        }
        [":goal", "status"] | [":goal"] => { let _ = sender.send(Intent::GetGoal).await; }
        [":goal", "pause"] => { let _ = sender.send(Intent::PauseGoal).await; }
        [":goal", "resume"] => { let _ = sender.send(Intent::ResumeGoal).await; }
        [":goal", "clear"] => { let _ = sender.send(Intent::ClearGoal).await; }
        [":goal", objective @ ..] if !objective.is_empty() => {
            let _ = sender.send(Intent::SetGoal { objective: objective.join(" "), budget: 20 }).await;
        }
        [":errors"] => { state.sidebar = rivo_tui::SidebarView::Errors; state.focus = rivo_tui::FocusTarget::Drawer; }
        [":errors", "clear"] => {
            match match session {
                Some(session) => session.clear_diagnostics_async().await.map(Some),
                None => Ok(None),
            } {
                Ok(_) => { state.diagnostics.clear(); state.selected_diagnostic = 0; state.notice = tui_text(state, "notice-diagnostics-cleared", "已清空本机诊断记录"); }
                Err(error) => state.notice = tui_text_with(state, "error-diagnostics-clear", &format!("清空诊断失败: {error}"), &[("error", error.to_string())]),
            }
        }
        [":errors", filter] => {
            state.diagnostic_filter = (*filter).into();
            state.sidebar = rivo_tui::SidebarView::Errors;
            state.focus = rivo_tui::FocusTarget::Drawer;
            state.notice = tui_text_with(state, "notice-error-filter", &format!("错误筛选: {filter}"), &[("filter", (*filter).to_owned())]);
        }
        [":permissions"] => {
            state.notice = match match session {
                Some(session) => session.session_permissions_async().await.map(Some),
                None => Ok(None),
            } {
                Ok(Some(grants)) if grants.is_empty() => tui_text(state, "notice-no-persistent-grants", "当前会话没有持久授权"),
                Ok(Some(grants)) => tui_text_with(state, "notice-persistent-grants", "当前会话授权: {$grants}", &[("grants", grants.join(", "))]),
                Ok(None) => tui_text(state, "notice-persistent-history-disabled", "当前会话未启用持久历史"),
                Err(error) => tui_text_with(state, "error-diagnostics-clear", &format!("读取会话授权失败: {error}"), &[("error", error.to_string())]),
            };
        }
        [":permissions", "clear"] => {
            state.notice = match match session {
                Some(session) => session.clear_session_permissions_async().await.map(Some),
                None => Ok(None),
            } {
                Ok(Some(count)) => tui_text_with(state, "notice-revoke-grants", &format!("已撤销 {count} 项会话授权"), &[("count", count.to_string())]),
                Ok(None) => tui_text(state, "notice-persistent-history-disabled", "当前会话未启用持久历史"),
                Err(error) => tui_text_with(state, "error-diagnostics-clear", &format!("撤销会话授权失败: {error}"), &[("error", error.to_string())]),
            };
        }
        [":new"] => {
            if matches!(state.turn_phase, rivo_tui::TurnPhase::Running | rivo_tui::TurnPhase::WaitingConfirmation | rivo_tui::TurnPhase::Cancelling) {
                state.notice = tui_text(state, "notice-turn-cancel-first", "当前 Turn 尚未结束；请先 /cancel，收到终态后再新建会话");
            } else { return TuiControl::NewSession; }
        }
        [":workspace"] | [":workspace", "list"] => {
            match session {
                Some(session) => match session.workspaces_async().await {
                    Ok(workspaces) if workspaces.is_empty() => state.notice = tui_text(state, "notice-workspace-empty", "暂无 Workspace"),
                    Ok(workspaces) => {
                        let choices = workspaces
                            .into_iter()
                            .map(|workspace| rivo_tui::WorkspaceChoice {
                                id: workspace.id,
                                display_name: workspace.display_name,
                                root_path: workspace.workspace_root,
                                is_default: workspace.is_default,
                            })
                            .collect();
                        state.open_workspace_selector(choices);
                    }
                    Err(error) => state.notice = tui_text_with(state, "error-workspace-read", &format!("读取 Workspace 失败: {error}"), &[("error", error.to_string())]),
                },
                None => state.notice = tui_text(state, "notice-session-not-ready", "会话尚未就绪"),
            }
        }
        [":workspace", "open", reference] => {
            if matches!(state.turn_phase, rivo_tui::TurnPhase::Running | rivo_tui::TurnPhase::WaitingConfirmation | rivo_tui::TurnPhase::Cancelling) {
                state.notice = tui_text(state, "notice-turn-cancel-first", "当前 Turn 尚未结束；请先 /cancel 后切换 Workspace");
            } else if let Some(session) = session {
                let reference = (*reference).to_owned();
                match session.workspaces_async().await {
                    Ok(workspaces) => {
                        let path = workspaces.into_iter().find(|workspace| workspace.id == reference).map(|workspace| PathBuf::from(workspace.workspace_root)).unwrap_or_else(|| PathBuf::from(&reference));
                        return TuiControl::SwitchWorkspace(path);
                    }
                    Err(error) => state.notice = tui_text_with(state, "error-workspace-read", &format!("读取 Workspace 失败: {error}"), &[("error", error.to_string())]),
                }
            } else {
                state.notice = tui_text(state, "notice-session-not-ready", "会话尚未就绪");
            }
        }
        [":sessions", "resume", id] => {
            if matches!(state.turn_phase, rivo_tui::TurnPhase::Running | rivo_tui::TurnPhase::WaitingConfirmation | rivo_tui::TurnPhase::Cancelling) {
                state.notice = tui_text(state, "notice-turn-cancel-first", "当前 Turn 尚未结束；请先 /cancel 后恢复历史会话");
            } else { return TuiControl::ResumeSession((*id).into()); }
        }
        [":sessions", "delete", id] => {
            if state.session_id.as_deref() == Some(*id) {
                state.notice = tui_text(state, "error-history-expired-delete", "不能删除当前正在使用的会话；请先恢复另一会话或退出 TUI");
            } else if !state.confirm_history_action(
                rivo_tui::HistoryDestructiveAction::Delete((*id).into()),
            ) {
                state.notice = tui_text_with(state, "notice-session-delete-confirm", &format!("将永久删除历史会话 {id}。请再次输入 /sessions delete {id} 确认；可按 Esc 或执行其他删除命令取消。"), &[("id", (*id).to_owned())]);
            } else {
                match delete_history_session_async((*id).into()).await {
                    Ok(()) => {
                        state.history.retain(|entry| entry.session_id != *id);
                        state.normalize_history_selection();
                        state.notice = tui_text_with(state, "notice-session-deleted", &format!("已删除历史会话 {id}"), &[("id", (*id).to_owned())]);
                    }
                    Err(error) => state.notice = tui_text_with(state, "error-session-delete", &format!("删除历史会话失败: {error}"), &[("error", error.to_string())]),
                }
            }
        }
        [":sessions"] => {
            state.sidebar = rivo_tui::SidebarView::History;
            state.focus = rivo_tui::FocusTarget::Drawer;
            state.notice = tui_text(state, "notice-history-help", "历史：↑/↓ 选择，Enter 恢复，Delete 删除（需再次确认），/new 新建会话");
        }
        [":settings"] => { state.open_session_settings(); }
        [":provider"] => {
            if state.session_settings.is_none() {
                state.open_session_settings();
            }
            if let Some(settings) = state.session_settings.as_mut() {
                settings.field = 2;
                let current = state.selection.as_ref().and_then(|selection| selection.provider.explicit_id()).unwrap_or("Auto");
                state.notice = tui_text_with(state, "notice-provider-settings-open", &format!("当前 provider={current}；已打开 Session Settings"), &[("provider", current.to_owned())]);
            }
        }
        [":provider", name] => {
            let Some(session) = session else {
                state.notice = tui_text(state, "error-route-not-ready", "会话尚未就绪，无法校验路由");
                return TuiControl::None;
            };
            if let Some(mut selection) = state.selection.clone() {
                selection.provider = if name.eq_ignore_ascii_case("auto") {
                    ProviderSelection::Auto
                } else {
                    ProviderSelection::Explicit { provider_id: (*name).into() }
                };
                match session.validate_route_selection(&selection) {
                    Ok(()) => {
                        let _ = sender.send(Intent::SelectModel { selection }).await;
                        state.notice = tui_text_with(state, "notice-provider-switching", &format!("正在切换 Provider: {name}"), &[("provider", (*name).to_owned())]);
                    }
                    Err(error) => state.notice = error.to_string(),
                }
            } else {
                state.notice = tui_text(state, "notice-model-required", "请先选择模型");
            }
        }
        ["/model"] => {
            if state.session_settings.is_none() {
                state.open_session_settings();
            }
            if let Some(settings) = state.session_settings.as_mut() {
                settings.field = 1;
                let models = state.catalog_models.iter().map(|model| model.id.clone()).collect::<Vec<_>>();
                state.notice = tui_text_with(state, "notice-model-settings-open", &format!("当前 model={}；已打开 Session Settings；可选: {}", state.model.as_deref().unwrap_or("未配置"), models.join(", ")), &[("model", state.model.as_deref().unwrap_or("未配置").to_owned()), ("models", models.join(", "))]);
            }
        }
        ["/model", model] => {
            let Some(session) = session else {
                state.notice = tui_text(state, "error-route-not-ready", "会话尚未就绪，无法校验路由");
                return TuiControl::None;
            };
            if let Some(entry) = state.catalog_models.iter().find(|entry| entry.id == *model) {
                let selection = ModelSelection {
                    vendor: entry.vendor.clone(),
                    model: entry.id.clone(),
                    provider: state.selection.as_ref().map(|s| s.provider.clone()).unwrap_or_default(),
                };
                match session.validate_route_selection(&selection) {
                    Ok(()) => {
                        let _ = sender.send(Intent::SelectModel { selection }).await;
                        state.notice = tui_text_with(state, "notice-model-switching", &format!("正在切换模型: {model}"), &[("model", (*model).to_owned())]);
                    }
                    Err(error) => state.notice = error.to_string(),
                }
            } else {
                state.notice = tui_text(state, "error-model-invalid", "模型不在有效目录中");
            }
        }
        [":cancel"] => {
            let _ = sender.send(Intent::CancelTurn).await;
        }
        [":retry"] => {
            if let Some(step_id) = state.last_retry_step_id.clone() {
                let _ = sender.send(Intent::RetryStep { step_id }).await;
            } else {
                state.notice = tui_text(state, "notice-no-retry", "没有可重试步骤");
            }
        }
        [":network", "on"] | [":network", "off"] => {
            state.turn_options.network = parts[1] == "on";
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-network-state", &format!("当前 Turn network={}", state.turn_options.network), &[("enabled", state.turn_options.network.to_string())]);
        }
        [":browser", "on"] | [":browser", "off"] => {
            state.turn_options.browser = parts[1] == "on";
            let _ = sender.send(Intent::SetTurnOptions { options: state.turn_options.clone() }).await;
            state.notice = tui_text_with(state, "notice-browser-state", &format!("当前 Turn browser={}; 页面内容不会写入会话历史", state.turn_options.browser), &[("enabled", state.turn_options.browser.to_string())]);
        }
        [":browser", "status"] | [":browser"] => {
            state.notice = tui_text_with(state, "notice-browser-status", &format!("当前 Turn browser={}; 浏览器需由用户以专用 CDP Profile 启动", state.turn_options.browser), &[("enabled", state.turn_options.browser.to_string())]);
        }
        [":mcp", "off"] => {
            state.turn_options.mcp_servers.clear();
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text(state, "notice-mcp-disabled", "当前 Turn MCP 已关闭");
        }
        [":mcp", server] => {
            if !state.available_mcp_servers.iter().any(|name| name == server) {
                let available = if state.available_mcp_servers.is_empty() {
                    "无；请先配置并启用 MCP server".into()
                } else {
                    state.available_mcp_servers.join(", ")
                };
                state.notice = tui_text_with(state, "notice-mcp-server-disabled", &format!("MCP server 未配置或未启用: {server}；可选: {available}"), &[("server", (*server).to_owned()), ("available", available)]);
            } else {
                state.turn_options.mcp_servers = vec![(*server).into()];
                let _ = sender
                    .send(Intent::SetTurnOptions {
                        options: state.turn_options.clone(),
                    })
                    .await;
                state.notice = tui_text_with(state, "notice-mcp-server-selected", &format!("当前 Turn MCP server={server}"), &[("server", (*server).to_owned())]);
            }
        }
        [":a2a", "off"] => {
            state.turn_options.a2a_peers.clear();
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text(state, "notice-a2a-cleared", "当前 Turn A2A peer 已清空");
        }
        [":a2a", peer_id] => {
            state.turn_options.a2a_peers.insert((*peer_id).into());
            if let Some(session) = session {
                if let Err(error) = session.validate_a2a_selection_async(&state.turn_options).await {
                    state.turn_options.a2a_peers.remove(*peer_id);
                    state.notice = tui_text_with(state, "error-a2a-selection", &format!("A2A peer 不可选择: {error}"), &[("error", error.to_string())]);
                    return TuiControl::None;
                }
            }
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-a2a-selected", &format!("当前 Turn 已选择 A2A peer={peer_id}"), &[("peer", (*peer_id).to_owned())]);
        }
        [":memory"] => {
            state.sidebar = rivo_tui::SidebarView::Evidence;
            state.notice = tui_text(state, "notice-memory-next-turn", "记忆候选通过事件确认；召回仅作用于下一 Turn");
        }
        [":memory", "on"] | [":memory", "off"] => {
            state.turn_options.memory_recall = parts[1] == "on";
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-memory-state", &format!("当前 Turn memory_recall={}", state.turn_options.memory_recall), &[ ("enabled", state.turn_options.memory_recall.to_string()) ]);
        }
        [":memory", "delete", memory_id] => {
            let _ = sender
                .send(Intent::DeleteMemory {
                    memory_id: (*memory_id).into(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-memory-delete", &format!("请求删除记忆: {memory_id}"), &[("id", (*memory_id).to_owned())]);
        }
        [":subagents", "on"] | [":subagents", "off"] => {
            state.turn_options.subagents_enabled = parts[1] == "on";
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-subagents-state", &format!("当前 Turn subagents={}", state.turn_options.subagents_enabled), &[ ("enabled", state.turn_options.subagents_enabled.to_string()) ]);
        }
        [":subagents", "max", value] => match value.parse::<usize>() {
            Ok(max_subagents) if max_subagents > 0 => {
                state.turn_options.max_subagents = max_subagents;
                let _ = sender
                    .send(Intent::SetTurnOptions {
                        options: state.turn_options.clone(),
                    })
                    .await;
                state.notice = tui_text_with(state, "notice-subagents-max", &format!("当前 Turn max_subagents={max_subagents}"), &[("max", max_subagents.to_string())]);
            }
            _ => state.notice = tui_text(state, "error-subagents-positive", "子代理并发数必须是正整数"),
        },
        [":subagents"] => {
            state.notice = tui_text_with(state, "notice-subagents-status", "当前 Turn subagents={$enabled}，max_subagents={$max}", &[("enabled", state.turn_options.subagents_enabled.to_string()), ("max", state.turn_options.max_subagents.to_string())]);
        }
        [":trace"] => {
            state.turn_options.trace = !state.turn_options.trace;
            state.sidebar = rivo_tui::SidebarView::Trace;
            let _ = sender
                .send(Intent::SetTurnOptions {
                    options: state.turn_options.clone(),
                })
                .await;
            state.notice = tui_text_with(state, "notice-trace-state", &format!("当前 Turn trace={}", state.turn_options.trace), &[("enabled", state.turn_options.trace.to_string())]);
        }
        _ => state.notice = tui_text(state, "notice-unknown-command-help", "未知命令，使用 /help 查看帮助"),
    }
    TuiControl::None
}

fn parse_capability(value: &str) -> Result<CapabilityProfile, String> {
    value.parse()
}

fn parse_mcp_toolset(value: &str) -> Result<String, String> {
    let Some((server, toolset)) = value.split_once('/') else {
        return Err("MCP toolset 格式必须为 <server>/<toolset>".into());
    };
    if server.trim().is_empty() || toolset.trim().is_empty() || toolset.contains('/') {
        return Err("MCP toolset 格式必须为 <server>/<toolset>".into());
    }
    Ok(value.into())
}

fn collect_mcp_toolsets(values: &[String]) -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    let mut result = BTreeMap::<String, BTreeSet<String>>::new();
    for value in values {
        let (server, toolset) = value
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("MCP toolset 格式必须为 <server>/<toolset>"))?;
        result
            .entry(server.into())
            .or_default()
            .insert(toolset.into());
    }
    Ok(result)
}

fn parse_policy(value: &str) -> Result<PolicyProfile, String> {
    value.parse()
}

fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, String> {
    match value {
        "default" | "provider_default" => Ok(ReasoningEffort::ProviderDefault),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        _ => Err(format!("不支持的 reasoning effort: {value}")),
    }
}

fn parse_supported_reasoning_efforts(value: &str) -> Result<BTreeSet<ReasoningEffort>, String> {
    let mut efforts = BTreeSet::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        efforts.insert(parse_reasoning_effort(item)?);
    }
    efforts.remove(&ReasoningEffort::ProviderDefault);
    Ok(efforts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use rivo_llm::{MockLlm, ProviderError, ProviderResponse, StopReason, Usage};

    fn fixture_config(model: &str) -> FileConfig {
        let mut file = FileConfig::default();
        file.vendors.insert(
            "fixture".into(),
            VendorCatalogEntry {
                display_name: "Fixture".into(),
            },
        );
        file.models.insert(
            model.into(),
            ModelCatalogEntry {
                vendor: "fixture".into(),
                display_name: model.into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiChatCompletions]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority: 0,
            },
        );
        let mut provider = RoutingProviderConfig {
            base_url: "https://example.test/v1".into(),
            auth: AuthConfig::default(),
            enabled: true,
            priority: 0,
            discovery: ModelDiscoveryCache::default(),
        };
        provider.discovery = ModelDiscoveryCache {
            config_fingerprint: Some(rivo_session::provider_connection_fingerprint(
                "primary", &provider,
            )),
            supported: Some(true),
            models: vec![model.into()],
            last_checked_at: Some(1),
            last_error: None,
        };
        file.providers.insert("primary".into(), provider);
        file.defaults.vendor = Some("fixture".into());
        file.defaults.model = Some(model.into());
        file
    }

    #[test]
    fn tui_startup_load_upgrades_nested_schema3_config_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut source = toml::to_string_pretty(&fixture_config("fixture-model")).unwrap();
        source = source.replacen("version = 6", "version = 3", 1);
        source.push_str("\n[supervision]\nenabled = false\n");
        std::fs::write(&path, source).unwrap();

        let config = load_tui_startup_file(&path).unwrap();
        assert_eq!(config.version, rivo_session::CONFIG_VERSION);
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("version = 6"));
        assert!(!persisted.contains("[supervision]"));
    }

    #[test]
    fn config_remove_confirmation_defaults_to_no() {
        for answer in ["", "\n", "n", "N", "no", "anything else"] {
            assert!(!is_affirmative_confirmation(answer), "{answer:?}");
        }
        for answer in ["y", "Y", "yes", "YES", " Yes \n"] {
            assert!(is_affirmative_confirmation(answer), "{answer:?}");
        }
    }

    #[test]
    fn missing_task_is_a_usage_error() {
        let error = RunCli::command()
            .try_get_matches_from(["rivo"])
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn tui_workspace_option_is_parsed_without_accepting_other_arguments() {
        assert_eq!(
            parse_tui_workspace(&["rivo".into(), "tui".into()]).unwrap(),
            None
        );
        assert_eq!(
            parse_tui_workspace(&[
                "rivo".into(),
                "tui".into(),
                "--workspace".into(),
                "C:\\repo".into(),
            ])
            .unwrap(),
            Some(PathBuf::from("C:\\repo"))
        );
        assert_eq!(
            parse_tui_workspace(&["rivo".into(), "tui".into(), "--workspace=C:\\repo".into(),])
                .unwrap(),
            Some(PathBuf::from("C:\\repo"))
        );
        assert!(
            parse_tui_workspace(&["rivo".into(), "tui".into(), "--workspace".into(),]).is_err()
        );
        assert!(parse_tui_workspace(&["rivo".into(), "tui".into(), "--unknown".into(),]).is_err());
    }

    #[tokio::test]
    async fn management_dispatch_help_paths_are_deterministic() {
        for args in [
            vec!["rivo".into(), "--help".into()],
            vec!["rivo".into(), "tui".into(), "--help".into()],
            vec!["rivo".into(), "config".into(), "--help".into()],
            vec!["rivo".into(), "mcp".into(), "--help".into()],
            vec!["rivo".into(), "a2a".into(), "--help".into()],
            vec!["rivo".into(), "sessions".into(), "--help".into()],
        ] {
            assert_eq!(run_program_with_args(args).await.unwrap(), 0);
        }
        run_config_command(vec!["--help".into()]).await.unwrap();
        run_mcp_command(vec!["--help".into()]).await.unwrap();
        run_a2a_command_at(std::path::Path::new("unused"), vec!["--help".into()])
            .await
            .unwrap();
        run_sessions_command_at(std::path::Path::new("unused"), vec!["--help".into()])
            .await
            .unwrap();
        assert!(diagnostics::run(vec!["--help".into()]).await.is_ok());
    }

    #[test]
    fn structured_options_are_parsed() {
        let cli = RunCli::try_parse_from([
            "rivo",
            "--provider-id",
            "claude-connection",
            "--model",
            "claude",
            "--profile",
            "research",
            "--policy",
            "workspace",
            "--network",
            "--shell",
            "--a2a-peer",
            "peer-1",
            "--a2a-peer",
            "peer-1",
            "--prompt",
            "question",
            "--image",
            "error.png",
        ])
        .unwrap();
        assert_eq!(cli.provider_id.as_deref(), Some("claude-connection"));
        assert_eq!(cli.profile, Some(CapabilityProfile::Research));
        assert_eq!(cli.prompt.as_deref(), Some("question"));
        assert_eq!(cli.image, vec![PathBuf::from("error.png")]);
        assert!(cli.network);
        assert!(cli.shell);
        assert!(cli.mcp_servers.is_empty());
        assert_eq!(cli.a2a_peers, vec!["peer-1", "peer-1"]);
        assert!(!cli.trace);
        assert!(!cli.no_memory_recall);
    }

    #[test]
    fn run_language_option_is_exposed_and_validated_by_session() {
        let cli =
            RunCli::try_parse_from(["rivo", "--language", "zh-CN", "--prompt", "任务"]).unwrap();
        assert_eq!(cli.language.as_deref(), Some("zh-CN"));
        let invalid =
            RunCli::try_parse_from(["rivo", "--language", "not_a_locale", "--prompt", "任务"])
                .unwrap();
        let error = ResolvedConfig::resolve(
            FileConfig::default(),
            &BTreeMap::new(),
            ConfigOverrides {
                locale: invalid.language,
                ..ConfigOverrides::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("locale"));
    }

    #[test]
    fn bundled_cli_catalogs_contain_help_message_ids() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        for locale in ["en-US", "zh-CN"] {
            let locale = LocaleId::parse(locale).unwrap();
            let catalog = ReloadableLanguageCatalog::load(&manager, &locale).unwrap();
            for id in CLI_MESSAGE_IDS {
                assert!(
                    catalog.catalog().format(id, None).is_some(),
                    "{locale} missing {id}"
                );
            }
        }
    }

    #[test]
    fn stdin_and_jsonl_options_are_part_of_the_stable_run_interface() {
        let cli = RunCli::try_parse_from([
            "rivo run",
            "--stdin",
            "--format",
            "jsonl",
            "--approval-mode",
            "deny",
        ])
        .unwrap();
        assert!(cli.stdin);
        assert_eq!(cli.format, OutputFormat::Jsonl);
        assert_eq!(cli.approval_mode, Some(ApprovalMode::Deny));
    }

    #[tokio::test]
    async fn headless_jsonl_run_consumes_session_events_to_a_terminal_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig::resolve(
            fixture_config("mock-model"),
            &BTreeMap::new(),
            ConfigOverrides {
                model: Some("mock-model".into()),
                workspace: Some(workspace.path().to_path_buf()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        let client = MockLlm::from_responses([Ok(ProviderResponse {
            message: rivo_core::Message::text(rivo_core::Role::Assistant, "fixture answer"),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 3,
                total_tokens: 5,
                ..Usage::default()
            },
            provider_raw: None,
            request_id: Some("fixture-request".into()),
            model: "mock-model".into(),
        })]);
        let session = Arc::new(Session::with_client(config, Arc::new(client)));
        let status = execute_headless_turn(
            session,
            "answer with a fixture".into(),
            TurnOptions::default(),
            HeadlessRuntime {
                format: OutputFormat::Jsonl,
                approval_mode: ApprovalMode::Deny,
                manifest: None,
                known_api_keys: vec![],
                media_refs: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(status, 0);
    }

    #[tokio::test]
    async fn headless_run_maps_provider_failure_to_a_nonzero_terminal_status() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig::resolve(
            fixture_config("mock-model"),
            &BTreeMap::new(),
            ConfigOverrides {
                model: Some("mock-model".into()),
                workspace: Some(workspace.path().to_path_buf()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        let client = MockLlm::from_responses([Err(ProviderError::invalid_response(
            "fixture provider failure",
        ))]);
        let session = Arc::new(Session::with_client(config, Arc::new(client)));
        let status = execute_headless_turn(
            session,
            "trigger fixture failure".into(),
            TurnOptions {
                trace: true,
                ..TurnOptions::default()
            },
            HeadlessRuntime {
                format: OutputFormat::Text,
                approval_mode: ApprovalMode::Deny,
                manifest: None,
                known_api_keys: vec![],
                media_refs: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(status, 1);
    }

    #[test]
    fn approval_hash_is_independent_of_object_key_order() {
        let left = serde_json::json!({"path":"README.md","force":false});
        let right = serde_json::json!({"force":false,"path":"README.md"});
        assert_eq!(canonical_json_sha256(&left), canonical_json_sha256(&right));
    }

    #[test]
    fn approval_manifest_defaults_to_one_call_and_hashes_nested_arrays_canonically() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let arguments = serde_json::json!([{"z": 1, "a": [true, null]}, "tail"]);
        let manifest_path = root.path().join("approval.json");
        std::fs::write(
            &manifest_path,
            serde_json::json!({
                "version": 1,
                "workspace": workspace.display().to_string(),
                "rules": [{
                    "tool_name": "read_file",
                    "arguments_sha256": canonical_json_sha256(&arguments),
                    "scope": "once"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let mut approvals = ManifestApprovals::load(&manifest_path, &workspace).unwrap();
        assert!(approvals.allows("read_file", &arguments, ApprovalScope::Once));
        assert!(!approvals.allows("read_file", &arguments, ApprovalScope::Once));
        assert!(matches!(
            ManifestApprovals::load(&root.path().join("missing.json"), &workspace),
            Err(error) if error.to_string().contains("无法读取审批清单")
        ));
    }

    #[test]
    fn tui_panic_hook_is_safe_to_install_before_entering_raw_mode() {
        install_tui_panic_hook();
    }

    #[test]
    fn mcp_http_install_uses_environment_references_and_exact_read_only_tools() {
        let server = parse_mcp_install(
            "http",
            "data",
            "https://mcp.example.test/stream",
            &[
                "--capability".into(),
                "tools,resources".into(),
                "--toolset".into(),
                "query:read_only:list_projects,query_data".into(),
                "--header-env".into(),
                "Authorization=DATA_MCP_TOKEN".into(),
            ],
        )
        .unwrap();
        assert!(server.allowed_capabilities.contains("tools"));
        assert_eq!(server.toolsets["query"].risk, McpToolRisk::ReadOnly);
        assert!(server.toolsets["query"].tools.contains("query_data"));
        assert!(matches!(
            server.transport,
            McpTransport::StreamableHttp { ref header_env, .. }
                if header_env["Authorization"] == "DATA_MCP_TOKEN"
        ));
    }

    #[test]
    fn mcp_http_install_supports_bearer_env_auth() {
        let server = parse_mcp_install(
            "http",
            "atlassian",
            "https://mcp.atlassian.com/v1/mcp",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "issues:read_only:getJiraIssue".into(),
                "--bearer-env".into(),
                "ATLASSIAN_API_TOKEN".into(),
            ],
        )
        .unwrap();
        assert!(matches!(
            server.transport,
            McpTransport::StreamableHttp { ref auth, .. }
                if *auth == McpHttpAuth::Bearer { token_env: "ATLASSIAN_API_TOKEN".into() }
        ));
    }

    #[test]
    fn mcp_show_details_are_bounded_to_safe_transport_references() {
        let stdio = parse_mcp_install(
            "stdio",
            "docs",
            "mcp-docs",
            &[
                "--env".into(),
                "TOKEN=DOCS_TOKEN".into(),
                "--capability".into(),
                "resources".into(),
            ],
        )
        .unwrap();
        let details = mcp_server_details(&stdio).unwrap();
        assert!(details
            .iter()
            .any(|line| line == "transport=stdio program=mcp-docs"));
        assert!(details.iter().any(|line| line == "env=TOKEN<-DOCS_TOKEN"));

        let oauth = parse_mcp_install(
            "http",
            "remote",
            "https://mcp.example.test/stream",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "read:read_only:search".into(),
                "--oauth-client-id".into(),
                "client-1".into(),
                "--oauth-token-env".into(),
                "REMOTE_TOKEN".into(),
                "--oauth-scope".into(),
                "read,offline".into(),
                "--auto-login".into(),
            ],
        )
        .unwrap();
        let details = mcp_server_details(&oauth).unwrap();
        assert!(details.iter().any(|line| line.contains("transport=http")));
        assert!(details
            .iter()
            .any(|line| line == "auth=oauth client_id=client-1 token_env=REMOTE_TOKEN scopes=read,offline auto_login=true"));
        assert!(details
            .iter()
            .any(|line| line.contains("toolset.read=risk:read_only")));
    }

    #[test]
    fn mcp_http_install_supports_oauth_with_dynamic_registration() {
        let server = parse_mcp_install(
            "http",
            "ado",
            "https://mcp.dev.azure.com/contoso",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "work:read_only:wit_list_work_items".into(),
                "--oauth".into(),
                "--oauth-scope".into(),
                "read,write".into(),
                "--oauth-token-env".into(),
                "ADO_OAUTH_ACCESS_TOKEN".into(),
            ],
        )
        .unwrap();
        match server.transport {
            McpTransport::StreamableHttp { auth, .. } => match auth {
                McpHttpAuth::OAuth(config) => {
                    assert!(config.client_id.is_none());
                    assert_eq!(config.scopes, vec!["read", "write"]);
                    assert_eq!(config.token_env.as_deref(), Some("ADO_OAUTH_ACCESS_TOKEN"));
                }
                other => panic!("expected OAuth auth, got {other:?}"),
            },
            other => panic!("expected StreamableHttp, got {other:?}"),
        }
    }

    #[test]
    fn mcp_http_install_auto_login_enables_oauth_and_flag() {
        let server = parse_mcp_install(
            "http",
            "ado",
            "https://mcp.dev.azure.com/contoso",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "work:read_only:wit_list_work_items".into(),
                "--auto-login".into(),
            ],
        )
        .unwrap();
        match server.transport {
            McpTransport::StreamableHttp { auth, .. } => match auth {
                McpHttpAuth::OAuth(config) => {
                    assert!(config.auto_login);
                    // --auto-login 亦隐式启用 OAuth。
                    assert!(config.client_id.is_none());
                }
                other => panic!("expected OAuth auth, got {other:?}"),
            },
            other => panic!("expected StreamableHttp, got {other:?}"),
        }
    }

    #[test]
    fn mcp_http_install_rejects_bearer_and_oauth_together() {
        let error = parse_mcp_install(
            "http",
            "data",
            "https://mcp.example.test/stream",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "query:read_only:list".into(),
                "--bearer-env".into(),
                "TOKEN".into(),
                "--oauth".into(),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be used together"));
    }

    #[test]
    fn mcp_install_rejects_tools_without_an_exact_toolset() {
        let error = parse_mcp_install(
            "stdio",
            "data",
            "mcp-data",
            &["--capability".into(), "tools".into()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("--toolset"));
    }

    #[test]
    fn mcp_install_stdio_preserves_bounded_arguments_and_rejects_cross_transport_flags() {
        let server = parse_mcp_install(
            "stdio",
            "docs",
            "mcp-docs",
            &[
                "--arg".into(),
                "--safe".into(),
                "--cwd".into(),
                "fixtures".into(),
                "--env".into(),
                "TOKEN=DOCS_TOKEN".into(),
                "--capability".into(),
                "tools,prompts".into(),
                "--toolset".into(),
                "read:read_only:search,open".into(),
                "--allow-sampling".into(),
            ],
        )
        .unwrap();
        assert!(server.allow_sampling);
        assert!(server.allowed_capabilities.contains("prompts"));
        assert_eq!(server.toolsets["read"].risk, McpToolRisk::ReadOnly);
        assert!(matches!(
            server.transport,
            McpTransport::Stdio { ref program, ref args, ref cwd, ref env }
                if program == "mcp-docs"
                    && args == &["--safe"]
                    && cwd.as_deref() == Some(std::path::Path::new("fixtures"))
                    && env["TOKEN"] == "DOCS_TOKEN"
        ));

        for (flag, value, expected) in [
            ("--oauth", None, "only supported for HTTP"),
            ("--allow-loopback", None, "only supported for HTTP"),
            (
                "--header-env",
                Some("Authorization=TOKEN"),
                "stdio MCP does not support --header-env",
            ),
        ] {
            let mut values = vec!["--capability".into(), "resources".into(), flag.into()];
            if let Some(value) = value {
                values.push(value.into());
            }
            let error = parse_mcp_install("stdio", "docs", "mcp-docs", &values).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn mcp_install_rejects_malformed_names_values_and_http_auth_options() {
        let cases = [
            (
                "http",
                "bad name",
                "https://mcp.example.test",
                vec!["--capability", "resources"],
                "name cannot be empty",
            ),
            (
                "http",
                "data",
                "https://mcp.example.test",
                vec!["--capability", "unknown"],
                "Unknown MCP capability",
            ),
            (
                "http",
                "data",
                "https://mcp.example.test",
                vec!["--capability", "tools", "--toolset", "bad:unsafe:list"],
                "toolset risk",
            ),
            (
                "http",
                "data",
                "https://mcp.example.test",
                vec![
                    "--capability",
                    "resources",
                    "--oauth-redirect-port",
                    "invalid",
                ],
                "must be a port number",
            ),
            (
                "http",
                "data",
                "https://mcp.example.test",
                vec![
                    "--capability",
                    "resources",
                    "--header-env",
                    "missing-equals",
                ],
                "<Header>=<ENV>",
            ),
        ];
        for (kind, name, target, values, expected) in cases {
            let values = values.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let error = parse_mcp_install(kind, name, target, &values).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn mcp_install_parser_covers_complete_transport_option_matrix() {
        let http = parse_mcp_install(
            "http",
            "catalog",
            "https://mcp.example.test/v1",
            &[
                "--capability".into(),
                "tools,resources,prompts".into(),
                "--toolset".into(),
                "catalog-read:read_only:list,get".into(),
                "--header-env".into(),
                "X-Trace=TRACE_ENV".into(),
                "--bearer-env".into(),
                "MCP_TOKEN".into(),
                "--allow-loopback".into(),
                "--allow-sampling".into(),
            ],
        )
        .unwrap();
        assert!(http.allow_sampling);
        assert_eq!(http.allowed_capabilities.len(), 3);
        assert!(matches!(
            http.transport,
            McpTransport::StreamableHttp {
                auth: McpHttpAuth::Bearer { token_env },
                allow_loopback: true,
                header_env,
                ..
            } if token_env == "MCP_TOKEN" && header_env["X-Trace"] == "TRACE_ENV"
        ));

        let oauth = parse_mcp_install(
            "http",
            "oauth-catalog",
            "https://mcp.example.test/v1",
            &[
                "--capability".into(),
                "resources".into(),
                "--oauth-client-id".into(),
                "client-id".into(),
                "--oauth-client-name".into(),
                "Rivo fixture".into(),
                "--oauth-token-env".into(),
                "OAUTH_TOKEN".into(),
                "--oauth-scope".into(),
                "read,write".into(),
                "--oauth-redirect-port".into(),
                "4321".into(),
                "--auto-login".into(),
            ],
        )
        .unwrap();
        assert!(matches!(
            oauth.transport,
            McpTransport::StreamableHttp {
                auth: McpHttpAuth::OAuth(OAuthConfig {
                    client_id: Some(client_id),
                    client_name: Some(client_name),
                    token_env: Some(token_env),
                    redirect_port: 4321,
                    auto_login: true,
                    scopes,
                }),
                ..
            } if client_id == "client-id" && client_name == "Rivo fixture" && token_env == "OAUTH_TOKEN" && scopes == vec!["read", "write"]
        ));

        let stdio = parse_mcp_install(
            "stdio",
            "local-catalog",
            "mcp-fixture",
            &[
                "--capability".into(),
                "tools".into(),
                "--toolset".into(),
                "safe:action:write".into(),
                "--arg".into(),
                "--serve".into(),
                "--cwd".into(),
                "fixture-workspace".into(),
                "--env".into(),
                "TOKEN=TOKEN_ENV".into(),
                "--allow-sampling".into(),
            ],
        )
        .unwrap();
        assert!(matches!(
            stdio.transport,
            McpTransport::Stdio { args, cwd: Some(cwd), env, .. }
                if args == vec!["--serve"] && cwd == std::path::Path::new("fixture-workspace") && env["TOKEN"] == "TOKEN_ENV"
        ));

        for (kind, values) in [
            ("invalid", vec!["--capability".into(), "resources".into()]),
            ("http", vec!["--unknown".into(), "value".into()]),
            ("http", vec!["--capability".into()]),
            (
                "stdio",
                vec!["--oauth".into(), "--capability".into(), "resources".into()],
            ),
            (
                "stdio",
                vec![
                    "--allow-loopback".into(),
                    "--capability".into(),
                    "resources".into(),
                ],
            ),
            (
                "stdio",
                vec![
                    "--auto-login".into(),
                    "--capability".into(),
                    "resources".into(),
                ],
            ),
        ] {
            assert!(parse_mcp_install(kind, "fixture", "target", &values).is_err());
        }
    }

    #[test]
    fn cli_value_parsers_and_mcp_toolset_collection_reject_ambiguous_values() {
        assert_eq!(
            parse_capability("coding").unwrap(),
            CapabilityProfile::Coding
        );
        assert_eq!(parse_policy("strict").unwrap(), PolicyProfile::Strict);
        assert_eq!(parse_mcp_toolset("docs/read").unwrap(), "docs/read");
        for value in ["docs", "/read", "docs/", "docs/read/extra"] {
            assert!(parse_mcp_toolset(value).is_err(), "{value}");
        }
        assert!(parse_capability("not-a-mode").is_err());
        assert!(parse_policy("not-a-policy").is_err());
        assert!(collect_mcp_toolsets(&[
            "docs/read".into(),
            "docs/read".into(),
            "code/search".into()
        ])
        .is_ok_and(|sets| sets["docs"] == BTreeSet::from(["read".into()])
            && sets["code"].contains("search")));
        assert!(collect_mcp_toolsets(&["missing-separator".into()]).is_err());
    }

    #[test]
    fn approval_manifest_accepts_only_the_exact_rule_and_call_limit() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let arguments = serde_json::json!({"path":"README.md","force":false});
        let manifest_path = root.path().join("approvals.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "workspace": workspace.display().to_string(),
                "rules": [{
                    "tool_name": "apply_patch",
                    "arguments_sha256": canonical_json_sha256(&arguments),
                    "scope": "once",
                    "max_calls": 1
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut approvals = ManifestApprovals::load(&manifest_path, &workspace).unwrap();
        assert!(!approvals.allows("apply_patch", &arguments, ApprovalScope::Session));
        assert!(approvals.allows("apply_patch", &arguments, ApprovalScope::Once));
        assert!(!approvals.allows("apply_patch", &arguments, ApprovalScope::Once));
        assert!(!approvals.allows(
            "apply_patch",
            &serde_json::json!({"path":"other.md","force":false}),
            ApprovalScope::Once
        ));
    }

    #[test]
    fn approval_manifest_rejects_invalid_or_non_once_rules() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = root.path().join("invalid-approvals.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "workspace": workspace,
                "rules": [{
                    "tool_name": "write_file",
                    "arguments_sha256": "a".repeat(64),
                    "scope": "session",
                    "max_calls": 1
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let error = ManifestApprovals::load(&path, &workspace).err().unwrap();
        assert!(error.to_string().contains("scope=once"));
    }

    #[test]
    fn approval_manifest_rejects_wrong_version_workspace_and_rule_shape() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let other_workspace = root.path().join("other-workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&other_workspace).unwrap();
        let path = root.path().join("approvals.json");
        let write = |value: serde_json::Value| {
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        };
        write(serde_json::json!({
            "version": 2,
            "workspace": workspace,
            "rules": []
        }));
        assert!(ManifestApprovals::load(&path, &workspace)
            .err()
            .unwrap()
            .to_string()
            .contains("版本"));
        write(serde_json::json!({
            "version": 1,
            "workspace": other_workspace,
            "rules": []
        }));
        assert!(ManifestApprovals::load(&path, &workspace)
            .err()
            .unwrap()
            .to_string()
            .contains("不匹配"));
        write(serde_json::json!({
            "version": 1,
            "workspace": workspace,
            "rules": [{
                "tool_name": "",
                "arguments_sha256": "not-a-hash",
                "scope": "once",
                "max_calls": 0
            }]
        }));
        assert!(ManifestApprovals::load(&path, &workspace)
            .err()
            .unwrap()
            .to_string()
            .contains("规则无效"));
        std::fs::write(&path, "not json").unwrap();
        assert!(ManifestApprovals::load(&path, &workspace)
            .err()
            .unwrap()
            .to_string()
            .contains("JSON"));
    }

    #[test]
    fn jsonl_projection_never_contains_tool_bodies() {
        let event = AgentEvent::ToolObservation {
            call_id: "tool-1".into(),
            name: "read_file".into(),
            result: serde_json::json!({"content":"secret body", "truncated":false}),
        };
        let text = public_jsonl_event(&event).to_string();
        assert!(text.contains("tool_observation"));
        assert!(!text.contains("secret body"));
    }

    #[test]
    fn jsonl_subagent_projection_exposes_lifecycle_without_handoff_or_summary() {
        let result = rivo_core::SubagentResult {
            agent_id: rivo_core::AgentId("agent-1".into()),
            task_id: rivo_core::AgentTaskId("task-1".into()),
            state: rivo_core::AgentTaskState::Completed,
            summary: "private child handoff and answer".into(),
            evidence: Vec::new(),
            truncated: true,
            usage: rivo_core::SubagentUsage {
                input_tokens: 5,
                output_tokens: 8,
                steps: 2,
            },
        };
        let queued = public_jsonl_event(&AgentEvent::SubagentQueued {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
            call_id: "call-audit".into(),
            label: "audit".into(),
        });
        let activity = public_jsonl_event(&AgentEvent::SubagentActivity {
            agent_id: result.agent_id.clone(),
            task_id: result.task_id.clone(),
            activity: rivo_session::SubagentToolActivity::Observed {
                call_id: "child-call".into(),
                name: "write_file".into(),
                failed: true,
                error: Some("private child error body".into()),
            },
        });
        assert_eq!(activity["kind"], "observed");
        assert_eq!(activity["failed"], true);
        assert!(!activity.to_string().contains("private child error body"));
        let completed = public_jsonl_event(&AgentEvent::SubagentCompleted { result });
        assert_eq!(queued["type"], "subagent_queued");
        assert_eq!(completed["type"], "subagent_completed");
        assert_eq!(completed["evidence_count"], 0);
        let text = format!("{queued}{completed}");
        assert!(!text.contains("private child handoff"));
        assert!(!text.contains("answer"));
    }

    #[test]
    fn jsonl_started_projection_preserves_frozen_selection_and_route() {
        for provider in [
            rivo_session::ProviderSelection::Auto,
            rivo_session::ProviderSelection::Explicit {
                provider_id: "b".into(),
            },
        ] {
            let selection = ModelSelection {
                vendor: "openai".into(),
                model: "gpt-5".into(),
                provider,
            };
            let route = rivo_session::ResolvedRoute {
                vendor: selection.vendor.clone(),
                model: selection.model.clone(),
                provider_id: "b".into(),
                protocol: ProviderKind::OpenAiResponses,
                base_url: "https://fixture.example/v1".into(),
                priority: 200,
            };
            let event = AgentEvent::Started {
                provider: route.protocol,
                model: route.model.clone(),
                selection: Some(selection.clone()),
                route: Some(route.clone()),
                turn_id: "turn-route".into(),
                capability_profile: CapabilityProfile::General,
                policy_profile: PolicyProfile::Strict,
            };
            let projected = public_jsonl_event(&event);
            assert_eq!(projected["type"], "started");
            assert_eq!(
                projected["selection"],
                serde_json::to_value(selection).unwrap()
            );
            assert_eq!(projected["route"], serde_json::to_value(route).unwrap());
            assert_eq!(
                serde_json::from_value::<AgentEvent>(projected).unwrap(),
                event
            );
        }
    }

    #[test]
    fn jsonl_projection_retains_public_metadata_for_all_non_tool_events() {
        let events = [
            AgentEvent::Started {
                provider: ProviderKind::OpenAiResponses,
                model: "fixture".into(),
                selection: None,
                route: None,
                turn_id: "turn-1".into(),
                capability_profile: CapabilityProfile::Plan,
                policy_profile: PolicyProfile::Medium,
            },
            AgentEvent::BudgetUpdated {
                used_tokens: 2,
                context_limit_tokens: 8,
                steps: 1,
                max_steps: 4,
            },
            AgentEvent::ContextCompressed {
                omitted_start: 1,
                omitted_end: 2,
            },
            AgentEvent::Stopped {
                reason: "fixture stopped".into(),
            },
        ];
        for event in events {
            let projected = public_jsonl_event(&event);
            assert_eq!(projected, serde_json::to_value(event).unwrap());
        }
        let observation = public_jsonl_event(&AgentEvent::ToolObservation {
            call_id: "call-1".into(),
            name: "read_file".into(),
            result: serde_json::json!({"truncated":true,"evidence":[]}),
        });
        assert_eq!(observation["type"], "tool_observation");
        assert_eq!(observation["truncated"], true);
        assert_eq!(observation["has_evidence"], true);
    }

    #[test]
    fn jsonl_stream_end_terminal_metadata_preserves_turn_identity_and_order() {
        let terminal = stream_end_terminal_metadata(rivo_session::EventMetadata {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            sequence: 42,
            timestamp_utc_ms: 1,
        });
        assert_eq!(terminal.session_id, "session-1");
        assert_eq!(terminal.turn_id, "turn-1");
        assert_eq!(terminal.sequence, 43);
        assert!(terminal.timestamp_utc_ms >= 1);
    }

    #[test]
    fn first_launch_setup_creates_provider_and_safe_credential_reference() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let setup = FirstLaunchSetup {
            provider_id: "first-run".into(),
            base_url: "https://example.test/v1".into(),
            workspace: workspace.clone(),
            credential: FirstLaunchCredential::Credential("first-run-key".into()),
        };
        let candidate = build_first_launch_config(&FileConfig::default(), &setup).unwrap();
        assert_eq!(
            candidate.defaults.workspace.as_deref(),
            Some(workspace.as_path())
        );
        assert_eq!(
            candidate.providers["first-run"].auth.credential.as_deref(),
            Some("first-run-key")
        );
        assert!(candidate.defaults.model.is_none());
        assert_eq!(candidate.providers["first-run"].discovery.supported, None);
        let mut state = tui_startup_state(&candidate).unwrap();
        let startup = state.startup.as_ref().expect("startup state");
        assert!(startup.route.is_none());
        assert!(!startup.catalog_models.is_empty());
        assert_eq!(
            handle_startup_key(&mut state, crossterm::event::KeyCode::Down.into()),
            StartupKeyAction::Continue
        );
        let startup = state.startup.as_mut().unwrap();
        startup.selected_row = startup.row_count() - 1;
        assert_eq!(
            handle_startup_key(&mut state, crossterm::event::KeyCode::Enter.into()),
            StartupKeyAction::Submit
        );
        assert_eq!(
            handle_startup_key(&mut state, crossterm::event::KeyCode::Esc.into()),
            StartupKeyAction::Exit
        );
        assert_eq!(
            handle_startup_key(
                &mut state,
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('c'),
                    crossterm::event::KeyModifiers::CONTROL,
                ),
            ),
            StartupKeyAction::Exit
        );
    }

    #[test]
    fn first_launch_escape_cancels_without_accepting_a_value() {
        let error = normalize_first_launch_input("\u{1b}\n", "default")
            .err()
            .unwrap();
        assert!(error.to_string().contains("First-launch setup cancelled"));
        assert_eq!(
            normalize_first_launch_input("\n", "default").unwrap(),
            "default"
        );
    }

    #[test]
    fn first_launch_draft_does_not_read_credentials_or_overwrite_provider() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let setup = FirstLaunchSetup {
            provider_id: "incomplete".into(),
            base_url: "https://example.test/v1".into(),
            workspace,
            credential: FirstLaunchCredential::Credential("incomplete-key".into()),
        };
        let file = build_first_launch_config(&FileConfig::default(), &setup).unwrap();
        assert!(file.providers["incomplete"].discovery.models.is_empty());
        assert!(build_first_launch_config(&file, &setup).is_err());
    }

    #[tokio::test]
    async fn first_launch_explicit_discovery_preserves_credentials_and_records_failure() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let config_path = directory.path().join("config.toml");
        let server = rivo_test_support::ScriptedHttpServer::start(vec![
            rivo_test_support::HttpResponse::json(r#"{"data":[{"id":"fixture-model"}]}"#),
        ])
        .unwrap();
        let setup = FirstLaunchSetup {
            provider_id: "verified".into(),
            base_url: format!("{}/v1", server.base_url()),
            workspace: workspace.clone(),
            credential: FirstLaunchCredential::Credential("verified-key".into()),
        };
        rivo_session::set_credential(&config_path, "verified-key", "first-launch-secret").unwrap();
        let mut file = FileConfig::default();
        file.models.insert(
            "fixture-model".into(),
            ModelCatalogEntry {
                vendor: "openai".into(),
                display_name: "Fixture model".into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiChatCompletions]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority: 0,
            },
        );
        std::fs::write(&config_path, toml::to_string_pretty(&file).unwrap()).unwrap();
        commit_first_launch_setup(&config_path, &mut file, &setup)
            .await
            .unwrap();
        assert_eq!(
            file.providers["verified"].discovery.models,
            vec!["fixture-model"]
        );
        assert!(file.defaults.model.is_none());
        assert!(config_path.is_file());
        assert_eq!(
            rivo_session::get_credential(&config_path, "verified-key").unwrap(),
            Some("first-launch-secret".into())
        );
        assert_eq!(server.finish().len(), 1);

        let failing_server =
            rivo_test_support::ScriptedHttpServer::start(vec![rivo_test_support::HttpResponse {
                status: 401,
                content_type: "application/json".into(),
                body: r#"{"error":{"message":"invalid credential"}}"#.into(),
            }])
            .unwrap();
        let failed_path = directory.path().join("failed-config.toml");
        let failed_setup = FirstLaunchSetup {
            provider_id: "failed".into(),
            base_url: format!("{}/v1", failing_server.base_url()),
            workspace,
            credential: FirstLaunchCredential::Credential("verified-key".into()),
        };
        let mut failed_file = FileConfig::default();
        let error = commit_first_launch_setup(&failed_path, &mut failed_file, &failed_setup)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("discovery failed"));
        assert!(failed_path.exists());
        assert!(failed_file.providers["failed"]
            .discovery
            .supported
            .is_none());
        assert_eq!(
            rivo_session::get_credential(&failed_path, "verified-key").unwrap(),
            Some("first-launch-secret".into())
        );
        assert_eq!(failing_server.finish().len(), 1);
    }

    #[test]
    fn startup_selection_persists_all_confirmed_defaults() {
        let mut file = fixture_config("new-model");
        let selection = rivo_tui::StartupSelection {
            selection: ModelSelection {
                vendor: "fixture".into(),
                model: "new-model".into(),
                provider: ProviderSelection::Auto,
            },
            turn_options: TurnOptions {
                locale: "en-US".into(),
                reasoning_effort: Default::default(),
                capability_profile: CapabilityProfile::Coding,
                policy_profile: PolicyProfile::FullControl,
                network: true,
                browser: true,
                computer: false,
                shell: false,
                mcp_servers: vec!["docs".into(), "code".into()],
                mcp_toolsets: [("docs".into(), ["read".into()].into_iter().collect())]
                    .into_iter()
                    .collect(),
                memory_recall: false,
                trace: true,
                subagents_enabled: false,
                max_subagents: 5,
                a2a_peers: Default::default(),
            },
        };
        apply_startup_defaults(&mut file, &selection).unwrap();
        assert_eq!(file.defaults.provider_id, None);
        assert_eq!(file.defaults.model.as_deref(), Some("new-model"));
        assert_eq!(file.defaults.capability, Some(CapabilityProfile::Coding));
        assert_eq!(file.defaults.policy, Some(PolicyProfile::FullControl));
        assert!(file.defaults.network && file.defaults.browser && file.defaults.trace);
        assert!(!file.defaults.memory_recall);
        assert_eq!(file.defaults.mcp_servers, vec!["docs", "code"]);
        assert!(file.defaults.mcp_toolsets["docs"].contains("read"));
        assert_eq!(file.defaults.workspace, None);

        let workspace = PathBuf::from("fixture-workspace");
        let overrides = startup_config_overrides(&selection, Some(workspace.clone()));
        assert_eq!(overrides.workspace, Some(workspace));
    }

    #[test]
    fn overlay_dispatch_keeps_palette_navigation_out_of_the_background() {
        use crossterm::event::KeyCode;

        let mut state = rivo_tui::AppState {
            command_input: String::new(),
            focus: rivo_tui::FocusTarget::Input,
            ..Default::default()
        };
        state.toggle_command_palette();

        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Down),
            Some(OverlayInputAction::Consumed)
        );
        assert_eq!(state.command_index, 1);
        assert_eq!(state.focus, rivo_tui::FocusTarget::Input);

        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Tab),
            Some(OverlayInputAction::Consumed)
        );
        assert_eq!(state.command_input, "quit");
        assert_eq!(state.focus, rivo_tui::FocusTarget::Input);

        state.command_input.clear();
        state.command_index = 3;
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Tab),
            Some(OverlayInputAction::Consumed)
        );
        assert_eq!(state.command_input, "retry");

        state.toggle_help_overlay();
        let sidebar = state.sidebar;
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::F(2)),
            Some(OverlayInputAction::Consumed)
        );
        assert_eq!(state.sidebar, sidebar);
    }

    #[test]
    fn overlay_dispatch_covers_empty_freeform_confirmation_and_detail_boundaries() {
        use crossterm::event::KeyCode;

        let mut state = rivo_tui::AppState {
            interaction: Some(rivo_tui::InteractionModalState {
                request: rivo_session::InteractionRequest {
                    request_id: "freeform-request".into(),
                    title: "fixture".into(),
                    body: "fixture body".into(),
                    options: Vec::new(),
                    allow_freeform: true,
                },
                selected: 0,
                freeform: String::new(),
            }),
            ..Default::default()
        };
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Enter),
            Some(OverlayInputAction::Consumed)
        );
        assert!(state.notice.contains("请先选择"));
        dispatch_overlay_key(&mut state, KeyCode::Char('答'));
        dispatch_overlay_key(&mut state, KeyCode::Char('案'));
        dispatch_overlay_key(&mut state, KeyCode::Backspace);
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Enter),
            Some(OverlayInputAction::RespondInteraction {
                request_id: "freeform-request".into(),
                value: "答".into(),
            })
        );
        dispatch_overlay_key(&mut state, KeyCode::PageDown);
        dispatch_overlay_key(&mut state, KeyCode::Esc);
        assert!(state.notice.contains("明确响应"));

        state.interaction = None;
        state.pending_request_id = Some("confirmation-request".into());
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Char('a')),
            Some(OverlayInputAction::Consumed)
        );
        assert!(state.notice.contains("不支持本会话授权"));
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Esc),
            Some(OverlayInputAction::Confirm {
                request_id: "confirmation-request".into(),
                allow: false,
                reason: Some("Esc 拒绝确认".into()),
                scope: ApprovalScope::Once,
            })
        );
        dispatch_overlay_key(&mut state, KeyCode::PageUp);
        dispatch_overlay_key(&mut state, KeyCode::Down);

        state.pending_request_id = None;
        state.pending_memory_candidate = Some(("memory-id".into(), "body-hash".into()));
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Char('y')),
            Some(OverlayInputAction::ConfirmMemory {
                candidate_id: "memory-id".into(),
                body_sha256: "body-hash".into(),
                allow: true,
            })
        );
        state.pending_memory_candidate = None;

        state.diagnostic_detail_open = true;
        dispatch_overlay_key(&mut state, KeyCode::PageDown);
        dispatch_overlay_key(&mut state, KeyCode::Up);
        dispatch_overlay_key(&mut state, KeyCode::Char('r'));
        assert!(state.notice.contains("没有可重试步骤"));
        dispatch_overlay_key(&mut state, KeyCode::Enter);
        assert!(!state.diagnostic_detail_open);

        state.help_overlay = true;
        dispatch_overlay_key(&mut state, KeyCode::PageUp);
        dispatch_overlay_key(&mut state, KeyCode::Down);
        dispatch_overlay_key(&mut state, KeyCode::Char('?'));
        assert!(!state.help_overlay);

        state.toggle_command_palette();
        dispatch_overlay_key(&mut state, KeyCode::Up);
        dispatch_overlay_key(&mut state, KeyCode::Char('x'));
        dispatch_overlay_key(&mut state, KeyCode::Backspace);
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Enter),
            Some(OverlayInputAction::Consumed)
        );
        assert!(!matches!(
            state.active_overlay(),
            Some(rivo_tui::ActiveOverlay::CommandPalette)
        ));
    }

    #[tokio::test]
    async fn command_palette_completion_executes_a_slash_command() {
        use crossterm::event::KeyCode;

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut state = rivo_tui::AppState::default();
        state.toggle_command_palette();
        state.push_command_input("network");
        state.complete_command();
        let action = dispatch_overlay_key(&mut state, KeyCode::Enter);
        assert_eq!(
            action,
            Some(OverlayInputAction::RunCommand("/network on".into()))
        );
        let Some(OverlayInputAction::RunCommand(command)) = action else {
            panic!("command palette should return a command");
        };
        handle_tui_command(&mut state, &sender, None, &command).await;
        assert!(state.turn_options.network);
        assert!(
            matches!(receiver.recv().await, Some(Intent::SetTurnOptions { options }) if options.network)
        );
    }

    #[tokio::test]
    async fn command_palette_key_dispatch_preserves_single_slash() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut state = rivo_tui::AppState::default();
        for (command, expected) in [
            ("network on", TuiControl::None),
            ("sessions", TuiControl::None),
            ("exit", TuiControl::Exit),
        ] {
            state.focus = rivo_tui::FocusTarget::Input;
            for code in
                std::iter::once(KeyCode::Char('/')).chain(command.chars().map(KeyCode::Char))
            {
                handle_tui_shortcut(
                    &mut state,
                    &sender,
                    None,
                    KeyEvent::new(code, KeyModifiers::NONE),
                )
                .await;
            }
            assert_eq!(
                handle_tui_shortcut(
                    &mut state,
                    &sender,
                    None,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                )
                .await,
                Some(TuiKeyAction::Rebind(expected))
            );
            assert!(state.active_overlay().is_none());
            assert!(state.input.is_empty());
            if command == "sessions" {
                assert_eq!(state.sidebar, rivo_tui::SidebarView::History);
                assert_eq!(state.focus, rivo_tui::FocusTarget::Drawer);
            }
        }
        assert!(state.turn_options.network);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Intent::SetTurnOptions { options }) if options.network
        ));
        assert!(receiver.is_empty());
    }

    #[tokio::test]
    async fn command_palette_exit_command_returns_exit_action() {
        use crossterm::event::KeyCode;

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let mut state = rivo_tui::AppState::default();
        state.toggle_command_palette();
        state.push_command_input("exit");
        state.complete_command();
        let Some(OverlayInputAction::RunCommand(command)) =
            dispatch_overlay_key(&mut state, KeyCode::Enter)
        else {
            panic!("command palette should return the exit command");
        };
        assert_eq!(command, "/exit");
        assert_eq!(
            handle_tui_command(&mut state, &sender, None, &command).await,
            TuiControl::Exit
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn blocking_overlay_has_priority_and_global_interrupt_is_detectable() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = rivo_tui::AppState {
            help_overlay: true,
            pending_request_id: Some("request-1".into()),
            ..Default::default()
        };
        assert_eq!(
            dispatch_overlay_key(&mut state, KeyCode::Down),
            Some(OverlayInputAction::Consumed)
        );
        assert_eq!(state.overlay_scroll, 1);
        assert!(state.help_overlay);
        assert!(matches!(
            dispatch_overlay_key(&mut state, KeyCode::Char('n')),
            Some(OverlayInputAction::Confirm { allow: false, .. })
        ));
        assert!(is_global_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
    }

    #[tokio::test]
    async fn terminal_shortcuts_are_deterministic_without_raw_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, mut intents) = tokio::sync::mpsc::channel(8);
        let mut state = rivo_tui::AppState {
            input: "abc".into(),
            input_cursor: 3,
            ..rivo_tui::AppState::default()
        };
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert_eq!(state.input_cursor, 2);

        state.clear_input();
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert!(matches!(
            state.active_overlay(),
            Some(rivo_tui::ActiveOverlay::CommandPalette)
        ));
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert_eq!(state.command_input, "m");
        state.toggle_command_palette();

        let initial_sidebar = state.sidebar;
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert_ne!(state.sidebar, initial_sidebar);

        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            )
            .await,
            Some(TuiKeyAction::Exit)
        );

        state.mode = Some(rivo_tui::UiMode::Running);
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert!(matches!(intents.try_recv(), Ok(Intent::CancelTurn)));
        assert!(state.notice.contains("正在取消"));
    }

    #[tokio::test]
    async fn terminal_shortcuts_cover_global_navigation_without_a_pty() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, _) = tokio::sync::mpsc::channel(8);
        let mut state = rivo_tui::AppState {
            tools: vec![rivo_tui::ToolActivity {
                call_id: "tool-1".into(),
                name: "read_file".into(),
                status: rivo_tui::ToolStatus::Completed,
                summary: "fixture".into(),
            }],
            ..Default::default()
        };
        for key in [
            KeyCode::F(2),
            KeyCode::F(3),
            KeyCode::F(4),
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::End,
            KeyCode::Tab,
            KeyCode::BackTab,
        ] {
            assert_eq!(
                handle_tui_shortcut(
                    &mut state,
                    &sender,
                    None,
                    KeyEvent::new(key, KeyModifiers::NONE),
                )
                .await,
                Some(TuiKeyAction::Continue)
            );
        }
        assert!(state.expanded_tools.contains("tool-1"));

        state.input = "discard me".into();
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;
        assert!(state.input.is_empty());
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .await;
        assert!(state.help_overlay);
    }

    #[tokio::test]
    async fn f6_opens_session_settings_and_preserves_draft() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, _) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState {
            selection: Some(ModelSelection {
                vendor: "fixture".into(),
                model: "fixture-model".into(),
                provider: ProviderSelection::Auto,
            }),
            ..Default::default()
        };

        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert_eq!(
            state.active_overlay(),
            Some(rivo_tui::ActiveOverlay::SessionSettings)
        );

        let settings = state.session_settings.as_mut().expect("settings opened");
        settings.search = "draft".into();
        settings.options.network = true;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
        )
        .await;
        let settings = state.session_settings.as_ref().expect("settings retained");
        assert_eq!(settings.search, "draft");
        assert!(settings.options.network);
    }

    #[tokio::test]
    async fn f6_does_not_break_through_blocking_overlays() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, _) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState {
            selection: Some(ModelSelection {
                vendor: "fixture".into(),
                model: "fixture-model".into(),
                provider: ProviderSelection::Auto,
            }),
            pending_request_id: Some("request-1".into()),
            ..Default::default()
        };
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.active_overlay(),
            Some(rivo_tui::ActiveOverlay::Confirmation)
        );

        state.pending_request_id = None;
        state.toggle_command_palette();
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.active_overlay(),
            Some(rivo_tui::ActiveOverlay::CommandPalette)
        );
    }

    #[tokio::test]
    async fn session_settings_uses_browse_and_single_field_edit_modes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, _) = tokio::sync::mpsc::channel(8);
        let mut state = rivo_tui::AppState {
            selection: Some(ModelSelection {
                vendor: "vendor-a".into(),
                model: "model-a".into(),
                provider: ProviderSelection::Auto,
            }),
            catalog_models: vec![
                rivo_tui::CatalogModel {
                    id: "model-a".into(),
                    vendor: "vendor-a".into(),
                    display_name: "Model A".into(),
                    providers: vec![],
                    supported_reasoning_efforts: BTreeSet::new(),
                },
                rivo_tui::CatalogModel {
                    id: "model-b".into(),
                    vendor: "vendor-a".into(),
                    display_name: "Model B".into(),
                    providers: vec![],
                    supported_reasoning_efforts: BTreeSet::new(),
                },
            ],
            ..Default::default()
        };
        state.open_session_settings();

        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().mode,
            rivo_tui::SessionSettingsMode::EditField
        );
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().mode,
            rivo_tui::SessionSettingsMode::Browse
        );

        let settings = state.session_settings.as_mut().unwrap();
        settings.field = 1;
        let original_model = settings.selection.model.clone();
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().selection.model,
            original_model
        );
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().selection.model,
            "model-b"
        );
    }

    #[tokio::test]
    async fn session_settings_text_and_multiselect_cancel_without_leaking_draft() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, _) = tokio::sync::mpsc::channel(8);
        let mut state = rivo_tui::AppState {
            selection: Some(ModelSelection {
                vendor: "vendor-a".into(),
                model: "model-a".into(),
                provider: ProviderSelection::Auto,
            }),
            available_mcp_servers: vec!["docs".into(), "code".into()],
            ..Default::default()
        };
        state.open_session_settings();

        state.session_settings.as_mut().unwrap().field = 14;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().mode,
            rivo_tui::SessionSettingsMode::EditText
        );
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        assert!(state
            .session_settings
            .as_ref()
            .unwrap()
            .edit_buffer
            .ends_with(' '));
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().mode,
            rivo_tui::SessionSettingsMode::Browse
        );

        state.session_settings.as_mut().unwrap().field = 9;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(
            state.session_settings.as_ref().unwrap().mode,
            rivo_tui::SessionSettingsMode::MultiSelect
        );
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;
        assert!(state
            .session_settings
            .as_ref()
            .unwrap()
            .options
            .mcp_servers
            .is_empty());
    }

    #[tokio::test]
    async fn tui_key_dispatch_covers_navigation_drawers_and_submission() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, mut intents) = tokio::sync::mpsc::channel(16);
        let mut state = rivo_tui::AppState {
            conversation: vec![
                rivo_tui::ConversationBlock {
                    id: 1,
                    role: "assistant".into(),
                    text: "one".into(),
                    collapsed: false,
                },
                rivo_tui::ConversationBlock {
                    id: 2,
                    role: "assistant".into(),
                    text: "two".into(),
                    collapsed: false,
                },
            ],
            diagnostics: vec![
                rivo_tui::DiagnosticRecord {
                    stage: "stream".into(),
                    code: "one".into(),
                    retryable: true,
                    recovery: "retry".into(),
                    message: "first".into(),
                    cause: "first".into(),
                },
                rivo_tui::DiagnosticRecord {
                    stage: "stream".into(),
                    code: "two".into(),
                    retryable: true,
                    recovery: "retry".into(),
                    message: "second".into(),
                    cause: "second".into(),
                },
            ],
            history: vec![rivo_tui::HistoryEntry {
                session_id: "saved-session".into(),
                created_at: "2026-07-31T00:00:00Z".into(),
                task_summary: Some("saved".into()),
                expiration_reason: None,
                event_count: 1,
                message_count: 1,
                expired: false,
            }],
            ..Default::default()
        };

        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        )
        .await;
        state.focus = rivo_tui::FocusTarget::Conversation;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(state.selected_conversation, 1);
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert!(state.conversation[1].collapsed);

        state.focus = rivo_tui::FocusTarget::Drawer;
        state.sidebar = rivo_tui::SidebarView::Errors;
        state.last_retry_step_id = Some("step-1".into());
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(state.selected_diagnostic, 1);
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await;
        assert!(state.diagnostic_detail_open);
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .await;
        assert!(
            matches!(intents.recv().await, Some(Intent::RetryStep { step_id }) if step_id == "step-1")
        );

        state.diagnostic_detail_open = false;
        state.sidebar = rivo_tui::SidebarView::History;
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Rebind(TuiControl::ResumeSession(
                "saved-session".into()
            )))
        );

        state.focus = rivo_tui::FocusTarget::Input;
        state.input = "hello".into();
        state.input_cursor = 5;
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert!(state.input.is_empty());
        assert!(
            matches!(intents.recv().await, Some(Intent::SubmitTask { text }) if text == "hello")
        );
        assert_eq!(
            state.conversation.last().map(|block| block.text.as_str()),
            Some("hello")
        );

        state.input = ":network on".into();
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Rebind(TuiControl::None))
        );
        assert!(state.turn_options.network);
        assert!(
            matches!(intents.recv().await, Some(Intent::SetTurnOptions { options }) if options.network)
        );

        state.input = "x".into();
        state.input_cursor = 1;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        )
        .await;
        assert_eq!(state.input, "x\n");
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(state.input, "x");
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(state.input, "x!");
    }

    #[tokio::test]
    async fn tui_overlays_forward_only_explicit_responses() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (sender, mut intents) = tokio::sync::mpsc::channel(8);
        let mut state = rivo_tui::AppState {
            interaction: Some(rivo_tui::InteractionModalState {
                request: rivo_session::InteractionRequest {
                    request_id: "interaction-1".into(),
                    title: "choose".into(),
                    body: "choose one".into(),
                    options: vec![rivo_session::InteractionOption {
                        id: "continue".into(),
                        label: "continue".into(),
                        description: "continue the run".into(),
                    }],
                    allow_freeform: false,
                },
                selected: 0,
                freeform: String::new(),
            }),
            ..rivo_tui::AppState::default()
        };
        assert_eq!(
            handle_tui_shortcut(
                &mut state,
                &sender,
                None,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            )
            .await,
            Some(TuiKeyAction::Continue)
        );
        assert!(state.interaction.is_none());
        assert!(matches!(
            intents.recv().await,
            Some(Intent::RespondInteraction { request_id, value }) if request_id == "interaction-1" && value == "continue"
        ));

        state.pending_request_id = Some("confirmation-1".into());
        state.pending_session_scope_available = true;
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(
            intents.recv().await,
            Some(Intent::Confirm { request_id, allow: true, scope: ApprovalScope::Session, .. }) if request_id == "confirmation-1"
        ));

        state.pending_request_id = None;
        state.pending_memory_candidate = Some(("memory-1".into(), "body-sha".into()));
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(
            intents.recv().await,
            Some(Intent::RejectMemory { candidate_id }) if candidate_id == "memory-1"
        ));

        state.pending_memory_candidate = None;
        state.diagnostic_detail_open = true;
        state.last_retry_step_id = Some("retry-1".into());
        handle_tui_shortcut(
            &mut state,
            &sender,
            None,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(
            intents.recv().await,
            Some(Intent::RetryStep { step_id }) if step_id == "retry-1"
        ));
    }

    #[tokio::test]
    async fn tui_commands_update_turn_options_without_submitting_text() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState::default();
        handle_tui_command(&mut state, &sender, None, ":network on").await;
        assert!(state.turn_options.network);
        assert!(
            matches!(receiver.recv().await, Some(Intent::SetTurnOptions { options }) if options.network)
        );

        handle_tui_command(&mut state, &sender, None, ":memory off").await;
        assert!(!state.turn_options.memory_recall);
        assert!(matches!(
            receiver.recv().await,
            Some(Intent::SetTurnOptions { options }) if !options.memory_recall
        ));

        handle_tui_command(&mut state, &sender, None, ":trace").await;
        assert!(state.turn_options.trace);
        assert!(matches!(
            receiver.recv().await,
            Some(Intent::SetTurnOptions { options }) if options.trace
        ));

        handle_tui_command(&mut state, &sender, None, "/browser on").await;
        assert!(state.turn_options.browser);
        assert!(matches!(
            receiver.recv().await,
            Some(Intent::SetTurnOptions { options }) if options.browser
        ));

        handle_tui_command(&mut state, &sender, None, ":a2a peer-1").await;
        assert!(state.turn_options.a2a_peers.contains("peer-1"));
        assert!(matches!(
            receiver.recv().await,
            Some(Intent::SetTurnOptions { options }) if options.a2a_peers.contains("peer-1")
        ));
        handle_tui_command(&mut state, &sender, None, ":a2a off").await;
        assert!(state.turn_options.a2a_peers.is_empty());
    }

    #[tokio::test]
    async fn tui_a2a_command_rejects_an_untrusted_peer_without_sending_options() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig::resolve(
            fixture_config("mock-model"),
            &BTreeMap::new(),
            ConfigOverrides {
                model: Some("mock-model".into()),
                workspace: Some(workspace.path().to_path_buf()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        let session = Session::with_client(config, Arc::new(MockLlm::default()));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState::default();

        handle_tui_command(&mut state, &sender, Some(&session), ":a2a peer-1").await;

        assert!(!state.turn_options.a2a_peers.contains("peer-1"));
        assert!(state.notice.contains("A2A peer 不可选择"));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn tui_exit_command_is_safe_and_supports_quit_alias() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState::default();

        assert_eq!(
            handle_tui_command(&mut state, &sender, None, "/exit").await,
            TuiControl::Exit
        );
        assert!(receiver.try_recv().is_err());

        state.turn_phase = rivo_tui::TurnPhase::Running;
        assert_eq!(
            handle_tui_command(&mut state, &sender, None, "/quit").await,
            TuiControl::None
        );
        assert!(state.notice.contains("再次执行 /exit"));
        assert!(matches!(receiver.recv().await, Some(Intent::CancelTurn)));

        state.turn_phase = rivo_tui::TurnPhase::Finished;
        assert_eq!(
            handle_tui_command(&mut state, &sender, None, "/quit").await,
            TuiControl::Exit
        );
    }

    #[tokio::test]
    async fn tui_mcp_command_only_accepts_the_resolved_allowlist() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let mut state = rivo_tui::AppState {
            available_mcp_servers: vec!["docs".into()],
            ..Default::default()
        };

        handle_tui_command(&mut state, &sender, None, "/mcp unknown").await;
        assert!(state.turn_options.mcp_servers.is_empty());
        assert!(state.notice.contains("未配置或未启用: unknown"));
        assert!(receiver.try_recv().is_err());

        handle_tui_command(&mut state, &sender, None, "/mcp docs").await;
        assert_eq!(state.turn_options.mcp_servers, vec!["docs"]);
        assert!(matches!(
            receiver.recv().await,
            Some(Intent::SetTurnOptions { options }) if options.mcp_servers == vec!["docs".to_owned()]
        ));
    }

    #[tokio::test]
    async fn tui_commands_cover_management_selection_and_runtime_toggles() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig::resolve(
            fixture_config("model-a"),
            &BTreeMap::new(),
            ConfigOverrides {
                workspace: Some(workspace.path().to_owned()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        let session = Session::with_client(config, Arc::new(MockLlm::default()));
        let mut state = rivo_tui::AppState {
            selection: Some(ModelSelection {
                vendor: "fixture".into(),
                model: "model-a".into(),
                provider: ProviderSelection::Explicit {
                    provider_id: "primary".into(),
                },
            }),
            model: Some("model-a".into()),
            catalog_models: rivo_tui::catalog_models(&fixture_config("model-a")).unwrap(),
            available_mcp_servers: vec!["docs".into()],
            ..Default::default()
        };

        handle_tui_command(&mut state, &sender, None, "/help").await;
        assert!(state.notice.contains("/cancel"));
        handle_tui_command(&mut state, &sender, None, "/errors network").await;
        assert_eq!(state.sidebar, rivo_tui::SidebarView::Errors);
        assert_eq!(state.diagnostic_filter, "network");
        handle_tui_command(&mut state, &sender, None, "/errors clear").await;
        assert!(state.notice.contains("清空"));
        handle_tui_command(&mut state, &sender, None, "/permissions").await;
        assert!(state.notice.contains("未启用持久历史"));
        handle_tui_command(&mut state, &sender, None, "/permissions clear").await;
        assert!(state.notice.contains("未启用持久历史"));

        assert_eq!(
            handle_tui_command(&mut state, &sender, None, "/new").await,
            TuiControl::NewSession
        );
        state.turn_phase = rivo_tui::TurnPhase::Running;
        handle_tui_command(&mut state, &sender, None, "/new").await;
        assert!(state.notice.contains("尚未结束"));
        state.turn_phase = rivo_tui::TurnPhase::Idle;
        assert_eq!(
            handle_tui_command(&mut state, &sender, None, "/sessions resume session-2").await,
            TuiControl::ResumeSession("session-2".into())
        );
        handle_tui_command(&mut state, &sender, None, "/sessions").await;
        assert_eq!(state.sidebar, rivo_tui::SidebarView::History);

        handle_tui_command(&mut state, &sender, None, "/provider").await;
        assert!(state.notice.contains("当前 provider=primary"));
        handle_tui_command(&mut state, &sender, None, "/provider missing").await;
        assert!(state.notice.contains("会话尚未就绪"));
        assert!(receiver.try_recv().is_err());
        handle_tui_command(&mut state, &sender, Some(&session), "/provider missing").await;
        assert!(!state.notice.contains("正在切换"));
        assert!(receiver.try_recv().is_err());
        handle_tui_command(&mut state, &sender, Some(&session), "/provider primary").await;
        handle_tui_command(&mut state, &sender, None, "/model").await;
        assert!(state.notice.contains("model-a"));
        handle_tui_command(&mut state, &sender, Some(&session), "/model unknown").await;
        assert!(state.notice.contains("不在有效目录"));
        handle_tui_command(&mut state, &sender, Some(&session), "/model model-a").await;

        handle_tui_command(&mut state, &sender, None, "/cancel").await;
        handle_tui_command(&mut state, &sender, None, "/retry").await;
        assert!(state.notice.contains("没有可重试步骤"));
        handle_tui_command(&mut state, &sender, None, "/browser on").await;
        assert!(state.turn_options.browser);
        handle_tui_command(&mut state, &sender, None, "/browser status").await;
        assert!(state.notice.contains("CDP"));
        handle_tui_command(&mut state, &sender, None, "/mcp docs").await;
        assert_eq!(state.turn_options.mcp_servers, vec!["docs"]);
        handle_tui_command(&mut state, &sender, None, "/mcp off").await;
        assert!(state.turn_options.mcp_servers.is_empty());
        handle_tui_command(&mut state, &sender, None, "/memory").await;
        assert_eq!(state.sidebar, rivo_tui::SidebarView::Evidence);
        handle_tui_command(&mut state, &sender, None, "/memory delete memory-1").await;
        handle_tui_command(&mut state, &sender, None, "/trace").await;
        assert!(state.turn_options.trace);
        handle_tui_command(&mut state, &sender, None, "/not-a-command").await;
        assert!(state.notice.contains("未知命令"));

        let mut intents = Vec::new();
        while let Ok(intent) = receiver.try_recv() {
            intents.push(intent);
        }
        assert!(intents.iter().any(|intent| matches!(
            intent,
            Intent::SelectModel { selection } if selection.provider.explicit_id() == Some("primary")
        )));
        assert!(intents.iter().any(|intent| matches!(
            intent,
            Intent::SelectModel { selection } if selection.model == "model-a"
        )));
        assert!(intents
            .iter()
            .any(|intent| matches!(intent, Intent::CancelTurn)));
        assert!(intents.iter().any(|intent| matches!(
            intent,
            Intent::DeleteMemory { memory_id } if memory_id == "memory-1"
        )));
        assert!(
            intents
                .iter()
                .filter(|intent| matches!(intent, Intent::SetTurnOptions { .. }))
                .count()
                >= 3
        );
    }

    #[tokio::test]
    async fn tui_language_commands_discover_switch_and_keep_the_last_valid_catalog() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let french = LocaleId::parse("fr-FR").unwrap();
        let french_path = manager.language_path(&french);
        let mut runtime =
            TuiLanguageRuntime::new(manager, LocaleId::parse("en-US").unwrap()).unwrap();
        let mut state = rivo_tui::AppState::default();
        runtime.sync_state(&mut state);
        assert_eq!(state.current_language.as_deref(), Some("en-US"));
        assert_eq!(state.available_languages, vec!["en-US", "zh-CN"]);

        std::fs::write(
            &french_path,
            "### rivo-language-schema: 1\n### locale: fr-FR\n### display-name: Français\n### revision: test.1\n\ncommon-ok = Oui\n",
        )
        .unwrap();
        assert!(!state.available_languages.contains(&"fr-FR".into()));

        handle_tui_command_with_language(
            &mut state,
            &sender,
            None,
            Some(&mut runtime),
            "/language reload",
        )
        .await;
        assert!(state.available_languages.contains(&"fr-FR".into()));

        handle_tui_command_with_language(
            &mut state,
            &sender,
            None,
            Some(&mut runtime),
            "/language fr-FR",
        )
        .await;
        assert_eq!(state.current_language.as_deref(), Some("fr-FR"));
        assert_eq!(
            runtime
                .active
                .catalog()
                .format("common-ok", None)
                .as_deref(),
            Some("Oui")
        );
        assert_eq!(
            state.messages.get("common-ok").map(String::as_str),
            Some("Oui")
        );

        std::fs::write(&french_path, "broken").unwrap();
        handle_tui_command_with_language(
            &mut state,
            &sender,
            None,
            Some(&mut runtime),
            "/language reload",
        )
        .await;
        assert!(state.notice.contains("Could not reload language"));
        assert_eq!(state.current_language.as_deref(), Some("fr-FR"));
        assert_eq!(
            runtime
                .active
                .catalog()
                .format("common-ok", None)
                .as_deref(),
            Some("Oui")
        );

        handle_tui_command_with_language(
            &mut state,
            &sender,
            None,
            Some(&mut runtime),
            "/language de-DE",
        )
        .await;
        assert!(state.notice.contains("not installed"));
        assert_eq!(state.current_language.as_deref(), Some("fr-FR"));
    }

    #[test]
    fn bundled_tui_catalogs_contain_every_code_owned_message_id() {
        for locale in ["en-US", "zh-CN"] {
            let manager = LocalLanguageManager::new(tempfile::tempdir().unwrap().path());
            let runtime =
                TuiLanguageRuntime::new(manager, LocaleId::parse(locale).unwrap()).unwrap();
            let mut state = rivo_tui::AppState::default();
            runtime.sync_state(&mut state);
            for id in TUI_MESSAGE_IDS {
                assert!(state.messages.contains_key(*id), "{locale} missing {id}");
            }
            for id in TUI_DYNAMIC_MESSAGE_IDS {
                assert!(
                    state.messages.contains_key(*id),
                    "{locale} missing dynamic {id}"
                );
            }
        }
    }

    #[tokio::test]
    async fn provider_commands_keep_same_url_independent_and_require_delete_confirmation() {
        const CHILD_ENV: &str = "RIVO_TEST_ISOLATED_PROVIDER_COMMANDS";
        if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
            // libtest output capture does not make inherited standard handles
            // non-TTY. Isolate the test instead of changing the runner's handles.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::provider_commands_keep_same_url_independent_and_require_delete_confirmation",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "isolated provider test failed: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert!(!std::io::stdin().is_terminal());
        assert!(!std::io::stderr().is_terminal());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        rivo_session::set_credential(&path, "fixture-credential", "test-only-value").unwrap();

        run_model_routing_config_command(
            &path,
            "provider",
            &[
                "set".into(),
                "primary".into(),
                "--base-url".into(),
                "https://api.example.test/v1".into(),
                "--credential-name".into(),
                "fixture-credential".into(),
            ],
        )
        .await
        .unwrap();
        run_model_routing_config_command(
            &path,
            "provider",
            &[
                "set".into(),
                "secondary".into(),
                "--base-url".into(),
                "https://api.example.test/v1".into(),
            ],
        )
        .await
        .unwrap();
        let file = read_file_config(&path).unwrap();
        assert_eq!(file.providers.len(), 2);
        assert_eq!(
            file.providers["primary"].auth.credential.as_deref(),
            Some("fixture-credential")
        );
        assert_eq!(
            file.providers["primary"].base_url,
            file.providers["secondary"].base_url
        );
        run_model_routing_config_command(&path, "provider", &["list".into()])
            .await
            .unwrap();
        run_model_routing_config_command(&path, "provider", &["show".into(), "primary".into()])
            .await
            .unwrap();

        let before_delete = std::fs::read(&path).unwrap();
        let error = run_model_routing_config_command(
            &path,
            "provider",
            &["remove".into(), "primary".into()],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("删除需要交互确认"));
        assert!(error.to_string().contains("--yes"));
        assert_eq!(std::fs::read(&path).unwrap(), before_delete);
        for provider in ["primary", "secondary"] {
            run_model_routing_config_command(
                &path,
                "provider",
                &["remove".into(), provider.into(), "--yes".into()],
            )
            .await
            .unwrap();
        }
        assert!(read_file_config(&path).unwrap().providers.is_empty());
    }

    #[tokio::test]
    async fn provider_management_lists_updates_and_rejects_invalid_forms() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        commit_model_routing_draft(&path, "missing", &FileConfig::default()).unwrap();
        run_model_routing_config_command(
            &path,
            "provider",
            &[
                "set".into(),
                "standalone".into(),
                "--base-url".into(),
                "https://api.example.test/v1".into(),
            ],
        )
        .await
        .unwrap();
        run_model_routing_config_command(&path, "provider", &["list".into()])
            .await
            .unwrap();
        assert_eq!(
            read_file_config(&path).unwrap().providers["standalone"].base_url,
            "https://api.example.test/v1"
        );
        assert!(run_model_routing_config_command(
            &path,
            "provider",
            &["set".into(), "missing-url".into()]
        )
        .await
        .is_err());
        assert!(
            run_model_routing_config_command(&path, "provider", &["unknown".into()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn model_protocols_can_be_enabled_disabled_or_replaced_on_builtin_models() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        commit_model_routing_draft(&path, "missing", &FileConfig::default()).unwrap();
        let protocols = |path: &std::path::Path| {
            merged_model_catalog(&read_file_config(path).unwrap()).unwrap()["glm-5.3"]
                .supported_protocols
                .clone()
        };
        let set = |args: &[&str]| {
            let mut command = vec!["set".to_string(), "glm-5.3".to_string()];
            command.extend(args.iter().map(|arg| (*arg).to_string()));
            command
        };
        let builtin = protocols(&path);
        assert!(builtin.len() > 1);

        run_model_routing_config_command(
            &path,
            "model",
            &set(&["--disable-protocol", "openai_responses,claude_messages"]),
        )
        .await
        .unwrap();
        assert_eq!(protocols(&path), vec![ProviderKind::OpenAiChatCompletions]);
        let local = &read_file_config(&path).unwrap().models["glm-5.3"];
        assert_eq!(local.vendor, "zhipu");
        assert_eq!(local.display_name, "glm-5.3");

        run_model_routing_config_command(
            &path,
            "model",
            &set(&["--enable-protocol", "claude_messages"]),
        )
        .await
        .unwrap();
        assert_eq!(
            protocols(&path),
            vec![
                ProviderKind::OpenAiChatCompletions,
                ProviderKind::ClaudeMessages
            ]
        );

        run_model_routing_config_command(
            &path,
            "model",
            &set(&["--protocols", "claude_messages,openai_chat_completions"]),
        )
        .await
        .unwrap();
        assert_eq!(
            protocols(&path),
            vec![
                ProviderKind::ClaudeMessages,
                ProviderKind::OpenAiChatCompletions
            ]
        );

        let before = std::fs::read(&path).unwrap();
        for args in [
            set(&[
                "--disable-protocol",
                "claude_messages,openai_chat_completions",
            ]),
            set(&["--protocols", "claude_messages,claude_messages"]),
            set(&["--enable-protocol", "not-a-protocol"]),
        ] {
            assert!(run_model_routing_config_command(&path, "model", &args)
                .await
                .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn explicit_config_path_updates_all_scalar_defaults_without_global_environment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        commit_model_routing_draft(&path, "missing", &FileConfig::default()).unwrap();
        let workspace = directory.path().to_string_lossy().into_owned();
        for (key, value) in [
            ("profile", "coding"),
            ("policy", "strict"),
            ("workspace", workspace.as_str()),
            ("budget.max_steps", "17"),
            ("budget.max_retries", "4"),
            ("budget.max_duration_seconds", "90"),
            ("budget.context_limit_tokens", "4096"),
            ("budget.max_observation_bytes", "2048"),
            ("logging.required", "true"),
            ("logging.retention_days", "3"),
        ] {
            run_config_command_at(&path, vec!["set".into(), key.into(), value.into()])
                .await
                .unwrap();
        }
        run_model_routing_config_command(
            &path,
            "provider",
            &[
                "set".into(),
                "primary".into(),
                "--base-url".into(),
                "https://api.example.test/v1".into(),
            ],
        )
        .await
        .unwrap();
        run_config_command_at(
            &path,
            vec!["set".into(), "provider_id".into(), "primary".into()],
        )
        .await
        .unwrap();
        assert_eq!(
            read_file_config(&path)
                .unwrap()
                .defaults
                .provider_id
                .as_deref(),
            Some("primary")
        );
        run_config_command_at(&path, vec!["path".into()])
            .await
            .unwrap();
        run_config_command_at(&path, vec!["show".into()])
            .await
            .unwrap();
        run_config_command_at(&path, vec!["validate".into()])
            .await
            .unwrap();
        let server = parse_mcp_install(
            "stdio",
            "docs",
            "mcp-docs",
            &["--capability".into(), "resources".into()],
        )
        .unwrap();
        run_config_command_at(
            &path,
            vec![
                "set".into(),
                "mcp.server".into(),
                serde_json::to_string(&server).unwrap(),
            ],
        )
        .await
        .unwrap();
        let file = read_file_config(&path).unwrap();
        assert_eq!(file.defaults.provider_id.as_deref(), Some("primary"));
        assert_eq!(file.mcp.servers.len(), 1);
        assert_eq!(file.defaults.capability, Some(CapabilityProfile::Coding));
        assert_eq!(file.defaults.policy, Some(PolicyProfile::Strict));
        assert_eq!(
            file.defaults.workspace,
            Some(directory.path().to_path_buf())
        );
        assert_eq!(file.defaults.budget.max_steps, 17);
        assert_eq!(file.defaults.budget.max_retries, 4);
        assert_eq!(file.defaults.budget.max_duration_seconds, 90);
        assert_eq!(file.defaults.budget.context_limit_tokens, 4096);
        assert_eq!(file.defaults.budget.max_observation_bytes, 2048);
        assert!(file.defaults.logging.required);
        assert_eq!(file.defaults.logging.retention_days, 3);

        for key in [
            "provider_id",
            "profile",
            "policy",
            "workspace",
            "budget.max_steps",
            "budget.max_retries",
            "budget.max_duration_seconds",
            "budget.context_limit_tokens",
            "budget.max_observation_bytes",
            "logging.required",
            "logging.retention_days",
        ] {
            run_config_command_at(&path, vec!["unset".into(), key.into()])
                .await
                .unwrap();
        }
        run_config_command_at(
            &path,
            vec!["unset".into(), "mcp.server".into(), "docs".into()],
        )
        .await
        .unwrap();
        let file = read_file_config(&path).unwrap();
        assert!(file.defaults.provider_id.is_none());
        assert!(file.mcp.servers.is_empty());
        assert_eq!(file.defaults.capability, None);
        assert_eq!(file.defaults.policy, None);
        assert_eq!(file.defaults.workspace, None);
        assert_eq!(file.defaults.budget, rivo_session::BudgetConfig::default());
        assert_eq!(
            file.defaults.logging,
            rivo_session::LoggingConfig::default()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_session_database_supports_safe_listing_showing_and_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let store = rivo_storage::Store::open(&path).unwrap();
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id: "session-one".into(),
                turn_id: "turn-one".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        store
            .append_message(&rivo_storage::StoredMessage {
                session_id: "session-one".into(),
                turn_id: "turn-one".into(),
                sequence: 1,
                role: "user".into(),
                body: "fixture task".into(),
            })
            .unwrap();
        drop(store);

        run_sessions_command_at(&path, vec!["path".into()])
            .await
            .unwrap();
        run_sessions_command_at(&path, vec!["list".into()])
            .await
            .unwrap();
        run_sessions_command_at(&path, vec!["show".into(), "session-one".into()])
            .await
            .unwrap();
        let refusal = run_sessions_command_at_with_terminal(
            &path,
            vec!["delete".into(), "session-one".into()],
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(refusal.to_string().contains("需要交互确认"));
        run_sessions_command_at(
            &path,
            vec!["delete".into(), "session-one".into(), "--yes".into()],
        )
        .await
        .unwrap();
        assert!(!rivo_storage::Store::open(&path)
            .unwrap()
            .has_session("session-one")
            .unwrap());

        let store = rivo_storage::Store::open(&path).unwrap();
        store
            .append_event(&rivo_storage::StoredEvent {
                session_id: "session-two".into(),
                turn_id: "turn-two".into(),
                sequence: 1,
                kind: "started".into(),
                body: "{}".into(),
            })
            .unwrap();
        drop(store);
        run_sessions_command_at(&path, vec!["clear".into(), "--yes".into()])
            .await
            .unwrap();
        assert!(rivo_storage::Store::open(&path)
            .unwrap()
            .list_sessions()
            .unwrap()
            .is_empty());
        assert!(run_sessions_command_at(&path, vec!["unknown".into()])
            .await
            .is_err());

        // A clean install has neither the database nor its parent data files;
        // maintenance commands remain safe and idempotent in that state.
        let missing = directory.path().join("missing-history.db");
        run_sessions_command_at(&missing, vec!["--help".into()])
            .await
            .unwrap();
        run_sessions_command_at(&missing, vec!["list".into()])
            .await
            .unwrap();
        run_sessions_command_at(&missing, vec!["clear".into(), "--yes".into()])
            .await
            .unwrap();
        assert!(!missing.exists());
    }

    #[test]
    fn credential_management_migrates_plaintext_only_through_the_explicit_command() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let credentials = rivo_session::credentials_file_path(&config);
        std::fs::write(
            &credentials,
            "format_version = 1\n[credentials]\nfixture_key = \"plaintext-fixture\"\n",
        )
        .unwrap();
        run_credentials_command(&config, &["show".into()]).unwrap();
        assert!(matches!(
            rivo_session::credential_names(&config).unwrap().as_slice(),
            [(name, rivo_session::CredentialStatus::Plaintext)] if name == "fixture_key"
        ));
        run_credentials_command(&config, &["migrate".into()]).unwrap();
        assert_eq!(
            rivo_session::get_credential(&config, "fixture_key")
                .unwrap()
                .as_deref(),
            Some("plaintext-fixture")
        );
        run_credentials_command(&config, &["unset".into(), "fixture_key".into()]).unwrap();
        assert!(rivo_session::credential_names(&config).unwrap().is_empty());
    }

    #[test]
    fn credential_set_rejects_masked_prompt_when_stdin_is_not_a_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let error =
            run_credentials_command(&config, &["set".into(), "fixture_key".into()]).unwrap_err();
        assert!(error.to_string().contains("--from-env"));
        assert!(rivo_session::credential_names(&config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn cli_rejects_invalid_turn_options_before_loading_configuration() {
        let zero = run_cli(vec![
            "rivo run".into(),
            "--max-subagents".into(),
            "0".into(),
            "--prompt".into(),
            "task".into(),
        ])
        .await
        .unwrap_err();
        assert!(zero.to_string().contains("max-subagents"));

        let missing_manifest = run_cli(vec![
            "rivo run".into(),
            "--approval-mode".into(),
            "manifest".into(),
            "--prompt".into(),
            "task".into(),
        ])
        .await
        .unwrap_err();
        assert!(missing_manifest.to_string().contains("approval-manifest"));

        let incompatible_manifest = RunCli::try_parse_from([
            "rivo run",
            "--approval-manifest",
            "approvals.json",
            "--prompt",
            "task",
        ])
        .unwrap_err();
        assert_eq!(incompatible_manifest.exit_code(), 2);
    }

    #[test]
    fn interactive_tui_entry_points_fail_closed_without_a_terminal() {
        let tui = require_interactive_terminal(
            false,
            false,
            "rivo tui 需要交互式终端；请在本地终端直接运行，自动化请使用 `rivo run --format jsonl`",
        )
        .unwrap_err();
        assert!(tui.to_string().contains("交互式终端"));

        let first_launch = require_interactive_terminal(
            false,
            false,
            "尚未配置 Provider；请在交互终端运行 `rivo tui`，或使用 `rivo config provider set ...`",
        )
        .unwrap_err();
        assert!(first_launch.to_string().contains("交互终端"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_help_and_noninteractive_destructive_guards_are_explicit() {
        run_mcp_command(vec!["--help".into()]).await.unwrap();
        run_config_command(vec!["--help".into()]).await.unwrap();
        assert!(ConfigCli::try_parse_from(["rivo config", "provider-profile", "--help"]).is_err());
        run_sessions_command(vec!["--help".into()]).await.unwrap();
        diagnostics::run(vec!["--help".into()]).await.unwrap();

        let deletion =
            confirm_history_deletion_with_terminal("fixture history", false, false, false)
                .unwrap_err();
        assert!(deletion.to_string().contains("需要交互确认"));
        assert!(confirm_history_deletion("fixture history", true).is_ok());
        for args in [
            vec!["wrong".into()],
            vec!["create".into()],
            vec!["create".into(), "out.zip".into(), "--invalid".into()],
        ] {
            assert!(diagnostics::run(args).await.is_err());
        }
    }

    #[test]
    fn removed_model_objects_and_flags_are_rejected() {
        for command in [
            "endpoint",
            "binding",
            "provider-profile",
            "migrate",
            "routing",
        ] {
            assert!(ConfigCli::try_parse_from(["rivo config", command]).is_err());
        }
        for flag in [
            "--provider",
            "--provider-profile",
            "--base-url",
            "--credential-name",
        ] {
            assert!(RunCli::try_parse_from(["rivo run", flag, "old", "--prompt", "task"]).is_err());
        }
        let cli = RunCli::try_parse_from([
            "rivo run",
            "--api-key-env",
            "FIXTURE_KEY",
            "--prompt",
            "task",
        ])
        .unwrap();
        assert_eq!(cli.api_key_env.as_deref(), Some("FIXTURE_KEY"));
        for old in ["profiles", "endpoints", "bindings", "provider_profiles"] {
            assert!(rivo_session::parse_file_config(&format!("version = 3\n[{old}]\n")).is_err());
        }
    }

    #[derive(Default)]
    struct RecordingA2aIdentityStore(std::sync::Mutex<Vec<String>>);

    impl A2aIdentityStore for RecordingA2aIdentityStore {
        fn delete(&self, identity_ref: &str) -> Result<(), rivo_a2a::A2aError> {
            self.0.lock().unwrap().push(identity_ref.into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a2a_peer_commands_only_show_safe_metadata_and_revoke_local_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a2a.db");
        let store = rivo_storage::Store::open(&path).unwrap();
        store
            .save_a2a_peer(&rivo_storage::StoredA2aPeer {
                snapshot: rivo_a2a::A2aTrustSnapshot {
                    peer_id: A2aPeerId("peer-1".into()),
                    interface_url: "https://10.0.0.3:8443/a2a".into(),
                    protocol_binding: "JSONRPC".into(),
                    tenant: None,
                    spki_sha256: "a".repeat(64),
                    agent_card_id: "card-1".into(),
                    agent_card_jcs_sha256: "b".repeat(64),
                    allowed_skills: ["research".into()].into_iter().collect(),
                    spec_revision: rivo_a2a::SPEC_REVISION.into(),
                    wire_protocol_version: "1.0".into(),
                    status: rivo_a2a::A2aTrustState::Trusted,
                    approved_at: "2026-07-31T00:00:00Z".into(),
                },
                identity_ref: Some("a2a-peer-1".into()),
            })
            .unwrap();
        let identities = RecordingA2aIdentityStore::default();
        revoke_a2a_peer_with_identity(&store, &A2aPeerId("peer-1".into()), &identities).unwrap();
        assert_eq!(
            identities.0.lock().unwrap().as_slice(),
            ["a2a-peer-1".to_owned()]
        );
        assert_eq!(
            store
                .a2a_peer(&A2aPeerId("peer-1".into()))
                .unwrap()
                .unwrap()
                .snapshot
                .status,
            rivo_a2a::A2aTrustState::Revoked
        );
        assert!(
            run_a2a_command_at(&path, vec!["inspect".into(), "missing".into()])
                .await
                .is_err()
        );
    }

    #[test]
    fn psk_tls_spki_is_stable_only_for_the_same_in_memory_secret() {
        let bind = "192.168.8.11".parse().unwrap();
        let first_secret = zeroize::Zeroizing::new([7u8; 32]);
        let second_secret = zeroize::Zeroizing::new([8u8; 32]);
        let (first, _) = ephemeral_psk_server_material(bind, &first_secret).unwrap();
        let (restarted, _) = ephemeral_psk_server_material(bind, &first_secret).unwrap();
        let (different, _) = ephemeral_psk_server_material(bind, &second_secret).unwrap();
        assert_eq!(
            server_spki_sha256(&first).unwrap(),
            server_spki_sha256(&restarted).unwrap()
        );
        assert_ne!(
            server_spki_sha256(&first).unwrap(),
            server_spki_sha256(&different).unwrap()
        );
    }

    #[test]
    fn a2a_discovery_timeout_is_bounded_and_does_not_accept_extra_arguments() {
        assert_eq!(
            parse_a2a_discovery_timeout(&["discover".into()]).unwrap(),
            5
        );
        assert_eq!(
            parse_a2a_discovery_timeout(&[
                "discover".into(),
                "--timeout-seconds".into(),
                "30".into(),
            ])
            .unwrap(),
            30
        );
        assert!(parse_a2a_discovery_timeout(&[
            "discover".into(),
            "--timeout-seconds".into(),
            "0".into(),
        ])
        .is_err());
        assert!(parse_a2a_discovery_timeout(&[
            "discover".into(),
            "--unexpected".into(),
            "1".into(),
        ])
        .is_err());
    }

    #[test]
    fn a2a_delegate_requires_stdin_and_explicit_external_data_confirmation() {
        assert_eq!(
            parse_a2a_delegate_args(&[
                "delegate".into(),
                "peer-01".into(),
                "research".into(),
                "--message-stdin".into(),
                "--yes".into(),
                "--stream".into(),
            ])
            .unwrap(),
            ("peer-01".into(), "research".into(), true)
        );
        assert!(parse_a2a_delegate_args(&[
            "delegate".into(),
            "peer-01".into(),
            "research".into(),
            "--message-stdin".into(),
        ])
        .is_err());
        assert!(parse_a2a_delegate_args(&[
            "delegate".into(),
            "peer-01".into(),
            "research".into(),
            "--yes".into(),
        ])
        .is_err());
    }

    #[test]
    fn a2a_control_requires_allowed_method_stdin_and_confirmation() {
        assert_eq!(
            parse_a2a_control_args(&[
                "control".into(),
                "peer-01".into(),
                "instance-01".into(),
                "health/read".into(),
                "--params-stdin".into(),
            ])
            .unwrap(),
            (
                "peer-01".into(),
                "instance-01".into(),
                "health/read".into(),
                false
            )
        );
        assert!(parse_a2a_control_args(&[
            "control".into(),
            "peer-01".into(),
            "instance-01".into(),
            "instance/stop".into(),
            "--params-stdin".into(),
        ])
        .is_err());
        assert!(
            parse_a2a_control_args(&[
                "control".into(),
                "peer-01".into(),
                "instance-01".into(),
                "instance/stop".into(),
                "--params-stdin".into(),
                "--yes".into(),
            ])
            .unwrap()
            .3
        );
        assert!(parse_a2a_control_args(&[
            "control".into(),
            "peer/unsafe".into(),
            "instance-01".into(),
            "health/read".into(),
            "--params-stdin".into(),
        ])
        .is_err());
        assert!(parse_a2a_control_args(&[
            "control".into(),
            "peer-01".into(),
            "instance-01".into(),
            "unknown/method".into(),
            "--params-stdin".into(),
        ])
        .is_err());
        assert!(parse_a2a_control_args(&[
            "control".into(),
            "peer-01".into(),
            "instance-01".into(),
            "health/read".into(),
            "--params-stdin".into(),
            "--params-stdin".into(),
        ])
        .is_err());
    }

    #[test]
    fn a2a_pair_requires_explicit_peer_endpoint_and_spki() {
        assert_eq!(
            parse_a2a_pair_args(&[
                "pair".into(),
                "peer-01".into(),
                "--endpoint".into(),
                "https://10.0.0.3:8443/a2a".into(),
                "--spki-sha256".into(),
                "a".repeat(64),
            ])
            .unwrap()
            .0,
            "peer-01"
        );
        assert!(parse_a2a_pair_args(&["pair".into(), "peer/unsafe".into()]).is_err());
    }

    #[test]
    fn local_a2a_automation_needs_both_explicit_test_switches() {
        assert!(!local_a2a_test_confirmation_enabled(false, false));
        assert!(!local_a2a_test_confirmation_enabled(true, false));
        assert!(!local_a2a_test_confirmation_enabled(false, true));
        assert!(local_a2a_test_confirmation_enabled(true, true));
    }

    #[test]
    fn a2a_server_commands_require_owner_config_and_exact_forms() {
        assert_eq!(
            parse_a2a_serve_args(&[
                "serve".into(),
                "research-lan".into(),
                "--config".into(),
                "owner-a2a-server.toml".into(),
            ])
            .unwrap()
            .1,
            Some("research-lan".into())
        );
        assert!(parse_a2a_serve_args(&["serve".into(), "--config".into()]).is_err());
        assert_eq!(
            parse_a2a_pairings_args(&[
                "pairings".into(),
                "list".into(),
                "--config".into(),
                "owner-a2a-server.toml".into(),
            ])
            .unwrap()
            .0,
            "list"
        );
        assert!(parse_a2a_pairings_args(&[
            "pairings".into(),
            "approve".into(),
            "pair-1".into(),
            "--wrong".into(),
            "owner-a2a-server.toml".into(),
        ])
        .is_err());
        let approval_id = format!("approval-{}", "a".repeat(64));
        assert_eq!(
            parse_a2a_approvals_args(&[
                "approvals".into(),
                "approve".into(),
                approval_id.clone(),
                "--config".into(),
                "owner-a2a-server.toml".into(),
            ])
            .unwrap()
            .1,
            Some(approval_id)
        );
        assert!(parse_a2a_approvals_args(&[
            "approvals".into(),
            "approve".into(),
            "approval-not-a-hash".into(),
            "--config".into(),
            "owner-a2a-server.toml".into(),
        ])
        .is_err());
    }

    struct RecoveringA2aTransport(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl rivo_a2a::A2aTransport for RecoveringA2aTransport {
        async fn request(
            &self,
            _trust: &rivo_a2a::A2aTrustSnapshot,
            request: rivo_a2a::JsonRpcRequest,
        ) -> Result<rivo_a2a::JsonRpcResponse, rivo_a2a::A2aError> {
            self.0.lock().unwrap().push(request.method.clone());
            if request.method != "tasks/get" {
                return Err(rivo_a2a::A2aError::Transport);
            }
            Ok(rivo_a2a::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: serde_json::Value::String(request.id),
                result: Some(serde_json::json!({
                    "id":"task-1", "status":{"state":"WORKING"},
                    "metadata":{"io.rivo.a2a.v1":{"revision":2}}
                })),
                error: None,
            })
        }

        async fn subscribe_sse(
            &self,
            _trust: &rivo_a2a::A2aTrustSnapshot,
            task_id: &rivo_a2a::A2aTaskId,
        ) -> Result<Vec<rivo_a2a::SseEvent>, rivo_a2a::A2aError> {
            self.0.lock().unwrap().push("tasks/resubscribe:sse".into());
            Ok(vec![rivo_a2a::SseEvent {
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
    async fn a2a_recovery_persists_snapshot_and_sse_terminal_without_resending_delegation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a2a.db");
        let store = rivo_storage::Store::open(&path).unwrap();
        let peer = rivo_storage::StoredA2aPeer {
            snapshot: rivo_a2a::A2aTrustSnapshot {
                peer_id: A2aPeerId("peer-1".into()),
                interface_url: "https://10.0.0.3:8443/a2a".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: None,
                spki_sha256: "a".repeat(64),
                agent_card_id: "card-1".into(),
                agent_card_jcs_sha256: "b".repeat(64),
                allowed_skills: ["research".into()].into_iter().collect(),
                spec_revision: rivo_a2a::SPEC_REVISION.into(),
                wire_protocol_version: rivo_a2a::WIRE_PROTOCOL_VERSION.into(),
                status: rivo_a2a::A2aTrustState::Trusted,
                approved_at: "2026-07-31T00:00:00Z".into(),
            },
            identity_ref: Some("a2a-peer-1".into()),
        };
        store.save_a2a_peer(&peer).unwrap();
        let initial = rivo_storage::StoredA2aTask {
            peer_id: peer.snapshot.peer_id.clone(),
            task_id: rivo_a2a::A2aTaskId("task-1".into()),
            request_sha256: "c".repeat(64),
            state: rivo_a2a::A2aTaskState::Submitted,
            revision: Some(1),
        };
        store.save_a2a_task(&initial).unwrap();
        let transport = Arc::new(RecoveringA2aTransport(std::sync::Mutex::new(Vec::new())));
        let saved = recover_a2a_task_to_storage(
            &path,
            &peer,
            &initial,
            &A2aTaskClient::new(transport.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            saved.iter().map(|task| task.state).collect::<Vec<_>>(),
            vec![
                rivo_a2a::A2aTaskState::Working,
                rivo_a2a::A2aTaskState::Completed
            ]
        );
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec!["tasks/get", "tasks/resubscribe:sse"]
        );
        let persisted = store
            .a2a_task(&peer.snapshot.peer_id, &initial.task_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.state, rivo_a2a::A2aTaskState::Completed);
        assert_eq!(persisted.revision, Some(3));
        assert!(store
            .save_a2a_task(&rivo_storage::StoredA2aTask {
                state: rivo_a2a::A2aTaskState::Working,
                revision: Some(2),
                ..persisted
            })
            .is_err());
    }

    #[test]
    fn a2a_recover_requires_stable_peer_and_task_identifiers() {
        assert_eq!(
            parse_a2a_recover_args(&["recover".into(), "peer-1".into(), "task-1".into()]).unwrap(),
            ("peer-1".into(), "task-1".into())
        );
        assert!(
            parse_a2a_recover_args(&["recover".into(), "peer/unsafe".into(), "task-1".into()])
                .is_err()
        );
    }

    #[test]
    fn a2a_cancel_requires_explicit_confirmation_and_stable_identifiers() {
        assert_eq!(
            parse_a2a_cancel_args(&[
                "cancel".into(),
                "peer-1".into(),
                "task-1".into(),
                "--yes".into(),
            ])
            .unwrap(),
            ("peer-1".into(), "task-1".into())
        );
        assert!(
            parse_a2a_cancel_args(&["cancel".into(), "peer-1".into(), "task-1".into(),]).is_err()
        );
        assert!(parse_a2a_cancel_args(&[
            "cancel".into(),
            "peer/unsafe".into(),
            "task-1".into(),
            "--yes".into(),
        ])
        .is_err());
    }
}

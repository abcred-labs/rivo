use async_trait::async_trait;
use rivo_core::{EvidenceRef, EvidenceSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::lookup_host;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        program: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        /// Per-server environment injection. Maps the child environment variable name to the name
        /// of a parent-process environment variable to read at connect time. Only variable *names*
        /// are stored in config, never secret values.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: String,
        header_env: BTreeMap<String, String>,
        /// 远端鉴权方式。默认 `None`（凭据由 `header_env` 静态注入）。`Bearer` 从父进程环境变量读取
        /// PAT/API token；`OAuth` 走 OAuth 2.1 授权码 + PKCE。
        #[serde(default)]
        auth: crate::oauth::McpHttpAuth,
        /// 显式放行回环/私网目标（自托管 ADO Server / Figma Dev Mode 本地端口等）。
        /// 仅影响本 MCP server 的 connect 校验，绝不波及 web_fetch 的 SSRF 防护。
        #[serde(default)]
        allow_loopback: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    #[serde(default)]
    pub allowed_capabilities: BTreeSet<String>,
    /// MCP tools are exposed only through an explicitly selected named toolset.
    #[serde(default)]
    pub toolsets: BTreeMap<String, McpToolsetConfig>,
    /// 是否允许该 server 通过 `sampling/createMessage` 驱动本地 LLM 补全。默认关闭：远端 server
    /// 驱动本地模型有成本/安全影响，须显式 opt-in（CLI `--allow-sampling`）。
    #[serde(default)]
    pub allow_sampling: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpToolRisk {
    ReadOnly,
    Action,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct McpToolsetConfig {
    /// Remote tool names, matched exactly. Empty toolsets are invalid when enabled.
    #[serde(default)]
    pub tools: BTreeSet<String>,
    #[serde(default)]
    pub risk: McpToolRisk,
}

impl McpServerConfig {
    /// 返回当前 Turn 允许暴露的 remote tools。未配置 toolsets 的旧 server 保持兼容并返回
    /// `None`（调用方可暴露全部已发现 tools）；一旦配置 toolsets，未选择即不暴露工具。
    pub fn selected_tool_names(
        &self,
        selected_toolsets: &BTreeSet<String>,
    ) -> Result<Option<BTreeSet<String>>, McpError> {
        if self.toolsets.is_empty() {
            return Ok(None);
        }
        let mut names = BTreeSet::new();
        for toolset in selected_toolsets {
            let config = self.toolsets.get(toolset).ok_or_else(|| {
                McpError::Protocol(format!(
                    "MCP server {} 不存在 toolset {}",
                    self.name, toolset
                ))
            })?;
            if config.tools.is_empty() {
                return Err(McpError::Protocol(format!(
                    "MCP server {} 的 toolset {} 为空",
                    self.name, toolset
                )));
            }
            names.extend(config.tools.iter().cloned());
        }
        Ok(Some(names))
    }
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP server 不在 allowlist: {0}")]
    NotAllowed(String),
    #[error("MCP server 未在当前 Turn 启用: {0}")]
    NotEnabled(String),
    #[error("MCP 能力数量超过 128 项")]
    CapabilityLimit,
    #[error("MCP 调用超时")]
    Timeout,
    #[error("MCP 协议错误: {0}")]
    Protocol(String),
}

const MAX_MCP_RESULT_BYTES: usize = 1024 * 1024;

/// server 发起请求（elicitation/sampling）被本地处理器拒绝或失败时的 JSON-RPC 错误。
#[derive(Debug, Clone)]
pub struct McpHandlerError {
    pub code: i64,
    pub message: String,
}

impl McpHandlerError {
    /// 客户端主动拒绝某项 server 能力（默认 headless 行为）。
    pub fn declined(feature: &str) -> Self {
        Self {
            code: -32000,
            message: format!("客户端未启用 {feature}，已拒绝该请求"),
        }
    }
}

/// 处理 MCP server 发起的请求与通知（G7：elicitation / sampling / tools list_changed）。
///
/// 默认实现对 `elicit`/`create_message` 一律**安全拒绝**，使非交互/CI 场景下 server 的越权请求
/// 得到明确的 JSON-RPC 错误而非挂起；上层（agent/cli）可提供真正接入交互面与 LLM 的实现。
#[async_trait]
pub trait McpServerHandler: Send + Sync {
    /// 处理 `elicitation/create`，返回 JSON-RPC result（含 `action` 与可选 `content`）。
    ///
    /// 默认按 MCP 规范以 `{"action":"decline"}` result 婉拒（而非 JSON-RPC error），使 server 能
    /// 优雅地继续而非视为协议失败；接入真正交互面的实现可覆写以返回 `accept`/用户输入。
    async fn elicit(&self, _params: Value) -> Result<Value, McpHandlerError> {
        Ok(json!({"action": "decline"}))
    }

    /// 处理 `sampling/createMessage`，返回 result（LLM 生成的消息）。
    async fn create_message(&self, _params: Value) -> Result<Value, McpHandlerError> {
        Err(McpHandlerError::declined("sampling"))
    }

    /// `notifications/tools/list_changed` 回调；client 亦会置位 `tools_changed` 标志。
    async fn tools_list_changed(&self) {}
}

/// 默认处理器：不广告任何 server 能力，收到越权请求一律拒绝。
#[derive(Debug, Default)]
pub struct DecliningServerHandler;

#[async_trait]
impl McpServerHandler for DecliningServerHandler {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDescription {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResourceDescription {
    pub uri: String,
    pub name: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptDescription {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCapabilities {
    pub tools: Vec<McpToolDescription>,
    pub resources: Vec<McpResourceDescription>,
    pub prompts: Vec<McpPromptDescription>,
}

pub fn resource_evidence(
    server: &str,
    resource: &McpResourceDescription,
    value: &Value,
    max_bytes: usize,
) -> Result<EvidenceRef, McpError> {
    let raw = serde_json::to_vec(value).map_err(|error| McpError::Protocol(error.to_string()))?;
    let truncated = raw.len() > max_bytes;
    let excerpt = String::from_utf8_lossy(&raw[..raw.len().min(max_bytes)]).into_owned();
    Ok(EvidenceRef {
        id: format!("mcp-{}-{}", server, resource.uri),
        source: EvidenceSource::Mcp {
            server: server.into(),
            uri: resource.uri.clone(),
            mime_type: resource.mime_type.clone(),
        },
        excerpt,
        truncated,
    })
}

pub enum McpClient {
    Stdio {
        child: Box<Child>,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
        next_id: u64,
        /// server 发起请求/通知的处理器（默认拒绝）。
        handler: Arc<dyn McpServerHandler>,
        /// 是否在 initialize 中向 server 广告 elicitation/sampling 能力。
        handler_advertised: bool,
        /// 收到 `notifications/tools/list_changed` 后置位，供上层择机重新拉取工具列表。
        tools_changed: bool,
    },
    StreamableHttp {
        client: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        next_id: u64,
        allow_localhost: bool,
        /// Captured from the `Mcp-Session-Id` response header on initialize and echoed back on
        /// every subsequent request, per the MCP Streamable HTTP transport.
        session_id: Option<String>,
        /// Negotiated protocol version returned by `initialize`, sent as `MCP-Protocol-Version`.
        protocol_version: Option<String>,
        /// server 发起请求/通知的处理器（默认拒绝）。
        handler: Arc<dyn McpServerHandler>,
        /// 是否在 initialize 中向 server 广告 elicitation/sampling 能力。
        handler_advertised: bool,
        /// 收到 `notifications/tools/list_changed` 后置位，供上层择机重新拉取工具列表。
        tools_changed: bool,
    },
}

/// connect 可选项。默认不放行私网/回环、不接 OAuth store（沿用静态 header/env 行为）。
#[derive(Default)]
pub struct McpConnectOptions<'a> {
    /// 放行私网/回环目标（仅 CLI 显式授权时使用；绝不影响 web_fetch 的 SSRF 防护）。
    pub unrestricted: bool,
    /// OAuth 会话存储（keychain/内存）。提供后 connect 对 `OAuth` server 自解析 Authorization 头。
    pub oauth_store: Option<&'a dyn crate::oauth::store::OAuthTokenStore>,
    /// 交互式登录实现；仅在 server 配置 `auto_login` 且此项为 `Some` 时于缺失/过期时触发。
    pub oauth_login: Option<&'a dyn crate::oauth::OAuthLogin>,
    /// server 发起请求/通知（G7 elicitation/sampling/list_changed）的处理器。
    /// 提供后会在 initialize 中向 server 广告对应能力；不提供则默认拒绝且不广告。
    pub server_handler: Option<Arc<dyn McpServerHandler>>,
}

impl McpClient {
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_with_options(config, McpConnectOptions::default()).await
    }

    pub async fn connect_with_network_scope(
        config: &McpServerConfig,
        unrestricted: bool,
    ) -> Result<Self, McpError> {
        Self::connect_with_options(
            config,
            McpConnectOptions {
                unrestricted,
                ..McpConnectOptions::default()
            },
        )
        .await
    }

    pub async fn connect_with_options(
        config: &McpServerConfig,
        options: McpConnectOptions<'_>,
    ) -> Result<Self, McpError> {
        let unrestricted = options.unrestricted;
        let handler_advertised = options.server_handler.is_some();
        let handler: Arc<dyn McpServerHandler> = options
            .server_handler
            .clone()
            .unwrap_or_else(|| Arc::new(DecliningServerHandler));
        match &config.transport {
            McpTransport::Stdio {
                program,
                args,
                cwd,
                env,
            } => {
                let mut command = Command::new(program);
                command
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                if let Some(cwd) = cwd {
                    command.current_dir(cwd);
                }
                for (key, source) in env {
                    let value = std::env::var(source).map_err(|_| {
                        McpError::Protocol(format!("MCP credential 环境变量 {source} 未设置"))
                    })?;
                    command.env(key, value);
                }
                let mut child = command
                    .spawn()
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| McpError::Protocol("MCP stdio 缺少 stdin".into()))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| McpError::Protocol("MCP stdio 缺少 stdout".into()))?;
                Ok(Self::Stdio {
                    child: Box::new(child),
                    stdin,
                    stdout: BufReader::new(stdout),
                    next_id: 1,
                    handler,
                    handler_advertised,
                    tools_changed: false,
                })
            }
            McpTransport::StreamableHttp {
                url,
                header_env,
                auth,
                allow_loopback,
            } => {
                // 本 server 显式放行回环/私网时，等同于对该 server 开启 unrestricted 网络作用域。
                let unrestricted = unrestricted || *allow_loopback;
                let parsed =
                    reqwest::Url::parse(url).map_err(|e| McpError::Protocol(e.to_string()))?;
                let local_fixture = parsed.scheme() == "http"
                    && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
                if !(parsed.scheme() == "https" || local_fixture || unrestricted)
                    || parsed.username() != ""
                    || parsed.password().is_some()
                {
                    return Err(McpError::Protocol(
                        "MCP HTTP URL 必须是无 userinfo 的 HTTPS（loopback fixture 可使用 HTTP）"
                            .into(),
                    ));
                }
                validate_http_host(&parsed, local_fixture || unrestricted).await?;
                let mut headers = BTreeMap::new();
                for (header, env) in header_env {
                    headers.insert(
                        header.clone(),
                        std::env::var(env).map_err(|_| {
                            McpError::Protocol(format!("MCP credential 环境变量 {env} 未设置"))
                        })?,
                    );
                }
                // Bearer/OAuth 鉴权：解析出的凭据以 Authorization 头注入。OAuth 主路径由
                // connect 从 OS keychain 自解析（命中→注入；过期→静默刷新；缺失/失败→按需登录），
                // token 全程只在内存与 keychain 间流转，绝不落日志。
                if let (crate::oauth::McpHttpAuth::OAuth(oauth_config), Some(store)) =
                    (auth, options.oauth_store)
                {
                    let oauth_http = reqwest::Client::builder()
                        .timeout(Duration::from_secs(30))
                        .build()
                        .map_err(|e| McpError::Protocol(e.to_string()))?;
                    // auto_login 决定缺失/刷新失败时是否交互登录；非交互上下文（无 login 实现）即报错不阻塞。
                    let login = if oauth_config.auto_login {
                        options.oauth_login
                    } else {
                        None
                    };
                    let (name, value) = crate::oauth::authorization_for_connect(
                        &oauth_http,
                        &config.name,
                        url,
                        oauth_config,
                        store,
                        login,
                    )
                    .await
                    .map_err(|error| McpError::Protocol(error.to_string()))?;
                    headers.insert(name, value);
                } else if let Some((name, value)) = auth
                    .resolve_authorization_header()
                    .map_err(|error| McpError::Protocol(error.to_string()))?
                {
                    headers.insert(name, value);
                }
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                Ok(Self::StreamableHttp {
                    client,
                    url: url.clone(),
                    headers,
                    next_id: 1,
                    allow_localhost: local_fixture || unrestricted,
                    session_id: None,
                    protocol_version: None,
                    handler,
                    handler_advertised,
                    tools_changed: false,
                })
            }
        }
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = match self {
            Self::Stdio { next_id, .. } | Self::StreamableHttp { next_id, .. } => {
                let id = *next_id;
                *next_id += 1;
                id
            }
        };
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response: Value = match self {
            Self::Stdio {
                stdin,
                ref mut stdout,
                handler,
                tools_changed,
                ..
            } => {
                let handler = handler.clone();
                let mut line =
                    serde_json::to_vec(&message).map_err(|e| McpError::Protocol(e.to_string()))?;
                line.push(b'\n');
                stdin
                    .write_all(&line)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                // 循环读取，边收边分发 server 请求/通知，直到收到匹配 id 的响应。
                tokio::time::timeout(Duration::from_secs(60), async {
                    let mut turns = 0usize;
                    loop {
                        turns += 1;
                        if turns > 512 {
                            return Err(McpError::Protocol(
                                "MCP stdio server 消息数超过上限".into(),
                            ));
                        }
                        let raw = read_mcp_line(stdout)
                            .await?
                            .ok_or_else(|| McpError::Protocol("MCP stdio 提前结束".into()))?;
                        let value: Value = serde_json::from_slice(&raw)
                            .map_err(|e| McpError::Protocol(e.to_string()))?;
                        match classify_inbound(value) {
                            InboundMessage::Response(value) => return Ok(value),
                            InboundMessage::ServerRequest {
                                id: server_id,
                                method,
                                params,
                            } => {
                                let reply = dispatch_server_request(
                                    handler.as_ref(),
                                    server_id,
                                    &method,
                                    params,
                                )
                                .await;
                                let mut reply_line = serde_json::to_vec(&reply)
                                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                                reply_line.push(b'\n');
                                stdin
                                    .write_all(&reply_line)
                                    .await
                                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                                stdin
                                    .flush()
                                    .await
                                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                            }
                            InboundMessage::Notification { method, .. } => {
                                if method == "notifications/tools/list_changed" {
                                    *tools_changed = true;
                                    handler.tools_list_changed().await;
                                }
                            }
                            InboundMessage::Other => {}
                        }
                    }
                })
                .await
                .map_err(|_| McpError::Timeout)??
            }
            Self::StreamableHttp {
                client,
                url,
                headers,
                allow_localhost,
                session_id,
                protocol_version,
                handler,
                tools_changed,
                ..
            } => {
                let client = client.clone();
                let url = url.clone();
                let headers = headers.clone();
                let allow_localhost = *allow_localhost;
                let protocol_version = protocol_version.clone();
                let handler = handler.clone();
                let session = session_id.clone();
                let (value, new_session, changed) =
                    tokio::time::timeout(Duration::from_secs(60), async move {
                        let mut session = session;
                        let mut request = client
                            .post(url.as_str())
                            .header("Accept", "application/json, text/event-stream")
                            .json(&message);
                        for (name, value) in headers.iter() {
                            request = request.header(name, value);
                        }
                        if let Some(version) = protocol_version.as_deref() {
                            request = request.header("MCP-Protocol-Version", version);
                        }
                        if let Some(id) = session.as_deref() {
                            request = request.header("Mcp-Session-Id", id);
                        }
                        let mut response = request
                            .send()
                            .await
                            .map_err(|e| McpError::Protocol(e.to_string()))?;
                        if !allow_localhost
                            && response
                                .remote_addr()
                                .is_some_and(|address| is_private_address(address.ip()))
                        {
                            return Err(McpError::Protocol("MCP HTTP 对端解析到受限地址".into()));
                        }
                        if !response.status().is_success() {
                            return Err(McpError::Protocol(format!(
                                "MCP HTTP {}",
                                response.status()
                            )));
                        }
                        if let Some(returned) = response
                            .headers()
                            .get("mcp-session-id")
                            .and_then(|value| value.to_str().ok())
                        {
                            session = Some(returned.to_owned());
                        }
                        let content_type = response
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        // 非 SSE：单条 JSON-RPC 响应，按原路径解析。
                        if !content_type
                            .trim_start()
                            .to_ascii_lowercase()
                            .starts_with("text/event-stream")
                        {
                            let (bytes, truncated) = read_response_limited(response).await?;
                            if truncated {
                                return Err(McpError::Protocol("MCP 结果超过 1 MiB".into()));
                            }
                            let value = parse_jsonrpc_payload(&content_type, &bytes, id)?;
                            return Ok((value, session, false));
                        }
                        // SSE：增量读取，边收边分发 server 请求/通知，直到收到匹配 id 的响应。
                        let mut buffer: Vec<u8> = Vec::new();
                        let mut total = 0usize;
                        let mut events = 0usize;
                        let mut tools_changed_local = false;
                        loop {
                            let chunk = response
                                .chunk()
                                .await
                                .map_err(|e| McpError::Protocol(e.to_string()))?;
                            let Some(chunk) = chunk else { break };
                            total += chunk.len();
                            if total > MAX_MCP_RESULT_BYTES {
                                return Err(McpError::Protocol("MCP 结果超过 1 MiB".into()));
                            }
                            buffer.extend_from_slice(&chunk);
                            while let Some(event) = take_sse_event(&mut buffer) {
                                events += 1;
                                if events > 512 {
                                    return Err(McpError::Protocol(
                                        "MCP SSE 事件数超过上限".into(),
                                    ));
                                }
                                let Some(data) = sse_event_data(&event) else {
                                    continue;
                                };
                                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                                    continue;
                                };
                                match classify_inbound(value) {
                                    InboundMessage::Response(value) => {
                                        return Ok((value, session, tools_changed_local));
                                    }
                                    InboundMessage::ServerRequest {
                                        id: server_id,
                                        method,
                                        params,
                                    } => {
                                        let reply = dispatch_server_request(
                                            handler.as_ref(),
                                            server_id,
                                            &method,
                                            params,
                                        )
                                        .await;
                                        post_jsonrpc_message(
                                            &client,
                                            &url,
                                            &headers,
                                            session.as_deref(),
                                            protocol_version.as_deref(),
                                            allow_localhost,
                                            &reply,
                                        )
                                        .await?;
                                    }
                                    InboundMessage::Notification { method, .. } => {
                                        if method == "notifications/tools/list_changed" {
                                            tools_changed_local = true;
                                            handler.tools_list_changed().await;
                                        }
                                    }
                                    InboundMessage::Other => {}
                                }
                            }
                        }
                        Err(McpError::Protocol("MCP SSE 响应缺少 JSON-RPC 消息".into()))
                    })
                    .await
                    .map_err(|_| McpError::Timeout)??;
                *session_id = new_session;
                if changed {
                    *tools_changed = true;
                }
                value
            }
        };
        if let Some(error) = response.get("error") {
            return Err(McpError::Protocol(error.to_string()));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol("MCP 响应缺少 result".into()))
    }
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let message = json!({"jsonrpc":"2.0","method":method,"params":params});
        match self {
            Self::Stdio { stdin, .. } => {
                let mut line =
                    serde_json::to_vec(&message).map_err(|e| McpError::Protocol(e.to_string()))?;
                line.push(b'\n');
                stdin
                    .write_all(&line)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
            }
            Self::StreamableHttp {
                client,
                url,
                headers,
                allow_localhost,
                session_id,
                protocol_version,
                ..
            } => {
                let mut request = client
                    .post(url.as_str())
                    .header("Accept", "application/json, text/event-stream")
                    .json(&message);
                for (name, value) in headers.iter() {
                    request = request.header(name, value);
                }
                if let Some(version) = protocol_version.as_deref() {
                    request = request.header("MCP-Protocol-Version", version);
                }
                if let Some(id) = session_id.as_deref() {
                    request = request.header("Mcp-Session-Id", id);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                if !*allow_localhost
                    && response
                        .remote_addr()
                        .is_some_and(|address| is_private_address(address.ip()))
                {
                    return Err(McpError::Protocol("MCP HTTP 对端解析到受限地址".into()));
                }
                if !response.status().is_success() {
                    return Err(McpError::Protocol(format!(
                        "MCP HTTP {}",
                        response.status()
                    )));
                }
            }
        }
        Ok(())
    }
    pub async fn initialize(&mut self) -> Result<Value, McpError> {
        // 仅当上层提供了 server 处理器时才广告 elicitation/sampling 能力，避免 headless 场景
        // 诱导 server 发起本地会拒绝的请求。
        let capabilities = if self.handler_advertised() {
            json!({"elicitation": {}, "sampling": {}})
        } else {
            json!({})
        };
        let result = self.request("initialize", json!({"protocolVersion":"2025-06-18","capabilities":capabilities,"clientInfo":{"name":"rivo","version":"0.1.0"}})).await?;
        if let Self::StreamableHttp {
            protocol_version, ..
        } = self
        {
            if let Some(version) = result.get("protocolVersion").and_then(|v| v.as_str()) {
                *protocol_version = Some(version.to_owned());
            }
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(result)
    }

    /// 是否已配置 server 处理器（决定 initialize 是否广告 elicitation/sampling 能力）。
    fn handler_advertised(&self) -> bool {
        match self {
            Self::Stdio {
                handler_advertised, ..
            }
            | Self::StreamableHttp {
                handler_advertised, ..
            } => *handler_advertised,
        }
    }

    /// 取出并清除「工具列表已变更」标志。收到 `notifications/tools/list_changed` 后返回 `true`，
    /// 上层据此择机重新 `list_tools`。
    pub fn take_tools_changed(&mut self) -> bool {
        match self {
            Self::Stdio { tools_changed, .. } | Self::StreamableHttp { tools_changed, .. } => {
                std::mem::replace(tools_changed, false)
            }
        }
    }

    /// 打开一条独立的 GET SSE 长连监听流（MCP Streamable HTTP 的 server→client 通道），接收
    /// server 主动发起的请求/通知，直至流关闭：server 请求经处理器分发并独立 POST 回发；
    /// `notifications/tools/list_changed` 置位 `tools_changed`（经 `take_tools_changed` 读取）。
    ///
    /// stdio 的 server 消息已在 `request` 循环内处理，故此方法仅对 StreamableHttp 生效，其它传输
    /// 直接返回 `Ok(())`。server 若不提供 GET 通道（`405`）亦安静返回。方法期间独占 `&mut self`，
    /// 由调用方决定何时运行；不设总量上限以支持长驻监听，仅对单条事件设 1 MiB 上限并加 5 分钟安全超时。
    pub async fn listen_server_stream(&mut self) -> Result<(), McpError> {
        let Self::StreamableHttp {
            client,
            url,
            headers,
            allow_localhost,
            session_id,
            protocol_version,
            handler,
            tools_changed,
            ..
        } = self
        else {
            return Ok(());
        };
        let client = client.clone();
        let url = url.clone();
        let headers = headers.clone();
        let allow_localhost = *allow_localhost;
        let protocol_version = protocol_version.clone();
        let session = session_id.clone();
        let handler = handler.clone();
        let changed = tokio::time::timeout(Duration::from_secs(300), async move {
            let mut request = client
                .get(url.as_str())
                .header("Accept", "text/event-stream");
            for (name, value) in headers.iter() {
                request = request.header(name, value);
            }
            if let Some(version) = protocol_version.as_deref() {
                request = request.header("MCP-Protocol-Version", version);
            }
            if let Some(id) = session.as_deref() {
                request = request.header("Mcp-Session-Id", id);
            }
            let mut response = request
                .send()
                .await
                .map_err(|e| McpError::Protocol(e.to_string()))?;
            if !allow_localhost
                && response
                    .remote_addr()
                    .is_some_and(|address| is_private_address(address.ip()))
            {
                return Err(McpError::Protocol("MCP HTTP 对端解析到受限地址".into()));
            }
            // 部分 server 不提供 GET 监听通道，返回 405；视为无 server→client 流，安静结束。
            if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                return Ok(false);
            }
            if !response.status().is_success() {
                return Err(McpError::Protocol(format!(
                    "MCP HTTP {}",
                    response.status()
                )));
            }
            let mut buffer: Vec<u8> = Vec::new();
            let mut tools_changed_local = false;
            loop {
                let chunk = response
                    .chunk()
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let Some(chunk) = chunk else { break };
                if buffer.len() + chunk.len() > MAX_MCP_RESULT_BYTES {
                    return Err(McpError::Protocol("MCP SSE 单事件超过 1 MiB".into()));
                }
                buffer.extend_from_slice(&chunk);
                while let Some(event) = take_sse_event(&mut buffer) {
                    let Some(data) = sse_event_data(&event) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    match classify_inbound(value) {
                        InboundMessage::ServerRequest { id, method, params } => {
                            let reply =
                                dispatch_server_request(handler.as_ref(), id, &method, params)
                                    .await;
                            post_jsonrpc_message(
                                &client,
                                &url,
                                &headers,
                                session.as_deref(),
                                protocol_version.as_deref(),
                                allow_localhost,
                                &reply,
                            )
                            .await?;
                        }
                        InboundMessage::Notification { method } => {
                            if method == "notifications/tools/list_changed" {
                                tools_changed_local = true;
                                handler.tools_list_changed().await;
                            }
                        }
                        InboundMessage::Response(_) | InboundMessage::Other => {}
                    }
                }
            }
            Ok(tools_changed_local)
        })
        .await
        .map_err(|_| McpError::Timeout)??;
        if changed {
            *tools_changed = true;
        }
        Ok(())
    }
    /// Collect a paginated `*/list` result, following `nextCursor` until exhausted. The 128-item
    /// capability cap is enforced across pages so a large server fails loudly instead of silently
    /// truncating. A page-count guard prevents a misbehaving server from looping forever.
    async fn list_paged(&mut self, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let mut items: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..128 {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let value = self.request(method, params).await?;
            if let Some(array) = value.get(key).and_then(Value::as_array) {
                items.extend(array.iter().cloned());
            }
            if items.len() > 128 {
                return Err(McpError::CapabilityLimit);
            }
            match value.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(items),
            }
        }
        Err(McpError::Protocol("MCP list 分页页数超过上限".into()))
    }
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDescription>, McpError> {
        let items = self.list_paged("tools/list", "tools").await?;
        let tools: Vec<McpToolDescription> = serde_json::from_value(Value::Array(items))
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        Self::validate_count(&tools)?;
        Ok(tools)
    }
    pub async fn list_resources(&mut self) -> Result<Vec<McpResourceDescription>, McpError> {
        let items = self.list_paged("resources/list", "resources").await?;
        let resources: Vec<McpResourceDescription> = serde_json::from_value(Value::Array(items))
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        Self::validate_count(&resources)?;
        Ok(resources)
    }
    pub async fn list_prompts(&mut self) -> Result<Vec<McpPromptDescription>, McpError> {
        let items = self.list_paged("prompts/list", "prompts").await?;
        let prompts: Vec<McpPromptDescription> = serde_json::from_value(Value::Array(items))
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        Self::validate_count(&prompts)?;
        Ok(prompts)
    }
    pub async fn discover(&mut self) -> Result<McpCapabilities, McpError> {
        Ok(McpCapabilities {
            tools: self.list_tools().await?,
            resources: self.list_resources().await?,
            prompts: self.list_prompts().await?,
        })
    }
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
    pub async fn read_resource(&mut self, uri: &str) -> Result<Value, McpError> {
        self.request("resources/read", json!({"uri":uri})).await
    }
    pub async fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.request("prompts/get", json!({"name":name,"arguments":arguments}))
            .await
    }
    pub async fn shutdown(&mut self) {
        if let Self::Stdio { child, .. } = self {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    fn validate_count<T>(values: &[T]) -> Result<(), McpError> {
        if values.len() > 128 {
            Err(McpError::CapabilityLimit)
        } else {
            Ok(())
        }
    }
}

async fn read_mcp_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>, McpError> {
    let mut response = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let read = reader
            .read(&mut byte)
            .await
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        if read == 0 {
            return Ok((!response.is_empty()).then_some(response));
        }
        response.push(byte[0]);
        if response.len() > MAX_MCP_RESULT_BYTES {
            return Err(McpError::Protocol("MCP 结果超过 1 MiB".into()));
        }
        if byte[0] == b'\n' {
            return Ok(Some(response));
        }
    }
}

async fn validate_http_host(url: &reqwest::Url, allow_localhost: bool) -> Result<(), McpError> {
    let host = url
        .host_str()
        .ok_or_else(|| McpError::Protocol("MCP URL 缺少主机名".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| McpError::Protocol("MCP URL 缺少端口".into()))?;
    let addresses = lookup_host((host, port))
        .await
        .map_err(|error| McpError::Protocol(error.to_string()))?;
    for address in addresses {
        if !allow_localhost && is_private_address(address.ip()) {
            return Err(McpError::Protocol(format!(
                "MCP URL 解析到受限地址 {}",
                address.ip()
            )));
        }
    }
    Ok(())
}

fn is_private_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
        }
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

async fn read_response_limited(
    mut response: reqwest::Response,
) -> Result<(Vec<u8>, bool), McpError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| McpError::Protocol(e.to_string()))?
    {
        let remaining = 1024 * 1024usize - bytes.len().min(1024 * 1024);
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok((bytes, true));
        }
    }
    Ok((bytes, false))
}

/// 服务器经 Streamable HTTP / stdio 发来的一条 JSON-RPC 消息的分类。
enum InboundMessage {
    /// 与本次请求 id 匹配的响应。
    Response(Value),
    /// server 发起的请求（elicitation/sampling 等），需回一条响应。
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// server 发来的通知（如 tools list_changed）。
    Notification { method: String },
    /// 其它（针对别的 id 的响应等），忽略。
    Other,
}

/// 依据 JSON-RPC 语义对入站消息分类：有 method+id 为 server 请求，有 method 无 id 为通知，
/// 无 method 且含 result/error 即本次请求的响应（stdio / 单 POST 均为串行，同一时刻只有一个
/// 在途请求，故无需按 id 严格匹配——这也兼容会回显固定 id 的简单 server）。
fn classify_inbound(value: Value) -> InboundMessage {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match method {
        Some(method) if value.get("id").is_some() => InboundMessage::ServerRequest {
            id: value.get("id").cloned().unwrap_or(Value::Null),
            method,
            params: value.get("params").cloned().unwrap_or_else(|| json!({})),
        },
        Some(method) => InboundMessage::Notification { method },
        None if value.get("result").is_some() || value.get("error").is_some() => {
            InboundMessage::Response(value)
        }
        None => InboundMessage::Other,
    }
}

/// 将 server 请求路由到处理器，构造对应的 JSON-RPC 响应消息（result 或 error）。
async fn dispatch_server_request(
    handler: &dyn McpServerHandler,
    id: Value,
    method: &str,
    params: Value,
) -> Value {
    let outcome = match method {
        "elicitation/create" => handler.elicit(params).await,
        "sampling/createMessage" => handler.create_message(params).await,
        other => Err(McpHandlerError {
            code: -32601,
            message: format!("不支持的 server 请求方法: {other}"),
        }),
    };
    match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": error.code, "message": error.message},
        }),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 从累积字节中取出下一条完整 SSE 事件（以空行分隔），成功则从前端移除该事件与分隔符。
fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (index, sep_len) = [b"\r\n\r\n".as_slice(), b"\n\n".as_slice()]
        .iter()
        .filter_map(|sep| find_subslice(buffer, sep).map(|index| (index, sep.len())))
        .min_by_key(|(index, _)| *index)?;
    let event: Vec<u8> = buffer.drain(..index + sep_len).collect();
    Some(event[..index].to_vec())
}

/// 拼接一条 SSE 事件里的所有 `data:` 行，返回其 JSON 文本。
fn sse_event_data(event: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(event);
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
    }
    (!data.is_empty()).then_some(data)
}

/// 经 Streamable HTTP 回发一条 JSON-RPC 消息（用于回应 server 发起的请求）。
#[allow(clippy::too_many_arguments)]
async fn post_jsonrpc_message(
    client: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
    allow_localhost: bool,
    message: &Value,
) -> Result<(), McpError> {
    let mut request = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(message);
    for (name, value) in headers.iter() {
        request = request.header(name, value);
    }
    if let Some(version) = protocol_version {
        request = request.header("MCP-Protocol-Version", version);
    }
    if let Some(id) = session_id {
        request = request.header("Mcp-Session-Id", id);
    }
    let response = request
        .send()
        .await
        .map_err(|e| McpError::Protocol(e.to_string()))?;
    if !allow_localhost
        && response
            .remote_addr()
            .is_some_and(|address| is_private_address(address.ip()))
    {
        return Err(McpError::Protocol("MCP HTTP 对端解析到受限地址".into()));
    }
    if !response.status().is_success() {
        return Err(McpError::Protocol(format!(
            "MCP HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

/// Parse a JSON-RPC response body from a Streamable HTTP response. Servers may reply with either
/// `application/json` or an SSE (`text/event-stream`) stream that carries the JSON-RPC message in
/// `data:` frames; the latter may interleave server-initiated notifications we must skip.
fn parse_jsonrpc_payload(
    content_type: &str,
    bytes: &[u8],
    request_id: u64,
) -> Result<Value, McpError> {
    if content_type
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("text/event-stream")
    {
        parse_sse_jsonrpc(bytes, request_id)
    } else {
        serde_json::from_slice(bytes).map_err(|e| McpError::Protocol(e.to_string()))
    }
}

fn parse_sse_jsonrpc(bytes: &[u8], request_id: u64) -> Result<Value, McpError> {
    let text = String::from_utf8_lossy(bytes);
    let mut data_buf = String::new();
    let mut fallback: Option<Value> = None;
    let consider = |data: &str, fallback: &mut Option<Value>| -> Option<Value> {
        let value = serde_json::from_str::<Value>(data).ok()?;
        if value.get("id").and_then(Value::as_u64) == Some(request_id) {
            return Some(value);
        }
        fallback.get_or_insert(value);
        None
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest);
        } else if line.is_empty() && !data_buf.is_empty() {
            if let Some(value) = consider(&data_buf, &mut fallback) {
                return Ok(value);
            }
            data_buf.clear();
        }
    }
    if !data_buf.is_empty() {
        if let Some(value) = consider(&data_buf, &mut fallback) {
            return Ok(value);
        }
    }
    fallback.ok_or_else(|| McpError::Protocol("MCP SSE 响应缺少 JSON-RPC 消息".into()))
}

#[derive(Debug, Default)]
pub struct McpRegistry {
    servers: BTreeMap<String, McpServerConfig>,
}

impl McpRegistry {
    pub fn add(&mut self, server: McpServerConfig) -> Result<(), McpError> {
        if self.servers.contains_key(&server.name) {
            return Err(McpError::Protocol("重复 server 名称".into()));
        }
        self.servers.insert(server.name.clone(), server);
        Ok(())
    }
    pub fn select(
        &self,
        name: &str,
        enabled: &BTreeSet<String>,
    ) -> Result<&McpServerConfig, McpError> {
        let server = self
            .servers
            .get(name)
            .ok_or_else(|| McpError::NotAllowed(name.into()))?;
        if !server.enabled || !enabled.contains(name) {
            return Err(McpError::NotEnabled(name.into()));
        }
        Ok(server)
    }

    pub fn into_servers(self) -> Vec<McpServerConfig> {
        self.servers.into_values().collect()
    }
    pub fn validate_capability_count(count: usize) -> Result<(), McpError> {
        if count > 128 {
            Err(McpError::CapabilityLimit)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stdio_line_reader_rejects_oversized_line_before_unbounded_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            let _ = writer
                .write_all(&vec![b'x'; MAX_MCP_RESULT_BYTES + 1])
                .await;
        });
        let error = read_mcp_line(&mut reader).await.unwrap_err();
        assert!(matches!(error, McpError::Protocol(message) if message.contains("1 MiB")));
        let _ = writer_task.await;
    }

    fn test_http_client(url: String) -> McpClient {
        test_http_client_with_handler(url, Arc::new(DecliningServerHandler), false)
    }

    fn test_http_client_with_handler(
        url: String,
        handler: Arc<dyn McpServerHandler>,
        handler_advertised: bool,
    ) -> McpClient {
        McpClient::StreamableHttp {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            url,
            headers: BTreeMap::new(),
            next_id: 1,
            allow_localhost: true,
            session_id: None,
            protocol_version: None,
            handler,
            handler_advertised,
            tools_changed: false,
        }
    }

    #[derive(Default)]
    struct RecordingHandler {
        elicit_params: std::sync::Mutex<Option<Value>>,
        sample_params: std::sync::Mutex<Option<Value>>,
        list_changed: std::sync::atomic::AtomicBool,
        elicit_result: Value,
        sample_result: Value,
    }

    #[async_trait]
    impl McpServerHandler for RecordingHandler {
        async fn elicit(&self, params: Value) -> Result<Value, McpHandlerError> {
            *self.elicit_params.lock().unwrap() = Some(params);
            Ok(self.elicit_result.clone())
        }
        async fn create_message(&self, params: Value) -> Result<Value, McpHandlerError> {
            *self.sample_params.lock().unwrap() = Some(params);
            Ok(self.sample_result.clone())
        }
        async fn tools_list_changed(&self) {
            self.list_changed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn classify_inbound_distinguishes_requests_notifications_and_responses() {
        match classify_inbound(
            json!({"jsonrpc":"2.0","id":"a","method":"elicitation/create","params":{"x":1}}),
        ) {
            InboundMessage::ServerRequest { id, method, params } => {
                assert_eq!(id, json!("a"));
                assert_eq!(method, "elicitation/create");
                assert_eq!(params["x"], 1);
            }
            _ => panic!("expected server request"),
        }
        assert!(matches!(
            classify_inbound(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})),
            InboundMessage::Notification { .. }
        ));
        assert!(matches!(
            classify_inbound(json!({"jsonrpc":"2.0","id":5,"result":{}})),
            InboundMessage::Response(_)
        ));
        // A bare acknowledgement (no method, no result/error) is ignored.
        assert!(matches!(
            classify_inbound(json!({"jsonrpc":"2.0","id":9})),
            InboundMessage::Other
        ));
    }

    #[test]
    fn sse_event_extraction_splits_on_blank_lines() {
        let mut buffer = b"data: {\"a\":1}\n\ndata: line1\ndata: line2\n\n".to_vec();
        let first = take_sse_event(&mut buffer).unwrap();
        assert_eq!(sse_event_data(&first).unwrap(), "{\"a\":1}");
        let second = take_sse_event(&mut buffer).unwrap();
        assert_eq!(sse_event_data(&second).unwrap(), "line1\nline2");
        assert!(take_sse_event(&mut buffer).is_none());
    }

    #[tokio::test]
    async fn declining_handler_replies_with_error_for_server_requests() {
        // elicitation 婉拒按 MCP 规范以 {action:"decline"} result 返回，而非 JSON-RPC error。
        let elicit = dispatch_server_request(
            &DecliningServerHandler,
            json!("e1"),
            "elicitation/create",
            json!({"message": "name?"}),
        )
        .await;
        assert_eq!(elicit["id"], "e1");
        assert_eq!(elicit["result"]["action"], "decline");
        assert!(elicit.get("error").is_none());
        // sampling 无 decline 形态，未启用时仍以 error 拒绝。
        let reply = dispatch_server_request(
            &DecliningServerHandler,
            json!("s1"),
            "sampling/createMessage",
            json!({}),
        )
        .await;
        assert_eq!(reply["id"], "s1");
        assert!(reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sampling"));
        let unknown = dispatch_server_request(
            &DecliningServerHandler,
            json!(3),
            "unknown/method",
            json!({}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn streamable_http_dispatches_elicitation_during_tool_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // Connection A: tools/call — reply with an SSE stream that first issues a server
            // elicitation request, then (after the client answers on connection B) the final result.
            let (mut a, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 8192];
            let n = a.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("tools/call"));
            a.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            a.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"e1\",\"method\":\"elicitation/create\",\"params\":{\"message\":\"name?\"}}\n\n").await.unwrap();
            a.flush().await.unwrap();
            // Connection B: the client's elicitation response.
            let (mut b, _) = listener.accept().await.unwrap();
            let mut buf2 = vec![0_u8; 8192];
            let n2 = b.read(&mut buf2).await.unwrap();
            let reply = String::from_utf8_lossy(&buf2[..n2]).to_string();
            b.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            b.flush().await.unwrap();
            // Final tool result on connection A.
            a.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n\n").await.unwrap();
            a.flush().await.unwrap();
            reply
        });
        let handler = Arc::new(RecordingHandler {
            elicit_result: json!({"action":"accept","content":{"name":"neo"}}),
            ..Default::default()
        });
        let mut client =
            test_http_client_with_handler(format!("http://{address}/mcp"), handler.clone(), true);
        let result = client.call_tool("ask", json!({})).await.unwrap();
        assert_eq!(result["content"][0]["text"], "done");
        let reply = server.await.unwrap();
        assert!(
            reply.contains("\"e1\""),
            "server must receive reply for id e1"
        );
        assert!(
            reply.contains("accept"),
            "reply must carry elicitation result"
        );
        assert_eq!(
            handler.elicit_params.lock().unwrap().as_ref().unwrap()["message"],
            "name?"
        );
    }

    #[tokio::test]
    async fn streamable_http_notification_sets_tools_changed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut a, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 8192];
            let _ = a.read(&mut buf).await.unwrap();
            a.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            a.write_all(
                b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            )
            .await
            .unwrap();
            a.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[]}}\n\n")
                .await
                .unwrap();
            a.flush().await.unwrap();
        });
        let handler = Arc::new(RecordingHandler::default());
        let mut client =
            test_http_client_with_handler(format!("http://{address}/mcp"), handler.clone(), true);
        client.call_tool("noop", json!({})).await.unwrap();
        server.await.unwrap();
        assert!(
            client.take_tools_changed(),
            "list_changed must set the flag"
        );
        assert!(!client.take_tools_changed(), "flag must clear after read");
        assert!(handler
            .list_changed
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn initialize_advertises_capabilities_when_handler_present() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut a, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 8192];
            let n = a.read(&mut buf).await.unwrap();
            let body = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#;
            a.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    response.len(),
                    response
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            a.flush().await.unwrap();
            // The initialized notification arrives on a second connection.
            let (mut b, _) = listener.accept().await.unwrap();
            let _ = b.read(&mut vec![0_u8; 4096]).await.unwrap();
            b.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            b.flush().await.unwrap();
            body
        });
        let mut client = test_http_client_with_handler(
            format!("http://{address}/mcp"),
            Arc::new(DecliningServerHandler),
            true,
        );
        client.initialize().await.unwrap();
        let body = server.await.unwrap();
        assert!(
            body.contains("elicitation"),
            "initialize must advertise elicitation"
        );
        assert!(
            body.contains("sampling"),
            "initialize must advertise sampling"
        );
    }

    #[tokio::test]
    async fn get_listener_dispatches_server_notifications_and_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // Connection A: the client's GET listen stream. Emit a list_changed notification and a
            // server elicitation request, then close the stream after the client answers on B.
            let (mut a, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 8192];
            let n = a.read(&mut buf).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with("GET "),
                "listener must open the server stream with GET"
            );
            a.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            a.write_all(
                b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            )
            .await
            .unwrap();
            a.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"g1\",\"method\":\"elicitation/create\",\"params\":{\"message\":\"stream?\"}}\n\n").await.unwrap();
            a.flush().await.unwrap();
            // Connection B: the client's elicitation response POST.
            let (mut b, _) = listener.accept().await.unwrap();
            let mut buf2 = vec![0_u8; 8192];
            let n2 = b.read(&mut buf2).await.unwrap();
            let reply = String::from_utf8_lossy(&buf2[..n2]).to_string();
            b.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            b.flush().await.unwrap();
            // Close the listen stream so the client returns.
            drop(a);
            reply
        });
        let handler = Arc::new(RecordingHandler {
            elicit_result: json!({"action":"accept","content":{"ok":true}}),
            ..Default::default()
        });
        let mut client =
            test_http_client_with_handler(format!("http://{address}/mcp"), handler.clone(), true);
        client.listen_server_stream().await.unwrap();
        let reply = server.await.unwrap();
        assert!(
            reply.contains("\"g1\""),
            "server must receive reply for id g1"
        );
        assert!(
            reply.contains("accept"),
            "reply must carry elicitation result"
        );
        assert!(
            client.take_tools_changed(),
            "list_changed must set the flag"
        );
        assert_eq!(
            handler.elicit_params.lock().unwrap().as_ref().unwrap()["message"],
            "stream?"
        );
    }

    #[tokio::test]
    async fn get_listener_returns_quietly_when_stream_unsupported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut a, _) = listener.accept().await.unwrap();
            let _ = a.read(&mut vec![0_u8; 4096]).await.unwrap();
            a.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            a.flush().await.unwrap();
        });
        let mut client = test_http_client_with_handler(
            format!("http://{address}/mcp"),
            Arc::new(DecliningServerHandler),
            true,
        );
        client.listen_server_stream().await.unwrap();
        server.await.unwrap();
        assert!(!client.take_tools_changed());
    }

    #[tokio::test]
    async fn streamable_http_mock_covers_discovery_and_calls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut initialized_notification_seen = false;
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let length = stream.read(&mut request).await.unwrap();
                let body = String::from_utf8_lossy(&request[..length]);
                let (status, response_body) = if body.contains("notifications/initialized") {
                    initialized_notification_seen = true;
                    ("202 Accepted", "")
                } else if body.contains("tools/list") {
                    (
                        "200 OK",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","input_schema":{"type":"object"}}]}}"#,
                    )
                } else if body.contains("resources/list") {
                    (
                        "200 OK",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"resources":[{"uri":"memory://one","name":"one","mimeType":"text/plain"}]}}"#,
                    )
                } else if body.contains("prompts/list") {
                    (
                        "200 OK",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"prompts":[{"name":"summarize","description":"summary"}]}}"#,
                    )
                } else if body.contains("tools/call") {
                    (
                        "200 OK",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#,
                    )
                } else {
                    ("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    response_body.len(), response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            initialized_notification_seen
        });
        let mut client = test_http_client(format!("http://{address}/mcp"));
        client.initialize().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            client.list_resources().await.unwrap()[0].uri,
            "memory://one"
        );
        assert_eq!(client.list_prompts().await.unwrap()[0].name, "summarize");
        assert_eq!(
            client.call_tool("echo", json!({"value":1})).await.unwrap()["content"][0]["text"],
            "ok"
        );
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn stdio_mock_covers_initialize_and_discovery() {
        let (program, args) = if cfg!(windows) {
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "$input | ForEach-Object { if ($_ -notlike '*notifications/initialized*') { '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"echo\",\"input_schema\":{\"type\":\"object\"}}]}}' } }".to_owned(),
                ],
            )
        } else {
            (
                "sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "while IFS= read -r line; do case \"$line\" in *notifications/initialized*) ;; *) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"echo\",\"input_schema\":{\"type\":\"object\"}}]}}' ;; esac; done".to_owned(),
                ],
            )
        };
        let config = McpServerConfig {
            name: "mock".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                program,
                args,
                cwd: None,
                env: Default::default(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: Default::default(),
            allow_sampling: false,
        };
        let mut client = McpClient::connect(&config).await.unwrap();
        client.initialize().await.unwrap();
        assert_eq!(client.list_tools().await.unwrap()[0].name, "echo");
        client.shutdown().await;
    }

    #[test]
    fn named_toolsets_require_explicit_selection() {
        let config = McpServerConfig {
            name: "docs".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                program: "unused".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: [(
                "read".into(),
                McpToolsetConfig {
                    tools: ["search_docs".into()].into_iter().collect(),
                    risk: McpToolRisk::ReadOnly,
                },
            )]
            .into_iter()
            .collect(),
            allow_sampling: false,
        };
        assert_eq!(
            config.selected_tool_names(&BTreeSet::new()).unwrap(),
            Some(BTreeSet::new())
        );
        assert_eq!(
            config
                .selected_tool_names(&["read".into()].into_iter().collect())
                .unwrap(),
            Some(["search_docs".into()].into_iter().collect())
        );
    }

    #[test]
    fn named_toolsets_reject_unknown_or_empty_selections() {
        let mut config = McpServerConfig {
            name: "docs".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                program: "unused".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: [("empty".into(), McpToolsetConfig::default())]
                .into_iter()
                .collect(),
            allow_sampling: false,
        };
        assert!(matches!(
            config.selected_tool_names(&["missing".into()].into_iter().collect()),
            Err(McpError::Protocol(message)) if message.contains("不存在 toolset")
        ));
        assert!(matches!(
            config.selected_tool_names(&["empty".into()].into_iter().collect()),
            Err(McpError::Protocol(message)) if message.contains("为空")
        ));
        config.toolsets.clear();
        assert_eq!(
            config
                .selected_tool_names(&["ignored".into()].into_iter().collect())
                .unwrap(),
            None
        );
    }

    #[test]
    fn sse_payload_skips_notifications_and_matches_request_id() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n",
            "\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n",
            "\n",
        );
        let value =
            parse_jsonrpc_payload("text/event-stream; charset=utf-8", body.as_bytes(), 7).unwrap();
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn sse_payload_supports_multiline_data() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"v\":2}}\n\n";
        let value = parse_jsonrpc_payload("text/event-stream", body.as_bytes(), 1).unwrap();
        assert_eq!(value["result"]["v"], 2);
    }

    #[test]
    fn json_payload_parsed_directly() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":1}}"#;
        let value = parse_jsonrpc_payload("application/json", body.as_bytes(), 1).unwrap();
        assert_eq!(value["result"]["ok"], 1);
    }

    #[tokio::test]
    async fn streamable_http_captures_and_echoes_session_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut echoed_session_on_call = false;
            for round in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let length = stream.read(&mut request).await.unwrap();
                let body = String::from_utf8_lossy(&request[..length]);
                let response = if body.contains("initialize") {
                    let json =
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nMcp-Session-Id: sess-123\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        json.len(), json
                    )
                } else if body.contains("notifications/initialized") {
                    "HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                        .to_owned()
                } else {
                    echoed_session_on_call = body.contains("sess-123");
                    let sse = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\r\n\r\n";
                    format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                        sse.len(), sse
                    )
                };
                let _ = round;
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            echoed_session_on_call
        });
        let mut client = test_http_client(format!("http://{address}/mcp"));
        client.initialize().await.unwrap();
        let result = client.call_tool("echo", json!({})).await.unwrap();
        assert_eq!(result["content"][0]["text"], "ok");
        assert!(server.await.unwrap(), "client must echo Mcp-Session-Id");
    }

    #[tokio::test]
    async fn streamable_http_follows_tools_list_pagination() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut page = 0;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let length = stream.read(&mut request).await.unwrap();
                let body = String::from_utf8_lossy(&request[..length]);
                if !body.contains("tools/list") {
                    break;
                }
                let json = if page == 0 {
                    page += 1;
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a","input_schema":{}}],"nextCursor":"p2"}}"#
                } else {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"b","input_schema":{}}]}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    json.len(), json
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                if page > 0 && json.contains("\"b\"") {
                    break;
                }
            }
        });
        let mut client = test_http_client(format!("http://{address}/mcp"));
        let tools = client.list_tools().await.unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn stdio_injects_configured_env_from_parent_variable() {
        // The child echoes the injected variable back inside a tools/list result so we can assert
        // it was passed through. Only the parent variable *name* lives in config.
        std::env::set_var("RIVO_TEST_MCP_TOKEN", "secret-value");
        let (program, args) = if cfg!(windows) {
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "$t=$env:CHILD_TOKEN; $input | ForEach-Object { if ($_ -notlike '*notifications/initialized*') { '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"'+$t+'\",\"input_schema\":{}}]}}' } }".to_owned(),
                ],
            )
        } else {
            (
                "sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "while IFS= read -r line; do case \"$line\" in *notifications/initialized*) ;; *) printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"%s\",\"input_schema\":{}}]}}\\n' \"$CHILD_TOKEN\" ;; esac; done".to_owned(),
                ],
            )
        };
        let config = McpServerConfig {
            name: "mock".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                program,
                args,
                cwd: None,
                env: [("CHILD_TOKEN".to_owned(), "RIVO_TEST_MCP_TOKEN".to_owned())]
                    .into_iter()
                    .collect(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: Default::default(),
            allow_sampling: false,
        };
        let mut client = McpClient::connect(&config).await.unwrap();
        client.initialize().await.unwrap();
        assert_eq!(client.list_tools().await.unwrap()[0].name, "secret-value");
        client.shutdown().await;
        std::env::remove_var("RIVO_TEST_MCP_TOKEN");
    }

    #[tokio::test]
    async fn stdio_missing_env_reference_fails_connect() {
        std::env::remove_var("RIVO_TEST_MCP_MISSING");
        let config = McpServerConfig {
            name: "mock".into(),
            enabled: true,
            transport: McpTransport::Stdio {
                program: "does-not-run".into(),
                args: vec![],
                cwd: None,
                env: [("CHILD".to_owned(), "RIVO_TEST_MCP_MISSING".to_owned())]
                    .into_iter()
                    .collect(),
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: Default::default(),
            allow_sampling: false,
        };
        let error = match McpClient::connect(&config).await {
            Ok(_) => panic!("connect should fail when env reference is unset"),
            Err(error) => error,
        };
        assert!(matches!(error, McpError::Protocol(message) if message.contains("未设置")));
    }

    #[tokio::test]
    async fn allow_loopback_gates_private_http_target() {
        let make = |allow_loopback: bool| McpServerConfig {
            name: "self-hosted".into(),
            enabled: true,
            transport: McpTransport::StreamableHttp {
                url: "http://10.0.0.5/mcp".into(),
                header_env: Default::default(),
                auth: Default::default(),
                allow_loopback,
            },
            allowed_capabilities: ["tools".into()].into_iter().collect(),
            toolsets: Default::default(),
            allow_sampling: false,
        };
        // 默认拒绝私网 http 目标（沿用 SSRF 防护）。
        assert!(McpClient::connect(&make(false)).await.is_err());
        // 显式 allow_loopback 后仅对本 server 放行校验。
        assert!(McpClient::connect(&make(true)).await.is_ok());
    }

    #[test]
    fn mcp_resource_results_become_bounded_evidence() {
        let resource = McpResourceDescription {
            uri: "memory://one".into(),
            name: Some("one".into()),
            mime_type: Some("text/plain".into()),
        };
        let evidence = resource_evidence("mock", &resource, &json!({"text":"abcdef"}), 10).unwrap();
        assert!(evidence.truncated);
        assert!(
            matches!(evidence.source, EvidenceSource::Mcp { ref server, .. } if server == "mock")
        );
    }
}

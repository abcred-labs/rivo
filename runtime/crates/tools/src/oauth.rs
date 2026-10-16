//! OAuth 2.1 授权码 + PKCE 客户端，用于连接需要授权的 MCP Streamable HTTP 远端
//! （Azure DevOps / Atlassian / Figma 官方远端均采用该模式）。
//!
//! 覆盖 MCP 授权规范引用的 RFC 链路：
//! - RFC 9728 受保护资源元数据（从 401 的 `WWW-Authenticate` 头发现）
//! - RFC 8414 授权服务器元数据
//! - RFC 7591 动态客户端注册
//! - RFC 7636 PKCE（S256）
//! - RFC 8707 资源指示器（`resource` 参数绑定到规范 MCP server URL）
//!
//! 交互式一步（打开浏览器 + 回环回调）抽象为 [`AuthorizationPrompt`] trait，便于单测以 mock
//! 授权服务器驱动完整流程，无需真实浏览器。access/refresh token 由上层加密存储（credentials.toml），
//! 本模块只负责协议与内存中的短暂状态，绝不落日志。

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// 授权流程在时间上要额外提前视为过期的安全余量。
const EXPIRY_SKEW: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("OAuth 协议错误: {0}")]
    Protocol(String),
    #[error("OAuth 网络错误: {0}")]
    Network(String),
    #[error("OAuth 授权被拒绝或取消")]
    Denied,
}

/// MCP StreamableHttp 的鉴权方式。默认 `None`（沿用静态 header）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpHttpAuth {
    /// 无内建鉴权，凭据由 `header_env` 静态注入。
    #[default]
    None,
    /// 静态 Bearer：从父进程环境变量读取 PAT/API token（Atlassian API token、ADO PAT 备选路径）。
    /// 只存变量名，不存明文。
    Bearer { token_env: String },
    /// OAuth 2.1 授权码 + PKCE（官方远端主推）。运行时使用已存储的 access token，过期时用
    /// refresh token 静默刷新；首个 token 由 `rivo mcp login` 交互式获取。
    OAuth(OAuthConfig),
}

impl McpHttpAuth {
    /// 在 connect 时解析出应注入的 `Authorization` 头（若有）。
    ///
    /// - `Bearer`：直接从 `token_env` 指向的父进程环境变量读取 PAT/API token。
    /// - `OAuth`：运行前上层已把刷新后的 access token 通过 `token_env` 暴露，运行时与 Bearer 同构；
    ///   未配置 `token_env`（尚未 `rivo mcp login`）则不注入，交由远端 401 触发登录提示。
    /// - `None`：不注入。
    ///
    /// 只读取环境变量名，绝不在配置中存明文；返回值仅用于内存中的请求头，不落日志。
    pub fn resolve_authorization_header(&self) -> Result<Option<(String, String)>, OAuthError> {
        let token_env = match self {
            McpHttpAuth::None => return Ok(None),
            McpHttpAuth::Bearer { token_env } => token_env.as_str(),
            McpHttpAuth::OAuth(config) => match config.token_env.as_deref() {
                Some(name) => name,
                None => return Ok(None),
            },
        };
        let value = std::env::var(token_env).map_err(|_| {
            OAuthError::Protocol(format!("MCP credential 环境变量 {token_env} 未设置"))
        })?;
        Ok(Some((
            "Authorization".to_owned(),
            format!("Bearer {value}"),
        )))
    }
}

/// OAuth server 静态配置。动态部分（client_id / token）在首次登录时获得并加密存储。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OAuthConfig {
    /// 预注册的 client_id；缺省则走 RFC 7591 动态客户端注册。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 请求的 scopes。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// 动态注册时上报的客户端名称。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// 回环回调端口；0 表示使用临时端口。
    #[serde(default)]
    pub redirect_port: u16,
    /// 运行时读取已刷新 access token 的父进程环境变量名（Bearer 回退路径；OAuth 主路径用 keychain）。
    /// 只存变量名，不存明文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// 运行时若 keychain 中无有效 token，是否在交互环境下自动拉起浏览器登录。
    /// headless/CI（未提供交互 login 实现）时即使为 true 也只报错、不阻塞。
    #[serde(default)]
    pub auto_login: bool,
}

/// RFC 7636 PKCE 参数对。
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub method: &'static str,
}

impl Pkce {
    /// 生成 43 字符的高熵 verifier 及其 S256 challenge。
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        Self::from_verifier(verifier)
    }

    fn from_verifier(verifier: String) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
            method: "S256",
        }
    }
}

/// 生成 URL-safe 的随机 state / nonce。
fn random_token() -> String {
    let mut bytes = [0_u8; 24];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 9728 受保护资源元数据。
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

/// RFC 8414 授权服务器元数据（仅保留本流程需要的字段）。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
}

/// RFC 7591 动态客户端注册结果。
#[derive(Debug, Clone, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// token 端点响应。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// 持久化的 token（加密存储）。含过期时间以便静默刷新。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix 秒；`None` 表示未知过期时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_owned()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl OAuthToken {
    /// 由 token 端点响应构造，`now` 为当前 Unix 秒（便于测试注入）。
    pub fn from_response(response: TokenResponse, now: u64) -> Self {
        let expires_at = response.expires_in.map(|seconds| now + seconds);
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at,
            token_type: response.token_type.unwrap_or_else(default_token_type),
        }
    }

    /// 是否已过期（含安全余量）。未知过期时间视为未过期。
    pub fn is_expired_at(&self, now: u64) -> bool {
        match self.expires_at {
            Some(expires_at) => now + EXPIRY_SKEW.as_secs() >= expires_at,
            None => false,
        }
    }

    /// 当前时刻是否已过期。
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(now_unix())
    }

    /// 构造 `Authorization` 头值。
    pub fn authorization_header(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }
}

/// 一次授权成功后需要持久化的完整会话：token + 后续静默刷新所需的端点与绑定参数。
/// 存入 keychain（[`store::KeyringStore`]），connect 时据此自解析并按需刷新。绝不落日志。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthSession {
    pub token: OAuthToken,
    pub token_endpoint: String,
    pub authorization_endpoint: String,
    pub client_id: String,
    /// RFC 8707 resource 指示器（规范 MCP server URL）。
    pub resource: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

impl OAuthSession {
    /// connect 时注入的 `Authorization` 头（名, 值）。
    pub fn authorization_header(&self) -> (String, String) {
        (
            "Authorization".to_owned(),
            self.token.authorization_header(),
        )
    }
}

/// 解析 `WWW-Authenticate: Bearer ... resource_metadata="URL" ...` 中的受保护资源元数据 URL。
pub fn parse_resource_metadata_url(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let segment = segment.strip_prefix("Bearer ").unwrap_or(segment);
        for param in segment.split(char::is_whitespace) {
            if let Some(rest) = param.trim().strip_prefix("resource_metadata=") {
                let value = rest.trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

/// 由 issuer 推导授权服务器元数据候选 URL，按优先级返回。
///
/// 覆盖三种实践形态，逐个尝试直到命中（Azure DevOps / Entra 使用 OIDC 与 oauth-authorization-server）：
/// 1. RFC 8414 §3.1 路径感知：`{origin}/.well-known/oauth-authorization-server{path}`；
/// 2. 追加式（OpenID Provider Configuration 1.0 常见变体）：`{issuer}/.well-known/oauth-authorization-server`；
/// 3. OIDC Discovery：`{issuer}/.well-known/openid-configuration`。
pub fn authorization_server_metadata_candidates(issuer: &str) -> Vec<String> {
    let trimmed = issuer.trim_end_matches('/');
    let mut candidates = Vec::new();
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let path = url.path().trim_end_matches('/');
        if let Some(origin) = url
            .join("/")
            .ok()
            .map(|base| base.as_str().trim_end_matches('/').to_owned())
        {
            // RFC 8414：well-known 段插入到 host 与 path 之间。
            candidates.push(format!(
                "{origin}/.well-known/oauth-authorization-server{path}"
            ));
        }
    }
    let append = format!("{trimmed}/.well-known/oauth-authorization-server");
    if !candidates.contains(&append) {
        candidates.push(append);
    }
    candidates.push(format!("{trimmed}/.well-known/openid-configuration"));
    candidates
}

/// 由 issuer 推导首选（RFC 8414 路径感知）授权服务器元数据 URL。
pub fn authorization_server_metadata_url(issuer: &str) -> String {
    authorization_server_metadata_candidates(issuer)
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("{issuer}/.well-known/oauth-authorization-server"))
}

/// 抽象交互式授权步骤：给定授权 URL 与回环 redirect_uri，驱动用户同意并返回完整回调 URL
/// （含 `code` 与 `state`）。默认实现见 [`LoopbackAuthorizer`]；测试可注入 mock。
#[async_trait]
pub trait AuthorizationPrompt: Send + Sync {
    async fn authorize(
        &self,
        authorization_url: &str,
        redirect_uri: &str,
    ) -> Result<String, OAuthError>;
}

/// 拉取受保护资源元数据（RFC 9728）。
pub async fn fetch_protected_resource_metadata(
    http: &reqwest::Client,
    metadata_url: &str,
) -> Result<ProtectedResourceMetadata, OAuthError> {
    let response = http
        .get(metadata_url)
        .send()
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Protocol(format!(
            "受保护资源元数据 HTTP {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| OAuthError::Protocol(error.to_string()))
}

/// 拉取授权服务器元数据（RFC 8414）。按候选 URL 逐个尝试，兼容 Entra/ADO 的 OIDC 与 oauth 变体。
pub async fn fetch_authorization_server_metadata(
    http: &reqwest::Client,
    issuer: &str,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    let mut last_error = OAuthError::Protocol("授权服务器元数据发现无可用候选 URL".into());
    for url in authorization_server_metadata_candidates(issuer) {
        let response = match http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = OAuthError::Network(error.to_string());
                continue;
            }
        };
        if !response.status().is_success() {
            last_error =
                OAuthError::Protocol(format!("授权服务器元数据 HTTP {}", response.status()));
            continue;
        }
        match response.json::<AuthorizationServerMetadata>().await {
            Ok(metadata) => return Ok(metadata),
            Err(error) => {
                last_error = OAuthError::Protocol(error.to_string());
                continue;
            }
        }
    }
    Err(last_error)
}

/// RFC 7591 动态客户端注册。
pub async fn register_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
    scopes: &[String],
) -> Result<RegisteredClient, OAuthError> {
    let mut body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if !scopes.is_empty() {
        body["scope"] = serde_json::Value::String(scopes.join(" "));
    }
    let response = http
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Protocol(format!(
            "动态客户端注册 HTTP {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| OAuthError::Protocol(error.to_string()))
}

/// 构造授权端点 URL（含 PKCE challenge、state、RFC 8707 resource 指示器）。
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    scopes: &[String],
    state: &str,
    resource: &str,
) -> Result<String, OAuthError> {
    let mut url = reqwest::Url::parse(authorization_endpoint)
        .map_err(|error| OAuthError::Protocol(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", pkce.method)
            .append_pair("state", state)
            .append_pair("resource", resource);
        if !scopes.is_empty() {
            query.append_pair("scope", &scopes.join(" "));
        }
    }
    Ok(url.into())
}

/// 用授权码换取 token（RFC 6749 §4.1.3 + PKCE）。
pub async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    verifier: &str,
    resource: &str,
) -> Result<TokenResponse, OAuthError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
        ("resource", resource),
    ];
    post_token(http, token_endpoint, &form).await
}

/// 用 refresh token 换取新的 access token。
pub async fn refresh_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    resource: &str,
) -> Result<TokenResponse, OAuthError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("resource", resource),
    ];
    post_token(http, token_endpoint, &form).await
}

async fn post_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, OAuthError> {
    let response = http
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Protocol(format!(
            "token 端点 HTTP {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| OAuthError::Protocol(error.to_string()))
}

/// 从回调 URL 中校验 state 并取出授权码。
pub fn extract_code(callback_url: &str, expected_state: &str) -> Result<String, OAuthError> {
    let url = reqwest::Url::parse(callback_url)
        .map_err(|error| OAuthError::Protocol(error.to_string()))?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(OAuthError::Protocol(format!(
            "授权服务器返回 error={error}"
        )));
    }
    if state.as_deref() != Some(expected_state) {
        return Err(OAuthError::Protocol("授权回调 state 不匹配".into()));
    }
    code.ok_or_else(|| OAuthError::Protocol("授权回调缺少 code".into()))
}

/// 端到端授权流程：发现 → （必要时）注册 → PKCE 授权码 → 换取 token。
///
/// - `server_url`：规范 MCP server URL，作为 RFC 8707 resource 指示器。
/// - `resource_metadata_url`：来自 401 `WWW-Authenticate` 的受保护资源元数据 URL。
/// - `redirect_uri`：回环回调地址；须与 `prompt` 监听地址一致。
/// - `prompt`：交互式同意驱动。
pub async fn run_authorization(
    http: &reqwest::Client,
    server_url: &str,
    resource_metadata_url: &str,
    redirect_uri: &str,
    config: &OAuthConfig,
    prompt: &dyn AuthorizationPrompt,
) -> Result<OAuthSession, OAuthError> {
    let resource_metadata = fetch_protected_resource_metadata(http, resource_metadata_url).await?;
    let issuer = resource_metadata
        .authorization_servers
        .first()
        .ok_or_else(|| OAuthError::Protocol("受保护资源元数据未列出授权服务器".into()))?;
    let server_metadata = fetch_authorization_server_metadata(http, issuer).await?;
    let resource = resource_metadata
        .resource
        .as_deref()
        .unwrap_or(server_url)
        .to_owned();

    let client_id = match &config.client_id {
        Some(client_id) => client_id.clone(),
        None => {
            let endpoint = server_metadata
                .registration_endpoint
                .as_deref()
                .ok_or_else(|| {
                    OAuthError::Protocol("未提供 client_id 且授权服务器不支持动态注册".into())
                })?;
            let client_name = config.client_name.as_deref().unwrap_or("Rivo");
            register_client(http, endpoint, redirect_uri, client_name, &config.scopes)
                .await?
                .client_id
        }
    };

    let pkce = Pkce::generate();
    let state = random_token();
    let authorization_url = build_authorization_url(
        &server_metadata.authorization_endpoint,
        &client_id,
        redirect_uri,
        &pkce,
        &config.scopes,
        &state,
        &resource,
    )?;
    let callback = prompt.authorize(&authorization_url, redirect_uri).await?;
    let code = extract_code(&callback, &state)?;
    let token_response = exchange_code(
        http,
        &server_metadata.token_endpoint,
        &code,
        redirect_uri,
        &client_id,
        &pkce.verifier,
        &resource,
    )
    .await?;
    Ok(OAuthSession {
        token: OAuthToken::from_response(token_response, now_unix()),
        token_endpoint: server_metadata.token_endpoint,
        authorization_endpoint: server_metadata.authorization_endpoint,
        client_id,
        resource,
        scopes: config.scopes.clone(),
    })
}

/// 用已存储的 refresh token 静默刷新（无需交互）。
pub async fn refresh_stored_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    token: &OAuthToken,
    client_id: &str,
    resource: &str,
) -> Result<OAuthToken, OAuthError> {
    let refresh = token
        .refresh_token
        .as_deref()
        .ok_or_else(|| OAuthError::Protocol("无 refresh token，需重新登录".into()))?;
    let response = refresh_token(http, token_endpoint, refresh, client_id, resource).await?;
    let mut refreshed = OAuthToken::from_response(response, now_unix());
    // 部分授权服务器刷新时不回传 refresh_token，此时沿用旧值。
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = token.refresh_token.clone();
    }
    Ok(refreshed)
}

/// 默认交互实现：绑定回环端口，提示用户在浏览器打开授权 URL，等待回调 GET 并返回其完整 URL。
///
/// 安全边界：仅监听 `127.0.0.1`，只接受携带 `state` 的单次回调，随后立即关闭。授权码存活时间极短，
/// 且回环地址不对外暴露。
pub struct LoopbackAuthorizer {
    listener: tokio::net::TcpListener,
    redirect_uri: String,
}

impl LoopbackAuthorizer {
    /// 在 `127.0.0.1:{port}` 绑定回环监听（port=0 使用临时端口）。
    pub async fn bind(port: u16) -> Result<Self, OAuthError> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|error| OAuthError::Network(error.to_string()))?;
        let local = listener
            .local_addr()
            .map_err(|error| OAuthError::Network(error.to_string()))?;
        Ok(Self {
            redirect_uri: format!("http://127.0.0.1:{}/callback", local.port()),
            listener,
        })
    }

    /// 分配到的回环 redirect_uri。
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// 阻塞等待单次回调 GET，返回其完整 URL。
    pub async fn wait_for_callback(&self) -> Result<String, OAuthError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|error| OAuthError::Network(error.to_string()))?;
        let mut buffer = [0_u8; 8192];
        let read = stream
            .read(&mut buffer)
            .await
            .map_err(|error| OAuthError::Network(error.to_string()))?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or_else(|| OAuthError::Protocol("回调请求行无法解析".into()))?;
        let body = "<html><body>授权完成，可关闭此页返回终端。</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        Ok(format!("http://127.0.0.1{path}"))
    }
}

/// OAuth 会话持久化抽象。默认实现 [`store::KeyringStore`] 写入 OS keychain；
/// [`store::MemoryStore`] 供单测使用。account 约定为 MCP server 名。
pub mod store {
    use super::{OAuthError, OAuthSession};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// keychain 中的服务命名空间。account = MCP server 名。
    pub const KEYRING_SERVICE: &str = "rivo-mcp-oauth";

    /// 存取已授权会话（含 token 与刷新所需端点）。实现须保证明文只落安全存储，绝不入日志。
    pub trait OAuthTokenStore: Send + Sync {
        fn load(&self, account: &str) -> Result<Option<OAuthSession>, OAuthError>;
        fn save(&self, account: &str, session: &OAuthSession) -> Result<(), OAuthError>;
        fn delete(&self, account: &str) -> Result<(), OAuthError>;
    }

    /// OS keychain 后端（Windows Credential Manager / macOS Keychain / Linux Secret Service）。
    #[derive(Debug, Default, Clone, Copy)]
    pub struct KeyringStore;

    impl KeyringStore {
        pub fn new() -> Self {
            Self
        }

        fn entry(account: &str) -> Result<keyring::Entry, OAuthError> {
            keyring::Entry::new(KEYRING_SERVICE, account)
                .map_err(|error| OAuthError::Protocol(format!("keychain 打开失败: {error}")))
        }
    }

    impl OAuthTokenStore for KeyringStore {
        fn load(&self, account: &str) -> Result<Option<OAuthSession>, OAuthError> {
            match Self::entry(account)?.get_password() {
                Ok(json) => {
                    let session = serde_json::from_str(&json).map_err(|error| {
                        OAuthError::Protocol(format!("keychain 会话解析失败: {error}"))
                    })?;
                    Ok(Some(session))
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(OAuthError::Protocol(format!("keychain 读取失败: {error}"))),
            }
        }

        fn save(&self, account: &str, session: &OAuthSession) -> Result<(), OAuthError> {
            let json = serde_json::to_string(session)
                .map_err(|error| OAuthError::Protocol(format!("会话序列化失败: {error}")))?;
            Self::entry(account)?
                .set_password(&json)
                .map_err(|error| OAuthError::Protocol(format!("keychain 写入失败: {error}")))
        }

        fn delete(&self, account: &str) -> Result<(), OAuthError> {
            match Self::entry(account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(OAuthError::Protocol(format!("keychain 删除失败: {error}"))),
            }
        }
    }

    /// 进程内存后端，仅用于测试；进程退出即清空。
    #[derive(Debug, Default)]
    pub struct MemoryStore {
        entries: Mutex<HashMap<String, OAuthSession>>,
    }

    impl OAuthTokenStore for MemoryStore {
        fn load(&self, account: &str) -> Result<Option<OAuthSession>, OAuthError> {
            Ok(self.entries.lock().unwrap().get(account).cloned())
        }

        fn save(&self, account: &str, session: &OAuthSession) -> Result<(), OAuthError> {
            self.entries
                .lock()
                .unwrap()
                .insert(account.to_owned(), session.clone());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), OAuthError> {
            self.entries.lock().unwrap().remove(account);
            Ok(())
        }
    }
}

/// 探测 MCP server 的受保护资源元数据 URL：优先读 401 的 `WWW-Authenticate`，
/// 回退到 RFC 9728 well-known 路径 `{origin}/.well-known/oauth-protected-resource`。
pub async fn discover_resource_metadata_url(
    http: &reqwest::Client,
    server_url: &str,
) -> Result<String, OAuthError> {
    let response = http
        .get(server_url)
        .send()
        .await
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    if let Some(header) = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
    {
        if let Some(url) = parse_resource_metadata_url(header) {
            return Ok(url);
        }
    }
    let mut url =
        reqwest::Url::parse(server_url).map_err(|error| OAuthError::Protocol(error.to_string()))?;
    url.set_path("/.well-known/oauth-protected-resource");
    url.set_query(None);
    Ok(url.into())
}

/// 交互式登录抽象，供 connect 在缺失/过期时按需触发。生产实现见 [`BrowserLogin`]，测试可注入 mock。
#[async_trait]
pub trait OAuthLogin: Send + Sync {
    async fn login(
        &self,
        http: &reqwest::Client,
        server_url: &str,
        config: &OAuthConfig,
    ) -> Result<OAuthSession, OAuthError>;
}

/// 默认交互式登录：探测发现 → 绑定回环 → 打开浏览器 → 授权码换 token。
#[derive(Debug, Default, Clone, Copy)]
pub struct BrowserLogin;

#[async_trait]
impl OAuthLogin for BrowserLogin {
    async fn login(
        &self,
        http: &reqwest::Client,
        server_url: &str,
        config: &OAuthConfig,
    ) -> Result<OAuthSession, OAuthError> {
        let resource_metadata_url = discover_resource_metadata_url(http, server_url).await?;
        let authorizer = LoopbackAuthorizer::bind(config.redirect_port).await?;
        let redirect_uri = authorizer.redirect_uri().to_owned();
        let prompt = BrowserPrompt { authorizer };
        run_authorization(
            http,
            server_url,
            &resource_metadata_url,
            &redirect_uri,
            config,
            &prompt,
        )
        .await
    }
}

/// [`AuthorizationPrompt`] 的浏览器实现：打开授权 URL（并回显以便远程/无浏览器环境手动打开），
/// 随后等待回环回调。
struct BrowserPrompt {
    authorizer: LoopbackAuthorizer,
}

#[async_trait]
impl AuthorizationPrompt for BrowserPrompt {
    async fn authorize(
        &self,
        authorization_url: &str,
        _redirect_uri: &str,
    ) -> Result<String, OAuthError> {
        // 回显授权 URL（不含任何 token），并尽力打开系统浏览器。
        eprintln!("请在浏览器完成授权：{authorization_url}");
        open_browser(authorization_url);
        self.authorizer.wait_for_callback().await
    }
}

/// 尽力打开系统默认浏览器（best-effort，失败不阻断，用户可手动复制回显 URL）。
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// connect 侧自解析 `Authorization` 头：
/// 1) keychain 命中且未过期 → 直接注入；
/// 2) 命中但过期 → refresh token 静默刷新，回存后注入；
/// 3) 缺失或刷新失败 → 若提供 `login`（auto_login 且交互）则登录并回存，否则报错提示 `rivo mcp login`。
///
/// token 全程只在内存与 keychain 间流转，绝不落日志。
pub async fn authorization_for_connect(
    http: &reqwest::Client,
    account: &str,
    server_url: &str,
    config: &OAuthConfig,
    store: &dyn store::OAuthTokenStore,
    login: Option<&dyn OAuthLogin>,
) -> Result<(String, String), OAuthError> {
    if let Some(session) = store.load(account)? {
        if !session.token.is_expired() {
            return Ok(session.authorization_header());
        }
        match refresh_stored_token(
            http,
            &session.token_endpoint,
            &session.token,
            &session.client_id,
            &session.resource,
        )
        .await
        {
            Ok(token) => {
                let refreshed = OAuthSession { token, ..session };
                store.save(account, &refreshed)?;
                return Ok(refreshed.authorization_header());
            }
            Err(_) if login.is_some() => { /* 刷新失败且允许交互 → 重新登录 */ }
            Err(error) => return Err(error),
        }
    }
    match login {
        Some(login) => {
            let session = login.login(http, server_url, config).await?;
            store.save(account, &session)?;
            Ok(session.authorization_header())
        }
        None => Err(OAuthError::Protocol(format!(
            "MCP server `{account}` 未登录或凭据已过期，请运行 `rivo mcp login {account}`"
        ))),
    }
}

/// CLI 便捷入口：交互式浏览器登录并把会话写入 `store`（account = server 名）。强制重新登录。
pub async fn interactive_login(
    server_url: &str,
    config: &OAuthConfig,
    store: &dyn store::OAuthTokenStore,
    account: &str,
) -> Result<(), OAuthError> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| OAuthError::Network(error.to_string()))?;
    let session = BrowserLogin.login(&http, server_url, config).await?;
    store.save(account, &session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::OAuthTokenStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned());
        // 来自 RFC 7636 附录 B 的已知向量。
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(pkce.method, "S256");
    }

    #[test]
    fn generated_pkce_roundtrips_through_sha256() {
        let pkce = Pkce::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        assert!(pkce.verifier.len() >= 43);
    }

    #[test]
    fn parses_resource_metadata_from_www_authenticate() {
        let header = r#"Bearer error="invalid_token", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#;
        assert_eq!(
            parse_resource_metadata_url(header).as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn missing_resource_metadata_returns_none() {
        assert_eq!(parse_resource_metadata_url("Bearer realm=\"x\""), None);
    }

    #[tokio::test]
    async fn oauth_metadata_and_registration_reject_non_success_and_malformed_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for (status, body) in [
                ("500 Internal Server Error", "{}"),
                ("200 OK", "not-json"),
                ("400 Bad Request", "{}"),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{address}");
        assert!(matches!(
            fetch_protected_resource_metadata(&http, &format!("{base}/status")).await,
            Err(OAuthError::Protocol(message)) if message.contains("500")
        ));
        assert!(matches!(
            fetch_protected_resource_metadata(&http, &format!("{base}/malformed")).await,
            Err(OAuthError::Protocol(_))
        ));
        assert!(matches!(
            register_client(&http, &format!("{base}/register"), "http://127.0.0.1/callback", "fixture", &[]).await,
            Err(OAuthError::Protocol(message)) if message.contains("400")
        ));
        handle.await.unwrap();
    }

    #[test]
    fn as_metadata_candidates_are_rfc8414_path_aware_with_oidc_fallback() {
        // Entra 风格 issuer 带租户路径：RFC 8414 段插入到 host 与 path 之间。
        let candidates = authorization_server_metadata_candidates(
            "https://login.microsoftonline.com/tenant-id/v2.0",
        );
        assert_eq!(
            candidates[0],
            "https://login.microsoftonline.com/.well-known/oauth-authorization-server/tenant-id/v2.0"
        );
        assert!(candidates.iter().any(|url| {
            url
            == "https://login.microsoftonline.com/tenant-id/v2.0/.well-known/openid-configuration"
        }));
        // 无路径 issuer：首选即根 well-known。
        let root = authorization_server_metadata_candidates("https://auth.example.com");
        assert_eq!(
            root[0],
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn authorization_url_includes_pkce_state_and_resource() {
        let pkce = Pkce::from_verifier("verifier-value-verifier-value-verifier-value".to_owned());
        let url = build_authorization_url(
            "https://auth.example.com/authorize",
            "client-123",
            "http://127.0.0.1:52111/callback",
            &pkce,
            &["read".to_owned(), "write".to_owned()],
            "state-xyz",
            "https://mcp.example.com/mcp",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some("client-123")
        );
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(pkce.challenge.as_str())
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("state-xyz"));
        assert_eq!(
            query.get("resource").map(String::as_str),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(query.get("scope").map(String::as_str), Some("read write"));
    }

    #[test]
    fn extract_code_validates_state() {
        let code = extract_code("http://127.0.0.1:9/callback?code=abc&state=good", "good").unwrap();
        assert_eq!(code, "abc");
        assert!(extract_code("http://127.0.0.1:9/callback?code=abc&state=bad", "good").is_err());
        assert!(matches!(
            extract_code("http://127.0.0.1:9/callback?error=access_denied", "good"),
            Err(OAuthError::Protocol(message)) if message.contains("error=access_denied")
        ));
    }

    #[test]
    fn token_expiry_accounts_for_skew() {
        let token = OAuthToken::from_response(
            TokenResponse {
                access_token: "a".into(),
                token_type: Some("Bearer".into()),
                expires_in: Some(100),
                refresh_token: Some("r".into()),
                scope: None,
            },
            1_000,
        );
        assert_eq!(token.expires_at, Some(1_100));
        assert!(!token.is_expired_at(1_050));
        // 1_071 + 30 余量 >= 1_100 → 视为过期。
        assert!(token.is_expired_at(1_071));
        assert_eq!(token.authorization_header(), "Bearer a");
    }

    #[test]
    fn memory_store_delete_is_idempotent() {
        let store = store::MemoryStore::default();
        store.delete("missing").unwrap();
        store
            .save("server", &sample_session("access", Some(now_unix() + 60)))
            .unwrap();
        assert!(store.load("server").unwrap().is_some());
        store.delete("server").unwrap();
        assert_eq!(store.load("server").unwrap(), None);
    }

    #[tokio::test]
    async fn token_and_resource_discovery_failures_are_explicit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, headers) in [("400 Bad Request", ""), ("401 Unauthorized", "")] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let token_error = exchange_code(
            &http,
            &format!("http://{address}/token"),
            "code",
            "http://127.0.0.1/callback",
            "client",
            "verifier",
            "https://mcp.example.com/mcp",
        )
        .await
        .unwrap_err();
        assert!(matches!(token_error, OAuthError::Protocol(message) if message.contains("400")));

        let discovered =
            discover_resource_metadata_url(&http, &format!("http://{address}/mcp?ignored=1"))
                .await
                .unwrap();
        assert_eq!(
            discovered,
            format!("http://{address}/.well-known/oauth-protected-resource")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authorization_metadata_uses_oidc_fallback_after_http_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let expected_issuer = issuer.clone();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = first.read(&mut request).await.unwrap();
            first
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut second, _) = listener.accept().await.unwrap();
            let _ = second.read(&mut request).await.unwrap();
            let body = format!(
                r#"{{"issuer":"{expected_issuer}","authorization_endpoint":"{expected_issuer}/authorize","token_endpoint":"{expected_issuer}/token"}}"#
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            second.write_all(response.as_bytes()).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let metadata = fetch_authorization_server_metadata(&http, &issuer)
            .await
            .unwrap();
        assert_eq!(metadata.issuer, issuer);
        assert!(metadata.authorization_endpoint.ends_with("/authorize"));
        server.await.unwrap();
    }

    /// 启动一个 mock 授权服务器，覆盖发现 / 注册 / token 交换全链路。
    async fn spawn_mock_authorization_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let issuer = base.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0_u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_owned();
                let payload = if target.contains("oauth-protected-resource") {
                    format!(
                        r#"{{"authorization_servers":["{issuer}"],"resource":"https://mcp.example.com/mcp"}}"#
                    )
                } else if target.contains("oauth-authorization-server") {
                    format!(
                        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","registration_endpoint":"{issuer}/register","code_challenge_methods_supported":["S256"]}}"#
                    )
                } else if target.contains("/register") {
                    r#"{"client_id":"dynamic-client-1"}"#.to_owned()
                } else if target.contains("/token") {
                    r#"{"access_token":"access-1","token_type":"Bearer","expires_in":3600,"refresh_token":"refresh-1"}"#.to_owned()
                } else {
                    "{}".to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, handle)
    }

    struct StaticCallbackPrompt {
        callback_template: String,
    }

    #[async_trait]
    impl AuthorizationPrompt for StaticCallbackPrompt {
        async fn authorize(
            &self,
            authorization_url: &str,
            _redirect_uri: &str,
        ) -> Result<String, OAuthError> {
            // 从授权 URL 回读 state，模拟授权服务器带 state 重定向回回环。
            let parsed = reqwest::Url::parse(authorization_url).unwrap();
            let state = parsed
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .unwrap();
            Ok(self.callback_template.replace("{state}", &state))
        }
    }

    #[tokio::test]
    async fn run_authorization_drives_discovery_registration_and_token_exchange() {
        let (base, handle) = spawn_mock_authorization_server().await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let prompt = StaticCallbackPrompt {
            callback_template: "http://127.0.0.1:1/callback?code=auth-code-1&state={state}".into(),
        };
        let config = OAuthConfig {
            client_id: None,
            scopes: vec!["read".into()],
            client_name: Some("Rivo-test".into()),
            redirect_port: 0,
            token_env: None,
            auto_login: false,
        };
        let session = run_authorization(
            &http,
            "https://mcp.example.com/mcp",
            &format!("{base}/.well-known/oauth-protected-resource"),
            "http://127.0.0.1:1/callback",
            &config,
            &prompt,
        )
        .await
        .unwrap();
        assert_eq!(session.token.access_token, "access-1");
        assert_eq!(session.token.refresh_token.as_deref(), Some("refresh-1"));
        assert!(session.token.expires_at.is_some());
        assert_eq!(session.client_id, "dynamic-client-1");
        assert_eq!(session.resource, "https://mcp.example.com/mcp");
        assert!(session.token_endpoint.ends_with("/token"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_stored_token_preserves_refresh_when_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            let payload = r#"{"access_token":"access-2","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let existing = OAuthToken {
            access_token: "access-1".into(),
            refresh_token: Some("refresh-keep".into()),
            expires_at: Some(0),
            token_type: "Bearer".into(),
        };
        let refreshed = refresh_stored_token(
            &http,
            &format!("http://{address}/token"),
            &existing,
            "client-123",
            "https://mcp.example.com/mcp",
        )
        .await
        .unwrap();
        assert_eq!(refreshed.access_token, "access-2");
        // 刷新响应未回传 refresh_token → 沿用旧值。
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-keep"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn loopback_authorizer_captures_callback_url() {
        let authorizer = LoopbackAuthorizer::bind(0).await.unwrap();
        let redirect = authorizer.redirect_uri().to_owned();
        let port = reqwest::Url::parse(&redirect).unwrap().port().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(b"GET /callback?code=xyz&state=abc HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut buffer = vec![0_u8; 1024];
            let _ = stream.read(&mut buffer).await.unwrap();
        });
        let callback = authorizer.wait_for_callback().await.unwrap();
        assert!(callback.contains("code=xyz"));
        assert!(callback.contains("state=abc"));
        let code = extract_code(&callback, "abc").unwrap();
        assert_eq!(code, "xyz");
        client.await.unwrap();
    }

    struct MockLogin {
        session: OAuthSession,
    }

    #[async_trait]
    impl OAuthLogin for MockLogin {
        async fn login(
            &self,
            _http: &reqwest::Client,
            _server_url: &str,
            _config: &OAuthConfig,
        ) -> Result<OAuthSession, OAuthError> {
            Ok(self.session.clone())
        }
    }

    fn sample_session(access: &str, expires_at: Option<u64>) -> OAuthSession {
        OAuthSession {
            token: OAuthToken {
                access_token: access.into(),
                refresh_token: Some("refresh-1".into()),
                expires_at,
                token_type: "Bearer".into(),
            },
            token_endpoint: "https://auth.example.com/token".into(),
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            client_id: "client-1".into(),
            resource: "https://mcp.example.com/mcp".into(),
            scopes: vec!["read".into()],
        }
    }

    #[tokio::test]
    async fn connect_resolve_errors_when_no_session_and_no_login() {
        let store = store::MemoryStore::default();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let config = OAuthConfig::default();
        let error = authorization_for_connect(
            &http,
            "ado",
            "https://mcp.example.com/mcp",
            &config,
            &store,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, OAuthError::Protocol(message) if message.contains("rivo mcp login"))
        );
    }

    #[tokio::test]
    async fn connect_resolve_triggers_login_and_persists_session() {
        let store = store::MemoryStore::default();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let config = OAuthConfig::default();
        let login = MockLogin {
            session: sample_session("access-new", Some(now_unix() + 3600)),
        };
        let (name, value) = authorization_for_connect(
            &http,
            "ado",
            "https://mcp.example.com/mcp",
            &config,
            &store,
            Some(&login),
        )
        .await
        .unwrap();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer access-new");
        // 登录成功后会话已回存 keychain/内存。
        assert_eq!(
            store.load("ado").unwrap().unwrap().token.access_token,
            "access-new"
        );
    }

    #[tokio::test]
    async fn connect_resolve_uses_fresh_session_without_login() {
        let store = store::MemoryStore::default();
        store
            .save(
                "ado",
                &sample_session("access-cached", Some(now_unix() + 3600)),
            )
            .unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let config = OAuthConfig::default();
        let (_, value) = authorization_for_connect(
            &http,
            "ado",
            "https://mcp.example.com/mcp",
            &config,
            &store,
            None,
        )
        .await
        .unwrap();
        assert_eq!(value, "Bearer access-cached");
    }
}

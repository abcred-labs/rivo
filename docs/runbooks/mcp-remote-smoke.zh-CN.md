# 远端 MCP OAuth 冒烟手册（以 Azure DevOps 为主）

本手册验证 Rivo 对**官方远端 MCP server**（OAuth 2.1 授权码 + PKCE）的端到端接入：安装 → 交互式登录 → 发现/调用只读工具。以 Azure DevOps 官方 MCP（`https://mcp.dev.azure.com/<org>`）为首选验证目标，Atlassian/Figma 官方远端流程同构。

凭据安全边界：

- OAuth access/refresh token 只写入 **OS keychain**（服务命名空间 `rivo-mcp-oauth`，account = MCP server 名），绝不落 `config.toml`、日志、SQLite 或诊断包。
- `rivo mcp show` 只显示环境变量名与鉴权类型，绝不显示任何 token 明文。
- `--allow-loopback` 仅放行**该 server** 的回环/私网校验（自托管 ADO Server / Figma Dev Mode 本地端口），绝不影响 `web_fetch` 的 SSRF 防护。

## 1. 前置

```powershell
rivo --version
rivo mcp --help
```

需要：一个可访问的 Azure DevOps 组织（`<org>`）、一台有系统浏览器的交互式终端（登录时会自动打开浏览器；无浏览器环境可手动复制终端回显的授权 URL）。

## 2. 安装（OAuth + 自动登录）

`--oauth` 启用 OAuth 2.1；缺省 `--oauth-client-id` 时走 RFC 7591 动态客户端注册。`--auto-login` 表示运行时若 keychain 无有效 token 且处于交互上下文，则自动拉起浏览器登录。

```powershell
rivo mcp install http ado https://mcp.dev.azure.com/<org> `
  --capability tools `
  --toolset work:read_only:wit_list_work_items `
  --oauth `
  --auto-login
```

首次安装通常尚未登录，实时发现会被跳过并落配置，输出形如：

```text
installed MCP server ado（OAuth 未登录，已跳过实时发现）；请运行 `rivo mcp login ado`，随后 `rivo mcp inspect ado` 验证
```

确认配置（不含任何 token 明文）：

```powershell
rivo mcp show ado
# transport=http url=https://mcp.dev.azure.com/<org> allow_loopback=false
# auth=oauth client_id=<dynamic> token_env=<unset> scopes= auto_login=true
```

## 3. 交互式登录

```powershell
rivo mcp login ado
```

流程（全部由 `rivo` 完成，token 不经过任何中间文件）：

1. 探测 server 的受保护资源元数据 URL（RFC 9728，读 `WWW-Authenticate`，回退 well-known）。
2. 发现授权服务器元数据（RFC 8414，按候选 URL 逐个尝试，兼容 Entra 的 OIDC 与 `oauth-authorization-server` 变体）。
3. 必要时动态注册客户端（RFC 7591）。
4. 绑定 `127.0.0.1` 回环回调，打开浏览器完成 Entra 授权（PKCE S256，RFC 7636；`resource` 指示器绑定规范 server URL，RFC 8707）。
5. 授权码换 token，写入 OS keychain。

成功输出：

```text
MCP server ado 登录成功，凭据已存入系统 keychain
```

## 4. 发现与只读调用验证

```powershell
rivo mcp inspect ado
```

预期：`status=ready`，并列出已选 toolset 中的只读工具（如 `wit_list_work_items`）。connect 会自动从 keychain 载入 token；若已过期则用 refresh token 静默刷新后回存。

## 5. 证据留存（脱敏）

保存以下命令输出作为验证证据，**务必确认其中不含任何 token/PAT**：

- `rivo mcp show ado`（应只见变量名与鉴权类型）
- `rivo mcp inspect ado`（应见 `status=ready` 与只读工具清单）

## 6. 排障

| 现象 | 处理 |
| --- | --- |
| `未登录或凭据已过期，请运行 rivo mcp login ado` | 运行 `rivo mcp login ado` 重新登录 |
| 授权服务器元数据发现失败 | 确认组织可访问；若 Entra 端点形态特殊，核对第 3 步候选 URL 推导 |
| 自托管 ADO Server 位于私网 | 安装时追加 `--allow-loopback`（仅放行该 server） |
| 无系统浏览器 | 复制终端回显的授权 URL 到有浏览器的机器完成授权 |

## 7. 其他官方远端

- **Atlassian（Jira/Confluence）**：`rivo mcp install http atlassian https://mcp.atlassian.com/v1/sse --capability tools --toolset ... --oauth --auto-login`，其余流程同构；社区 stdio + API token 为回退路径（用 `--bearer-env`）。
- **Figma**：官方 Dev Mode 为本地桌面端口（`http://127.0.0.1:3845/sse`），属回环 fixture 直接可用；如经由私网主机名，安装追加 `--allow-loopback`。

---

[English](mcp-remote-smoke.md)

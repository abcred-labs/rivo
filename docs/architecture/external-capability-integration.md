# 通用外部能力接入与完备性审计

状态：拟议实现中。本文把 Rivo「作为通用 Agent 接入所有外部开放能力」的目标收口为一套可验证的接入契约，并以 Azure DevOps、Jira（Atlassian Rovo）、Figma 三家**官方公布的 AI 接入方案**为检验清单，审计我方 MCP 客户端的能力完备性。安全强制边界仍以 [安全策略](safety-policy.md) 为准，工具执行契约见 [工具运行时契约](tool-runtime.md)。

## 定位

- MCP 是 Rivo 接入外部开放能力的**唯一通用边界**：任何外部系统（DevOps、需求跟踪、设计、知识库、数据源）都以 MCP server 的形式接入，不为单一厂商编写原生工具。
- ADO / Jira / Figma 是「通用接入」的三个示例，用途是**反向验证**：如果我方能连上这三家官方主推的接入方式，就证明通用接入能力基本完备；连不上的地方即为能力缺口。
- 完备性判据：以「能连上厂商官方主推的远端 server 并完成一次工具调用」为准，而非仅覆盖本地/备选路径。

## 三家官方接入现状

三家官方主推方案高度一致，均为 **Remote Streamable HTTP + OAuth 2.1**：

| 厂商 | 官方主推 | 端点 | 鉴权 | 备选路径 |
| --- | --- | --- | --- | --- |
| Azure DevOps | 远端（推荐，公开预览） | `https://mcp.dev.azure.com/{organization}` | Microsoft Entra OAuth（浏览器登录） | 本地 stdio `npx -y @azure-devops/mcp <org>`（`--authentication azcli`，`-d` domains 分组过滤工具） |
| Jira / Atlassian Rovo | 仅远端 | `https://mcp.atlassian.com/v1/mcp`（或 `/v1/mcp/authv2`） | OAuth 2.1 | API token（可选） |
| Figma | 远端（首选） | `https://mcp.figma.com/mcp` | OAuth | 桌面本地 server（回环 SSE，企业特定场景） |

补充约束：

- **Azure DevOps**：工具面广、按 domain（core、work、work-items、search、test-plans、repositories、wiki、pipelines、advanced-security）分组，需分页与 toolset 过滤。
- **Atlassian**：站点级速率限制；MCP server 只是集成层，不存储 Jira/Confluence 内容，按登录用户权限代理。
- **Figma**：远端 server **仅允许 Figma MCP Catalog 名单内的客户端连接**。这是 Rivo 的外部阻塞项——未入目录前只能走桌面本地 server。

## 能力完备性矩阵

以官方远端接入为标尺，对照我方 MCP 客户端（`rivo-tools::mcp` / `rivo-tools::oauth`）现状（✅=已实现并单测，🚧=部分/待定，❌=未实现）：

| 官方要求 | 我方现状 | 判定 | 缺口 |
| --- | --- | --- | --- |
| OAuth 2.1（`401` → 受保护资源元数据 RFC 9728 → 授权服务器元数据 RFC 8414 → 动态客户端注册 RFC 7591 → PKCE 授权码 → 本地回环回调 → token 存储/刷新） | `oauth.rs` 实现发现/注册/PKCE/授权 URL/换取/刷新与回环 authorizer；token 存 **OS keychain**（`store::KeyringStore`），connect 自解析（命中→注入；过期→refresh 静默刷新→回存；缺失/失败→按需登录）；`rivo mcp login` 交互式登录 + `--auto-login`；AS 元数据发现兼容 Entra 的 RFC 8414/OIDC 变体；mock 授权服务器 + MemoryStore 全链路单测通过 | ✅（真实远端 ADO 冒烟见 runbook） | G1 |
| Streamable HTTP 正确性（`text/event-stream` SSE 响应解析、`Mcp-Session-Id` 会话、`MCP-Protocol-Version` 请求头、GET 监听流） | SSE 解析、`Mcp-Session-Id` 回传、`MCP-Protocol-Version` 头、`listen_server_stream` GET SSE 长连监听（含 405 静默降级）均已实现并单测 | ✅ | G2 |
| `tools/list` 等分页 `nextCursor` | `list_paged` 循环翻页至无游标，128 能力上限保护，已单测 | ✅ | G3 |
| API token / PAT 备选（bearer 头） | `McpHttpAuth::Bearer { token_env }` 从父进程环境变量注入 `Authorization`，已单测 | ✅ | G4 |
| stdio 每-server 环境变量注入 | `McpTransport::Stdio.env` 映射子进程变量名→父变量名，connect 时注入，已单测 | ✅ | G5 |
| 本地回环放行（Figma 桌面、ADO 本地） | `StreamableHttp.allow_loopback` + `--allow-loopback` 仅对该 server 放行回环/私网校验，不波及 `web_fetch` SSRF，已单测 | ✅ | G6 |
| `notifications/tools/list_changed`、elicitation、sampling | `McpServerHandler` trait + initialize 能力协商 + 流式 SSE 分发 server 发起的请求/通知（POST 在途 + GET 长连两条通道）；`take_tools_changed` 暴露列表变更。**agent 侧**：sampling 经 opt-in `allow_sampling` 门控桥接本地 LLM（`SamplingHandler`：模型固定为默认模型、`tools:[]`/`ToolChoice::None`、输出 token 硬顶 1024、每连接调用预算 8）；elicitation 经并发侧信道（`mpsc<PendingElicitation>` + `oneshot` 应答）桥接会话交互面：仅交互前端（`channel_with_metadata`：TUI/app_server/CLI）注入通道并渲染 `InteractionRequired`，`RespondInteraction` 回传 accept/decline/cancel（按 `requestedSchema` 归一化内容）；headless/一次性运行无通道→默认 `{action:"decline"}`，5 分钟超时/应答方断开亦兜底 decline；mock server 单测覆盖 elicit/sampling 往返、list_changed、GET 监听分发、sampling 桥接与预算、elicitation accept/decline/cancel 映射与内容归一化 | ✅ | G7 |

**净结论**：G1–G7 协议层与 agent 侧接线均已实现并单测。G1 的 OAuth 2.1 端到端链路（发现→动态注册→PKCE→换取/刷新→回环回调）已在 `rivo-tools::oauth` 落地，token 存 OS keychain 并由 connect 自解析/静默刷新，交互式 `rivo mcp login` 与 `--auto-login` 就绪，AS 元数据发现兼容 Entra。G7 在 `rivo-tools::mcp` 落地了 server 发起请求/通知的处理框架（`McpServerHandler`）、initialize 能力协商、流式 SSE 边收边分发（server 请求经独立 POST 回应、通知置位 `tools_changed`）、以及 `listen_server_stream` GET SSE 长连监听通道（无在途请求时接收 server 发起的请求/通知，405 静默降级）。agent 侧已完成接线：sampling 经 per-server `allow_sampling` 门控桥接本地 LLM（模型固定、无工具递归、token/调用数硬顶）；elicitation 经并发侧信道桥接会话交互面（TUI/app_server/CLI 渲染 `InteractionRequired`，`RespondInteraction` 按 `requestedSchema` 归一化回传 accept/decline/cancel），headless/无交互面运行默认 decline，5 分钟超时兜底。真实官方远端（Azure DevOps）冒烟步骤见 [远端 MCP OAuth 冒烟手册](../runbooks/mcp-remote-smoke.md)，并附 `#[ignore]` 冒烟骨架 `e2e_remote_mcp_oauth_readonly_smoke`（env 门控、需人工浏览器 OAuth，默认不跑）。

## 接入架构

```text
外部开放能力（ADO / Jira / Figma / …）
        │  官方 MCP server（远端 Streamable HTTP + OAuth 2.1 为主）
        ▼
MCP 客户端  ── 传输层：Streamable HTTP（SSE/会话/协议头）| stdio（结构化 program/args/env）
           ── 鉴权层：OAuth 2.1（PKCE/动态注册/刷新）| API token/PAT | 本地回环（受限放行）
           ── 发现层：initialize → 分页 tools/resources/prompts
        ▼
ToolCatalog → ToolSelector（mode/capability/toolset）→ ToolRegistry → policy → runtime
```

- 传输、鉴权、发现三层解耦：新增一家厂商只需选择传输 + 鉴权组合，不改上层。
- toolset / domain 过滤在发现层收敛工具数量，满足模型可见工具上限（64）与每 server 能力上限（128）。

## 鉴权契约

### OAuth 2.1（G1，主路径）

- 首次调用遇 `401 Unauthorized` 时，从 `WWW-Authenticate` 或受保护资源元数据（RFC 9728）发现授权服务器；再取授权服务器元数据（RFC 8414）。
- 若无预注册客户端，执行动态客户端注册（RFC 7591）。
- 使用 PKCE（S256）授权码流；本地起一次性回环 HTTP listener 接收 redirect callback；打开系统浏览器完成登录与授权。
- 换取 access/refresh token，写入 **OS keychain**（`store::KeyringStore`，服务名 `rivo-mcp-oauth`，account = server 名）；connect 自解析，过期时用 refresh token 静默刷新并回存。
- token 只存 server 维度、入 OS keychain，绝不进入日志、SQLite 明文、诊断包或事件流。
- 发现兼容性：授权服务器元数据按候选 URL 逐个尝试（RFC 8414 路径感知 → 追加式 → OIDC `openid-configuration`），兼容 Azure DevOps / Entra 的端点形态。
- 配置：`rivo mcp install http <name> <url> --oauth [--oauth-scope <s,s>] [--oauth-client-id <id>] [--oauth-client-name <name>] [--oauth-token-env <ENV>] [--oauth-redirect-port <port>] [--auto-login]`。无 `--oauth-client-id` 时走动态注册。交互式登录：`rivo mcp login <name>`。实现位于 `rivo-tools::oauth`（`run_authorization` / `LoopbackAuthorizer` / `refresh_stored_token` / `authorization_for_connect` / `BrowserLogin` / `store`）；`--auto-login` 时 connect 在交互上下文自动拉起登录，headless/CI 则报错不阻塞。

### API token / PAT（G4，备选）

- 以 `header_env` / 加密凭据引用注入 bearer 头，覆盖 Atlassian API-token、ADO PAT、Figma REST token 等场景。
- 只存环境变量名或凭据键，不存明文。
- 配置：`rivo mcp install http <name> <url> --bearer-env <ENV>`（`McpHttpAuth::Bearer`，connect 时从父进程变量读取并注入 `Authorization: Bearer`）。

### 本地回环（G6，兜底）

- 仅对**显式配置、在 allowlist 内、且 per-server 标记 `allow_loopback`** 的本地 MCP server 放行回环地址；默认关闭。
- 该放行只作用于 MCP 客户端，**绝不改动 `web_fetch` 等通用 HTTP 的 SSRF 防护**。

## 传输契约（Streamable HTTP，G2）

- 请求头带 `Accept: application/json, text/event-stream`；发送 `MCP-Protocol-Version`；`initialize` 后从响应头捕获 `Mcp-Session-Id` 并在后续请求回传。
- 响应 `Content-Type` 为 `text/event-stream` 时按 SSE 帧解析出 JSON-RPC 消息；为 `application/json` 时直接解析。
- 保持 1 MiB 结果上限、60 秒调用超时、无自动 redirect 的既有边界。

## 发现契约（G3）

- `tools/list`、`resources/list`、`prompts/list` 遇 `nextCursor` 时循环翻页，直至无游标；总量仍受每 server 128 能力上限约束，超限显式失败而非静默截断。

## 验证方式

- 已实现单元/集成（`rivo-tools`）：
  - OAuth：mock 授权服务器覆盖「受保护资源元数据 → 授权服务器元数据 → 动态注册 → PKCE 授权码 → token 交换」全链路；PKCE 用 RFC 7636 已知向量校验 S256；`WWW-Authenticate` 解析、授权 URL 构造、state 校验、token 过期余量、refresh 保留、回环 authorizer 捕获回调各有独立用例；AS 元数据候选 URL 推导（RFC 8414 路径感知 + OIDC 回退）以 Entra 风格 issuer 验证；connect 自解析（缺失报错 / mock 登录回存 / 命中免登录）以 `MemoryStore` 验证。
  - Streamable HTTP：mock SSE server 覆盖会话与流式解析（跳过 notifications、匹配 request_id、多行 data）；`Mcp-Session-Id` 捕获与回传；`nextCursor` 多页分页。
  - stdio env 注入：子进程回显被注入变量以断言透传；缺失引用连接失败。
  - CLI：`mcp install http` 的 `--bearer-env`、`--oauth`（动态注册）、`--auto-login`、bearer/oauth 互斥各有解析用例；`allow_loopback` 对私网 http 目标的放行门控有 connect 用例。
  - G7 server 发起消息：`classify_inbound` 区分 server 请求/通知/响应；`take_sse_event`/`sse_event_data` 增量 SSE 分帧；`DecliningServerHandler` 对 server 请求回 JSON-RPC error；mock SSE server 覆盖「tools/call 期间 server 发 elicitation → 客户端独立 POST 回应 → 服务端回最终结果」往返、`notifications/tools/list_changed` 置位 `tools_changed`、initialize 在有处理器时广告 elicitation/sampling 能力；`listen_server_stream` GET 监听单测覆盖「GET 流内 list_changed + elicitation → 客户端独立 POST 回应」及 405 静默降级。
- 安全回归：确认 OAuth token / PAT 不落日志、SQLite 明文与诊断包（token 只进 OS keychain；配置只存环境变量名/鉴权类型）；确认 `allow_loopback` 放行不波及 `web_fetch`。
- 显式 live smoke（需人工浏览器授权，不进默认测试）：按 [远端 MCP OAuth 冒烟手册](../runbooks/mcp-remote-smoke.md) 对 ADO `mcp.dev.azure.com/{org}` 完成一次 `read_only` 工具调用并留存证据；Jira/Figma 同构。

## 开放问题

- Figma 远端目录准入：未入名单前以桌面本地 server 兜底，是否申请入目录待定。
- 协议特性（G7：`tools/list_changed`、elicitation、sampling）已落地（`McpServerHandler` + 流式 SSE 分发 + 能力协商 + `take_tools_changed` + `listen_server_stream` GET 长连监听）。**agent 侧接线已完成**：sampling 经 per-server `allow_sampling` 显式 opt-in 门控，`SamplingHandler` 把 `sampling/createMessage` 桥接到本地 LLM（模型固定为默认模型、`tools:[]`/`ToolChoice::None` 防递归、输出 token 硬顶 1024、每连接调用预算 8、不记内容仅记 server 名/token 数）；未启用时沿用默认拒绝。elicitation 经并发侧信道桥接会话交互面：`McpBridgeHandler` 持 `mpsc::Sender<PendingElicitation>`，在 `elicitation/create` 到达时构造 `InteractionRequest`（附 `requestedSchema` 字段提示 + 「拒绝」选项）经侧信道送出并 `await` `oneshot` 应答；session 的 turn 循环在 agent 事件流之外 `select!` 该信道→登记共享注册表→发 `AgentEvent::InteractionRequired`，`Intent::RespondInteraction` 取回注册表项→按 `requestedSchema` 把用户答复归一化为 accept 内容（JSON 对象原样 / 单字段类型强转 / `{"value":原文}` 回退）/decline（哨兵）/cancel（空白），经 `oneshot` 回传桥接处理器构造 MCP `{action,content}`。**门控**：仅交互前端（`channel_with_metadata`：TUI/app_server/CLI）注入侧信道；headless/一次性运行无通道→默认 `{action:"decline"}`；5 分钟超时或应答方断开亦兜底 decline；内容不入日志、无新增 config flag。**已接受的限制**：mid-stream elicitation 无法 SQLite 持久化恢复（进程重启则该次工具调用失败）。

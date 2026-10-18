# A2A 双角色互操作架构

> 状态：`ready`（V1 Windows 运行时；跨平台协议互操作验收规格）
> 协议基线：Agent2Agent Protocol `v1.0.1`
> 范围：Rivo 作为 A2A Client 与受控局域网 A2A Server 的互操作架构
> 非目标：本设计不修改既有 V1 契约或中立 app-server host 契约，不把 `rivo app-server --stdio` 变为网络服务。

## 1. 目标与边界

Rivo 的 A2A 能力分为两个可独立交付的 feature：

- **A2A Client**：本地 Rivo 发现、配对并委派任务给受信任的远端 Agent。
- **A2A Server**：本地 Rivo 在受控局域网中以明确、受认证的技能接受其他 Agent 的任务。

两种角色可以部署在同一台 Rivo 实例中，但各自拥有独立的配置、凭据、启用开关、策略、审计和资源预算。启用 Client 不会监听端口；启用 Server 不会自动向任何对端委派任务。

A2A 解决独立 Agent 之间的发现、任务协作、流式状态和结果交换问题。它不替代 MCP：MCP 用于 Agent 调用工具与资源，A2A 用于 Agent 与 Agent 合作。A2A 也不规定一个 Agent 如何实现内部子 Agent 编排、模型循环或工具调用；这些继续由 Rivo 内核及 MCP 负责。

## 2. 产品原则

- **发现不等于信任**：mDNS/DNS-SD 只能产生候选对端。只有本机用户完成配对并固定身份后，候选才可进入委派或入站授权路径。
- **远端不拥有本机权限**：远端 Client 不能批准本机文件写入、进程、MCP action、网络外发或凭据使用；这些行为始终由本机所有者批准。
- **最小可见面**：Client 只在当前 turn 明确选择的 peer 中委派；Server 只公开配置中定义的技能和最小公开 Agent Card。
- **本地内核唯一**：A2A 只负责边界适配。所有本机任务仍通过 `Session -> Agent -> policy -> tools` 执行，禁止复制第二套 Agent loop。
- **显式数据外流**：发送给远端 Agent 的文本、证据与 Artifact 都是数据外流；远端返回内容一律是不可信外部输入，不能改变提示词、policy、确认或工具边界。
- **演进优先于锁定 SDK**：Rivo 固定 A2A `v1.0.1` schema facade，并以内部 trait 隔离 JSON-RPC、HTTP+JSON/REST、gRPC、发现与认证实现，避免上层依赖某个第三方 SDK 的公共类型。

## 3. 分层与依赖

```text
CLI / TUI / 本机审批命令
  -> A2A 管理命令、审批界面、当前 turn peer 选择
  -> Session Intent / AgentEvent
  -> Agent loop -> ToolRegistry
  -> policy -> tools::a2a -> rivo-a2a Client
  -> rivo-a2a Server -> Session 驱动器 -> Agent loop
  -> storage / telemetry / OS keychain
```

拟议新增 `rivo-a2a` crate，职责如下：

- 固定版本的 A2A schema facade、Agent Card、Message、Task、Artifact、错误码与状态映射。
- `A2aTransport`、`A2aClient`、`A2aServer`、`PeerTrustStore`、`A2aTaskStore` 与 `ApprovalBroker` trait。
- HTTPS/mTLS、JSON-RPC 请求与 SSE 事件的边界处理、协议版本协商、大小限制和脱敏错误。
- 对端状态、配对身份和任务映射的序列化模型。

`rivo-a2a` 只依赖 `core` 与基础网络/序列化依赖，不依赖 `session`、`agent`、`tools`、CLI 或 TUI。`tools::a2a` 是出站委派工具适配器；Server 的 Session 驱动器由 `session` 实现 A2A crate 的反向 trait；监听进程和命令行入口位于 `cli`。

`app-server` 继续是本机 stdin/stdout bridge。它不监听端口，不持有 A2A TLS 身份，不解析 Agent Card，不能被 A2A Server 复用或通过网络转发。

## 4. 协议与传输

### 4.1 冻结协议面

首期以 A2A `v1.0.1` 的规范 Protobuf 数据模型作为唯一语义来源。仓库固定经过审计的 `a2a.proto` 输入及其生成版本，并在 `rivo-a2a` 暴露本地 facade；升级协议版本必须新增兼容矩阵、fixture、迁移说明和显式版本协商测试。

首期支持的 A2A 语义为：

- Agent Card 获取与受认证的 Extended Agent Card 获取。
- `SendMessage`、`SendStreamingMessage`、`GetTask`、`CancelTask` 与 `SubscribeToTask`。
- `Task`、`Message`、文本或 JSON `Part`、文本或 JSON `Artifact`、Task 状态和状态更新事件。
- `SUBMITTED`、`WORKING`、`COMPLETED`、`FAILED`、`CANCELED`、`INPUT_REQUIRED`、`REJECTED` 与 `AUTH_REQUIRED` 状态。

初期拒绝二进制 Part、任意 URL 下载、远端文件上传、未声明 extension、push notification、跨组织 OAuth 委派和跨版本宽松降级。它们保留为未来扩展点。

### 4.2 绑定策略

首个实现绑定为 **JSON-RPC 2.0 over HTTPS**：

- 普通请求使用 JSON-RPC 请求/响应。
- 流式任务状态使用 `SendStreamingMessage` 与 SSE。
- 每个 `AgentInterface` 明确声明 binding、URL、协议版本和认证要求；Client 只能选择自己支持且 Agent Card 已声明的 interface。
- HTTP+JSON/REST 与 gRPC 以独立 `A2aTransport` 实现加入，不改变 `A2aClient`、Session 或工具接口。

Client 不因连接失败猜测 URL、切换未声明 binding 或绕过证书验证。不能协商到受支持 interface 时返回 `a2a_unsupported_interface`。

## 5. 稳定对象与公共接口

`core` 负责跨 Session、storage、CLI 和未来 IPC 可序列化的稳定身份：

| 类型 | 含义 |
| --- | --- |
| `A2aPeerId` | 本机生成且稳定的受信任对端标识，不等于 URL、证书指纹或 Agent Card name。 |
| `A2aTaskId` | 对端 Task 标识的安全包装，不可与本机 `TurnId` 或 `SessionId` 混用。 |
| `A2aDelegationRef` | 本地 turn/tool call 与 peer/task/context 的脱敏关联。 |
| `A2aTrustSnapshot` | 配对时固定的 endpoint、SPKI 指纹、Agent Card identity/hash、允许技能和授信状态。 |
| `TurnOrigin` | `Local`、`A2aOutbound`、`A2aInbound`；用于审计、提示词边界和策略展示，不授予权限。 |

`rivo-a2a` 对上提供以下稳定 trait：

```text
A2aTransport
  discover_interface(card) -> SelectedInterface
  send_message(request) -> TaskOrMessage
  stream_message(request) -> EventStream
  get_task(peer, task) -> Task
  cancel_task(peer, task) -> Task

PeerTrustStore
  list_candidates() / get_peer() / approve_pairing() / revoke()
  verify_snapshot(peer, card, certificate) -> TrustDecision

A2aTaskStore
  reserve_outbound() / record_update() / resolve_idempotency()
  reserve_inbound() / map_session() / terminalize()

ApprovalBroker
  request_local_approval() -> LocalApproval
  request_remote_input() -> RemoteInput
```

这些 trait 不返回原始 TLS 对象、私钥、认证 header、Provider frame 或本机绝对路径。

## 6. A2A Client

### 6.1 当前 turn 选择

`TurnOptions` 拟议增加 `a2a_peers: Vec<A2aPeerId>`。它遵循 Web 与 MCP 的规则：

- 默认空，不会持久化为自动启用。
- CLI 使用可重复的 `--a2a-peer <peer-id>`；TUI 使用 `:a2a <peer-id>|off`。
- 只有已配对、未撤销且快照未变化的 peer 可以被选中。
- peer 选择只让模型看见单一 `a2a_delegate` 工具，不为每个远端技能注册独立工具。

`a2a_delegate` 参数固定为 `peer_id`、`skill_id`、`message`、`stream` 和可选、受限制的文本/JSON Artifact。远端技能列表由本机缓存的、已批准 Agent Card 提供；模型不能调用发现、配对、撤销或修改授信范围。

### 6.2 策略与数据外流

`policy` 拟议新增 `Operation::A2aDelegation`。canonical request 必须包含：

- 固定 peer ID、已验证 endpoint、证书指纹和 Agent Card hash。
- 目标 skill、任务摘要、数据分类、文本/Artifact 字节数、并发占用和期望超时。
- 发送的 EvidenceRef 仅引用允许外发的内容摘要；不得自动展开本地文件、历史、记忆、凭据或 MCP 原始结果。

默认行为如下：

- `strict` 拒绝所有 A2A 委派。
- `medium` 对已选、已信任 peer 的只读/研究型委派要求一次性数据外流确认；确认绑定 peer、skill、摘要和内容 hash。
- `full_control` 不绕过 peer 选择、信任快照、协议限制、资源预算、凭据脱敏或远端身份检查。

远端结果作为不可信 `ToolObservation` 进入当前模型上下文。结果不得携带系统指令、工具定义、确认指令或可执行 Shell；只允许受 schema 校验、上限约束的文本/JSON。

### 6.3 任务生命周期

Client 发送任务时先持久化本地 idempotency reservation，再调用 `SendMessage` 或 `SendStreamingMessage`。网络中断后：

- 未确认请求是否到达对端时，先以已保存的 task/context 或 message ID 查询任务，不盲目重发。
- 可恢复时使用 `GetTask` 或 `SubscribeToTask` 继续读取。
- 对端 terminal 状态只映射为当前工具调用的结构化成功、取消、拒绝或失败，不能伪造本机 turn 已完成。

默认资源限制：

| 项目 | 默认值 |
| --- | ---: |
| 每 peer 活动任务 | 1 |
| 本机 Client 活动任务 | 3 |
| 单个请求正文 | 64 KiB |
| 单个远端观察进入模型上下文 | 256 KiB |
| 单个任务时限 | 30 分钟 |
| 对端 Artifact 数量 | 16 |

超限、超时、证书变化、Card 变化、协议不兼容和取消都使用稳定 A2A 错误分类并写入审计。

## 7. 受控局域网发现与配对

### 7.1 发现

Rivo 定义 `rivo-a2a-lan-v1` mDNS/DNS-SD 扩展。广播仅可包含：

- service endpoint、最小 Agent Card URL、A2A protocol version。
- Server public key/SPKI 指纹、实例 nonce、过期时间和公开名称。

广播禁止包含 client certificate、私钥、访问 token、workspace、完整 Extended Card、任务内容或本机用户身份。收到广播后，Client 只显示“候选”，不自动连接执行任务。

### 7.2 配对

配对采用本机用户批准的 TOFU 加 SAS + CSR。Client 先固定 Server SPKI、生成不可导出私钥和
CSR，再核对绑定 request ID、Server SPKI 与 Client 公钥 hash 的六词 SAS；完整 request、claim、
证书与撤销规则见 11.2。Client 在 OS keychain 保存私钥/证书引用，配置只保存 peer ID、公开
endpoint、固定指纹、Card hash、技能 allowlist 与状态。任一固定信任字段变化都立即停用 peer 并要求
重新批准。

## 8. A2A Server

### 8.1 网络边界

Server 仅由显式命令启动：

```text
rivo a2a serve <server-profile>
```

默认关闭。启用时必须满足：

- 仅绑定明确的 RFC1918 IPv4 或 ULA IPv6 地址；拒绝 `0.0.0.0`、`::`、公网地址、DNS 名称推导和自动端口暴露。
- 仅接受 HTTPS，TLS 1.2+，使用受保护的服务端私钥和 mTLS 客户端认证。
- 最小 Agent Card 可匿名读取，但不包含私有工作区、MCP、凭据或扩展技能信息。
- Extended Agent Card、任务请求、任务查询、流式状态和取消都要求已配对 mTLS 身份。

`app-server`、普通 CLI stdin/stdout 和 Browser CDP 不可作为 A2A Server 的传输层或身份层。

### 8.2 服务技能

Server 通过不可被远端覆盖的 `ServiceProfile` 声明技能。每个 profile 固定：

- workspace alias，而非客户端提供的路径。
- capability profile、policy ceiling、网络/Browser 开关和准确 MCP toolset。
- 最大并发、任务时限、消息/Artifact 上限与允许对端。
- 是否允许本机人工审批、审批超时和审计保留策略。

默认 profile 只允许 `research` + `strict`，网络、Browser、MCP 和记忆写入均关闭。`coding` 必须显式开启并使用 `medium` 或更严格的 ceiling；A2A Server 永不接受远端传入的 `full_control`、模型配置、MCP URL、credential、workspace 或 Browser 参数。

### 8.3 入站会话与状态

收到请求后，Server 按以下顺序处理：

```text
TLS/mTLS -> peer grant -> Card/skill -> quota/idempotency
  -> A2A task reservation -> new local Session -> Agent loop
  -> local approval broker -> A2A task events
```

每个入站 A2A Task 对应独立本机 Session 和 Turn。远端 `taskId`、`contextId` 不暴露本机 SessionId、TurnId、工作区路径、历史或审计 ID。远端只能读取自己身份创建的 task，不能列举本机历史或其他 peer 的任务。

入站幂等键为 `client_certificate_fingerprint + message_id + request_hash`，并受 ServiceProfile 保留期约束；同 identity/message 而请求 hash 不同返回 `a2a_idempotency_conflict`。Server 重启后从 storage 恢复任务映射和终态；无法恢复的活动任务固定为 `FAILED`，不能重新执行或误报完成。

### 8.4 本机所有者审批

远端调用方无权批准本机副作用。进入确认时：

- Server 向本机 `ApprovalBroker` 发送完整、已脱敏的 policy 确认请求。
- 对端只收到不含路径、命令、参数正文或凭据的 `INPUT_REQUIRED` 状态和“等待服务端本机授权”摘要。
- 本机 TUI 或 `rivo a2a approvals` 批准一次后，才向同一 Session 投递 `Intent::Confirm`。
- 当前 CLI 实现通过 Server state SQLite 的本机审批队列跨 `serve` 与独立 owner 终端传递请求；
  队列记录 task/turn/request/call/tool 及 canonical 参数 hash，不记录原始工具 JSON。`approvals`
  命令需要本机 TTY 和二次确认，批准会原子消费为 `Once`；该队列不对应任何 TLS/HTTP 路由。
- 本机拒绝、审批超时、撤销配对或 Server 中断分别产生稳定的 `REJECTED`、`FAILED` 或 `CANCELED` terminal 状态。
- 未配置可用 `ApprovalBroker` 时，任何需要确认的操作默认拒绝，绝不将确认权降级给远端 Client。

远端需要补充业务信息时，Server 可使用单独的 `INPUT_REQUIRED` 交互请求；此类请求只能收集 ServiceProfile 明确允许的、受 schema 限制的内容，不能作为本机 policy 确认的替代。

## 9. 配置、密钥与存储

Client 的 peer trust snapshot、任务映射和撤销状态由 SQLite 持久化；私钥、mTLS 客户端证书、配对 token 和未来 OAuth token 只存 OS keychain。Server 使用独立的 owner-only `owner-a2a-server.toml`，其真实 schema 为顶层 `schema_version`、`state_db`、`workspace_aliases`、`[[profiles]]` 和 `[profiles.profile]`，不得写成既有主配置中的 `[a2a.server_profiles]`。可复制模板见[跨平台研究集群组建手册](../runbooks/a2a-cross-platform-cluster.zh-CN.md)。配置和 storage 均不得保存私钥、token、Authorization header、完整 TLS 握手、原始 MCP/Provider 帧或未脱敏凭据。

## 10. 可扩展性

下列能力通过已有抽象演进，不改变 `FEAT-021-a2a-client` / `FEAT-022-a2a-server` 的语义：

- `A2aTransport` 增加 HTTP+JSON/REST、gRPC 与协议版本适配。
- `PeerTrustStore` 增加企业目录、OIDC/OAuth、短期工作负载身份和集中撤销。
- `A2aTaskStore` 增加 push notification、分布式任务恢复、加密 Artifact 存储和多租户隔离。
- `ApprovalBroker` 可增加审计审批队列和多管理员策略。
- mDNS 候选发现可被企业目录替代，但目录下发仍必须进入同一信任快照和批准流程。

公网生产部署、多租户控制面、跨组织委派、push callback、二进制 Artifact、开放注册中心和无人值守高风险操作均不属于 `FEAT-021-a2a-client` / `FEAT-022-a2a-server`。

## 11. 互操作约束

本节的规则优先于本文中较早的概述性表述，并作为两个 A2A Feature 的实现前置条件。

### 11.1 版本、Card 与 wire

- 规范发布基线固定为 v1.0.1；配置字段 spec_revision 只记录该基线。Agent Card 的 interface 线协议版本固定为 1.0，并由独立的 wire_protocol_version 字段记录；两者不得互换。
- 最小公开 Agent Card 固定为 GET /.well-known/agent-card.json。V2 Server 的唯一 interface 是 https://<private-ip>:<port>/a2a，binding 为 JSONRPC，wire version 为 1.0；普通请求和响应是 application/json，流式响应为 SSE。
- Card 只宣布 mTLS、text/json 输入输出，以及实际启用的 streaming/extended-card capability。tenant 只有在 Card interface 明确声明时才透传，且必须和信任快照完全匹配。
- 接收 Card 时拒绝重复 JSON key，并对 RFC 8785 JCS 规范化的 UTF-8 字节计算 SHA-256。快照固定完整 Card hash、interface URL、binding、tenant、wire version、SPKI 和技能 allowlist；任一值变化即 suspended。
- JSON-RPC wire 错误只使用 A2A v1.0.1 的数值 code/data；a2a_* 仅是本机稳定语义码。仓库必须保存审计过的规范输入来源、tag、SHA-256、生成器版本和 fixture，禁止手写或猜测错误映射。

### 11.2 SAS + CSR 配对

配对是 rivo-a2a-lan-v1 身份引导扩展，不是 A2A Task API。每个 ServiceProfile 都有独立
issuer_ref，引用 OS keychain 中的 P-256 私有 CA；Server TLS 身份使用独立 identity_ref 的
P-256 自签名证书，SAN 必须为 profile 的私网 bind IP。Client 信任 Server 时校验 Card URL 的 IP
SAN、证书有效期和首次配对确认的 SPKI pin，而不依赖公网 CA。

Client 在 OS 安全存储生成不可导出的 P-256 私钥和 CSR。POST
/rivo-a2a-lan-v1/pairings 只接受 version 和 csr_pem；Server 创建 10 分钟、单次使用且限速的
pending request，返回 request_id、server_spki_sha256、client_public_key_sha256、expires_at、
六词 SAS 与 256-bit claim_token。Server 只持久化 claim_token 的 SHA-256。

SAS 固定为 SHA-256(rivo-a2a-lan-v1/sas || request_id || server_spki_sha256 ||
client_public_key_sha256) 的前 66 bit，经固定 2,048 词表编码为六词；双方必须独立计算并显示。
Server 本机用户通过 rivo a2a pairings list|approve|reject <request-id> 核对该 SAS、endpoint、
SPKI、Card hash 与 Client 公钥摘要。

批准后，Client 向 POST /rivo-a2a-lan-v1/pairings/{request_id}/claim 发送 request_id、
claim_token 及使用 CSR 私钥签名的 claim proof。Server 验证 token hash、SPKI、Client 公钥和 proof
后一次性返回证书链。Client 证书有效期为 365 天，具有 digitalSignature、clientAuth EKU、随机
serial 与不透明 peer ID URI SAN；不含用户、机器或 workspace 信息。证书到期、撤销或 Client
公钥变化均要求重新 SAS + CSR 配对，不提供自动续期。

Task API 在 TLS 连接建立时同时校验 issuer、有效期、clientAuth EKU、serial/fingerprint 与 profile
grant。删除或禁用 grant 即时拒绝新连接、取消活动 task 并撤销未完成审批，不维护公开 CRL。Client
撤销本机立即生效；在线时尝试通知 Server，失败时必须报告需要在 Server 本机撤销，不能声称双端已同步。

### 11.3 状态、恢复与外发数据

- 私有状态固定放入 metadata 的 io.rivo.a2a.v1 对象，不新增 Proto 字段或 required extension。对象可含 request_kind、request_id 和 revision；外部 A2A 对端可以安全忽略。
- INPUT_REQUIRED 的 local_approval 只包含 request ID 和脱敏摘要；business_input 在 TaskStatus.message 的 JSON Part 中声明 request ID 与 JSON Schema。前者只接受本机 ApprovalBroker 裁决；后者只接受同 task/context 的 schema 校验消息。
- 状态转换固定为 SUBMITTED 到 WORKING，WORKING 与 INPUT_REQUIRED 双向转换，最终为 COMPLETED、FAILED、CANCELED 或 REJECTED。本机拒绝、无 broker、审批超时和 policy 拒绝为 REJECTED；取消或撤销为 CANCELED；恢复失败、工具不可恢复失败和业务输入超时为 FAILED。无效 business input 保持 INPUT_REQUIRED。
- 取消、撤销与审批超时优先于尚未执行的确认，并使对应 broker request 失效。以持久化 revision 的 compare-and-swap 终止 task；已开始的工具使用本机 Session 取消语义；每个 task 只允许一个 terminal 状态。
- 入站幂等键是 client_certificate_fingerprint、message_id 与 request_hash，并在 ServiceProfile 的保留期内持久化。相同 identity/message/hash 返回原结果；相同 identity/message 而 hash 不同返回 idempotency conflict。无法在重启后恢复的活动 task 固定 FAILED，绝不重放。
- SSE 订阅先发送完整 Task snapshot，再在同一 metadata 位置发送单调 revision 的状态/Artifact 更新。Client 先 GetTask 后订阅并按 revision 去重；外部对端缺少 revision 时按状态时间和 Artifact ID 去重；断线恢复不能创建第二个业务 task。
- 外发数据分类为 user_provided、public_external、workspace_derived、private_memory、credential 或 unknown。private_memory 和 credential 硬拒绝；其余类别在 medium 与 full_control 下都要求一次性外发确认，确认绑定 peer、skill、摘要、message hash 和所有 Artifact hash。
- a2a_delegate 不得在通用 ToolRequested 或 ToolObservation 公共事件中暴露原始 message/Artifact；仅发出脱敏 A2A 专用事件。原始内容仅进入本机 Session history，且不进入 JSONL、审计摘要、诊断包或 app-server。

### 11.4 ServiceProfile

ServiceProfile 是启动时校验的只读配置。A2A 使用独立 schema_version = 1；缺失整个 A2A
section 时默认关闭，不改变现有 FileConfig version。每个 profile 必须包含 skill_id、workspace alias
注册引用、capability/policy ceiling、network/browser 开关、精确 MCP server/toolset allowlist、每 peer
与全局并发、消息/输出字节上限、任务/审批时限、允许 peer、issuer_ref、grant、幂等保留期和审计保留期。
workspace alias 必须在加载和 serve 启动时 canonicalize 到本地目录；coding 最大为 medium，禁止
full_control。A2A 不兼容 schema 迁移先备份原配置后原子写回；旧二进制不得静默降级读取 A2A section。
远端请求不得覆盖任一字段。

### 11.5 发现与冻结输入

mDNS/DNS-SD 服务名固定为 _rivo-a2a._tcp.local.。TXT 只允许 v、card_path、spki_sha256、
instance_nonce、expires_unix；TTL 必须在 10 到 300 秒内。Client 仅接收私网 A/AAAA 记录，并在
expires_unix 后删除候选，不因同名、同 endpoint 或同 Card 自动合并。

规范输入固定在 runtime/crates/a2a/protocol/v1.0.1/，包含 a2a.proto、上游 tag/commit、SHA-256、
许可证、生成器版本及 JSON-RPC/SSE fixture。cargo xtask a2a-verify-protocol 必须验证这些输入与
生成物；数值错误映射仅由冻结输入产生。

## 12. 安全与验收不变量

- A2A 默认关闭；既有 V1 turn、MCP、SQLite 历史、CLI/TUI 与 app-server 语义不变。
- 对端无法改变本机 workspace、policy、模型、MCP server/toolset、网络、Browser、凭据或审批范围。
- 任何 Card、端点、SPKI、mTLS 身份或技能 grant 的变化都使既有信任失效，直到本机重新批准。
- 所有网络输入、远端消息、Artifact、Agent Card 描述和 extension 都按不可信数据处理。
- 默认协议边界 Gate 使用 Windows、Linux、macOS 都可运行的本机双进程/多进程 smoke：独立的
  `rivo` Server 与 Client 子进程经 loopback mTLS 通信，并隔离 SQLite、数据目录和测试身份。
  单元层继续使用 mock JSON-RPC Server、mTLS fixture、伪造发现广播和固定 Agent Card fixture。
  真实双主机局域网互操作只作为显式人工兼容性 smoke，不替代或阻断本机 Gate。

---

[English](a2a-interoperability.md)

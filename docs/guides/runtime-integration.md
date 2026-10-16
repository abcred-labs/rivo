# Rivo 基座接入：Desktop App 与 Remote Agent Node

> 核验日期：2026-09-11。本文说明本仓库现有运行时的接入机制与能力边界，不表示 Desktop、远程客户端或当前候选已通过产品验收。V1 产品预览仍为 Windows 11 x64；范围见产品路线图，验收见当前状态与 preview-state.json。

本文是接入操作说明。当前候选验证状态以 preview-state.json 和设计与实现状态为准；不能仅凭存在这些接口就认定为稳定可交付。

部署规格按任务负载选择，见场景分级与资源容量建议。知识对话、数据/报告生成和开发环境的资源与并发口径不同；容量分档不扩大接入权限或解除当前限制。

多个节点协作的组建顺序、任务拆分、结果交付及单节点对比，见独立的[Agent 集群工作指南](agent-cluster-workflows.md)。当前点对点委派不等于自动调度平台。

## 1. 两类产品能力，一套执行内核

**Rivo 不仅可以直接在终端使用，也可以为其他应用或 Agent 节点提供执行基座。** 两类能力应分开理解：

| 能力类别 | 面向谁 | 提供什么 | 主要入口 |
| --- | --- | --- | --- |
| 本地终端执行 | 直接操作 Rivo 的用户、脚本 | 提交任务、模型与工具执行、审批、历史和结果 | `rivo run`、`rivo tui` |
| Desktop App 基座 | 同机桌面应用的开发者 | 可嵌入的 Session/Turn 生命周期、流式事件、受控工具执行与交互回应 | `rivo app-server --stdio`；按需评估本机 `--pipe` |
| Remote Agent Node 基座 | 远程宿主或其他 Agent 的开发者 | 让执行端机器承接远程会话或独立 Agent 任务 | 宿主适配 app-server，或受控局域网 A2A Server |

后两行属于同一类“延展/基座能力”，但接入契约不同。Desktop 和 Remote Agent Node 不是新的任务模式，也不自动获得 CLI/TUI 的全部能力。

```text
终端执行：
  用户 → CLI / TUI ──────────────────────────────┐
                                                │
Desktop App 基座：                              │
  Desktop UI → 同机适配器 → app-server ──────────┤
                                                ├→ Session → Agent → policy → tools / LLM
Remote Agent Node 基座：                        │                ↓
  Web/Mobile/控制端 → 远程宿主 → app-server ──────┤         SQLite / 事件 / 日志
  另一个 Agent → HTTPS/mTLS A2A → 入站驱动器 ─────┘
```

其中 Web/Mobile 与远程宿主的完整链路需要接入方实现；A2A 则已有独立的受控网络协议实现。不能将两者统称为“把 app-server 暴露到网络”。

## 2. Desktop App 如何接入

### 2.1 机制与分工

建议从**同机子进程 + stdio JSON-RPC**接入：Desktop 的运行时适配器以结构化参数启动 `rivo app-server --stdio`，通过 stdin 发请求，持续读取 stdout 的事件与响应。不要抓取 TUI 屏幕或解析 CLI 人类文案来驱动应用。

| Rivo 基座负责 | Desktop 接入方负责 |
| --- | --- |
| Session/Turn、Agent 循环、模型路由、policy 与工具限制 | UI、输入、流式渲染、审批对话框和错误展示 |
| SQLite 历史及恢复、模型与工具事件投影 | 子进程生命周期、请求 ID、Session/Turn 关联和本地 UI 状态 |
| JSON-RPC 校验、版本、幂等、大小限制与脱敏 | 版本匹配、能力检测、异常退出与重连处理 |
| 将批准/拒绝交给已有执行边界 | 展示准确的操作对象、风险及一次性确认，不代用户自动批准 |

`--pipe` 是同一用户的 Windows Named Pipe 接入，不是网络监听，也不是 stdio 的无差别替代品：Credential 等能力有不同传输限制。首次 Desktop 集成优先核对 stdio 契约；本仓库不规定 Desktop 的私有 IPC、UI 技术或分发方式。

### 2.2 接入前准备

1. 固定要集成的 Rivo 版本/候选，并匹配 `rivo-app-server-v2` 协议；不要把历史二进制与当前 schema 5 配置任意混用。
2. 在执行端完成 Provider、Credential、discovery 与默认模型配置，确认本地路由可用。
3. 在执行端配置允许的工作区；`session/start` 的目录必须规范化后与该配置匹配，不允许客户端任意指定本机目录。
4. 以受控本机用户身份启动进程，分开处理 stdout 协议和 stderr 诊断；日志不得记录完整请求或凭据。

配置步骤见[配置与模型管理](configuration/README.md)，完整宿主准备见[本机宿主运行手册](../runbooks/app-server-local-host.md)。

### 2.3 最小会话时序

下列是宿主发给子进程的 JSON-lines 示例，不是在 PowerShell 中执行的命令。每行一个请求；等待上一步成功后继续，替换工作区和返回的 ID。示例假定已配置可用默认模型，本次文档修改未执行真实模型调用。

**握手并检查执行端：**

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"rivo-app-server-v2","client":{"name":"local-host","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"health","params":{}}
```

读取握手返回的 `capabilities`、`supported_runtime_settings` 和 `limits`，不要仅凭客户端自报名称判断权限。

**创建只读计划会话，再提交任务：**

```json
{"jsonrpc":"2.0","id":3,"method":"session/start","params":{"working_directory":"<configured-local-workspace>","runtime_settings":{"collaboration_mode":"plan","permission_preset":"read_only","network":false},"idempotency_key":"session-create-001"}}
{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{"session_ref":"<returned-session-ref>","prompt":"只读分析这个项目，列出实现步骤与验收条件。","runtime_settings":{},"idempotency_key":"turn-start-001"}}
```

`session/start` 返回 `session_ref`；`turn/start` 返回 `turn_id`，只表示任务已接受。一个 Session 同时只能有一个运行中或等待交互的 Turn。

**持续处理事件与交互：**

| 消息 | 接入方动作 |
| --- | --- |
| `message_start/delta/end` | 按 Session、Turn、message 标识更新内容；保留截断提示。 |
| `tool_call` / `status` | 展示受限摘要与状态，不索取或猜测原始工具 payload。 |
| `interaction_request` | 展示审批或补充输入；按原 interaction 标识回应。 |
| `error` | 展示稳定错误码、脱敏信息及是否可重试，不静默换模型或放宽权限。 |
| `turn_complete` | 以此作为本轮唯一终态，不以请求响应或暂时无文本判断完成。 |

例如，对仍有效的审批请求明确拒绝：

```json
{"jsonrpc":"2.0","id":5,"method":"interaction/respond","params":{"session_ref":"<returned-session-ref>","interaction_id":"<received-interaction-id>","decision":"deny","idempotency_key":"interaction-deny-001"}}
```

允许使用 `decision: "allow"`，但始终是一次性授权；普通信息补充使用 `respond`，并遵守服务端给出的选项/自由输入约束。不是每个任务都会产生审批请求，不能提前发送不存在的 interaction ID。

**停止、关闭与恢复：**

- 主动取消：`turn/interrupt`；自然完成可能与取消竞争，仍以最终 `turn_complete` 为准。
- 关闭：`session/close`；关闭会取消活动任务，不代表删除历史。
- 子进程退出/stdin EOF：活动任务会被取消，不保证脱离宿主继续运行。
- 新宿主进程恢复：重新握手，用已有 `session_ref` 调用 `session/resume`；历史不存在或过期时明确报错，不伪造新会话为恢复成功。
- 同一次状态变更重试应使用相同幂等键和语义；幂等缓存有范围和容量限制，不承诺跨重启无限去重。

完整字段、事件与错误码见 [app-server 协议](../architecture/app-server-protocol.md)。

### 2.4 Desktop 基座开放到什么程度

当前接口可以承接“任务提交 → 流式结果 → 工具活动 → 一次性确认 → 取消/恢复”的闭环；具体支持与 Remote Node 的差异见第 4 节。

它不是完整 CLI 功能的 RPC 镜像：模式选择仅 `plan/code`，工具正文与历史详情经过受限投影。模型运行选择、模型配置管理及 Credential 写入是不同权限面；`credential/upsert` 的本机 stdio 限定不能扩展成任意客户端的凭据上传/读取接口。

Desktop 产品由独立仓库负责，本页说明的是**可供 Desktop 使用的中立能力**，不是对其现有 UI 或路线图的重新定义。见 Desktop 仓库分离计划。

## 3. Remote Agent Node 如何接入

Remote Agent Node 是“执行任务的机器/进程”，不是 Web 或 Mobile 页面本身。应先确定调用方需要的是**远程会话控制**，还是**把独立任务委派给另一个 Agent**。

### 3.1 路径 A：远程宿主适配 app-server

适合需要从 Web/Mobile 或控制端提交任务、看流式过程、回复交互和恢复会话的应用。

```text
用户客户端
  → 接入方的认证、授权与加密传输
  → 执行端宿主/适配器
  → 本机 app-server
  → Session / Agent
```

接入步骤：

1. 在每个执行端配置模型、凭据、工作区及本机权限，确定节点与用户的绑定方式。
2. 执行端宿主按 Desktop 相同的 stdio 握手和会话时序连接 Rivo。远程客户端不能直接连接 stdin 或 Named Pipe。
3. 接入方另行实现客户端到宿主的认证、传输加密、授权、请求限流和会话归属；本页不规定或宣称已有某种远程 HTTP/WebSocket API。
4. 只映射明确允许的操作，每次订阅、恢复、提交与审批都校验用户对节点/工作区/会话的权限；不要透明转发所有 JSON-RPC 方法，尤其是配置与 Credential 方法。
5. 将 Rivo 的有界事件映射为前端消息，保留 Session、Turn、序号、截断和终态信息。
6. 分别定义“客户端断网”和“执行端宿主退出”的处理。前者是否保持宿主运行由接入方设计；后者会触发本机 bridge 生命周期，不得承诺无条件后台续跑。

**这条路径的 Agent 能力仍受 app-server 限制，不会因为部署在服务器上变多。** 当前 `general/research/review` 模式选择、Web、Browser、Shell 和 A2A peer 选择都没有开放；内部子代理已按本机配置默认开放，宿主可用 `runtime_settings.subagents_enabled`/`max_subagents` 调整。

这里可复用的是本机运行接口；远程认证、多用户隔离、客户端和服务运维不是本仓库已交付的完整产品。单个 Session 的并发限制也不能等同于整个节点已具备多租户调度能力。

### 3.2 路径 B：A2A 独立任务节点

适合由另一个 Agent 发来一项有边界的任务，再获取状态与结果的场景。Rivo 在节点上显式启动 A2A Server：

```text
受信任的 Client Agent
  → 发现 / 双端配对 / 技能授权
  → HTTPS + mTLS，JSON-RPC / SSE
  → Rivo A2A Server
  → ServiceProfile 固定本机配置
  → 独立本机 Session / Agent
  → A2A Task 状态与结果
```

该路径不经过通用 app-server 会话 RPC，也不把远端调用者变成本机交互用户。A2A 协议语义与本机 Session/Turn ID 分离，不能拿远端 Task ID 调本机 `session/resume`。

接入步骤：

1. **所有者定义服务**：配置 ServiceProfile 的私网 bind、skill、工作区 alias、任务模式、policy ceiling、允许 peer、预算及本机审批。模型由节点本机配置决定。
2. **显式启动**：使用 `rivo a2a serve <server-profile> --config <owner-a2a-server.toml>`；不默认监听公网。
3. **发现并配对**：Client 发现或指定 endpoint、固定指纹；双端核对 SAS，Server 本机所有者批准配对并授权技能。发现到节点不等于可以提交任务。
4. **提交任务**：A2A Client 按 Agent Card 声明的 interface/skill 发送有界任务；Rivo Client 可通过 `rivo run --a2a-peer <peer-id> --prompt "<task>"` 选择对端，外发仍需 policy 授权。
5. **观察和交互**：使用 Task 查询、SSE 订阅与取消；需要节点本机副作用审批时，远端只收到状态，由节点所有者在本机审批，不向网络开放批准接口。
6. **恢复与撤销**：连接中断先查询已有 Task 再订阅，不重发原委派。节点重启不能恢复的活动任务按失败处理，不重演副作用；身份或 Card 变化需要重新审阅信任。

首选验证路径为 mTLS；显式 PSK 互操作 smoke 不是失败时的自动降级，也不替代当前 mTLS 门禁。完整操作与配置见 [A2A 配对手册](../runbooks/a2a-lan-pairing.md)和 A2A Server Feature。

### 3.3 节点控制不等于任务执行

`instance-control` 的 `process-control` skill 用于受控实例/会话管理，是独立权限面：显式配对、grant、请求身份及有效期校验后，专用 LAN control 路径才可转交本机 Named Pipe。它不是把全部 app-server RPC 放到 A2A 网络。

`instance_control` profile 仅允许 strict，不能启用 Web、Browser、MCP，也不接收普通 A2A 任务作为研究/编码服务。它不等于 FEAT-024 中心监督平台；本仓库不再交付中心监督服务，FEAT-024 仅保留未来设计参考，不承诺 V2 实现。

## 4. 两种基座的能力支持矩阵

“接口支持”表示当前代码有该入口，不表示集成产品或候选验收已完成。“固定配置”表示只能由执行端所有者设置，调用者不能在任务中覆盖。Terminal 的完整矩阵另见[功能矩阵](feature-matrix.md)。

| 能力 | Desktop：本机 app-server | Remote Node A：宿主 + app-server | Remote Node B：A2A Server |
| --- | --- | --- | --- |
| 接入与交互单位 | stdio/本机 Pipe；Session/Turn | 接入方远程链路 + 本机 bridge；Session/Turn | 私网 HTTPS/mTLS；A2A Task |
| 任务模式 | 接口支持 `plan/code` | 同 Desktop 接口 | 服务任务为 `research/coding`；模式由 ServiceProfile 固定 |
| general / review 等其他模式选择 | 当前不支持 | 当前不支持 | 当前不支持远端选择 |
| 工作区与模型 | 固定本机工作区；模型可选本机有效目录项 | 同左，另需宿主用户授权 | 工作区 alias 与本机模型固定，远端不能改 |
| 文件读取 | 接口支持，仍由 Agent + policy 裁决 | 同左，在节点执行 | 固定 profile + policy 内执行 |
| 文件变更与构建/测试进程 | `code` + policy + 必要确认 | 同左，宿主必须识别合法审批人 | 显式 `coding`；需要副作用时由节点本机审批 |
| policy | 接口映射 strict / medium / full_control | 接口同左，不意味着远端默认可提权 | 只允许 strict / medium；research 固定 strict，禁止 full_control |
| Web / Browser | 当前关闭，无运行设置字段 | 同左 | 本机 profile 可配置，仍受 policy/CDP 等前提限制，远端不可开启 |
| MCP | 选择本机 allowlist 中的 server/toolset | 同左 | 本机 profile 固定，远端不安装或改连接 |
| 已确认记忆召回 | 默认启用，无 RPC 切换字段 | 同左，须评估工作区数据可见性 | 入站任务关闭，不继承节点记忆 |
| 记忆管理 | 本地 Session 绑定的 `memory/list`、`memory/confirm`、`memory/reject`、`memory/delete`；不可跨 Workspace | 同左，宿主必须继续绑定本地 Session 与用户身份 | 不向远端开放本机记忆管理 |
| Shell / 再选 A2A peer 委派 | 当前关闭 | 当前关闭 | 入站任务关闭，不继承本机选择 |
| 内部子代理 | 按配置默认开启；`runtime_settings.subagents_enabled`/`max_subagents` 覆盖；投影 `subagent`、子代理 `tool_call` 与确认 | 同左，宿主须展示任务来源 | 入站任务关闭，不继承本机选择 |
| 流式结果与工具过程 | 接口提供有界文本/工具摘要 | 宿主映射同类事件 | Task 状态与受限结果，不是完整 Session 事件镜像 |
| 取消和恢复 | interrupt / resume；有界订阅 | 同左，网络恢复另由宿主负责 | 查询/订阅/取消自己的 Task；恢复不重发委派 |
| 历史范围 | 本机持久元数据及会话恢复接口 | 宿主必须限制用户可访问范围 | 不能枚举本机会话/历史或其他 peer 的任务 |
| 操作审批 | 合法 interaction 回应，一次性授权 | 宿主绑定用户身份后回应 | 节点本机 owner 审批，不交给远端 Agent |
| Credential、任意文件传输、客户端设备控制 | 非通用接口；Credential 有专门本机边界 | 不得作为通用远程 RPC 暴露 | 不支持远端注入凭据、任意文件上传或设备控制 |
| 公网服务、多租户与移动推送 | 非本机基座职责 | 需接入方单独设计与交付 | 不属于当前受控局域网服务范围 |

尤其不能用“远程模式不支持 Web/Browser”概括所有路径：**app-server 不开放全部 CLI/TUI 能力，但提供已登记的本机受限入口；A2A 服务允许节点所有者配置，再由 policy 限制实际操作**。同样，app-server 默认记忆召回与 A2A 入站关闭记忆，是两个不同的数据边界。

当前实现依据：

- [app-server](../../runtime/crates/cli/src/app_server.rs)：`RuntimeSettings`、`turn_options`、请求分发与事件投影。
- [A2A ServiceProfile](../../runtime/crates/a2a-server/src/lib.rs)：`ServiceProfile::validate`、技能及授权边界。
- [A2A 入站 Session 驱动](../../runtime/crates/cli/src/main.rs)：`CliInboundSessionDriver::fixed_config` / `start`，固定模型与工作区、关闭记忆/子代理/再委派。
- [instance-control](../../runtime/crates/instance-control/src/lib.rs)及 [LAN control 路由](../../runtime/crates/a2a-server/src/instance_control_route.rs)：独立节点管理边界。

## 5. 接入完成应验证什么

这些是接入方应执行的检查，不是本次文档改动已经取得的测试证据。

| 验证主题 | Desktop / 宿主 app-server | A2A 节点 |
| --- | --- | --- |
| 协议与能力 | 握手版本、支持设置、未知字段拒绝、错误展示 | Card/interface、协议版本、技能声明与真实配置一致 |
| 正常任务 | 创建、提交、流式、唯一终态、关闭 | 配对后提交、状态、结果、Task owner 绑定 |
| 权限拒绝 | 错工作区、未知模型/MCP、无合法交互的回应被拒绝 | 未配对、未授予 skill、越权查 Task、远端审批被拒绝 |
| 取消与恢复 | 取消竞争、EOF、宿主退出、历史过期、有限事件恢复 | 断线查询/订阅、节点重启失败态、不重发任务 |
| 幂等与数据 | 重复提交不误执行，摘要截断与脱敏，凭据不回传 | Task 去重、revision 防回退、信任变化、撤销与脱敏 |
| 远程补充项 | Remote Node A 另验身份、工作区隔离、会话劫持防护、弱网和重复审批 | 真实独立进程 mTLS；不能只用进程内 mock 或 PSK smoke 代替 |

建议顺序：先用 `plan/read_only` 完成本机宿主的最小会话闭环；需要独立 Agent 协作时，再从 `research/strict` 的受控 A2A 节点开始。不要为打通接入而默认扩大 policy 或暴露未授权方法。

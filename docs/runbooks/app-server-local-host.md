# App-server 本机宿主运行手册

本手册面向在同一台机器上启动 `rivo app-server --stdio` 的宿主进程，例如 AgentRemoter runtime adapter。它说明本机集成、排障和恢复流程；完整 JSON-RPC schema 以[app-server 协议](../architecture/app-server-protocol.md)为准。

## 1. 边界与前提

app-server 是本机子进程 bridge：只读 stdin、只写 stdout，不监听 TCP 或 HTTP 端口。宿主负责远程认证、用户权限、Relay token、TLS 和传输可靠性；rivo 不接收这些远程配置。

启动前在本机完成以下配置：

- 配置显式 `version = 6` 的 Vendor/Model 目录；每个 Model 还必须声明非空、有序的 `supported_protocols`，路由使用第一项决定协议；Provider 只自有 `base_url` 和可选 `auth.credential` 后备引用，不携带协议。运行期环境变量仅通过 `rivo run --api-key-env <name>` 使用，不写入 Provider。显式 `config provider models refresh <provider-id>` 后，canonical Model 完整 ID 必须与认证 discovery 精确匹配；未刷新、失败、禁用或指纹不符均不可运行。
- 为宿主允许访问的项目设置 rivo `workspace`。`session/start` 和 `session/resume` 的 `working_directory` 必须规范化后与该本机 workspace 相同，不能由远程端任意指定。
- 验证本机配置，不把 Credential、环境变量值或完整 prompt 写入宿主日志：

```powershell
rivo config validate
rivo config show
rivo config route --vendor <vendor> --model <canonical-model-id> --provider-id <provider-id>
```

这些查询离线只读，结构 validate 不证明路由可用。已支持的旧 `config.toml`、`credentials.toml` 与 `instances.json` 格式按[持久化文件兼容与自动升级契约](../architecture/persistence-file-compatibility.md)自动升级；迁移后未知字段、损坏或未来版本明确拒绝且零写入。旧会话 schema 仍拒绝，不保留旧模型运行时或自动清空步骤。受控重置须按 [地址契约](../architecture/provider-endpoint-ownership.md) 保留 Credential、记忆、MCP、A2A 信任与非会话数据。FEAT-029 保持 `in_progress`；历史宿主报告只证明原候选，不可用于本次新候选。

## 2. 启动与握手

宿主以管道方式启动：

```text
rivo app-server --stdio
```

stdout 只能读取单行 UTF-8 JSON-RPC 2.0；不要把 stdout 当作人类日志流。stderr 仅用于已脱敏的本机诊断。启动后必须先请求 `initialize`，版本固定为 `rivo-app-server-v2`：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"rivo-app-server-v2","client":{"name":"local-host","version":"1"}}}
```

收到成功结果后再请求 `health`，然后执行 `session/start` 或 `session/resume`。所有状态变更请求必须带 `idempotency_key`；重试时必须复用原 key 和原参数。

## 3. 宿主执行流程

1. 用 `session/start` 创建持久 SQLite session，或用 `session/resume` 恢复明确存在的 `session_ref`。
2. 用 `turn/start` 提交一轮任务；成功结果只表示已接受并返回 `turn_id`。
3. 消费 `event` notification，按单 session 的严格递增 `seq` 去重/排序，并以 `turn_complete` 作为唯一终态。
4. 收到 `interaction_request` 时，使用 `interaction/respond` 回复当前 interaction。confirmation 只能 `allow`/`deny`，且 allow 永远是一次性授权；用户输入必须使用宿主收到的 option id，或仅在声明允许时发送 freeform text。
5. 需要停止时调用 `turn/interrupt`；自然完成与取消竞争时，以最终 `turn_complete` 的实际 outcome 为准。
6. 结束使用 `session/close`。stdin EOF 也会触发活动 Turn 取消；宿主断开后应重新启动进程并按需要 resume。

同一 session 不能并发两个活动或等待交互的 turn；第二个 `turn/start` 返回 `turn_in_progress`，不会排队。

## 4. Runtime settings 与本机能力

支持的 `runtime_settings` 为：

- `vendor/model/provider_id`：仅选择本机有效目录与 Provider；局部省略沿用会话默认，显式不可用返回 `unsupported_setting`，不换模型或回退其他 Provider。
- `auto_provider: true`：显式选择 Auto，与非空 `provider_id` 互斥；`provider_id: null` 不能代替它。Auto 按 Provider priority 降序、ID 升序选一次，不将结果写成显式默认。
- `collaboration_mode`：`plan` 映射 `plan`，`code` 映射 `coding`。
- `permission_preset`：`read_only`、`workspace_write`、`full_access` 分别映射本机 `strict`、`medium`、`full_control` policy。
- `mcp_servers`：当前 Turn 可见的本机已启用 MCP server 名称数组。
- `mcp_toolsets`：以 server 名称映射到该 server 已配置 toolset 名称集合；toolset 的 server 必须同时出现在 `mcp_servers`。

MCP 设置只能引用本机 allowlist：宿主不能传入 URL、stdio command、header、token、OAuth 结果或任意远程工具；未知/禁用 server、未选择 server 的 toolset 或未知 toolset 返回 `unsupported_capability`。`reasoning_effort` 不支持。Web、Browser、memory 等能力使用本机安全默认值，不能被远程 RPC 提权或注入 Provider 配置、base URL、API key、环境变量或任意目录。

例如，宿主可在 `turn/start` 中传入：

```json
{"runtime_settings":{"mcp_servers":["data"],"mcp_toolsets":{"data":["query"]}}}
```

这只选择管理员/用户已在本机通过 `rivo mcp install` 配置的 `data/query`，不会安装或修改 MCP 配置。

模型选择示例（仅展示 `runtime_settings` 片段，需放入完整 `turn/start` 请求）：

```json
{"runtime_settings":{"vendor":"<vendor>","model":"<canonical-model-id>","auto_provider":true}}
```

显式连接则用 `provider_id` 替代 `auto_provider`。Session 使用 `SelectModel` / `ModelSelected`；bridge 不直接转发同名选择通知，宿主从运行开始的 `status.selection/route` 获取真实路由。活动 Turn/retry 保持快照，后续 Turn 和恢复重新校验。运行请求不能写连接；获授权的 `model_routing/*` 配置 RPC 另遵循 revision/确认计划/原子提交，且不隐式 discovery。`credential/upsert` 仍仅限既有授权的 Desktop 本机 stdio 子进程，Named Pipe 拒绝，A2A/LAN 不得转发；普通 stdio 握手不产生该权限。

## 5. 排障与恢复

| 现象/错误码 | 宿主动作 |
| --- | --- |
| `unsupported_protocol_version` | 停止调用，升级或降级宿主到 `rivo-app-server-v2`。 |
| `invalid_request` / `unsupported_setting` | 修正 method、字段、大小限制、workspace 或 runtime settings；不要盲目重试。 |
| `session_not_found` / `session_resume_unavailable` | 不会自动新建；由宿主明确决定新建 session。 |
| `turn_in_progress` | 等待该 session 的 `turn_complete` 或先 interrupt。 |
| `runtime_timeout` / `runtime_unavailable` | 仅在 `retryable=true` 时使用原 idempotency key 重试；检查本机 Provider 配置。 |
| `storage_unavailable` | 检查本机数据目录与 SQLite 可用性；不清空历史来“修复”错误。 |

`health` 只返回 app-server、session storage、Provider 配置和活动 Turn 的脱敏摘要；它不会给出路径、密钥、原始 Provider 错误、prompt 或工具参数。

## 6. 数据与日志安全

宿主应把 stdin、stdout、stderr 和本地诊断都当作敏感边界。bridge 不会输出 Provider 原始帧、Credential、header、Cookie、环境变量值、完整 prompt/transcript、原始工具参数或工具输出。事件文字和展示摘要有大小上限、截断元数据，并按本机已知 key 值脱敏。

不要记录完整 JSON-RPC 请求；如需审计，仅记录脱敏 session/turn 标识、method、错误码、seq、延迟与计数。更完整的字段限制和错误语义见[协议文档](../architecture/app-server-protocol.md)。

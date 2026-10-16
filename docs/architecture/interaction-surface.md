# 交互面与 Session 契约

本文档固定 CLI、TUI 与中立 app-server host 的前端边界。公共默认值见 Dev Ready 统一契约。

## 分层边界

```text
CLI / TUI / app-server host
  -> Session（Intent 输入 / AgentEvent 输出）
  -> Agent 循环
  -> LLM / policy / tools / storage / telemetry
```

前端不得直接调用 LLM、policy、tools、storage 或 telemetry。脱敏、确认绑定、预算和停止判定必须在事件进入前端前完成。

## V1 并发模型

- 一个 session 保存多轮消息和多个历史 turn。
- 一个 session 同时最多一个活动或等待确认的 turn；第二个 `SubmitTask` 或 `AppendMessage` 返回结构化 Error，不会排队或启动并发 turn。
- CLI 在用户通过 `--prompt` 或 `--stdin` 显式提交任务时创建一个 session 并运行一次任务；`rivo run` 是规范入口；TUI 可以继续同一 session。
- 多 session 可以存在于 SQLite，但 TUI V1 只激活一个 session；并行 turn 属于 post-V1。

## 稳定标识

公共对象使用 SessionId、TurnId、StepId、ToolCallId、RequestId 和 EvidenceId。每个 AgentEvent 包含 turn 内递增 sequence 和 UTC 时间，SQLite 按 sequence 回放。

## Intent 输入

V1 Intent 覆盖：

- `SubmitTask`：任务文本和 turn 选项。
- `AppendMessage`：同一 session 的追加消息；没有活动或等待确认的 turn 时开始下一轮，其他情况下明确拒绝。
- `Confirm`：对 RequestId 允许一次或拒绝，可附拒绝原因。
- `CancelTurn`：取消当前 provider、tool 或 confirmation wait。序列化名称为 `cancel_turn`；`cancel` 明确拒绝。
- `RetryStep`：重试最近一个可重试失败 Feature。
- `SetTurnOptions`：切换基础模式、policy、Web、MCP、记忆和追踪；MCP 只能选择本机配置的 server/toolset，不能放宽已等待确认的请求。
- `SelectModel { selection }`：选择 `vendor/model/provider`，其中 Provider 为 `{"mode":"auto"}` 或 `{"mode":"explicit","provider_id":"<id>"}`，唯一 resolver 校验成功后才改变后续 turn 选择；活动 turn 与 retry 保持快照。字段与持久化边界见 Dev Ready 模型选择契约。
- `ConfirmMemory`、`RejectMemory`、`DeleteMemory`：管理持久记忆。

V1 不提供 pause、skip 或批量确认。`Confirm.scope=session` 只复用相同风险类别和边界的中等模式授权，不是“本 session 全部允许”。Intent 必须可序列化，供未来 IPC 复用。

## AgentEvent 输出

事件至少覆盖：

- Started、PromptPrepared、ModelDelta、FinalAnswer（FinalAnswer 携带最终 Usage）。
- ToolRequested、ToolObservation、ConfirmationRequired；工具执行失败会以 Failure/Error 与 Stopped 表达，V1 尚未承诺独立的 ToolStarted、ToolFailed 或 ToolCancelled 事件。
- BudgetUpdated、ContextCompressed、MemoryRecalled、MemoryInjected、MemoryCandidate。
- Error、Failure、RetryAvailable 和 Stopped。
- `ModelSelected { selection, route }`：成功选择的无秘密选择信息与必填路由投影；wire 名为 `model_selected`，不替代 turn 生命周期事件。对应 Intent 为 `select_model`，移除旧模型选择事件，不保留别名解码；app-server 通知需另经其受限事件投影，不直接转发 Session payload。

规则：

- SubmitTask 的第一个事件必须是 `Started`。
- 模型文本和工具生命周期实时推送，不在 session 内累计成 Vec 后再返回。
- 启动前配置/密钥/数据库错误走外层 Result；流开始后的错误变为事件。
- 每个 turn 只允许一个 `FinalAnswer` 或 `Stopped` 终态。
- 所有工具正文和错误进入前端前已经应用资源边界；日志事件另行脱敏。
- 明确提出查库、查数据源、核验、纠偏或补充事实的 Submit/Append 可在创建 TurnContext 前，仅追加当前配置中 enabled 的 `read_only` MCP toolset。该选择只属于该 turn，不改写 Session 的 `SetTurnOptions`，且目标歧义仍由模型请求用户输入。

## 确认语义

- ConfirmationRequired 包含 RequestId、turn/step/tool-call 标识、参数摘要、目标、命令、可能副作用、capability、policy 和已裁决的执行约束；前端只渲染这些字段，不能重新计算授权。
- Confirm 必须匹配 session、turn、step、tool call、RequestId 和参数摘要。
- 允许只对本次请求生效；参数、步骤或 turn 状态变化后确认失效。
- V1 用户选择允许一次、同风险边界的本 session 允许或拒绝，不编辑 policy 约束。
- CLI 在 stdin/stdout 均为 TTY 时逐请求采集 y/n；非交互模式默认拒绝，显式 `prompt` 也不能阻塞等待读取 stdin。自动化变更只能通过显式 `--approval-mode manifest --approval-manifest <path>`，清单按 tool、规范 JSON 参数 SHA-256、`scope: once` 和最大调用次数精确匹配；TUI 确认区采集 y/n。

## 前端复用矩阵

| 能力 | CLI | TUI | app-server host |
| --- | --- | --- | --- |
| Submit/Append | 参数或 `--stdin` | 输入区 | 结构化请求 |
| ModelDelta | text stdout 或 `--format jsonl` | 33 ms 节流渲染 | 事件流 |
| Tool/Policy | 脱敏 JSONL 投影 | 活动与追踪面板 | 脱敏事件 |
| Confirmation | TTY y/n、清单或拒绝 | y/n 确认区 | 一次性确认请求 |
| History | 命令查询 | sessions 视图 | 结构化历史 |
| Evidence/Report | Markdown | 证据侧栏/报告视图 | 受限数据引用 |

## 兼容规则

- 新增事件必须提供未知事件的向前兼容处理；前端不得因非关键新事件崩溃。
- 删除或改变现有 Intent/Event 字段属于契约破坏，必须同步 fixture、存储版本和所有前端。FEAT-029 的模型旧事件明确拒绝、旧会话受控重置，不要求永久 migration 或兼容解码；其他事件规则不变。
- 任何宿主序列化都不能要求回改 V1 的核心语义。
- `--format jsonl` 每行均有 `schema_version`、session/turn、sequence、UTC 时间和事件；工具参数与工具正文不会进入该公共投影，且每个 turn 只有一个终态事件。

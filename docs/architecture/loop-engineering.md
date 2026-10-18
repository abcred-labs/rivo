# Agent 循环工程契约

V1 使用顺序 ReAct 循环。跨层规则见[工具运行时](tool-runtime.md)与[交互面](interaction-surface.md)。

## 职责

agent 层负责：

- 维护当前 turn 上下文、步骤、usage、时间和重试预算。
- 向 LLM 发送当前消息和已启用工具定义。
- 将工具调用依次路由到 schema 校验、policy 和 tools。
- 等待并验证 confirmation，回灌结构化观察。
- 压缩上下文、生成事件并判定唯一终态。

agent 不实现协议 HTTP、具体工具、policy 规则、SQLite 或前端渲染。

## 状态机

```text
Created
  -> ModelPending
  -> ModelStreaming
     -> FinalAnswer -> Completed
     -> ToolCalls -> ToolValidation
        -> Invalid -> Observation -> ModelPending
        -> PolicyDenied -> Observation/Stopped
        -> ConfirmationPending -> Allowed/Denied/Cancelled
        -> ToolRunning -> Observation -> ModelPending
  -> Compressing -> ModelPending
  -> Cancelled / BudgetExceeded / Failed
```

- 同一响应的多个工具调用按顺序处理。
- 任何 tool execute 前必须持有本次 policy 返回的 ExecutionConstraints。
- 参数错误和可修复工具失败作为 ToolResult 回灌；拒绝、超限和不可恢复失败不盲目重试。

## 默认预算

| 预算 | 默认值 |
| --- | ---: |
| 最大步骤 | 20 |
| turn 时间 | 30 分钟 |
| 上下文基线 | 128K tokens |
| 压缩阈值 | 80% |
| 硬停止阈值 | 95% |
| 单观察入上下文 | 256 KiB |
| 每步骤语义重试 | 1 |
| 每 turn 语义重试 | 3 |

配置只能在 policy 和资源允许范围内调整。Provider usage 可用时使用真实值；缺失时使用确定性估算并在事件中标记 estimated。

## 压缩

- 保留系统指令、初始任务、最近四轮、全部有效 EvidenceId/来源、待确认请求、当前失败和预算。
- 被替换的旧事件范围记录在 ContextCompressed 中。
- 首选固定 schema 的无工具模型总结；失败时使用确定性裁剪。
- 压缩不能使过期确认重新有效，也不能把 truncated 观察描述为完整结果。

## 失败与重试归属

- llm 层：429、5xx、网络、幂等请求 timeout 的最多三次传输尝试。
- agent 层：上下文长度压缩后重试、模型工具参数修正、可恢复工具失败。
- tools 层：单次执行 timeout、输出/文件/网络硬上限和取消。
- policy deny、confirmation deny、资源超限和内容策略不自动重试。

## 取消

Session 为活动 turn 创建 cancellation token，并传给 provider stream、tool execute 和 confirmation wait。取消优先于普通完成；收到取消后不得启动新的工具或模型调用。每个被中断工具产生 ToolCancelled，turn 最终产生 UserCancelled。

## 停止顺序

1. 用户取消。
2. policy/confirmation 安全终止。
3. 时间、token、上下文、观察或步骤预算。
4. 不可恢复 provider/tool 错误。
5. 模型最终答案。

每种停止都映射为 core 的结构化 StopReason，并进入 Event、SQLite 和脱敏日志。

## Post-V1

多工具并行、多 turn 并发、pause/skip、长期后台任务和向量化上下文压缩不属于 FEAT-001–015。

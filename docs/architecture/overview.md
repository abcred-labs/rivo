# Rivo 架构

Rivo V1 使用分层 Rust 运行时并保持清晰模块边界。FEAT-001–015 的固定接口和默认限制见 Dev Ready 统一契约，代码状态见 设计与实现状态。

## 定位对架构的影响

Rivo V1 定位为通用 Agent，通过 general、research、plan、coding、review 五种基础模式和按 turn 启用的 Web、MCP、记忆能力组合行为。research 强制证据追溯，plan 形成可执行方案，coding 提供受控工作区操作，review 保持只读审查。模式提示词与工具可见性详见 Agent 提示词架构。

这一定位带来以下架构约束：

- CLI 层应启动边界清晰的 Agent 任务，而不是长期无人值守后台任务。
- 智能体循环层应围绕证据、工具观察结果、停止条件和最终报告组织上下文。
- Agent 循环将稳定核心提示词、模式覆盖和 turn 能力摘要置于历史消息前；提示词不替代工具注册、policy 或确认。
- LLM 提供方层应支持多种协议适配，但向运行时暴露稳定统一的决策结果。
- 工具运行时层按基础模式和每 turn 附加能力暴露只读、编码、Web 与 MCP 工具；浏览器自动化属于 post-V1。
- 策略层应默认保护工作区、网络、Shell、写入、浏览器交互、MCP server 和凭据边界。
- 遥测层应让模型决策、工具调用和策略结果可审阅，便于技术、业务和合规用户复核。

## 分层

1. 前端层（CLI / TUI）
   - 解析用户输入和配置。
   - 启动单次智能体运行。
   - 将命令选项转换为运行时配置。

2. Session 层
   - 接收结构化 Intent 并输出 AgentEvent stream。
   - 管理单 session、单活动 turn、多轮、确认、取消和重试。
   - 隔离前端与内核实现。

3. Agent 循环层
   - 维护对话消息。
   - 向 LLM 提供方发送工具定义。
   - 将请求的工具调用路由到策略和工具运行时边界。
   - 在最终答案、不可恢复错误、达到最大步骤数或策略拒绝时停止。

4. LLM 提供方层
   - 在稳定的提供方契约后抽象模型提供方。
   - 支持用于设计验证的确定性本地模拟。

5. 工具运行时层
   - 使用 JSON 模式注册候选工具。
   - 按基础模式与 turn 能力开关暴露文件系统、编码、网络和 MCP 工具。
   - 验证工具调用参数。
   - 执行已批准的工具调用并返回结构化观察结果。

6. 策略层
   - 阻止或门控高风险工具调用。
   - 执行工作区、网络、Shell、写入、MCP 外部调用和凭据访问限制。
   - 执行 [安全策略](./safety-policy.md) 定义的严格、当前 workspace 和 Full Control 三档模型。
   - 生成绑定请求摘要的一次性确认请求。

7. Storage 层
   - 使用 SQLite 保存会话、事件、消息和经确认的记忆。
   - 提供 migration、回放、删除和 FTS 召回。

8. 遥测层
   - 记录模型决策、运行时编排和工具执行的追踪事件。
   - 在不暴露密钥的前提下支持调试和可行性分析。

## 交互面演进

V1 已实现 CLI、TUI 和前端无关会话契约，把“意图输入”和“事件输出”标准化。`rivo app-server` 是中立本机 host 边界，其稳定的 Intent/Event、安全、幂等、脱敏、工作区和一次性确认语义不携带任何特定 UI 或私有传输要求。

```text
CLI / TUI / 中立本机 host
   -> 会话内核契约（意图输入 / 事件输出）
   -> 智能体循环层
   -> LLM 提供方层 / 策略层 / 工具运行时层 / Storage / 遥测层
```

TUI 的跨平台 smoke 属于 V1 发布验收。Desktop 由独立仓库维护，不构成本仓库的交互面规划或验收；分离边界见 Desktop 仓库分离计划。

V1 完成门包含 A2A Client 与受控局域网 A2A Server（仅 Windows 运行时）；Client 以受 policy 保护的委派工具调用受信任对端，Server 将已认证的远端任务投影为独立本机 Session。该能力不复用 `app-server` 的本机 stdio 协议，完整边界见 [A2A 双角色互操作](./a2a-interoperability.md)。Linux/macOS 原生运行时适配属于 V2，但 A2A 协议跨平台互操作可按[跨平台研究集群组建手册](../runbooks/a2a-cross-platform-cluster.md)单独验收。

## 运行时形态

实现布局位于 [runtime/](../../runtime/README.md) 下的 Rust workspace,按 step 逐步扩展。计划分层如下,每个 crate 在对应 step 落地:

```text
runtime/
  crates/
    cli/
    core/
    llm/
    agent/
    prompts-management/
    tools/
    policy/
    storage/
    telemetry/
    session/
    tui/
    test-support/
    xtask/
```

所有列出的 crate 均已存在并有实现；`prompts-management` 负责提示词资产与持久指令边界，`test-support` 提供隔离测试环境。交付状态以 release-state.json 为准，功能路线以 directory-layout.md 与 steps 为准。

## V1 运行时流程

```text
用户提示
   -> CLI / TUI
   -> Session 层
   -> Agent 循环层
   -> LLM 提供方决策
   -> 可选工具调用
   -> 策略检查
   -> 工具运行时观察结果
   -> 继续直到最终答案或停止条件
```

V1 单 session 只运行一个活动 turn，模型返回的多个工具调用按顺序执行。多 turn 并发和多工具并行属于 post-V1。

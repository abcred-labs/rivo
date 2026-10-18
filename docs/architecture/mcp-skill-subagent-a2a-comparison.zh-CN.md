# MCP、Skill、SubAgent 与 A2A 对比

本文用于解释 Rivo 中四个容易混淆的概念。用户提到的 “SugAgent” 按仓库正式术语解释为 `SubAgent`（内部子代理）。

## 1. 先记住四句话

| 概念 | 一句话定义 | 主要解决的问题 |
| --- | --- | --- |
| **Skill** | Agent 能完成的一项有边界的能力或职责描述。 | Agent 应该提供什么能力，以及这项能力允许做什么。 |
| **MCP** | Agent 与外部工具、资源和提示模板之间的标准协议。 | Agent 如何安全、结构化地使用外部系统。 |
| **SubAgent** | 同一 Rivo run 中由根 Agent 创建的一层受控子任务。 | 如何把一个复杂任务拆开并行或分工。 |
| **A2A** | 独立 Agent 之间的发现、信任、任务协作和结果交换协议。 | 不同 Agent 如何跨边界协作。 |

最简关系是：**Skill 描述能力，MCP 连接工具，SubAgent 拆分内部任务，A2A 连接独立 Agent。**

## 2. 核心差异

| 对比维度 | Skill | MCP | SubAgent | A2A |
| --- | --- | --- | --- | --- |
| 连接对象 | Agent 与一项能力 | Agent 与工具、资源或 prompt | 根 Agent 与子任务 | Agent 与独立 Agent |
| 所在层级 | 能力/职责层 | 外部能力接入层 | Agent 编排层 | Agent 互操作边界层 |
| 是否通信协议 | 否，通常是声明和授权单元 | 是 | 不是网络协议，是运行时调度机制 | 是 |
| 执行位置 | 声明该能力的本机或服务端 | MCP server 或其后端系统 | 同一 Rivo 进程/运行时边界内 | 远端 Agent 所在的进程或机器 |
| 典型传输 | Agent Card、ServiceProfile、配置元数据 | stdio、Streamable HTTP | 内部任务消息和事件 | HTTPS/mTLS、JSON-RPC、SSE 等 |
| 生命周期 | 随配置、profile 或 Agent Card 生效 | 连接、发现、调用、断开 | 排队、运行、完成/失败/取消 | 发现、配对、提交、查询、流式更新、恢复/取消 |
| 权限边界 | 限定可暴露的能力和 policy ceiling | schema、allowlist、toolset 和 policy | 继承父 turn 的 mode、policy、模型和能力 | peer 信任、skill allowlist、本机 policy 和本机审批 |
| 结果形态 | 能力声明或能力产出契约 | 工具结果、resource evidence | 有界摘要、Evidence 和用量 | Task 状态、Message、Artifact 和事件 |
| 典型范围 | `research`、`coding` 等固定职责 | 查库、调用 Jira、读取文件等 | 并行研究、审阅、拆分编码任务 | 委派给局域网专业 Agent |

### Skill 不是另一种传输协议

Skill 更接近“可被调用的能力目录项”或“服务契约”。例如，A2A Server 可以声明一个 `research` skill，说明它接受什么输入、返回什么结果以及使用什么限制；真正把请求送到该 Server 的，是 A2A 协议。一个 Skill 也可以在本机由 Agent 直接执行，未必需要网络通信。

## 3. 四种典型流程

### 3.1 Agent 使用 MCP 工具

```text
用户问题
  -> 当前 Agent 选择已启用的 MCP toolset
  -> MCP client 调用外部 MCP server
  -> 工具返回结构化结果/Evidence
  -> 当前 Agent 继续推理并回答
```

适合“需要外部数据或动作，但不需要另一个 Agent 独立负责任务”的情况，例如查询项目管理系统或读取数据库。

### 3.2 根 Agent 使用 SubAgent

```text
根 Agent
  -> delegate_task("分别检查三个模块")
  -> 同一 turn 内的 SubAgent 执行子任务
  -> 回传有界摘要、Evidence、用量和终态
  -> 根 Agent 汇总结果
```

SubAgent 继承父 turn 的 mode、policy、模型、workspace 和已启用能力。它不是独立网络服务，也不用于建立跨机器信任关系。

### 3.3 Client Agent 使用 A2A

```text
本机 Agent
  -> 发现候选 peer
  -> 用户完成配对并明确选择 peer
  -> A2A delegate 提交受限任务
  -> 远端 Agent 返回状态、结果和 Artifact
```

适合需要独立运行环境、不同专业职责或另一台机器资源的任务。发送给远端的文本和 Artifact 属于数据外流，必须经过本机 policy 和确认。

### 3.4 A2A Server 的内部执行

```text
A2A 请求
  -> Server 校验 peer、skill、配额和幂等性
  -> 创建本机 Session/Turn
  -> 本机 Agent 使用 Skill、MCP 和 tools
  -> 本机 policy/所有者审批副作用
  -> A2A 返回脱敏状态与结果
```

A2A 只负责边界适配；它不会替换本机 Agent loop，也不会把远端调用者变成本机权限拥有者。

## 4. 如何选择

| 需求 | 首选 | 判断理由 |
| --- | --- | --- |
| 读取一个外部系统或调用一个外部动作 | MCP | 目标是工具或资源，不是另一个 Agent。 |
| 为 Agent 定义可公开、可授权的职责 | Skill | 目标是描述能力、输入输出和限制。 |
| 把当前复杂问题拆成多个独立小问题 | SubAgent | 任务属于同一 turn，适合复用本地上下文和策略。 |
| 把任务交给另一台机器或独立服务 | A2A | 需要跨进程/跨机器的 Agent 协作和任务状态。 |
| 需要持续查询、取消或断线恢复的远端任务 | A2A | A2A 有 Task 生命周期和状态交换语义。 |
| 只想给模型增加一段说明文字 | Skill 或 prompt | 不需要 MCP 或 A2A 通信。 |

可以组合使用：例如，A2A Server 暴露 `research` Skill，Server 内部再通过 MCP 查询知识库；Client 也可以先由 SubAgent 整理输入，再通过 A2A 委派。

## 5. Rivo 中的安全边界

- MCP server 必须经过配置 allowlist、启用状态、当前 turn 选择和 toolset 选择，并继续经过 schema 与 policy。
- SubAgent 不能递归创建子代理，不能获得父 turn 之外的凭据或授权；根 Turn 取消或预算耗尽时，子任务会被级联处理。
- A2A 的发现不等于信任。当前 Rivo 主要面向受控局域网，要求显式配对、固定身份和可撤销的 peer 信任。
- A2A Server 的本机 workspace、凭据、历史、MCP endpoint 和高风险副作用不会因为远端请求而自动开放；需要确认时由 Server 所在机器的所有者裁决。
- 远端 Agent 返回的内容是不可信外部输入，不能改变本机 system prompt、policy、确认语义或工具边界。

## 6. 常见误区

| 误区 | 正确理解 |
| --- | --- |
| “Skill 就是 MCP” | Skill 是能力描述；MCP 是连接外部能力的协议。Skill 可以通过 MCP 暴露，也可以本机执行。 |
| “MCP 可以让两个 Agent 协作” | MCP 让 Agent 调用工具/资源；跨 Agent 委派、状态和结果交换属于 A2A。 |
| “SubAgent 就是远端 Agent” | SubAgent 是同一 Rivo run 内部的受控子任务；远端 Agent 协作应使用 A2A。 |
| “A2A 会授予远端完整控制权” | A2A 只传递任务；本机 policy、预算、确认和审批仍是最终控制点。 |
| “发现到一个 Agent 就可以直接调用” | A2A 发现只产生候选，必须完成配对、信任和当前 turn 的显式 peer 选择。 |

## 7. 相关 Rivo 文档

- [工具运行时与 MCP](tool-runtime.zh-CN.md)
- [A2A 双角色互操作](a2a-interoperability.zh-CN.md)
- FEAT-023 内部子代理
- A2A 业务场景适用性基线

---

[English](mcp-skill-subagent-a2a-comparison.md)

# Agent 业务与能力 Baseline

状态：拟议实现中。本文把 Rivo 的任务模式、提示词、工具与 MCP 管理收口为一套可验证的 V1 基线；安全强制边界仍以 [安全策略](safety-policy.md) 为准。

## 市场结论

| 产品/框架类别 | 任务组织 | 提示词与知识 | 工具与 MCP | 对 Rivo 的结论 |
| --- | --- | --- | --- | --- |
| Codex、Claude Code、Cursor、Copilot | 通用 loop 配合 plan、coding、review 等任务行为 | 常驻工程指令、路径/作用域规则、按需 skill 分离 | 工具可见性、确认与 hook 独立于 prompt；MCP 有全局/项目/本次会话治理 | V1 应采用固定模式加分层指令，而非给用户暴露可任意覆盖的 system prompt。 |
| Dify、CrewAI | 用 role/goal/task 或 Agent node 配置不同工作 | 指令、上下游变量、记忆和迭代预算声明化 | 工具有描述、参数与凭据，执行有迭代上限 | ModeSpec 必须同时规定目标、输出契约、工具范围和预算，而不仅是一个 prompt 文本。 |
| LangChain/LangGraph | single agent、skills、router、handoff、subagent 可组合 | skill 只在相关时加载，隔离上下文优于堆叠长提示 | 多 Agent 用于上下文隔离、并行或独立维护 | V1 保持单 Agent；subagent/router/workflow 等待确有并行与隔离需求后再引入。 |
| MCP 官方规范 | host 为每个 server 维护独立 client 与生命周期 | resources 是数据、prompts 是模板、tools 才是动作 | initialize/能力协商、transport、auth、tools/resources/prompts 分离 | 不得把 resource/prompt 伪装为普通 action tool；server 发现不等于 turn 授权。 |

调研来源：

- [OpenAI Codex customization](https://developers.openai.com/codex/concepts/customization)
- [Claude Code extension model](https://code.claude.com/docs/en/features-overview)
- [Cursor Rules](https://cursor.com/docs/rules)
- [GitHub Copilot MCP](https://docs.github.com/en/copilot/customizing-copilot/extending-copilot-chat-with-mcp)
- [Dify Agent node](https://docs.dify.ai/en/cloud/use-dify/nodes/agent)
- [LangChain multi-agent](https://docs.langchain.com/oss/python/langchain/multi-agent)
- [MCP architecture](https://modelcontextprotocol.io/docs/learn/architecture) 与 [MCP security](https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices)

## V1 需求

- 提供固定的 `general`、`research`、`plan`、`coding`、`review` 五种模式；Web、MCP 和记忆是每次 turn 的附加能力，不能作为独占模式。
- 提示词必须可版本化、可测试、可审计，但完整 system prompt、指令正文和密钥不得进入日志、SQLite 或诊断包。
- 用户可在 Rivo 主配置目录提供 `AGENTS.md`；项目从 workspace root 到 turn cwd 按层加载 `AGENTS.md`。项目中更具体的指令优先于项目根和用户层。
- 持久指令只能提出建议，不能覆盖内置核心、模式契约、工具可见性、policy、确认或当前任务的明确要求；V1 不支持 prompt imports、任意模板执行和用户自定义 Agent。
- 工具暴露由 ModeSpec、turn capability、Toolset 和 MCP server selection 的交集决定；policy 是唯一的强制授权点。
- MCP 必须同时通过配置 allowlist、server enabled、turn server selection 和 toolset selection；任何一层缺失都不暴露远程 action。唯一例外是用户明确要求查库、查数据源、核验、纠偏或补充事实时，Session 可为该 turn 补全 configured `read_only` toolset；该补全不创建配置、不选择 action/unknown toolset，且仍受其余层与 policy 限制。

## 架构

```text
内置核心 + ModeSpec + turn capability
       + 用户/项目 AGENTS.md + 已披露记忆/证据
                         -> PromptComposer -> PromptEnvelope

ToolCatalog -> ToolSelector(mode/capability/toolset) -> ToolRegistry -> policy -> runtime

MCP server -> initialize/discover -> tools | resources | prompts
                                  -> action tools | EvidenceRef | 显式任务模板
```

`rivo-prompts-management` 是独立 crate，负责提示词资产、`AGENTS.md` 发现、32 KiB 指令预算和 `PromptEnvelope`。它不依赖或替代 policy；运行时只将其 system 文本传给 provider。

### 模式矩阵

| 模式 | 内置 Toolset | MCP 规则 | 交付物 |
| --- | --- | --- | --- |
| general | `fs.read` | 显式选择；明确核验意图可补全 `read_only` Toolset；副作用仍需 policy 确认 | 结论、依据、下一步 |
| research | `fs.read`、可选 `web.read` | 仅 `read_only` Toolset；明确核验意图可补全 | 结论、证据、限制、未决问题 |
| plan | `fs.read`、显式开启的 `web.read`、声明为 `read_only` 的 MCP resource/toolset | permission ceiling 不能扩大工具目录；禁止编码、进程、Shell、Browser、Computer、A2A、跨会话消息、媒体和子代理 | 可执行步骤、依赖、验收、EvidenceRef、风险 |
| coding | `fs.read`、`fs.write`、`process.verify` | 显式选择；明确核验意图只补全 `read_only` Toolset | 修改、验证、未验证项、风险 |
| review | `fs.read`、可选 `web.read` | 仅 `read_only` Toolset；明确核验意图可补全 | 按严重性定位的发现与验证缺口 |

旧 `mcp` profile 是兼容输入，归一化为 `general`；新的 MCP 授权不再依赖它。

### Prompt 组合与预算

每次 provider 调用按固定顺序组合：受保护核心 → 模式契约 → 当前 turn 的 policy、能力和 Toolset 摘要 → 用户 `AGENTS.md` → 项目 `AGENTS.md` → 记忆/证据 → 用户任务与会话历史。达到 32 KiB 时优先保留更具体的项目指令，并在 `PromptEnvelope` 标记截断和来源 hash。

### MCP 与 Toolset

- `McpServerConfig.toolsets` 使用名称到精确 remote tool 名称集合的映射，并标注 `read_only`、`action` 或 `unknown` 风险；未知工具默认不属于任何 Toolset。
- 已配置 Toolset 的 server，未在 turn 中选择 Toolset 时不向模型暴露 remote tool；未配置 Toolset 的旧 server 暂时保留全工具兼容路径，并在配置输出中标记为 `legacy-all-tools`。
- MCP `tools` 进入 `mcp__<server>__<tool>` 命名空间；`resources` 通过每 server 一个受 schema 限制的 read tool 转成 EvidenceRef；`prompts` 只能由用户显式发现/选择，不进入 system prompt，也不能由模型自行调用。
- `rivo mcp install http|stdio` 在写入前执行连接、initialize 和 discovery；`list` 显示配置状态，`show <server>` 显示 capabilities/toolsets，`inspect <server>` 显示已发现的 tools/resources/prompts，`enable`、`disable`、`remove` 管理 allowlist。它们不显示 header、环境变量正文或凭据。
- initialize `instructions` 以不可信、512 字节上限的工作流参考附到工具描述；普通 MCP tool call 和 resource read 都提供可追溯 EvidenceRef，tool evidence 可包含 server 返回的 audit ID。

## 验收

- 五模式 golden test 覆盖核心、模式文本、能力摘要、只读限制和最终输出契约。
- `AGENTS.md` 覆盖用户/项目优先级、workspace 边界、32 KiB 截断和不可覆盖的安全核心。
- Toolset 覆盖未知 Toolset、未选择 Toolset、只读模式拒绝 action Toolset、重复工具名和模型可见工具上限。
- MCP fixture 覆盖 stdio/Streamable HTTP、tools/resources/prompts 分流、超时、结果上限、server 退出和无密钥审计。
- 兼容性覆盖旧 `mcp` profile、旧 server-only turn selection 和未配置 Toolset 的已有配置。

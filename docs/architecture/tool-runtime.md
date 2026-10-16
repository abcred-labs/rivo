# 工具运行时契约

tools 层实现具体执行和结构化观察，不决定 Agent 流程。公共上限、写入和外部能力规则见 Dev Ready 统一契约。

## 工具注册

每个工具必须声明：

- 稳定 name、描述、基础模式和附加能力要求。
- JSON 参数 schema 与结构化结果 schema。
- Operation、目标、风险、副作用和默认资源预算。
- 是否允许自动执行、逐次确认或在 V1 禁止。

agent 先通过 `ToolCatalog` 收集描述，再按基础模式、turn capability、MCP Toolset 和 policy 形成当前 turn 的 `ToolRegistry`。模型最多可见 64 个工具；超过上限必须缩小 Toolset，不能静默裁剪。Agent 的 policy 预检只生成确认展示和绑定的 RequestId；ToolRuntime 对同一规范化 ToolRequest 重新裁决并使用它返回的 ExecutionConstraints 执行，因此执行器才是强制授权点。两层共用 canonical request 构造，不存在分别重建摘要、参数或路径范围的入口。

## 统一观察状态

观察状态固定为 completed、partial、failed、denied、confirmation_required、timeout、cancelled。观察包含 tool call ID、耗时、大小、truncated、错误分类、EvidenceRef 和适合模型回灌的结构化结果。

## 本地只读工具

- `read_file`：有界读取 UTF-8 文本，返回路径、行范围、编码和 evidence。
- `list_dir`：最多返回 1,000 项和 truncated。
- `search`：受匹配、文件、字节、深度和 30 秒总预算限制。
- 不跟随造成 workspace 逃逸的符号链接；Full Control 仍使用统一 PathScope resolver。

硬限制为文件 1 MiB、目录/匹配 1,000、搜索文件 10,000、总读取 64 MiB、深度 64。调用参数只能缩小限制。

## 编码与进程工具

- `apply_patch`：expected hash、最小文本替换、原子写入和 diff。
- `write_file`：文本创建/覆盖，1 MiB，expected hash/absent。
- `write_binary`：Base64 或字节引用写入，10 MiB，每次确认。
- `move_path`：同文件系统移动，目标存在拒绝，每次确认。
- `delete_path`：移动到 `.rivo/trash/<turn-id>`，生成恢复 manifest，每次确认。
- `bulk_file_ops`：最多 100 项，全量预检、顺序执行、失败反向回滚，每次确认。
- `run_process`：结构化 program/args/cwd、allowlist、timeout、取消、stdout/stderr 独立硬上限和脱敏。

V1 不提供原始 Shell、跨文件系统 move、永久删除和生产部署工具。

## Git 与原子写入

Git 状态必须区分 tracked-clean、tracked-dirty、untracked、ignored、not-repository 和 unavailable，并在 patch 结果中披露是否有预存改动。所有文本 patch 都以同一 `TextPatch` policy identity 请求确认；Git unavailable 明确失败。

写入流程：读取与 hash -> 同目录临时文件 -> 写入/flush/sync -> 权限复制 -> 二次 hash -> 原子替换。任何失败保持原文件完整。

## Web 工具

- `web_fetch`：HTTP(S) 地址检查、20 秒、5 redirect、2 MiB、HTML/text/JSON、可读正文和 URL evidence。
- `web_search`：Brave Search API，query 500 字符、最多 10 结果，key 来自 `BRAVE_SEARCH_API_KEY`。
- 拒绝 userinfo、loopback、私网、link-local 和 redirect/DNS 逃逸。
- Search snippet 不是页面事实证据；关键结论应 fetch 页面。

## MCP 工具

通用外部能力接入模型、以 ADO / Jira / Figma 官方接入为标尺的完备性矩阵与缺口优先级，见 [通用外部能力接入与完备性审计](external-capability-integration.md)。

- 支持 stdio 和 Streamable HTTP。`rivo mcp install` 在写入 allowlist 前必须连接、initialize、发现能力，并验证声明的 toolset 只引用已发现的精确远程工具名；HTTP header 只保存环境变量引用。
- server 必须在本机 allowlist、enabled 且被当前 turn 选择后才可见。已声明 Toolset 的 server 必须选择至少一个 Toolset 才暴露远程 action；用户明确要求查库、查数据源、核验、纠偏或补充事实时，Session 只为该 turn 自动选择已配置的 `read_only` Toolset。该补全不持久化、不会选择 `action`/`unknown` Toolset，也不会解决目标歧义。
- 连接 10 秒、调用 60 秒、结果 1 MiB、每 server 最多 128 项能力。
- tool 使用 `mcp__<server>__<tool>` 命名空间进入注册表/policy；每次调用返回原始结果供当前模型回合使用，并附带 server、tool、结果大小和可选 audit ID 的 EvidenceRef。resource 经每 server 的受 schema 限制读取入口形成外部 evidence；prompt 作为用户显式发现的模板，不进入模型可调用工具或 system prompt。
- 初始化响应中的 `instructions` 以最多 512 字节的“不可信工作流参考”附到 MCP 工具描述。它可帮助模型先发现项目和数据源，但不能覆盖 system prompt、用户意图、schema、policy 或确认。
- `read_only` 模式只允许 MCP 声明为 `read_only` 的 Toolset；MCP annotations 只能辅助描述，主机配置和 policy 才是最终风险判断。
- server 名称、transport、URI、Evidence 与外部信任边界进入事件与审计，credential 不进入。公共 JSONL 与 app-server 只投影脱敏、限长摘要；本机 SQLite 中的有界 tool/model 内容仍按敏感数据处理。

## 取消与资源回收

所有可能阻塞的工具接受 cancellation token。取消、timeout 或输出超限后必须回收直接子进程、关闭网络响应和 MCP 调用，并返回结构化终态。

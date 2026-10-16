# 运行时工作区

此目录是 Rivo 的 Rust workspace，当前包含：

- `core`：消息、内容片段、工具定义和研究证据契约。
- `llm`：统一 LLM 契约、MockLlm 和三种标准协议适配器。
- `policy`：安全档位、能力 profile、确认绑定和执行约束。
- `tools`：受策略保护的文件、搜索、patch、进程、Web 和 MCP 边界工具。
- `agent`：顺序工具注册表、policy 门控和多步循环。
- `session`：配置解析、Intent/AgentEvent 流和会话编排。
- `storage`：bundled SQLite 历史与记忆。
- `telemetry`：脱敏 JSON 日志和诊断写入。
- `tui`：ratatui/crossterm 状态 reducer 与终端渲染。
- `xtask`：统一质量检查入口。

Session 当前提供双向 Intent/Event channel、多轮上下文、确认恢复、取消、RetryStep、SQLite 事件持久化、`sessions resume` 和预算事件；配置支持预算、日志和 MCP allowlist，MCP tools/resources/prompts 通过每个 Turn 选择进入 Agent。TUI 提供工具/trace/evidence/history 侧栏、滚动、命令面板和记忆候选确认；原生历史 smoke、完整 MCP/Web 矩阵和外部 live/native 验收仍按 Step 继续收口。依赖方向与默认限制见 统一契约。浏览器自动化属于 post-V1；Desktop 由独立仓库负责。

## FEAT-029 模型路由收口

2026-09-09 状态为 `in_progress`，核心目标契约见 FEAT-029与 Dev Ready 契约。此说明不表示 Rust 实现或候选门禁已完成。

- 纯配置 schema 4：Vendor、canonical Model、Provider 与独立 Credential；Provider 自有 `base_url` 和可选 Credential 后备引用，运行期环境变量仅由 `rivo run --api-key-env` 提供，无模型 Profile/Binding/独立 Endpoint 或兼容迁移层。
- 唯一有效目录与 resolver 供 CLI/TUI/Session/app-server/查询复用，认证 discovery 完整 ID 精确匹配；Auto 按 Provider priority 降序、ID 升序，显式不可用拒绝。不 alias、不 runtime failover。
- 公共路由只保留 `vendor/model/provider_id/protocol/base_url/priority`；新选择使用 `SelectModel { selection }` / `ModelSelected { selection, route }`，`ModelSelection.provider` 为 Auto/Explicit 枚举，新会话无旧模型 profile 列与事件解码。Turn/retry 快照固定，恢复与后续 Turn 重校验。
- 旧配置/会话拒绝，不自动清空；受控重置必须单独列明路径、表与影响，保护 Credential、记忆、MCP、policy 和 A2A 信任。基础模式 profiles、审批绑定、非模型 endpoint 不变。
- 本轮阶段 A 仅改文档；历史验收不可用于新候选，完整实现后重跑下列质量门与 Windows preview 外部证据。

## 验证

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
```

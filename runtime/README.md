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

Session 提供双向 Intent/Event channel、多轮上下文、确认恢复、取消、RetryStep、SQLite 事件持久化、`sessions resume` 和预算事件。工具请求通过 policy 后执行；Web、MCP、浏览器及 A2A 有各自的启用和信任边界。Windows 归档包含浏览器扩展与 native host，Linux/macOS 提供通用 CLI/TUI；Desktop 由独立仓库负责。详见[功能矩阵](../docs/guides/feature-matrix.md)与[安全策略](../docs/architecture/safety-policy.md)。

## 当前配置与模型路由

配置格式为 schema 6，协议由 Model 的有序 `supported_protocols` 决定，Provider 自有 `base_url`、认证引用与 discovery 快照。详见[地址归属与协议契约](../docs/architecture/provider-endpoint-ownership.md)。

- Vendor、canonical Model、Provider 与独立 Credential 分离；运行期凭据环境变量通过 `rivo run --api-key-env` 引用，不把秘密正文写入配置。
- 唯一有效目录与 resolver 供 CLI/TUI/Session/app-server/查询复用，认证 discovery 完整 ID 精确匹配；Auto 按 Provider priority 降序、ID 升序，显式不可用拒绝。不 alias、不 runtime failover。
- 公共路由只保留 `vendor/model/provider_id/protocol/base_url/priority`；新选择使用 `SelectModel { selection }` / `ModelSelected { selection, route }`，`ModelSelection.provider` 为 Auto/Explicit 枚举，新会话无旧模型 profile 列与事件解码。Turn/retry 快照固定，恢复与后续 Turn 重校验。
- 支持的历史配置格式按[兼容与自动升级契约](../docs/architecture/persistence-file-compatibility.md)迁移；不能安全迁移的输入零写入拒绝。旧会话按独立存储规则处理，不自动清空用户数据。
- 下列检查覆盖本地代码质量；跨平台发布必须另外通过[发布验收](../docs/runbooks/releasing.md)。

## 验证

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
```

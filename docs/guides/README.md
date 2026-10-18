# Rivo 使用指南与功能帮助

- 适用范围：当前公开 CLI/TUI；五个发布目标和系统专属能力见[安装指南](one-click-install.md)。
- 本目录面向 Rivo 使用者，说明功能操作、验证结果和常见问题。架构决策、实现计划与验收证据仍以其各自文档为准。

## 开始使用

- [架构、能力与使用场景](architecture-and-capabilities.md)：新用户先读，理解 Rivo 能做什么、如何工作，以及如何选择模式与能力。
- [下载安装](one-click-install.md)：下载 Linux、macOS 或 Windows 归档，校验后配置 PATH。
- [功能矩阵：本地与远程接入](feature-matrix.md)：比较 CLI、TUI、Desktop、Web、Mobile 的当前支持、接口限制与候选目标。
- [Desktop App 与 Remote Agent Node 基座接入](runtime-integration.md)：面向集成者，分别说明本机嵌入、远程宿主与 A2A 节点的接入机制、操作时序和能力支持。
- [Agent 集群组建与工作方式对比](agent-cluster-workflows.md)：如何做受控跨节点分工、何时值得使用、如何汇总结果，以及与单节点/MCP/CI 的务实比较。
- [从源码运行与配置](../../SOURCE_CONFIGURATION_AND_TESTING.md)：准备运行环境、配置模型并执行 CLI/TUI 验证。
- [二进制包运行](../runbooks/binary-runtime.md)：使用已构建二进制的运行、配置和诊断流程。
- [配置与模型](configuration/README.md)：管理 Vendor、Model、Provider、Credential、模型刷新和路由。

## 使用 Rivo

| 主题 | 帮助入口 | 说明 |
| --- | --- | --- |
| CLI 与 TUI | [开发模式运行手册](../runbooks/development-runtime.md) | 运行 CLI/TUI、会话、配置与本地诊断。 |
| 会话与历史 | [二进制包运行](../runbooks/binary-runtime.md) | 查询、删除和清空会话历史；删除操作需要独立确认。 |
| 模式、policy 与工具 | [安全策略](../architecture/safety-policy.md)与[工具运行时](../architecture/tool-runtime.md) | 当前支持的基础模式、policy 和工具边界。 |

## 扩展与互联

| 主题 | 帮助入口 | 说明 |
| --- | --- | --- |
| MCP | [远程 MCP 冒烟手册](../runbooks/mcp-remote-smoke.md) | MCP 安装、认证和远程连通性验证。 |
| A2A | [局域网发现与配对手册](../runbooks/a2a-lan-pairing.md) | 局域网发现、配对和信任管理。 |
| app-server | [本机宿主运行手册](../runbooks/app-server-local-host.md) | 使用 stdio 启动本机 app-server 宿主。 |

## 验收与排查

- [配置故障排查](configuration/troubleshooting.md)：模型配置、认证、刷新和路由问题。
- [发布流程](../runbooks/releasing.md)：质量门、归档运行验收与草稿发布。

## 文档边界

| 位置 | 职责 |
| --- | --- |
| `docs/guides/` | 面向使用者的操作、验证与排查。 |
| `docs/runbooks/` | 构建、验收、发布和运维流程。 |
| `docs/architecture/` | 架构决策、数据模型与技术契约。 |

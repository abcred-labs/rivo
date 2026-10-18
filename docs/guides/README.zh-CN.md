# Rivo 使用指南

本目录提供 CLI、TUI 和本机 Runtime 的公开使用文档。每篇页面都有同目录下的英文主文档，文件名不带 `.zh-CN` 后缀。

## 从这里开始

- [架构、能力与使用场景](architecture-and-capabilities.zh-CN.md)
- [安装发行版](one-click-install.zh-CN.md)
- [TUI 使用指南](tui.zh-CN.md)
- [配置指南](configuration/README.zh-CN.md)
- [源码配置与测试](../../SOURCE_CONFIGURATION_AND_TESTING.md)
- [二进制运行](../runbooks/binary-runtime.zh-CN.md)
- [功能矩阵](feature-matrix.zh-CN.md)
- [Runtime 接入](runtime-integration.zh-CN.md)
- [Agent 集群工作流](agent-cluster-workflows.zh-CN.md)

## 常见任务

| 任务 | 入口 |
| --- | --- |
| 无界面执行任务 | [`rivo run`](../../README.zh-CN.md) 与 [CLI 契约](../architecture/cli.zh-CN.md) |
| 使用交互终端 | [TUI 指南](tui.zh-CN.md) |
| 配置 Provider 和 Model | [配置指南](configuration/README.zh-CN.md) |
| 排查路由或 Credential | [配置故障排查](configuration/troubleshooting.zh-CN.md) |
| 使用已解压发行版 | [二进制运行手册](../runbooks/binary-runtime.zh-CN.md) |
| 接入 MCP 或 A2A | [MCP 手册](../runbooks/mcp-remote-smoke.zh-CN.md) 与 [A2A 配对](../runbooks/a2a-lan-pairing.zh-CN.md) |

架构和协议页面说明实现契约，runbook 说明可重复的构建、发布和接入流程。已安装
二进制自带的命令帮助仍是参数和退出行为的权威来源。

[English](README.md)

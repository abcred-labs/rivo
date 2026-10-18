# TUI 使用指南

Rivo 的终端 TUI 使用与 `rivo run` 相同的 Session 和模型路由运行时。它要求
真实交互式终端，不提供后台服务，也不会绕过 policy 或工具确认。

## 启动会话

```text
rivo tui
rivo tui --workspace <PATH>
```

stdin 和 stdout 都必须是交互终端。脚本、管道和 CI 应使用
`rivo run --format jsonl`。`--workspace` 只覆盖本次进程的工作区；已有配置时
不会静默改写持久化默认工作区。

首次运行前可检查：

```text
rivo --version
rivo config validate
rivo config provider list
rivo config model list --state enabled --format table
```

## 首次配置

没有有效 Provider 时，TUI 会打开首次配置表单：

1. 输入或选择 Credential 引用。密钥只能通过专用遮罩提示输入，或使用
   `config credentials set NAME --from-env ENV` 写入。
2. 输入 Provider ID 和 Base URL。URL 是请求前缀，通常以 `/v1` 结尾；不要填
   `/chat/completions`、`/responses` 或 `/messages`。
3. 确认是否保存并执行 discovery。确认前取消不会保存草稿；已单独保存的
   Provider 不会因后续 discovery 或 Model 选择失败而回滚。
4. 选择 Vendor、完整 canonical Model ID，以及 Auto 或显式 Provider。

必须完成 discovery 才能路由。TUI 不会把本地目录中存在的 Model 直接视为可用。

## 选择 Model 和 Provider

按 `F6` 或执行 `/settings` 打开 Session Settings。Browse 模式下使用方向键或
Tab 移动，Enter 编辑；枚举字段使用方向键，布尔和多选使用 Space，文本和数字
直接输入。Enter 提交字段，`Ctrl+Enter` 将全部设置应用到后续 Turn；Esc 取消
当前字段或关闭面板。

路由字段包括：

- Vendor：模型发布方目录项。
- Model：完整 canonical ID，不支持别名或前缀匹配。
- Provider：`Auto` 或显式 Provider ID。
- Reasoning effort：只显示所选 Model 支持的值。

`/model` 打开 Model 字段；`/model <canonical-model-id>` 校验并为后续 Turn 选择
Model。`/provider` 打开 Provider 字段；`/provider auto` 选择 Auto，
`/provider <provider-id>` 选择指定 Provider。无效的显式 Provider 或不可用 Model
会直接拒绝，不会静默回退。

Auto 按 Provider priority 降序、ID 升序选择已启用且已刷新成功的 Provider。请求
开始后不会运行时故障切换。请求协议取所选 Model 的 `supported_protocols` 第一项。

## Turn 控制与命令

输入框用于提交普通任务。输入为空时按 `/` 打开命令面板。可用命令如下：

| 命令 | 作用 |
| --- | --- |
| `/help` | 显示命令列表。 |
| `/new` | 当前 Turn 结束后新建 Session。 |
| `/cancel` | 取消当前 Turn。 |
| `/retry` | 重试最近一个可重试步骤。 |
| `/errors [filter]` / `/errors clear` | 打开或清空诊断。 |
| `/permissions` / `/permissions clear` | 查看或撤销 Session 授权。 |
| `/sessions` | 打开历史；`/sessions resume ID` 恢复。 |
| `/sessions delete ID` | 删除历史前要求二次确认。 |
| `/workspace` / `/workspace open PATH_OR_ID` | 列出或切换工作区。 |
| `/network on\|off` | 切换当前 Turn 的 Web。 |
| `/browser` / `/browser on\|off\|status` | 查看或切换 CDP Browser。 |
| `/mcp NAME` / `/mcp off` | 选择已启用 MCP server 或关闭 MCP。 |
| `/a2a PEER` / `/a2a off` | 选择已配对 A2A peer 或关闭 A2A。 |
| `/subagents on\|off` | 开关内部 SubAgent 委派。 |
| `/trace` | 开关当前 Turn 追踪。 |
| `/memory` / `/memory delete ID` | 打开记忆证据或删除记忆。 |
| `/language` / `/language LOCALE` / `/language reload` | 查看、切换或重载 UI 语言。 |
| `/goal`、`/goal status`、`/goal pause`、`/goal resume`、`/goal clear` | 管理本地 Goal。 |
| `/exit` 或 `/quit` | 当前 Turn 终态后退出。 |

MCP 和 A2A 命令只能选择已经配置并允许的条目，不能在 Turn 中新增 server 或
信任 peer。

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `F1` 或空输入时的 `/` | 打开命令面板。 |
| `?` | 打开帮助。 |
| `F2` | 打开错误面板。 |
| `F3` | 打开权限面板。 |
| `F4` | 打开历史。 |
| `F6` | 打开 Session Settings。 |
| `Tab` / `Shift+Tab` | 切换焦点或字段。 |
| 方向键 | 移动焦点、选择项或滚动。 |
| `PageUp` / `PageDown` / `End` | 滚动当前视图。 |
| `Enter` | 提交文本、确认对话框或编辑字段。 |
| `Esc` | 关闭覆盖层、取消编辑或拒绝确认。 |
| 输入为空时的 `q` | 空闲时退出；运行中先取消 Turn。 |
| `Ctrl+C` | 取消启动或当前操作。 |
| 历史中的 `Delete` | 进入二次删除确认。 |

确认覆盖层中 `y` 允许，`n` 或 Esc 拒绝；只有界面显示 Session scope 可用时才能
使用 `a`。授权只适用于当前显示的请求和范围，不是全局授权。

## 会话与恢复

`/sessions` 打开当前 Workspace 的历史。选择条目后按 Enter，或执行
`/sessions resume <SESSION_ID>`。恢复必须在所属 Workspace 中进行。切换工作区、
新建 Session、恢复其他 Session 或退出前，必须先取消正在运行的 Turn。

Turn 失败时使用 `/errors`。诊断面板会保留 TUI 可操作状态，不会把整个工作区永久
锁定在错误页面。

## 平台限制

Windows 优先使用 ConPTY，只有标准句柄不可用时才使用控制台句柄。Linux 和 macOS
使用原生终端流。终端适配层会在正常退出、错误和 panic 清理时恢复 raw mode、备用
屏幕、bracketed paste 和光标。不提供交互输入输出的终端不能安全运行 TUI。

Windows 归档另外包含 Computer Use 和浏览器扩展接线；Linux/macOS 归档提供通用
CLI/TUI，不包含 Windows 专属集成。归档和系统库要求见运行和安装手册。

相关页面：[配置指南](configuration/README.zh-CN.md)、[模型路由](configuration/model-routing.zh-CN.md)、
[二进制运行手册](../runbooks/binary-runtime.zh-CN.md)。

---

[English](tui.md)

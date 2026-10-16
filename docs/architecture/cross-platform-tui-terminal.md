# 跨平台 TUI 终端架构

## 目标

TUI 不依赖 Windows Terminal、PowerShell 或具体 shell。`rivo-tui` 只负责状态 reducer、布局和 ratatui 渲染；CLI 负责终端 I/O、能力探测和生命周期。

## 能力探测

启动前生成 `TerminalCapabilities`：交互输入、交互输出、Unicode、颜色、bracketed paste、alternate screen 和 resize。Windows 在标准句柄不可用时检查 `CONIN$`/`CONOUT$`，兼容 ConPTY；普通管道仍拒绝进入 TUI。Linux/macOS 使用标准 terminal 判定并参考 `TERM`/`COLORTERM`。

## 生命周期

终端进入、事件读取、尺寸查询和退出由 CLI 终端适配层统一处理。RAII cleanup 必须恢复 raw mode、alternate screen、bracketed paste、光标和 stdout；正常错误和 panic 都执行幂等 cleanup，cleanup 错误不能覆盖原始错误。

## 测试边界

默认测试使用 TestBackend 和注入事件源；平台集成测试使用 portable-pty（Windows ConPTY、Linux/macOS Unix PTY）。真实宿主 smoke 仍分别验证 Windows Terminal/PowerShell、Linux 终端和 macOS Terminal/iTerm2。

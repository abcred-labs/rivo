# Rivo

Rivo 是使用 Rust 编写的 Agent Runtime，提供 CLI 与终端 TUI。它把自然语言任务转成可观察、受 policy 控制的工作过程，统一处理模型路由、工具、确认、证据和可恢复的会话历史。

## 当前能力

- `rivo run` 无界面执行任务，`rivo tui` 提供交互式终端会话。
- 共享 Session/Turn/Event 状态，以及 SQLite 历史、取消、确认和恢复。
- 受 policy 控制的文件、进程、Web、Browser、MCP、记忆和 A2A 工具。
- 本机 app-server 接入和受信任的本机/局域网实例控制。
- 脱敏诊断与日志，以及显式的模型/Provider 路由。

Rivo 面向本地 Workspace 中用户可见的工作。每个工具请求在执行前都会经过 schema、policy、资源限制和必要的确认。Workspace 或 Git worktree 不等同于操作系统沙箱。

本仓库不提供托管网关、多租户执行服务、公网认证或手机/Web 客户端；这些属于独立产品界面。

## 安装发行版

在 Linux 或 macOS 安装最新稳定版本：

```sh
curl -fsSL https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.sh | sudo sh
rivo --version
```

在管理员 PowerShell 中安装 Windows 最新稳定版本：

```powershell
irm https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.ps1 | iex
rivo --version
```

安装器会先校验 Release manifest 和 SHA-256，再安装完整归档。安装需要管理员权限，
用户配置、Credential 和会话仍保存在安装目录之外。固定版本命令、手动归档安装和故障处理见
[安装指南](docs/guides/one-click-install.zh-CN.md)。可用归档和各版本命令见
[GitHub Releases](https://github.com/abcred-labs/rivo/releases)。

Linux 发布归档支持 Debian 12（glibc 2.36）及更新的兼容 GNU/glibc 系统，并需要 `libdbus-1.so.3`。

检查并升级到更新版本：

```text
rivo upgrade --check
rivo upgrade --apply
```

`rivo upgrade --apply` 会在终端中请求确认；非交互自动化必须追加 `--yes`，并保留一个上一版本。

## 从源码构建

前置条件：Git、rustup、Python 3.11+；浏览器扩展测试需要 Node.js 22；Windows 构建还需要带 C++ 和 Windows SDK 的 Visual Studio Build Tools。

Linux 需要 C/C++ 工具链、CMake、pkg-config 和 D-Bus 头文件（Ubuntu：`sudo apt-get install build-essential cmake pkg-config libdbus-1-dev`）；运行 Linux 归档需 `libdbus-1.so.3`。macOS 需要 Xcode Command Line Tools。

```powershell
Set-Location runtime
cargo build --workspace --all-targets --locked
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

先运行 `rustup toolchain install 1.88.0 --profile minimal` 安装最低支持版本检查使用的工具链，再运行确定性质量门禁：

```powershell
cargo run -p xtask -- check
```

构建不会访问模型 Provider，也不需要凭据。

## 配置与运行

执行任务前请先配置 Provider、Model 和本机 Credential 引用。不要把 API key 写入命令参数、配置文件、日志或报告。

```powershell
rivo run --prompt "分析当前仓库并总结主要模块。"
rivo tui
```

详见[配置与测试](SOURCE_CONFIGURATION_AND_TESTING.md)、[架构说明](docs/guides/architecture-and-capabilities.md)和[Runtime 接入](docs/guides/runtime-integration.md)。

交互式终端使用请参阅 [TUI 指南](docs/guides/tui.zh-CN.md)。Provider、Credential、
Vendor 和自定义 Model 的配置见[配置指南](docs/guides/configuration/README.zh-CN.md)。

## 贡献与安全

提交 Pull Request 前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。安全问题请按 [SECURITY.md](SECURITY.md) 的流程报告，不要在公开 issue 中披露凭据。

Rivo 使用 [MIT License](LICENSE) 发布。第三方授权见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

---

[English](README.md)

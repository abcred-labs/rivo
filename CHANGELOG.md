# Changelog

## 0.2.9（2026-10-21）

### 安装与升级

- 一键安装脚本（`rivo-install.sh` / `rivo-install.ps1`）和 `rivo upgrade` 新增中国大陆下载镜像支持：设置 `RIVO_GITHUB_MIRROR`（如 `https://github-mirror.abc.red`）与 `RIVO_RELEASE_ASSETS_MIRROR`（如 `https://gh-release-assets.abc.red`）后，逐跳改写 GitHub 与 release-assets 的重定向。镜像只加速传输，manifest、归档名称和 SHA-256 校验不变；两个变量都必须是 `https://` 地址。
- 安装与升级现在手动逐跳跟随最多 5～6 次 HTTPS 重定向，拒绝降级到 HTTP 的重定向。
- Unix 安装脚本在解包前拒绝含绝对路径或 `..` 的归档成员，并以 `--no-same-owner` 解包。
- 安装指南、README 与 Release 说明新增“中国大陆安装”章节；修正二进制运行手册中过时的 `rivo upgrade` 描述。

### 兼容与发布

- Cargo 工作区版本与发布 tag 对齐，`rivo --version`、语言包版本门控和 app-server 上报的运行时版本保持一致。
- 发布手册改为描述实际流程：全部门禁和五目标矩阵通过后自动发布为 latest。

## 0.2.8（2026-10-19）

### 兼容与发布

- 保持 Linux 发布构件的 Debian 12（glibc 2.36）最低支持基线、Ubuntu 22.04 构建和 Debian 12 smoke 验证。
- 修正 Debian 12 smoke 配置写入脚本的 ShellCheck 兼容性，确保 Release 门禁在启用 workflow lint 后通过。

## 0.2.7（2026-10-19）

### 兼容与发布

- 延续 Linux 发布构件的 Debian 12（glibc 2.36）最低支持基线，并保留 Ubuntu 22.04 构建与 Debian 12 smoke 验证。
- Release CI 的 secret 扫描明确允许 A2A 协议测试夹具中的固定公钥哈希，避免把公开测试向量误报为凭据。

## 0.2.6（2026-10-19）

### 兼容与发布

- Linux 发布构件改用 Ubuntu 22.04 构建，并在 Debian 12（glibc 2.36）容器中验证，支持 Debian 12 及更新的兼容 GNU/glibc 系统。
- Release CI 增加 GLIBC ABI 上限检查，防止构件重新依赖 Debian 12 不提供的更高版本符号。
- Windows 安装器加强 manifest、归档和 SHA-256 sidecar 校验。

## 0.2.0（2026-10-18）

首个 MIT 开源版本。

### 能力

- CLI/TUI、模型路由、文件与进程工具、确认、历史恢复、Web/MCP、记忆、A2A 和本机 app-server。
- Windows amd64、Linux amd64/arm64、macOS amd64/arm64 原生归档；包含语言资源、完整第三方许可证和构件验收报告。
- 修复源码启动命令的二进制选择；更新 TLS、QUIC 与 TUI 依赖，移除本次 RustSec 扫描发现的漏洞和警告。

### 兼容与升级

- 主配置为 schema 6，Model 的有序协议列表决定协议，Provider 保存地址及认证引用。支持的历史配置按文档迁移；无法安全迁移的输入零写入拒绝。
- SQLite 会话 schema 为 18；schema 17 的 Workspace 迁移必须显式备份并确认，不随普通启动静默执行。其他旧会话请按存储兼容规则处理，不删除整个数据目录。
- `rivo upgrade --check` 只查询 GitHub Releases；`rivo upgrade --apply` 会下载并校验归档，确认后原子替换安装并保留一个上一版本。凭据与用户数据不得随包分发。

### 已知限制

- 0.x 阶段，尚不承诺所有旧系统和交互终端兼容；Linux 包为 GNU/glibc 目标。
- Windows 专属终端、Computer Use 和浏览器扩展接线不在 Linux/macOS 提供；真实浏览器、交互 TUI、真实模型及跨节点互操作须单独验收。
- 归档未做平台代码签名；校验和仅验证完整性。CLI/TUI 文案尚未完全本地化。
- 不提供托管网关、公网认证、多租户服务、中心监督、Desktop UI 或 Web/Mobile 客户端。

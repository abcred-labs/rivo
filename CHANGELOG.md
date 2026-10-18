# Changelog

## 0.1.0（候选，2026-10-18）

首个 MIT 开源版本，正式发布日期以 GitHub Release 为准。

### 能力

- CLI/TUI、模型路由、文件与进程工具、确认、历史恢复、Web/MCP、记忆、A2A 和本机 app-server。
- Windows amd64、Linux amd64/arm64、macOS amd64/arm64 原生归档；包含语言资源、完整第三方许可证和构件验收报告。
- 修复源码启动命令的二进制选择；更新 TLS、QUIC 与 TUI 依赖，移除本次 RustSec 扫描发现的漏洞和警告。

### 兼容与升级

- 主配置为 schema 6，Model 的有序协议列表决定协议，Provider 保存地址及认证引用。支持的历史配置按文档迁移；无法安全迁移的输入零写入拒绝。
- SQLite 会话 schema 为 18；schema 17 的 Workspace 迁移必须显式备份并确认，不随普通启动静默执行。其他旧会话请按存储兼容规则处理，不删除整个数据目录。
- `rivo upgrade` 只查询 GitHub Releases，不自动安装；下载并校验后保留归档完整目录，手动替换。凭据与用户数据不得随包分发。

### 已知限制

- 0.x 阶段，尚不承诺所有旧系统和交互终端兼容；Linux 包为 GNU/glibc 目标。
- Windows 专属终端、Computer Use 和浏览器扩展接线不在 Linux/macOS 提供；真实浏览器、交互 TUI、真实模型及跨节点互操作须单独验收。
- 归档未做平台代码签名；校验和仅验证完整性。CLI/TUI 文案尚未完全本地化。
- 不提供托管网关、公网认证、多租户服务、中心监督、Desktop UI 或 Web/Mobile 客户端。

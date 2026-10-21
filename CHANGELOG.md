# Changelog

英文摘要见 [CHANGELOG.en.md](CHANGELOG.en.md)，Release 说明会附带对应版本的英文条目。

## 0.2.12（2026-10-21）

### 安装

- `rivo-install.sh` 严格解析 Release manifest：每个键必须独占一行且恰好出现一次，值限定为安全字符集；其他布局一律拒绝，不再用 `sed` 猜测。
- 修复镜像地址检查中 `printf` 格式串丢失转义的问题。

### 文档

- 安装指南新增“验证构建来源与 SBOM”：`gh attestation verify`、离线 bundle 验证（需以 UTF-8 读写）和 CycloneDX SBOM 说明。

## 0.2.11（2026-10-21）

### 升级与安装加固

- `rivo upgrade` 要求 `RIVO_RELEASE_BASE_URL` 使用 `https://`（仅回环地址允许 `http://`）；下载流式写盘并限制 512 MiB，manifest 与校验和响应限制 1 MiB。
- ZIP/tar 解包拒绝符号链接，限制条目数（10,000）和解包总大小（2 GiB）。Unix 安装脚本、PowerShell 安装脚本与 `rivo upgrade` 在安装前校验归档成员路径，替换已有版本目录时先挪开再换入，失败可回滚，不再先删除。
- `rivo-install.sh` / `rivo-install.ps1` 的 `--base-url` / `-BaseUrl` 同样要求 https（回环除外）。

### 发布供应链

- 每个归档新增 CycloneDX SBOM（`rivo-<version>-<platform>-<arch>.sbom.cdx.json`）和 GitHub 构建来源 attestation，可用 `gh attestation verify <归档> --repo abcred-labs/rivo` 验证。归档仍未做平台代码签名。
- Release workflow 增加守卫：tag 版本必须与 `runtime/Cargo.toml` 一致，`CHANGELOG.md` 与 `CHANGELOG.en.md` 必须有对应条目，提交必须位于 `main`。
- CI 合并了重复的测试步骤；Release 说明附带英文条目。

## 0.2.10（2026-10-21）

### 安装与升级

- 一键安装脚本（`rivo-install.sh` / `rivo-install.ps1`）和 `rivo upgrade` 新增中国大陆下载镜像支持：设置 `RIVO_GITHUB_MIRROR`（如 `https://github-mirror.abc.red`）与 `RIVO_RELEASE_ASSETS_MIRROR`（如 `https://gh-release-assets.abc.red`）后，逐跳改写 GitHub 与 release-assets 的重定向。镜像只加速传输，manifest、归档名称和 SHA-256 校验不变；两个变量都必须是 `https://` 地址。
- 安装与升级现在手动逐跳跟随最多 5～6 次 HTTPS 重定向，拒绝降级到 HTTP 的重定向。
- Unix 安装脚本在解包前拒绝含绝对路径或 `..` 的归档成员，并以 `--no-same-owner` 解包。
- 安装指南、README 与 Release 说明新增“中国大陆安装”章节；修正二进制运行手册中过时的 `rivo upgrade` 描述。

### 安全

- 更新 `rustls-webpki`（0.103.16）和 `webpki-roots`（1.0.10），修复 RUSTSEC-2026-0382（可能接受已被撤销信任的中间证书）和 RUSTSEC-2026-0381（内置根证书信任被撤销的 emSign 中间证书）。

### 兼容与发布

- 0.2.9 的 tag 因 RustSec 门禁失败而未发布任何构件，且版本 tag 不可改写，因此改以 0.2.10 发布；不存在 0.2.9 的 Release。
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

# Rivo CLI 安装

GitHub Releases 提供 Linux amd64、Linux arm64、macOS amd64、macOS arm64 和 Windows amd64 归档。发布资产包含归档、`.sha256` 校验文件以及对应的 `release-manifest-<platform>-<arch>.json`。

## 下载与校验

从 [最新 Release](https://github.com/abcred-labs/rivo/releases/latest) 下载与你的平台匹配的归档和校验文件，在解压前验证 SHA-256：

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
```

macOS 可使用 `shasum -a 256 -c`；Windows PowerShell 可使用：

```powershell
(Get-FileHash .\rivo-<version>-windows-amd64.zip -Algorithm SHA256).Hash
Get-Content .\rivo-<version>-windows-amd64.zip.sha256
```

确认摘要相同后解压，保持归档目录完整，将该目录加入用户自己的 PATH。归档内包含 LICENSE、完整第三方声明、语言资源，以及 Windows 专属的浏览器扩展资源。归档未做平台代码签名；SHA-256 用于校验文件完整性，不等同于发布者身份签名。

五个目标均须在各自原生 runner 验收。Windows 系统集成以 Windows 11 x64 为使用边界；Linux/macOS 不提供 Windows 专属终端、Computer Use 和浏览器扩展接线。Linux 归档为 GNU/glibc 目标，不是 musl/Alpine 构件；尚未声明旧发行版或旧 macOS 的兼容性。具体 runner、验证结果与限制见[发布流程](../runbooks/releasing.md)。

## 从源码构建

```powershell
cd runtime
cargo build --release --locked
```

构建产物位于 `runtime/target/release/`。发布归档由 `scripts/package.py` 生成；详见仓库中的 CI workflow。

## 升级检查

已安装的 CLI 可以运行 `rivo upgrade` 检查 GitHub Releases 上的最新版本。该命令只报告可用更新，不会自动替换正在运行的程序。

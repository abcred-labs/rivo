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

确认摘要相同后解压归档，将 `rivo`（Windows 为 `rivo.exe`）放入用户自己的 PATH。归档内还包含 LICENSE、语言资源和浏览器扩展资源。

## 从源码构建

```powershell
cd runtime
cargo build --release --locked
```

构建产物位于 `runtime/target/release/`。发布归档由 `scripts/package.py` 生成；详见仓库中的 CI workflow。

## 升级检查

已安装的 CLI 可以运行 `rivo upgrade` 检查 GitHub Releases 上的最新版本。该命令只报告可用更新，不会自动替换正在运行的程序。

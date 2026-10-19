# Rivo CLI 安装

## 安装最新版本

在 Linux 或 macOS 中以管理员权限运行短安装命令：

```sh
curl -fsSL https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.sh | sudo sh
rivo --version
```

在管理员 PowerShell 中运行 Windows 安装命令：

```powershell
irm https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.ps1 | iex
rivo --version
```

脚本会检测支持的平台和架构，下载匹配的 manifest 与归档，校验两份摘要后安装完整资源目录。
Unix 安装到 `/usr/local/lib/rivo` 并通过 `/usr/local/bin/rivo` 提供入口；Windows 安装到
`%ProgramFiles%\Rivo\current` 并加入系统 PATH。脚本支持 `--version`、`--install-root`、
`--base-url`（PowerShell 使用对应的命名参数）。归档未签名，SHA-256 只验证完整性，不证明发布者身份。
Unix 使用其他 `--install-root` 时，启动链接会写入 `<install-root>/bin/rivo`，便于本地镜像测试且不会修改 `/usr/local/bin`。

每个 Release 都会上传 `rivo-install.sh` 和 `rivo-install.ps1`。仓库中的
`print-install-commands.py` 只打印手动归档命令，不执行安装或修改 PATH。

## 手动安装固定版本

Rivo 当前发布五种便携归档：Windows amd64 为 `.zip`，Linux amd64/arm64 和 macOS amd64/arm64 为 `.tar.gz`。当前 Release 不提供 MSI、PKG 或 DEB。

## 下载、校验和解压

从[最新 Release](https://github.com/abcred-labs/rivo/releases/latest)下载匹配平台的归档、`.sha256` 和对应 `release-manifest-<platform>-<arch>.json`。解压前先校验：

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
cd rivo-<version>-linux-amd64
export PATH="$PWD:$PATH"
./rivo --version
```

macOS 使用 `shasum -a 256 -c`。Windows PowerShell：

```powershell
(Get-FileHash .\rivo-<version>-windows-amd64.zip -Algorithm SHA256).Hash
Get-Content .\rivo-<version>-windows-amd64.zip.sha256
Expand-Archive .\rivo-<version>-windows-amd64.zip -DestinationPath .\rivo-unpacked
Set-Location .\rivo-unpacked\rivo-<version>-windows-amd64
$env:PATH = "$PWD;$env:PATH"
.\rivo.exe --version
```

计算值必须与 sidecar 第一列相同。归档未签名；SHA-256 只验证完整性，不证明发布者身份。请保持解压目录完整，其中包含 LICENSE、第三方声明、中英文语言资源和 mock provider 示例。Linux 归档支持 Debian 12（glibc 2.36）及更新的兼容 GNU/glibc 系统，并需要 `libdbus-1.so.3`，不是 musl/Alpine 构件。

需要复制命令时可运行：

```text
python scripts/print-install-commands.py --platform linux --arch amd64 --package ./rivo-<version>-linux-amd64.tar.gz --version <version>
```

脚本只打印校验、解压、临时 PATH 和版本检查命令，不调用安装器，也不修改系统 PATH。

## 从源码构建

```powershell
Set-Location runtime
cargo build --release --locked
```

完整环境与门禁见[源码配置与测试](../../SOURCE_CONFIGURATION_AND_TESTING.md)。构建不会访问模型 Provider，也不需要 Credential。

## 升级和卸载

```text
rivo upgrade --check
rivo upgrade --apply
```

`rivo upgrade --check` 只检查当前平台 manifest。`rivo upgrade --apply` 会下载并校验新归档，
在终端请求确认后替换安装，并保留一个上一版本用于回滚。非交互自动化使用
`rivo upgrade --apply --yes`。升级前请停止其他 Rivo 进程；Windows 会使用短生命周期替换助手，
因为运行中的可执行文件处于锁定状态。配置、Credential 和会话数据位于归档目录之外。

卸载时从 PATH 移除归档目录；只有确认要丢弃数据时才删除 `rivo config path`、`rivo config credentials path` 和 `rivo sessions path` 显示的用户文件。

[English](one-click-install.md) | [二进制运行手册](../runbooks/binary-runtime.zh-CN.md) | [配置指南](configuration/README.zh-CN.md)

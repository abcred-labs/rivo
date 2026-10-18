# Rivo CLI 安装

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

计算值必须与 sidecar 第一列相同。归档未签名；SHA-256 只验证完整性，不证明发布者身份。请保持解压目录完整，其中包含 LICENSE、第三方声明、中英文语言资源和 mock provider 示例。Linux 归档需要 glibc 与 `libdbus-1.so.3`，不是 musl/Alpine 构件。

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
```

`rivo upgrade` 只检查当前平台 manifest，不会下载、安装、替换或重启自身。升级时停止正在运行的 Rivo，下载新归档并校验后手动替换旧目录，再运行 `rivo --version`。配置、Credential 和会话数据位于归档目录之外。

卸载时从 PATH 移除归档目录；只有确认要丢弃数据时才删除 `rivo config path`、`rivo config credentials path` 和 `rivo sessions path` 显示的用户文件。

[English](one-click-install.md) | [二进制运行手册](../runbooks/binary-runtime.zh-CN.md) | [配置指南](configuration/README.zh-CN.md)

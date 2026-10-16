# 二进制包运行手册

## 获取发布包

从 [GitHub Releases](https://github.com/abcred-labs/rivo/releases) 下载与你的平台匹配的归档、`.sha256` 文件和 `release-manifest-<platform>-<arch>.json`。先验证 SHA-256，再解压并将 `rivo`（Windows 为 `rivo.exe`）放入用户自己的 PATH。

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
```

macOS 使用 `shasum -a 256 -c`；Windows 使用 PowerShell 的 `Get-FileHash`。归档内包含 LICENSE、语言资源及 Windows 浏览器扩展资源。

## 升级检查

```text
rivo upgrade
```

`rivo upgrade` 从 GitHub Releases 读取当前平台的 manifest，只报告是否存在新版本，不会自动重启或替换运行中的 CLI、TUI 或 app-server。可通过 `RIVO_RELEASE_BASE_URL` 指向兼容的镜像目录；镜像需要提供对应平台的 `release-manifest.json`。

## 首次检查

```powershell
rivo --version
rivo --help
rivo config --help
rivo config path
rivo config credentials path
```

配置、Credential、模型路由和诊断流程请参阅[配置指南](../guides/configuration/README.md)与[从源码运行手册](development-runtime.md)。

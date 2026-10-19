# Rivo 归档使用指南

Rivo 在 [GitHub Releases](https://github.com/abcred-labs/rivo/releases) 发布便携归档：Windows amd64 使用 ZIP，Linux amd64/arm64 和 macOS amd64/arm64 使用 tar.gz。当前 Release 不提供 MSI、PKG 或 DEB。

## 校验和解压

从同一 Release 下载归档及 `.sha256` sidecar，解压前校验：

```powershell
(Get-FileHash .\rivo-<version>-windows-amd64.zip -Algorithm SHA256).Hash
Get-Content .\rivo-<version>-windows-amd64.zip.sha256
Expand-Archive .\rivo-<version>-windows-amd64.zip -DestinationPath .\rivo-unpacked
Set-Location .\rivo-unpacked\rivo-<version>-windows-amd64
$env:PATH = "$PWD;$env:PATH"
.\rivo.exe --version
```

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
cd rivo-<version>-linux-amd64
export PATH="$PWD:$PATH"
./rivo --version
```

macOS 使用 `shasum -a 256 -c`。Linux 归档支持 Debian 12（glibc 2.36）及更新的兼容 GNU/glibc 系统，需要 `libdbus-1.so.3`，不是 musl/Alpine 目标。归档未签名，摘要只验证完整性。

## 本机 Mock Provider

归档包含确定性的 loopback Provider。先用 Python 3 启动：

```text
python share/rivo/examples/mock_provider.py
```

另开终端完成目录与路由配置：

```text
rivo config vendor set mock-local --display-name "Local Mock"
rivo config model set mock-local-model --vendor mock-local --display-name "Local Mock" --protocols openai_chat_completions
rivo config provider set mock-local --base-url http://127.0.0.1:18780/v1
rivo config provider models refresh mock-local
rivo config route --vendor mock-local --model mock-local-model --provider-id mock-local
rivo run --vendor mock-local --model mock-local-model --provider-id mock-local --prompt "Reply RIVO_MOCK_OK"
```

该 fixture 只监听 loopback，不访问外部模型服务。

## Credential 和数据

使用 `rivo config credentials set <credential-name>` 保存密钥，或用 `rivo run --api-key-env ENV` 只为一次运行提供。不要把 key 写进参数、配置、日志、报告或归档。路径查询：

```text
rivo config path
rivo config credentials path
rivo sessions path
```

[English](README.md) | [安装指南](../../docs/guides/one-click-install.zh-CN.md) | [配置指南](../../docs/guides/configuration/README.zh-CN.md)

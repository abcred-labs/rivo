# 二进制包运行手册

## 获取发布包

从 [GitHub Releases](https://github.com/abcred-labs/rivo/releases) 下载与你平台匹配的归档、`.sha256` 和 manifest。当前支持 Windows amd64 ZIP、Linux amd64/arm64 tar.gz、macOS amd64/arm64 tar.gz。解压前必须校验摘要，并保持解压目录完整。

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
```

macOS 使用 `shasum -a 256 -c`，Windows 使用 PowerShell `Get-FileHash`。Linux 归档支持 Debian 12（glibc 2.36）及更新的兼容 GNU/glibc 系统，需要 `libdbus-1.so.3`，不是 musl/Alpine 包。

## 首次检查

```text
rivo --version
rivo --help
rivo config --help
rivo config path
rivo config credentials path
rivo sessions path
```

按[配置指南](../guides/configuration/README.zh-CN.md)配置 Credential、Vendor、Model 和 Provider，真实任务前运行 `rivo config provider models refresh <provider-id>`。

## CLI 和 TUI

无界面任务使用 `rivo run --prompt <TEXT>` 或 `rivo run --stdin`。只有在交互终端中才使用 `rivo tui [--workspace <PATH>]`。快捷键和 slash 命令见 [TUI 指南](../guides/tui.zh-CN.md)。

## 升级检查

```text
rivo upgrade --check
```

`--check` 读取当前平台 manifest 并报告是否有更新，不会下载或安装。`rivo upgrade --apply`（非交互加 `--yes`）会下载归档、校验 manifest 与 SHA-256，原子替换安装并保留一个上一版本，执行前请停止其他 Rivo 进程。

`RIVO_RELEASE_BASE_URL` 可指向自建镜像，镜像必须提供 `<platform>-<arch>/release-manifest.json`。GitHub 访问较慢的网络可设置 `RIVO_GITHUB_MIRROR` 与 `RIVO_RELEASE_ASSETS_MIRROR`（均为 `https://` 地址，例如 `https://github-mirror.abc.red` 和 `https://gh-release-assets.abc.red`），改写 GitHub 与 release-assets 的重定向；校验逻辑不变。详见[安装指南](../guides/one-click-install.zh-CN.md#中国大陆安装)。

手动升级时停止 Rivo，下载并校验新归档，替换旧目录，再运行 `rivo --version`。已发布版本和归档不可覆盖，修复应发布新的 patch 版本。

## 本地数据和诊断

配置和 Credential 不在归档内。备份或删除前先查看 `config path`、`config credentials path` 和 `sessions path`。导出脱敏诊断包：

```text
rivo diagnostics create <destination-path> --yes
```

诊断包和会话备份可能包含脱敏后的业务内容，应按敏感文件处理，不得包含 API key。

[English](binary-runtime.md) | [安装指南](../guides/one-click-install.zh-CN.md) | [TUI 指南](../guides/tui.zh-CN.md)

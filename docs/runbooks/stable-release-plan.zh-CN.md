# 稳定版发布计划（草案）

Rivo 目前仍是 0.x 项目：行为、文件格式和平台支持都可能变化，变更日志中也如此声明。本文记录 1.0 之前必须满足的条件、如何验证仓库安全设置，以及代码签名需要什么。它是维护者持有的草案，决策变化时请随时修改。

## 发版准备

```text
python scripts/bump_version.py <下一个版本>
```

该脚本会修改 `runtime/Cargo.toml` 的版本，刷新 `Cargo.lock` 与 `licenses/`，并在 `CHANGELOG.md`、`CHANGELOG.en.md` 中加入需要填写的条目。它不会提交或打 tag。Release workflow 会拒绝以下情况：tag 版本与 `Cargo.toml` 不一致、缺少 CHANGELOG 条目或未填写完、提交不在 `main` 上。门禁和打 tag 步骤见[发布手册](releasing.zh-CN.md)。

## 仓库安全检查

```text
GH_TOKEN=<具备仓库管理读取权限的令牌> python scripts/check_repo_security.py
```

脚本会核对：生效中的 tag 规则集、`main` 的分支保护或规则集、Secret scanning 与 push protection、Dependabot 安全更新、私密漏洞报告、`dependabot.yml` 和 `SECURITY.md`。没有令牌时，仅管理员可见的项目显示为 `UNKNOWN`；只有确认缺失时才以非零状态退出。建议每次发版前运行。

## 1.0 发布条件（草案）

1. **兼容性承诺。** 配置 schema、会话 schema、app-server 协议和升级 manifest 被文档声明为稳定，并对每个受支持的 0.2.x 版本做过升级测试。
2. **真实环境验收。** 真实浏览器、交互 TUI、真实模型服务和跨节点 A2A 有可重复的验收流程，而不只是基于 mock 的 smoke 测试。默认测试中标为 ignored 的项目要么启用，要么明确声明不在范围内。
3. **平台支持矩阵。** 受支持的操作系统、架构和 libc 版本有清单并在 CI 中运行；不支持的目标（如 musl/Alpine）继续明确排除。
4. **供应链。** 归档具备校验和、CycloneDX SBOM 与构建来源 attestation（已完成），代码签名明确不做（见下）。
5. **仓库规范。** `scripts/check_repo_security.py` 带令牌运行通过，未处理的依赖更新已分流。
6. **文档。** 安装、升级、回滚和卸载有中英文文档，并在每个受支持平台上验证过。

## 代码签名

决定：Rivo 归档在所有平台都不做代码签名（不使用 Authenticode、Apple Developer ID/公证或 Linux 分离签名）。完整性与来源由 SHA-256 sidecar、CycloneDX SBOM 和 GitHub 构建来源 attestation 覆盖；它们不向操作系统证明发布者身份，因此通过浏览器下载归档时，Windows SmartScreen 和 macOS Gatekeeper 可能给出警告。安装脚本和 `rivo upgrade` 使用 `curl`/HTTP 客户端下载，不会触发这些隔离检查。只有当项目要通过必须签名的渠道分发时才重新评估。

[发布手册](releasing.zh-CN.md)。

## 仓库安全检查

```text
GH_TOKEN=<具备仓库管理读取权限的令牌> python scripts/check_repo_security.py
```

脚本会核对：生效中的 tag 规则集、`main` 的分支保护或规则集、Secret scanning 与 push protection、Dependabot 安全更新、私密漏洞报告、`dependabot.yml` 和 `SECURITY.md`。没有令牌时，仅管理员可见的项目显示为 `UNKNOWN`；只有确认缺失时才以非零状态退出。建议每次发版前运行。

## 1.0 发布条件（草案）

1. **兼容性承诺。** 配置 schema、会话 schema、app-server 协议和升级 manifest 被文档声明为稳定，并对每个受支持的 0.2.x 版本做过升级测试。
2. **真实环境验收。** 真实浏览器、交互 TUI、真实模型服务和跨节点 A2A 有可重复的验收流程，而不只是基于 mock 的 smoke 测试。默认测试中标为 ignored 的项目要么启用，要么明确声明不在范围内。
3. **平台支持矩阵。** 受支持的操作系统、架构和 libc 版本有清单并在 CI 中运行；不支持的目标（如 musl/Alpine）继续明确排除。
4. **供应链。** 归档具备校验和、CycloneDX SBOM 与构建来源 attestation（已完成），代码签名明确不做（见下）。
5. **仓库规范。** `scripts/check_repo_security.py` 带令牌运行通过，未处理的依赖更新已分流。
6. **文档。** 安装、升级、回滚和卸载有中英文文档，并在每个受支持平台上验证过。

## 代码签名

目前归档未签名；SHA-256 和 attestation 证明完整性与来源，但不向操作系统证明发布者身份。

决定：Windows 和 Linux 归档保持不签名；macOS 尚未决定，取决于组织持有哪种 Apple 证书（见下）。

| 平台 | 需要 | 说明 |
| --- | --- | --- |
| Windows | 不计划。如需要则用 Authenticode 证书或 Azure Trusted Signing 账号，保存为仓库 secret | 在打包前对 `rivo.exe` 和 `rivo-browser-extension-host.exe` 签名，使归档校验和覆盖已签名文件。可消除下载 ZIP 时的 SmartScreen 警告。 |
| macOS | 未决定。需要 Developer ID Application 证书（Apple Developer Program 组织账号）和公证凭据 | 用 hardened runtime 签名 `rivo` 并公证。裸二进制无法 staple；通过 `curl` 下载的文件没有隔离标记，安装脚本路径不受影响，但浏览器下载的归档会受益。 |
| Linux | 不计划。可选的分离签名（如 cosign） | attestation 已把归档绑定到工作流。 |

Apple 的*企业版*（Enterprise / in-house）证书不适用：它面向组织内部设备分发，不能公证，也不满足 Gatekeeper 对公开下载的要求。普通的 Apple Developer Program 才提供 Developer ID 证书，决定前请先确认组织持有的是哪一种。

没有维护者持有的凭据就无法完成以上工作。凭据就绪后，在 `.github/workflows/release.yml` 的“Build release binaries”与“Package archive”之间加入签名步骤，并扩展 `scripts/release_smoke.py` 验证签名。

[发布手册](releasing.zh-CN.md) | [English](stable-release-plan.md)

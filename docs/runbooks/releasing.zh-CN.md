# 开源版本发布与验收

公开交付对象为 CLI/TUI 归档。发布版本、源码提交、manifest 与解压后二进制版本必须一致。每次发布以相同提交的 GitHub Actions 记录和 `smoke-<platform>-<arch>.json` 为证据；文档和历史测试不能代替当前构件验收。

## 1. 发布前质量门

从仓库根目录执行；Rust 工具链由 `rust-toolchain.toml` 固定，Python 需要 3.11+，扩展测试使用 Node.js 22。`xtask check` 的最低版本检查还需要 `rustup toolchain install 1.88.0 --profile minimal`。

```text
cd runtime
cargo run -p xtask -- check
cd ..
python -m compileall -q scripts packaging
python scripts/check_markdown_links.py
python scripts/check_browser_extension.py
python scripts/package-test.py
node --test packaging/browser-extension/tests/content-script.test.mjs packaging/browser-extension/tests/service-worker.test.mjs
cargo audit --file runtime/Cargo.lock --deny warnings
python scripts/generate_licenses.py --check
```

审计工具安装、允许的许可证及声明再生成步骤见[第三方声明](../../THIRD_PARTY_NOTICES.md)。更新依赖必须同时更新 `licenses/`；归档脚本拒绝与 Cargo.lock 不匹配的声明。CI 在 Windows、Linux、macOS 执行 Rust 与扩展检查，另执行 Rust 1.88 和依赖门禁；Release 复用同一 CI。

## 2. 五个平台的归档验收

| 目标 | 原生构建 runner | 归档 |
| --- | --- | --- |
| Windows amd64 | windows-latest | ZIP |
| Linux amd64 | ubuntu-22.04 | tar.gz |
| Linux arm64 | ubuntu-22.04-arm | tar.gz |
| macOS amd64 | macos-15-intel | tar.gz |
| macOS arm64 | macos-15 | tar.gz |

Release 在各 runner 构建 release 二进制；Linux 另外执行 glibc ABI 检查和 Debian 12 容器 smoke；随后打包并执行 `scripts/release_smoke.py`。该检查解压归档，在隔离配置、数据目录和工作区中验证：

- SHA-256、manifest 平台/版本/大小与实际二进制版本。
- 中英文帮助、schema 6 空配置、两份打包 FTL 与完整第三方声明。
- 归档携带的本机 mock provider：discovery、路由和真实流式请求。
- 本机 HTTP 镜像上的升级清单，以及实际下载归档的摘要。
- Windows 打包 native host 的多实例握手、请求/响应和 EOF 恢复。

各报告随 Release 资产保留。任何失败阻止草稿创建，不允许只验证假二进制就宣布原生构件可用。真实浏览器、交互 TUI、实际模型服务和跨节点 A2A 仍需要相应环境单独验收；默认 Rust 测试中标为 ignored 的项目不算通过。

Windows 系统集成面向 Windows 11 x64。Linux/macOS 提供通用 CLI/TUI，未包含 Windows 专属 Computer Use 和浏览器扩展接线。Linux 使用 GNU/glibc 目标，运行时需要 `libdbus-1.so.3`（Ubuntu 的 `libdbus-1-3`）；源码构建另需 `libdbus-1-dev`、`pkg-config`、C/C++ 工具链和 CMake。Linux 构件在 Ubuntu 22.04 构建，并在 Debian 12 中执行 smoke；Debian 12 的 glibc 2.36 是当前 Linux 最低支持基线。不支持将其当作 musl/Alpine 包。所有归档未做平台代码签名，SHA-256 仅校验完整性。

## 3. 先准备草稿，再公开发布

1. 更新 [CHANGELOG](../../CHANGELOG.md) 的功能、兼容性及已知限制，完成源码和依赖门禁，提交并同步到目标仓库。
2. 在 Actions 的 Release workflow 使用 `workflow_dispatch`，输入 `major.minor.patch` 生成候选；或推送相应 `v<version>` tag。两条路径都只创建 **draft Release**，不会自动成为 latest。
3. 检查五个目标的构件、checksum、manifest 和 smoke 报告，确认其 commit 一致；核对下载说明与限制。相同版本不允许替换已公开的构件，修复须递增版本。
4. 在 GitHub 上核验私密漏洞报告、Secret scanning/push protection、Dependabot，以及主分支和版本 tag 保护。私密报告入口须与 [SECURITY](../../SECURITY.md) 一致。
5. 维护者审核草稿后手动公开。当前版本和升级协议仅支持三段数字版本，manifest 使用 `stable` 通道；尚未实现独立 prerelease 升级通道。首个 0.x 版本仍须明确实验性能力及限制，不能宣称生产稳定。
6. 公开后从实际 Release 下载归档和校验文件复核，验证 `rivo upgrade` 的 GitHub 默认地址；如发布后发现问题，撤下有问题版本的 latest 标记并发布修复版本，不改写原构件。

分支保护应要求当前 CI 的五个检查：三种 `Quality (...)`、`Rust 1.88 minimum version`、`Dependency security and licenses`。设置 tag 规则时须允许维护者/Release workflow 创建新版本，同时禁止改写或删除已发布 tag。

---

[English](releasing.md)

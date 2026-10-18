# 源码配置与测试

所有源码命令从仓库根目录的 `runtime/` 执行，Rust 工具链由 `rust-toolchain.toml` 固定。

## 构建前置条件

安装 rustup。Linux 需要 C/C++ 工具链、CMake、pkg-config 和 D-Bus 头文件；Windows
需要带 C++ 和 Windows SDK 的 Visual Studio Build Tools；macOS 需要 Xcode Command Line
Tools。仓库检查需要 Python 3.11+，浏览器扩展测试需要 Node.js 22。

```powershell
Set-Location runtime
cargo build --workspace --all-targets --locked
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

## 配置本机 Mock 或真实 Provider

不要把 key 提交到仓库或写入命令参数、TOML、日志、截图和报告。完整流程见[配置指南](docs/guides/configuration/README.zh-CN.md)：

```text
cargo run -p rivo -- config credentials set provider-key
cargo run -p rivo -- config vendor list
cargo run -p rivo -- config model list --format table
cargo run -p rivo -- config provider set gateway --base-url https://api.example.com/v1 --credential-name provider-key
cargo run -p rivo -- config validate
cargo run -p rivo -- config provider models refresh gateway
cargo run -p rivo -- config route --vendor <vendor> --model <exact-model-id> --provider-id gateway
```

无凭据确定性检查可使用 `packaging/archive/mock_provider.py` 或发布归档中的副本。真实
Provider 请求可能产生费用。

## 运行 CLI 和 TUI

```text
cargo run -p rivo -- run --prompt "Reply only: RIVO_OK"
cargo run -p rivo -- run --format jsonl --stdin
cargo run -p rivo -- tui
```

TUI 需要交互终端，自动化使用 `rivo run --format jsonl`。Model/Provider 选择、设置、命令、
确认和恢复见 [TUI 指南](docs/guides/tui.zh-CN.md)。

## 确定性检查

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
Set-Location ..
python scripts/check_markdown_links.py
python scripts/check_doc_parity.py
python scripts/package-test.py
python scripts/install-command-test.py
python scripts/check_browser_extension.py
node --test packaging/browser-extension/tests/content-script.test.mjs packaging/browser-extension/tests/service-worker.test.mjs
```

`xtask` 门禁还检查 Rust 1.88 MSRV 和依赖/许可证：

```text
rustup toolchain install 1.88.0 --profile minimal
cargo run -p xtask -- check
```

[English](SOURCE_CONFIGURATION_AND_TESTING.en.md) | [安装指南](docs/guides/one-click-install.zh-CN.md)

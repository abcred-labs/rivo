# Contributing to Rivo

感谢参与 Rivo。提交前请先在 issue 中说明较大的行为变化或公共接口变化。

## 开发环境

- Rust 工具链以 `rust-toolchain.toml` 为准。
- 运行 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo build --workspace --all-targets --locked` 和 `cargo test --workspace --all-targets --locked`。
- 运行 `python scripts/check_markdown_links.py` 检查文档链接。

## Pull Request 要求

- 说明用户可观察的行为变化和兼容性影响。
- 为修复添加覆盖真实行为的测试；不要提交凭据、用户数据、构建产物或本地配置。
- 保持 MIT 和第三方授权声明完整。
- 不要把未经审查的网络端点、CI 密钥或内部基础设施写入源码和文档。

提交即表示你有权在 MIT License 下贡献这些内容。

# 源码配置与测试

所有源码命令从 runtime 目录执行：

~~~powershell
Set-Location runtime
cargo run -p rivo -- --help
cargo run -p rivo -- config credentials set my-provider-key
cargo run -p rivo -- config provider set openai --base-url https://api.openai.com/v1 --credential-name my-provider-key
cargo run -p rivo -- config validate
~~~

Provider key 必须通过 Credential 命令输入，或只在单次运行中由环境变量提供。不要把 key 提交到仓库、命令历史、TOML、日志、截图或测试报告。

## 确定性检查

~~~powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
Set-Location ..
python scripts/check_markdown_links.py
~~~

也可以运行统一质量门禁：

~~~powershell
cargo run -p xtask -- check
~~~

## 本机 Mock Provider

无凭据的 loopback Provider 示例见 packaging/archive/README.md。它是确定性的，不会访问外部模型服务。

## 真实 Provider

配置 Provider 后，先刷新 exact model catalog，再运行任务：

~~~powershell
cargo run -p rivo -- config provider models refresh <provider-id>
cargo run -p rivo -- config route --vendor <vendor> --model <exact-model-id> --provider-id <provider-id>
cargo run -p rivo -- run --vendor <vendor> --model <exact-model-id> --provider-id <provider-id> --prompt "Reply only: RIVO_OK"
~~~

真实 Provider 测试可能产生费用并需要凭据。不要把真实端点或响应写入公开 fixture。

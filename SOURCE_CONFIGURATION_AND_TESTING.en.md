# Source configuration and testing

Run source commands from the repository root's `runtime/` directory. The pinned toolchain is declared by `rust-toolchain.toml`.

## Build prerequisites

Install rustup. Linux needs a C/C++ toolchain, CMake, pkg-config and D-Bus headers; Windows needs Visual Studio Build Tools with C++ and a Windows SDK; macOS needs Xcode Command Line Tools. Python 3.11+ is required for repository checks and Node.js 22 for browser-extension tests.

```powershell
Set-Location runtime
cargo build --workspace --all-targets --locked
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

## Configure a local mock or real Provider

Never commit a key or put it in command arguments, TOML, logs, screenshots or reports. Use the complete [configuration guides](docs/guides/configuration/README.md):

```text
cargo run -p rivo -- config credentials set provider-key
cargo run -p rivo -- config vendor list
cargo run -p rivo -- config model list --format table
cargo run -p rivo -- config provider set gateway --base-url https://api.example.com/v1 --credential-name provider-key
cargo run -p rivo -- config validate
cargo run -p rivo -- config provider models refresh gateway
cargo run -p rivo -- config route --vendor <vendor> --model <exact-model-id> --provider-id gateway
```

For a credential-free deterministic check, use `packaging/archive/mock_provider.py` or the copy included in a release archive. Real-provider requests can incur charges.

## Run CLI and TUI

```text
cargo run -p rivo -- run --prompt "Reply only: RIVO_OK"
cargo run -p rivo -- run --format jsonl --stdin
cargo run -p rivo -- tui
```

TUI requires an interactive terminal. Use `rivo run --format jsonl` in automation. See the [TUI guide](docs/guides/tui.md) for model/provider selection, settings, commands, confirmations and recovery.

## Deterministic checks

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

The `xtask` gate additionally checks the Rust 1.88 MSRV and dependency/license state:

```text
rustup toolchain install 1.88.0 --profile minimal
cargo run -p xtask -- check
```

[Chinese version](SOURCE_CONFIGURATION_AND_TESTING.md) | [Install guide](docs/guides/one-click-install.md)

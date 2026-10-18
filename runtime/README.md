# Rivo runtime workspace

`runtime/` is the Rust Cargo workspace for the Rivo CLI, TUI, Session, Agent, LLM, policy, tools, storage, app-server and A2A crates. The repository is distributed as a CLI/TUI product; it is not a hosted gateway.

## Build and test

Use the pinned toolchain from the repository root:

```text
cd runtime
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
```

Linux development additionally needs D-Bus headers and pkg-config; Windows needs Visual Studio C++ tools and a Windows SDK; macOS needs Xcode Command Line Tools.

## Run

```text
cargo run -p rivo -- --help
cargo run -p rivo -- tui
cargo run -p rivo -- run --format jsonl --prompt "Inspect this workspace."
```

Configure Provider, Credential and exact Model discovery before a real model request. See the [configuration guides](../docs/guides/configuration/README.md) and [TUI guide](../docs/guides/tui.md).

## Workspace layout

- `crates/cli`: command dispatch, terminal lifecycle and app-server host.
- `crates/tui`: state, layout and rendering.
- `crates/session`: config, routing, credentials, persistence and intents/events.
- `crates/agent`, `llm`, `policy`, `tools`, `storage`: execution layers.
- `crates/a2a`, `a2a-server`, `instance-control`: trusted integration surfaces.

[简体中文](README.zh-CN.md)

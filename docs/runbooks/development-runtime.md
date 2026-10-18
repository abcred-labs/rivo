# Development runtime runbook

This runbook is for running Rivo from the repository. It follows the current schema 6 CLI and uses the pinned toolchain in `rust-toolchain.toml`.

## 1. Prerequisites and commands

Install Rust through rustup. Linux builds additionally need a C/C++ toolchain, CMake, pkg-config and D-Bus headers. Windows builds need Visual Studio Build Tools with C++ and a Windows SDK. macOS needs Xcode Command Line Tools. Python 3.11+ is required for repository checks; Node.js 22 is required for browser-extension tests.

Run Cargo commands from `runtime/`:

```powershell
Set-Location runtime
cargo --version
rustc --version
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

When running from source, replace `rivo` below with `cargo run -p rivo --`. The repository quality gate is:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
```

From the repository root also run `python scripts/check_markdown_links.py`,
`python scripts/check_doc_parity.py`, `python scripts/package-test.py`, and the browser-extension checks.

## 2. Configuration and environment

The main configuration must declare `version = 6`. `RIVO_CONFIG` selects an alternate config path; the Credential file remains beside it. `RIVO_CONFIG_CONTENT` supplies a read-only schema 6 snapshot for tests and causes every write to fail without modifying disk.

Inspect authoritative paths with:

```text
cargo run -p rivo -- config path
cargo run -p rivo -- config credentials path
cargo run -p rivo -- sessions path
```

Model selection precedence is CLI flags, then `RIVO_VENDOR`, `RIVO_MODEL` and `RIVO_PROVIDER_ID`, then persisted defaults. `--auto-provider` overrides a configured explicit Provider and conflicts with `--provider-id`. `RIVO_PROFILE` and policy settings remain separate from model routing. Old connection environment variables are rejected.

Credential values must be entered through `config credentials set` or referenced by `--api-key-env` for one run. They must never appear in command history, TOML, logs, screenshots or test reports. The local Credential file is not an OS keychain.

## 3. Configure a Provider and Model

Use the complete flow in the [configuration guides](../guides/configuration/README.md). The minimum pattern is:

```text
cargo run -p rivo -- config credentials set provider-key
cargo run -p rivo -- config provider set gateway --base-url https://api.example.com/v1 --credential-name provider-key
cargo run -p rivo -- config validate
cargo run -p rivo -- config provider models refresh gateway
cargo run -p rivo -- config route --vendor <vendor> --model <exact-model-id> --provider-id gateway
```

Create a Vendor and custom Model when the exact ID is not in the built-in catalog. Model protocol order is explicit and Provider does not carry a protocol. Connection changes require the printed impact confirmation; discovery is never an implicit prerequisite for local configuration writes.

## 4. TUI and headless execution

Run an interactive session with:

```text
cargo run -p rivo -- tui
cargo run -p rivo -- tui --workspace <PATH>
```

TUI requires interactive stdin and stdout. Use `rivo run --format jsonl` for automation. The [TUI guide](../guides/tui.md) documents first-launch configuration, Session Settings, Model/Provider selection, slash commands, confirmations, keyboard shortcuts and recovery.

Headless examples:

```text
cargo run -p rivo -- run --profile research --prompt "Inspect this project and cite file paths."
cargo run -p rivo -- run --profile coding --approval-mode prompt --prompt "Apply the requested change and run its tests."
```

`--network`, `--browser`, `--shell`, MCP and A2A selections are Turn-scoped. `--format jsonl` is stable for scripts; text output is intended for people.

## 5. Workspace and sessions

Workspaces are normalized local directories. `workspace open` registers and selects one; `sessions list` defaults to the current workspace. Use `--all-workspaces` or `--unscoped` explicitly when auditing history. Resume requires the owning workspace and is rejected before routing or tool execution when it does not match.

```text
cargo run -p rivo -- workspace list
cargo run -p rivo -- workspace open <PATH|WORKSPACE_ID>
cargo run -p rivo -- sessions list --workspace <PATH|WORKSPACE_ID>
cargo run -p rivo -- sessions resume <SESSION_ID> --workspace <PATH|WORKSPACE_ID> --prompt "Continue"
```

Schema 17 to 18 Workspace migration is explicit and creates a backup:

```text
cargo run -p rivo -- sessions migrate --yes
cargo run -p rivo -- sessions migrate --backup <BACKUP_PATH> --yes
```

Do not delete the entire data directory to repair an old Session. Follow the persistence compatibility contract and retain Credentials, memory, MCP and A2A trust.

## 6. Diagnostics and controlled reset

```text
cargo run -p rivo -- config show
cargo run -p rivo -- config validate
cargo run -p rivo -- diagnostics create <DESTINATION> --yes
cargo run -p rivo -- sessions backup <BACKUP_PATH>
```

`config show` redacts secrets. A diagnostic bundle can contain redacted business content and must be reviewed before sharing. Session reset accepts only the documented backup/schema input and does not delete configuration or Credential data.

## 7. Removed and unsupported paths

The repository no longer delivers a central supervision service. Do not add supervision configuration or restore old provider-profile/binding/endpoint commands. P3 remains documentation-only. The supported extension surfaces are local instance-control, MCP, A2A and the local app-server.

[Chinese version](development-runtime.zh-CN.md) | [Binary runtime](binary-runtime.md) | [Configuration guides](../guides/configuration/README.md)

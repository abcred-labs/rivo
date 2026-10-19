# Rivo

Rivo is a Rust-based agent runtime with a command-line interface and terminal UI. It turns a natural-language task into an observable, policy-controlled workflow with model routing, tools, confirmations, evidence, and resumable session history.

## Features

- `rivo run` for headless tasks and `rivo tui` for interactive terminal sessions.
- Shared Session/Turn/Event state with SQLite history, cancellation, confirmation, and recovery.
- Policy-controlled filesystem, process, web, browser, MCP, memory, and A2A tools.
- Local app-server integration and trusted local/LAN instance control.
- Redacted diagnostics and explicit model/provider routing.

Rivo is designed for user-visible work in a local workspace. Tool requests are checked by schema, policy, resource limits, and required confirmation before execution. A workspace or Git worktree is not an operating-system sandbox.

The repository does not provide a hosted gateway, multi-tenant execution service, public authentication, or mobile/web clients. Those are separate product surfaces.

## Install a release

Install the latest stable release on Linux or macOS:

```sh
curl -fsSL https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.sh | sudo sh
rivo --version
```

Install the latest stable release on Windows from an elevated PowerShell:

```powershell
irm https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.ps1 | iex
rivo --version
```

The installers verify the release manifest and SHA-256 checksum before installing
the complete archive. They require administrator privileges and install the
system-wide CLI while keeping user configuration, credentials and sessions outside
the installation directory. For fixed-version commands, manual archive installs,
and troubleshooting, see the [installation guide](docs/guides/one-click-install.md).
Available archives and release-specific commands are listed on the [GitHub Releases page](https://github.com/abcred-labs/rivo/releases).

Linux release archives support Debian 12 (glibc 2.36) and newer compatible GNU/glibc systems, and require `libdbus-1.so.3`.

Check for and apply a newer release:

```text
rivo upgrade --check
rivo upgrade --apply
```

`rivo upgrade --apply` asks for confirmation in a terminal, preserves one previous
installation, and requires `--yes` in non-interactive automation.

## Build from source

Prerequisites:

- Git and [rustup](https://rustup.rs/); the repository toolchain is pinned in [rust-toolchain.toml](rust-toolchain.toml).
- Python 3.11+ for repository checks; Node.js 22 for browser-extension tests.
- Windows builds require Visual Studio Build Tools with C++ and a Windows SDK.
- Linux builds require a C/C++ toolchain, CMake, pkg-config and D-Bus headers (Ubuntu: `sudo apt-get install build-essential cmake pkg-config libdbus-1-dev`); running the archive requires `libdbus-1.so.3`. macOS requires Xcode Command Line Tools.

```powershell
Set-Location runtime
cargo build --workspace --all-targets --locked
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

Install the additional Rust 1.88.0 toolchain with `rustup toolchain install 1.88.0 --profile minimal` to run the MSRV check, then run the deterministic quality gate:

```powershell
cargo run -p xtask -- check
```

The build does not contact a model provider and does not require credentials.

## Configure and run

Configure a provider, model, and local credential reference before sending a task. Never put API keys in command arguments, configuration files, logs, or reports.

```powershell
rivo run --prompt "Inspect this repository and summarize its main modules."
rivo tui
```

See [configuration and testing](SOURCE_CONFIGURATION_AND_TESTING.en.md), [architecture](docs/guides/architecture-and-capabilities.md), and [runtime integration](docs/guides/runtime-integration.md).

For interactive terminal usage, see the [TUI guide](docs/guides/tui.md). Provider,
Credential, Vendor and custom Model setup is covered by the
[configuration guides](docs/guides/configuration/README.md). The same pages are
available in [简体中文](README.zh-CN.md) and through the Chinese document links.

## Contributing and security

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request. Report security issues using the process in [SECURITY.md](SECURITY.md); do not disclose credentials in public issues.

Rivo is distributed under the [MIT License](LICENSE). Third-party notices are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

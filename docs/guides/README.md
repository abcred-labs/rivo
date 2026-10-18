# Rivo guides

This directory contains public user documentation for the CLI, TUI and local runtime. Each page has a Simplified Chinese twin with the `.zh-CN.md` suffix.

## Start here

- [Architecture, capabilities and use cases](architecture-and-capabilities.md)
- [Install a release](one-click-install.md)
- [TUI user guide](tui.md)
- [Configuration guides](configuration/README.md)
- [Source configuration and testing](../../SOURCE_CONFIGURATION_AND_TESTING.en.md)
- [Binary runtime](../runbooks/binary-runtime.md)
- [Feature matrix](feature-matrix.md)
- [Runtime integration](runtime-integration.md)
- [Agent cluster workflows](agent-cluster-workflows.md)

## Common tasks

| Task | Entry point |
| --- | --- |
| Run a headless task | [`rivo run`](../../README.md) and the [CLI contract](../architecture/cli.md) |
| Use the interactive terminal | [TUI guide](tui.md) |
| Configure a provider and model | [Configuration guides](configuration/README.md) |
| Diagnose routing or credentials | [Configuration troubleshooting](configuration/troubleshooting.md) |
| Use an extracted release | [Binary runtime](../runbooks/binary-runtime.md) |
| Connect MCP or A2A | [MCP runbook](../runbooks/mcp-remote-smoke.md) and [A2A pairing](../runbooks/a2a-lan-pairing.md) |

Architecture and protocol pages describe implementation contracts. Runbooks describe
repeatable build, release and integration procedures. The command help shipped with
the installed binary remains authoritative for flags and exit behavior.

[简体中文](README.zh-CN.md)

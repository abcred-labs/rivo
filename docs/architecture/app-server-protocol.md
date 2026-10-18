# Local app-server protocol

`rivo app-server --stdio` is a local JSON-RPC bridge. Windows also supports `--pipe`, a current-user named pipe derived from `RIVO_DATA_DIR`. The bridge has bounded messages and never exposes credentials or arbitrary process control.

## Session requests

Hosts submit a bounded turn request, append messages, confirm or reject a request, cancel a Turn, retry a step, inspect sessions, and apply runtime settings. The host owns authentication, transport protection, relay tokens and user identity.

`runtime_settings` may select Vendor, canonical Model, Auto/explicit Provider, capability profile, policy, Web, Browser, MCP toolsets and A2A peers. The server validates the complete selection with the same resolver as CLI/TUI. An unusable value returns `unsupported_setting`; no Model or Provider fallback occurs.

## Configuration RPCs

Model routing preview and delete-plan methods expose Vendor/Model/Provider, protocol, base URL, priority, affected Models, revision and confirmation token. Secrets and raw Credential values never cross the protocol. Configuration writes are atomic and require the same confirmation semantics as CLI.

## Events and errors

The server emits bounded AgentEvents with sequence, Turn identity, diagnostics, confirmations and terminal status. Errors distinguish invalid request, unsupported setting, policy denial, cancellation and runtime failure. The host must preserve event order and present confirmation scope to the user.

## Operational limits

The app-server is a local host bridge, not a public HTTP service or hosted gateway. Restrict its process and pipe boundary, validate workspace ownership, and keep logs free of credentials.

[Local host runbook](../runbooks/app-server-local-host.md) | [Chinese version](app-server-protocol.zh-CN.md)

# Local app-server host runbook

`rivo app-server --stdio` starts a local JSON-RPC bridge. Windows also supports `--pipe`. The host owns authentication, process identity, transport protection, relay tokens and user authorization.

## Start and handshake

Start the binary as a child process with stdin/stdout pipes, exchange the supported protocol/version handshake, and keep stdout reserved for JSON-RPC. Send logs to a separate channel. For named pipes, restrict access to the current user boundary.

## Turn flow

The host submits a bounded turn, receives ordered AgentEvents, displays confirmation requests, and sends Confirm, RespondInteraction or CancelTurn intents. It must preserve request/turn IDs and terminal status. A second active Turn is rejected rather than queued.

## Runtime settings

`runtime_settings.vendor`, `model`, `provider_id` and `auto_provider` select only the local valid catalog. MCP servers/toolsets and A2A peers must be configured and allowlisted. Invalid values return `unsupported_setting`; the server never falls back to another Model or Provider. Model protocol comes from the Model catalog; the host cannot send an arbitrary URL or Credential.

## Recovery and security

Use the same Workspace and Session identity for resume. Handle provider errors, policy denial, confirmation rejection and cancellation separately. Keep Credential values out of requests, logs and diagnostics. Validate the release protocol and schema before integration.

[App-server protocol](../architecture/app-server-protocol.md) | [Chinese version](app-server-local-host.zh-CN.md)

# MCP remote smoke runbook

This procedure verifies a configured MCP server without weakening Rivo policy. It covers stdio and Streamable HTTP, discovery, OAuth or environment-token authentication, allowlists and failure recovery.

## Install and inspect

Use `rivo mcp install http|stdio` with a stable server name and the documented endpoint/command. Installation performs connection, initialize and capability discovery before writing configuration. HTTP headers and bearer tokens must be referenced through environment variables.

```text
rivo mcp list
rivo mcp show <server>
rivo mcp inspect <server>
```

Inspect tools, resources and prompts before enabling a toolset. Discovery is not authorization to execute a write.

## Login and run a smoke task

For OAuth servers use `rivo mcp login <server>` and complete the local browser/redirect flow. For token servers inject the documented environment variable. Select only an enabled server/toolset for a Turn:

```text
rivo run --mcp-server <server> --mcp-toolset <server>/<toolset> --prompt "Read the test resource and summarize it."
```

## Failure and cleanup

Check TLS, endpoint path, redirect port, scope, environment variable, server allowlist and policy when discovery fails. Use `rivo mcp disable <server>` to stop Turn selection and remove only after reviewing dependent configuration. Never include headers, tokens or private resource bodies in smoke reports.

[External capability architecture](../architecture/external-capability-integration.md) | [Chinese version](mcp-remote-smoke.zh-CN.md)

# External capability integration

External integrations are opt-in surfaces with independent trust and data boundaries. Rivo does not provide a hosted gateway or public authentication layer.

## MCP

MCP supports stdio and Streamable HTTP. Installation performs connection, initialize and discovery before writing configuration. HTTP headers use environment-variable references; secrets are not written to configuration or displayed by `list`, `show` or `inspect`. Only configured and enabled servers/toolsets can be selected for a Turn.

## OAuth and tokens

OAuth uses the local browser/redirect flow and stores references and tokens in the protected local credential boundary. API token/PAT fallback reads an environment variable. Loopback is a local fallback only. The host must review the provider's scopes and data policy.

## Discovery and execution

Discovery records server capabilities, tools, resources and prompts with bounded metadata. Tool calls enter the same schema, policy, confirmation, resource-limit and audit pipeline as local tools. Discovery is not authorization to perform a write.

## Verification

Use the MCP remote smoke runbook for HTTP/stdio installation, OAuth login, capability inspection and failure recovery. Verify network address, TLS, redirect port, allowlist and policy before enabling a server.

[MCP runbook](../runbooks/mcp-remote-smoke.md) | [Chinese version](external-capability-integration.zh-CN.md)

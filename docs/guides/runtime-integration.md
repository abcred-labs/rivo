# Runtime integration

Rivo exposes three integration surfaces: an in-process local runtime for a Desktop host, a local app-server bridge, and a trusted A2A node. They reuse Session and routing behavior but have different transport and trust boundaries.

## Local host

A host starts the local app-server with `rivo app-server --stdio` or the Windows named-pipe form. It owns process identity, transport protection, relay tokens and user identity. `runtime_settings` can select a Vendor, canonical Model, Auto/explicit Provider, policy, capability profile, MCP servers and A2A peers. The server validates the complete selection before starting a Turn and returns `unsupported_setting` for unusable values.

## A2A node

A paired client delegates a task to a trusted peer; a server authenticates the peer and runs the task under its local policy and owner approvals. Discovery is only candidate enumeration. Pairing verifies endpoint, SPKI, SAS and public-key material, and revocation removes the peer's trust. PSKs and private keys must come from controlled stdin or OS protected storage, never command arguments or logs.

## Model routing

All surfaces use the same schema 6 resolver. A Model supplies protocol preference; a Provider supplies URL and Credential reference. Remote hosts cannot supply arbitrary URLs or secrets in `runtime_settings`, and an explicit unavailable Provider does not fall back to another.

## Operational checklist

- Configure and validate the local catalog before starting a host.
- Refresh Provider discovery and verify exact Model IDs.
- Restrict MCP/A2A allowlists and policy profiles.
- Keep app-server and A2A endpoints on trusted local/LAN boundaries.
- Record cancellation, confirmation and terminal events for the host's audit trail.
- Test Windows, Linux and macOS transports separately; a TestBackend or protocol fixture does not prove terminal compatibility.

See the [app-server runbook](../runbooks/app-server-local-host.md), [A2A pairing runbook](../runbooks/a2a-lan-pairing.md), and [configuration guides](configuration/README.md).

[Chinese version](runtime-integration.zh-CN.md)

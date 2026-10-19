# Feature matrix: local and remote surfaces

This matrix describes the shipped public surfaces at the current release. A completed implementation is not the same as platform acceptance; use the matching release smoke and CI evidence for a specific version.

| Surface | Status | What is usable | Limits |
| --- | --- | --- | --- |
| CLI | shipped | Headless tasks, JSONL events, configuration, sessions and diagnostics | Requires a configured route for real model calls. |
| TUI | shipped | Interactive tasks, Model/Provider selection, confirmations, history and recovery | Requires interactive stdin/stdout and a compatible terminal. |
| Windows native integrations | shipped for Windows amd64 | Computer Use, browser-extension host and local instance control | Windows 11 x64 boundary; not present in Linux/macOS archives. |
| Linux/macOS CLI/TUI | shipped for listed amd64/arm64 targets | Common CLI/TUI and local tools | Linux supports Debian 12 (glibc 2.36) and newer compatible GNU/glibc systems; D-Bus is required. |
| MCP | shipped with configuration | stdio and Streamable HTTP servers, allowlists and toolsets | Requires explicit local configuration and per-Turn selection. |
| A2A | shipped with pairing | Trusted local/LAN pairing, delegation and local server | Discovery does not establish trust; cross-node acceptance is separate. |
| Local app-server | shipped | stdio or Windows named-pipe host bridge | Host owns identity, transport security and user authorization. |
| Hosted gateway | not delivered | — | No hosted multi-tenant execution or public authentication. |
| Desktop/Web/Mobile clients | separate surfaces | Local or remote host integration points | Not shipped by this repository. |

Model routing uses schema 6: Model owns protocol preference; Provider owns endpoint and Credential reference; authenticated discovery must contain the exact Model ID. Auto Provider sorts priority then ID and has no runtime failover.

Use the [guides](README.md), [configuration pages](configuration/README.md), and [release runbook](../runbooks/releasing.md) for operating details.

[Chinese version](feature-matrix.zh-CN.md)

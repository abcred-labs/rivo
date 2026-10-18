# Architecture, capabilities and use cases

Rivo is a local agent runtime driven by a configured model Provider. It turns a natural-language task into a policy-checked workflow with observable events, confirmations, evidence and resumable history. Local execution does not mean that model context or external tool data stays on the machine; confirm the Provider and service data policies before using private material.

## What it is for

- `rivo run` executes a headless task and can emit stable JSONL for scripts.
- `rivo tui` provides an interactive terminal workspace with routing selectors, overlays, confirmations and history.
- File, process, Web, Browser, MCP, memory, SubAgent and A2A capabilities are enabled only through their policy and trust boundaries.
- The local app-server and instance-control surfaces let another local host reuse Session behavior without introducing a hosted gateway.

Use `research` for read-only investigation, `plan` for a proposed change, `coding` for controlled edits, and `review` for verification. Policy is separate: `strict`, `medium` (default) and `full_control` change confirmation and capability defaults; they never remove schema or resource validation.

## Model, Provider and Credential

Vendor answers who publishes a Model. Model answers which exact canonical ID and protocol preference to use. Provider answers which base URL and priority to use. Credential answers which separately stored authentication reference is available. Create the catalog, configure the connection, refresh discovery and then route an exact ID. A route is unavailable when discovery is stale, disabled, unsupported or missing the exact ID.

## Data and safety

A workspace is a path boundary for Rivo workflows, not an operating-system sandbox. Tool calls are checked by schema, policy, resource limits and confirmation. TTY runs prompt by default; non-interactive runs deny by default. Public JSONL, diagnostics and app-server events redact credentials and bound tool content, while local history may contain business data.

## Getting started

1. Install a matching archive and verify its SHA-256.
2. Run `rivo config credentials set <credential-name>`, configure a Vendor/Model/Provider and refresh discovery.
3. Run `rivo config route` and a minimal request.
4. Start `rivo tui` for interactive work or `rivo run --format jsonl` for automation.
5. Read the [TUI guide](tui.md), [configuration guides](configuration/README.md), and [safety policy](../architecture/safety-policy.md) before enabling external tools.

## Integration boundaries

MCP connects configured external tools and resources. A2A connects explicitly paired peers; discovery is not trust. The app-server is a local JSON-RPC bridge and a host must provide identity, transport protection and authorization. Browser uses a dedicated user-started CDP profile. Hosted gateway, public auth and Web/Mobile clients are outside this repository.

[Chinese version](architecture-and-capabilities.zh-CN.md)

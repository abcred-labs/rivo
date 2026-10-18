# Rivo architecture overview

Rivo is a local runtime with CLI, TUI, app-server and trusted A2A entry points. All entry points drive one Session/Agent loop and share model routing, policy, tools, persistence and telemetry.

## Layers

```text
CLI / TUI / app-server / A2A
        -> Session intents and AgentEvents
        -> Agent loop
        -> LLM / policy / tools / storage / telemetry
```

Front ends cannot call an LLM or tool directly. Session validates selection, policy and confirmation before execution and emits redacted events for rendering or transport.

## Runtime forms

- CLI performs one headless task or JSONL stream.
- TUI provides one active interactive Session, overlays and recovery.
- app-server bridges a local host over stdio or Windows named pipe.
- A2A server accepts authenticated paired tasks and executes them under local policy.

Hosted gateways, public authentication, Desktop UI and Web/Mobile clients are outside this repository.

## Main flow

The user selects a capability profile, policy, workspace and exact Model route. Session prepares a Turn, the Agent loop calls the selected Provider and tools, requests confirmation when required, records evidence and persists events. Cancellation stops the active provider/tool and records one terminal status. History enables inspection and context recovery but does not replay external side effects automatically.

## Data and security

Configuration schema 6 separates Vendor, Model, Provider and Credential. SQLite history and diagnostics may contain business content. Credential values remain in a separate local store and never enter public projections. Workspace boundaries and Git worktrees are not OS sandboxes.

[Interaction surface](interaction-surface.md) | [Safety policy](safety-policy.md) | [Chinese version](overview.zh-CN.md)

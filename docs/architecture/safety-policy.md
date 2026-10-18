# Safety policy contract

Rivo runs locally and applies policy before every tool request. A workspace or Git worktree is not an operating-system sandbox. Provider requests can send task context, and Web, Browser, MCP and A2A add separate external boundaries.

## Capability profiles

`general`, `research`, `plan`, `coding` and `review` select prompt and tool capability defaults. `mcp` remains a compatibility alias. Policy profiles are `strict`, `medium` (default) and `full_control`; `workspace` remains a compatibility alias.

## Decision order

The runtime validates tool schema, selected capability profile, policy profile, workspace and resource limits, then evaluates confirmation requirements. A denial, timeout or cancellation stops the tool and is recorded as a diagnostic event. Higher policy does not disable schema validation, resource limits or audit.

## Confirmation

TTY runs default to prompting; non-interactive runs default to deny. `--approval-mode manifest` accepts only a version 1 JSON manifest whose canonical workspace matches the run and whose rule binds a tool name, canonical argument hash, `scope: once` and a call limit. There is no universal approval switch. TUI overlays display the exact request and scope.

## Web, MCP and Browser

`--network`, `--browser`, `--mcp-server` and `--mcp-toolset` enable capabilities for the current Turn only. MCP entries must already be configured and allowlisted. Browser uses a dedicated user-started CDP profile. Windows browser extension and Computer Use integration are not provided by Linux/macOS archives.

## Credentials and persistence

Credential values are managed separately and never enter public events, JSONL, diagnostics or ordinary configuration. SQLite history and logs may contain business content and should be treated as sensitive. Diagnostic exports and backups require explicit user action.

[CLI contract](cli.md) | [Tool runtime](tool-runtime.md) | [Chinese version](safety-policy.zh-CN.md)

# Tool runtime contract

Tools are registered by capability and executed through one policy pipeline. The Agent cannot bypass schema validation, workspace/resource limits, confirmation, cancellation or audit.

## Registration

Read tools inspect workspace files and directories. Coding tools include patch, write, move, delete and structured process execution. Web, Browser, MCP, memory, SubAgent and A2A tools register only when their Turn options, configuration and policy allow them.

## Observation

Every tool call has a bounded activity record with tool name, status, duration and redacted result summary. Public JSONL and app-server output never contains Credential values or unbounded tool bodies. Local SQLite history may contain bounded business content.

## Local and process tools

Paths are normalized against the workspace policy. Process execution uses structured program/argument fields and applies timeout, output and resource limits. Raw shell execution is a separate confirmation-protected coding capability and is not an automatic permission.

## Web, MCP and Browser

Web is enabled per Turn with `--network`. MCP servers and toolsets must be configured and allowlisted; discovery does not grant write authorization. Browser uses a user-started loopback CDP profile and keeps page content outside ordinary Session text history. Windows browser-extension integration is platform-specific.

## Cancellation and cleanup

Cancellation propagates to the provider/tool, closes bounded queues and records one terminal event. Workers are joined before terminal cleanup. A tool failure is distinct from a policy denial, confirmation rejection or cancellation.

[Safety policy](safety-policy.md) | [Chinese version](tool-runtime.zh-CN.md)

# Interaction surface and Session contract

CLI, TUI and the local app-server are front ends over one Session and Agent runtime. A front end sends serializable intents and renders redacted AgentEvents; it does not call LLM, policy, tools, storage or telemetry directly.

## Concurrency and identity

A Session contains multiple turns and messages. At most one Turn is active or waiting for confirmation in a Session. A second submit is rejected rather than queued. SessionId, TurnId, StepId, ToolCallId, RequestId and EvidenceId are stable identifiers; events have a monotonic per-Turn sequence and UTC timestamp.

## Intents

The public intent set includes SubmitTask, AppendMessage, Confirm, CancelTurn, RetryStep, SetTurnOptions, SelectModel, ConfirmMemory, RejectMemory and DeleteMemory. `SelectModel` accepts a Vendor/Model and Provider mode `auto` or explicit ID. The resolver validates the complete selection before later Turns change; an active Turn and retry retain their snapshot.

There is no pause, skip or bulk confirmation in V1. Session-scope confirmation reuses only the same risk category and boundary; it is not “allow everything in this Session”.

## Events and redaction

Events cover start, prompt preparation, model deltas, final answer, tool activity, confirmation requests, diagnostics, memory decisions, cancellation and terminal status. CLI JSONL and app-server projections include bounded redacted summaries. Tool argument hashes may be public; credentials and raw secrets never are.

## Front-end matrix

| Surface | Input | Output | Persistence |
| --- | --- | --- | --- |
| CLI | prompt/stdin and flags | text or JSONL | one task Session and Turn |
| TUI | keyboard, slash commands and overlays | live conversation, drawers and confirmations | resumable local Session |
| app-server | bounded JSON-RPC intents | structured events | host-owned Session |

All surfaces use the same route resolver, policy decision and cancellation semantics. Recovery reuses history for context and inspection; it does not imply that external side effects are automatically replayed.

[CLI contract](cli.md) | [TUI guide](../guides/tui.md) | [Chinese version](interaction-surface.zh-CN.md)

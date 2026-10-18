# Agent loop engineering contract

The Agent loop converts Session intents into bounded model/tool steps and emits ordered AgentEvents. Session owns cancellation, confirmation, budgets, persistence and terminal status; the UI only renders the result.

## State machine

A Turn moves from prepared to running, may wait for confirmation or interaction, and ends once in completed, failed or cancelled. A second active Turn in the same Session is rejected. Events include step identity and sequence so clients can recover the visible state.

## Budgets and compression

The loop applies turn, tool, output and subagent budgets. When context approaches its limit, it compresses bounded history while preserving the current task, policy, tool results needed for the next step and evidence references. Compression is not permission to discard safety decisions.

## Failure and retry

Provider, tool, policy and persistence failures have distinct diagnostics. `RetryStep` is accepted only for a retryable step and keeps the original Turn snapshot. Cancellation propagates to the active provider/tool, closes queues and records one terminal cancelled event.

## Stop order

Stop new intents, cancel the active operation, drain bounded event queues, persist the terminal event, join worker threads and restore terminal state. Cleanup errors cannot replace the original failure.

Post-V1 work such as pause/skip and parallel Turns is outside the current contract.

[Interaction surface](interaction-surface.md) | [Chinese version](loop-engineering.zh-CN.md)

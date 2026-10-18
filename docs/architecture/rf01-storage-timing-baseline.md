# RF-01 storage timing and failure baseline

RF-01 records the measurable storage and cancellation baseline for the local runtime. It is evidence for release acceptance, not a public latency guarantee.

## Completed baseline

The current implementation uses bounded queues, a dedicated storage worker, ordered event sequences, atomic configuration writes and explicit terminal status. Session persistence failures become diagnostics and do not silently report success.

## Remaining gates

Release evidence must measure scheduling delay, cancellation response, queue saturation, SQLite write latency and recovery after injected storage failure on each supported runner. Measurements must identify build commit, OS, workload and whether the result came from a real terminal or a fixture.

## Failure injection

Inject provider timeout, tool cancellation, queue closure, SQLite interruption and malformed persisted input. Verify that the original error is preserved, workers are joined, no duplicate terminal event is emitted, and configuration/data files remain recoverable.

## Gate conditions

A candidate passes only when the measured reports accompany the same release commit, all supported platforms complete the required smoke checks, and documented limits are updated when evidence changes.

[Release runbook](../runbooks/releasing.md) | [Chinese version](rf01-storage-timing-baseline.zh-CN.md)

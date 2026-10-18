# Cross-platform TUI terminal architecture

The TUI crate owns state reduction, layout and ratatui rendering. The CLI owns terminal I/O, capability detection and process lifecycle. The TUI never calls LLM, policy, tools, storage or telemetry directly; it sends Session intents and renders redacted AgentEvents.

## Capability detection

Startup records interactive input/output, Unicode, colors, bracketed paste, alternate screen and resize support. Windows uses standard handles and checks `CONIN$`/`CONOUT$` when the standard handles are unavailable, which keeps ConPTY support separate from ordinary pipe execution. Linux and macOS use native terminal streams and `TERM`/`COLORTERM` hints.

TUI refuses to start when stdin or stdout is not interactive. Automation uses `rivo run --format jsonl` instead.

## Lifecycle and cleanup

The terminal adapter enters raw mode and alternate screen, enables bracketed paste when available, and restores every changed terminal property on normal exit, error and panic cleanup. Cleanup is idempotent and its error cannot replace the original failure. The input reader is a single bounded thread; queues are closed before the reader is joined.

## Platform tests

Unit tests use ratatui TestBackend and injected key events. Platform tests use portable-pty for Windows ConPTY and Unix PTYs. Release acceptance separately exercises Windows Terminal/PowerShell, Linux terminals and macOS Terminal/iTerm2. Passing a TestBackend test does not prove compatibility with every terminal emulator.

[User-facing TUI guide](../guides/tui.md) | [Chinese version](cross-platform-tui-terminal.zh-CN.md)

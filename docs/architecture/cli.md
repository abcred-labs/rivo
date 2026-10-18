# CLI contract

`rivo run --prompt <TEXT> [OPTIONS]` and `rivo run --stdin [OPTIONS]` are the stable headless entry points. With no command, Rivo prints help. Positional task text is not accepted.

## Input and output

- `--prompt` requires non-empty task text; `--stdin` reads non-empty UTF-8 input and conflicts with `--prompt`.
- Default `--format text` writes model text to stdout and diagnostics to stderr.
- `--format jsonl` emits one redacted public SessionEvent projection per line with schema version, Session/Turn IDs, sequence and UTC timestamp. Tool arguments are represented by hashes; tool bodies and secrets are not emitted.
- Exit codes are `0` for completion, `1` for run failure/cancellation, `2` for argument/config/startup errors, and `3` for authorization denial.
- Media tools report local asset readiness on stderr in text mode and emit `media_started`, `media_ready` and `media_failed` events in JSONL mode.

## Help and argument errors

`rivo <command> --help` and nested `--help` are side-effect free: they do not create or migrate configuration, open the keychain, read stdin, access the network, or start a service. Invalid or missing arguments return `2` and write usage to stderr. Help output is localized through the bundled language catalog while machine fields and error codes remain stable.

## Model routing commands

The schema 6 routing surface is:

```text
rivo config vendor list|show|set|remove
rivo config model list|show|set|enable|disable|providers|remove
rivo config provider list|show|set|models|remove
rivo config credentials path|show|set|unset|migrate
rivo config route --vendor <ID> [--model <ID>] [--provider-id <ID>]
```

Model protocol is selected from the first item in its ordered `supported_protocols` list. Provider stores the URL and Credential reference and never stores a protocol. `config model list` supports `--vendor`, `--state enabled|disabled|all`, and `--format text|table|json`. `enable` and `disable` accept multiple IDs; unknown IDs are reported without losing valid atomic updates.

`config provider set` requires `--base-url`, can accept `--credential-name`, `--enabled`, `--priority`, and `--refresh`, and rejects `--protocol`. URL or Credential changes print an impact plan and require the same command repeated with the fresh `--confirm` token. Removal requires interactive confirmation or `--yes`. Only `provider models remote` and `provider models refresh` contact the remote directory.

`config route` is offline. It returns Vendor, Model, Provider, protocol, base URL and priority. An explicit unusable Provider or disabled Model fails; Rivo does not silently choose a different Model or Provider. Auto Provider sorting is Provider priority descending then ID ascending.

Runtime Model selection has precedence CLI flags, then `RIVO_VENDOR`/`RIVO_MODEL`/`RIVO_PROVIDER_ID`, then persisted defaults. `--auto-provider` conflicts with `--provider-id`. `rivo run --api-key-env <ENV>` is the only runtime Credential override; its non-empty value wins over the Provider reference for that process.

## Authorization and TUI

Non-interactive runs default to deny. Interactive TTY runs default to prompt. Automation must use `--approval-mode manifest --approval-manifest <path>` with a version 1 manifest whose canonical workspace matches the run. There is no blanket approval flag.

`rivo tui [--workspace <PATH>]` requires interactive stdin and stdout. TUI uses the same Session resolver as CLI and applies Model/Provider changes only to later Turns. Use [the TUI guide](../guides/tui.md) for commands and keyboard behavior.

## Workspaces and sessions

A workspace owns Sessions, Turns, Messages and Events. `workspace open` registers a normalized path and makes it default. Session list scope flags are mutually exclusive; cross-workspace resume is rejected before routing or tool execution. Schema 17 to 18 migration requires an explicit backup and `sessions migrate --yes`.

Destructive history operations require a second confirmation interactively or `--yes` in automation. Credentials, memory, MCP, A2A trust and non-session data are not deleted by session migration or controlled reset.

[Chinese version](cli.zh-CN.md)

# TUI user guide

Rivo's terminal UI is an interactive client for the same Session and model-routing
runtime used by `rivo run`. It is intended for a real terminal. It does not provide
a background service and it does not bypass policy or confirmation checks.

## Start a session

```text
rivo tui
rivo tui --workspace <PATH>
```

Both stdin and stdout must be interactive terminals. Use
`rivo run --format jsonl` for CI, pipes, or other automation. `--workspace` selects
the workspace for this process; it does not silently change the persisted default
workspace when an existing configuration is used.

Before the first task, verify the local installation and configuration:

```text
rivo --version
rivo config validate
rivo config provider list
rivo config model list --state enabled --format table
```

## First-launch configuration

When no valid Provider is configured, TUI opens a first-launch form. The form keeps
credentials out of the draft and follows this order:

1. Enter or select a Credential reference. The secret is entered through the
   dedicated masked prompt or supplied with `config credentials set NAME --from-env ENV`.
2. Enter a Provider ID and base URL. The URL is a request prefix, normally ending
   in `/v1`; do not paste `/chat/completions`, `/responses`, or `/messages`.
3. Confirm whether to save and run discovery. Cancelling before confirmation leaves
   the draft unsaved. A Provider already saved is not rolled back if a later
   discovery or model selection step fails.
4. Select Vendor, exact canonical Model ID, and either Auto or an explicit Provider.

Discovery is required before a route can run. TUI never treats a model merely
listed in a local catalog as reachable.

## Selecting a Model and Provider

Press `F6` or run `/settings` to open Session Settings. In Browse mode, use arrow
keys or Tab to move between fields. Press Enter to edit a field; use arrows for
enumerated fields, Space for booleans and multi-select fields, and type directly
in text or numeric fields. Press Enter to commit a field and `Ctrl+Enter` to apply
all settings to subsequent turns. Esc cancels the current field or closes the panel.

The routing fields are:

- Vendor: the model publisher catalog entry.
- Model: the exact canonical ID; aliases and prefix matching are not used.
- Provider: `Auto` or an explicit Provider ID.
- Reasoning effort: only values supported by the selected Model are offered.

`/model` opens the Model field. `/model <canonical-model-id>` validates and selects
the Model for later turns. `/provider` opens the Provider field. `/provider auto`
selects Auto, while `/provider <provider-id>` selects a specific Provider. An
invalid explicit Provider or unavailable Model is rejected without fallback.

Auto chooses an enabled, refreshed Provider by Provider priority descending and ID
ascending. It does not fail over after a request has started. The selected Model's
first `supported_protocols` entry determines the request protocol.

## Turn controls and commands

The input line accepts ordinary task text. A leading `/` opens the command palette
when the input is empty. The following commands are available:

| Command | Effect |
| --- | --- |
| `/help` | Show the command list. |
| `/new` | Start a new Session after the current Turn is finished. |
| `/cancel` | Cancel the active Turn. |
| `/retry` | Retry the most recent retryable step. |
| `/errors [filter]` / `/errors clear` | Open or clear diagnostics. |
| `/permissions` / `/permissions clear` | Inspect or revoke Session grants. |
| `/sessions` | Open history; use `/sessions resume ID` to resume. |
| `/sessions delete ID` | Request a second confirmation before deleting history. |
| `/workspace` / `/workspace open PATH_OR_ID` | List or switch workspaces. |
| `/network on\|off` | Toggle Web for the current Turn. |
| `/browser` / `/browser on\|off\|status` | Inspect or toggle the CDP Browser tool. |
| `/mcp NAME` / `/mcp off` | Select an enabled MCP server or disable MCP. |
| `/a2a PEER` / `/a2a off` | Select a paired A2A peer or disable A2A. |
| `/subagents on\|off` | Toggle internal SubAgent delegation. |
| `/trace` | Toggle tracing for the current Turn. |
| `/memory` / `/memory delete ID` | Open memory evidence or delete a memory. |
| `/language` / `/language LOCALE` / `/language reload` | Inspect, switch, or reload UI language. |
| `/goal`, `/goal status`, `/goal pause`, `/goal resume`, `/goal clear` | Manage the local goal when goal support is enabled. |
| `/exit` or `/quit` | Exit after an active Turn has reached a terminal state. |

MCP and A2A commands only accept configured and allowed entries. They cannot add a
new server or trust a peer from inside a Turn.

## Keyboard reference

| Key | Action |
| --- | --- |
| `F1` or `/` on an empty input | Open the command palette. |
| `?` | Open help. |
| `F2` | Open errors. |
| `F3` | Open permissions. |
| `F4` | Open history. |
| `F6` | Open Session Settings. |
| `Tab` / `Shift+Tab` | Cycle focus or fields. |
| Arrow keys | Move focus, selection, or scroll. |
| `PageUp` / `PageDown` / `End` | Scroll the focused view. |
| `Enter` | Submit text, confirm a dialog, or edit the selected setting. |
| `Esc` | Close an overlay, cancel editing, or reject a confirmation. |
| `q` with an empty input | Quit when idle; cancel the active Turn first. |
| `Ctrl+C` | Cancel startup or the active operation. |
| `Delete` in history | Start the second-step history deletion confirmation. |

Confirmation overlays accept `y` to allow, `n` or Esc to reject, and `a` only when
the displayed Session scope is available. A confirmation applies only to the
request and scope shown by the overlay; it is not a blanket authorization.

## Sessions and recovery

`/sessions` opens history for the current Workspace. Select an entry and press
Enter to resume, or use `/sessions resume <SESSION_ID>`. A Session must be resumed
from its owning Workspace. A running Turn must be cancelled before switching
Workspace, creating a new Session, resuming another Session, or exiting.

Use `/errors` when a Turn fails. The diagnostic view keeps the TUI usable after a
failure and does not turn the whole workspace into a permanent error screen.

## Platform limits

Windows uses ConPTY when available and falls back to console handles only when
standard handles are unavailable. Linux and macOS use the native terminal streams.
The terminal adapter restores raw mode, alternate screen, bracketed paste and the
cursor on normal exit, errors and panic cleanup. A terminal emulator or redirect
that does not provide interactive input/output cannot run TUI safely.

The Windows release also contains Computer Use and browser-extension integration;
Linux and macOS archives provide the common CLI/TUI surface without those Windows
integrations. See the platform runtime and installation guides for archive and
system-library requirements.

Related pages: [configuration guides](configuration/README.md),
[model routing](configuration/model-routing.md), and
[binary runtime](../runbooks/binary-runtime.md).

---

[简体中文](tui.zh-CN.md)
